//! Coverage grid reconstruction, metadata, and streaming formats.

mod overlay;
mod report;
pub use overlay::{build_route_overlay, CoverageOverlay, MAX_COVERAGE_IMAGE_SIDE};

use crate::{invalid, limited, numeric, project, rows, ExportError, Transform};
use serde::Serialize;
use serde_json::{json, Value};
use std::{collections::HashMap, io::Write};

const STATES: [&str; 6] = [
    "covered",
    "uncovered",
    "unknown_terrain",
    "unresolved",
    "not_evaluated",
    "outside_area",
];
const PREDICTION_WARNING: &str =
    "Terrain-based RF planning estimate; not measured field coverage or a delivery guarantee.";

pub type Cell = Result<Value, ExportError>;

/// The manifest stays borrowed, with validated grid parameters and a projected
/// copy of the optional drawn ring. Transforms must match its terrain CRS.
pub struct CoverageGrid<'a> {
    job: &'a Value,
    reverse: Transform<'a>,
    forward: Transform<'a>,
    bounds: [f64; 4],
    rows: usize,
    columns: usize,
    requested: usize,
    size: f64,
    ring: Vec<[f64; 2]>,
    projected_ring: Vec<[f64; 2]>,
}

impl<'a> CoverageGrid<'a> {
    pub fn new(
        job: &'a Value,
        reverse: Transform<'a>,
        forward: Transform<'a>,
    ) -> Result<Self, ExportError> {
        let count = |value: &Value| value.as_u64().and_then(|n| usize::try_from(n).ok());
        let rows = count(&job["rows"]).unwrap_or(0);
        let columns = count(&job["columns"]).unwrap_or(0);
        let requested = count(job.get("requested_cells").unwrap_or(&job["total"])).unwrap_or(0);
        if !(1..=16_384).contains(&requested) || rows.checked_mul(columns) != Some(requested) {
            return Err(invalid("Saved coverage grid dimensions are inconsistent"));
        }
        let bounds: [f64; 4] = numeric(&job["grid_bounds_projected"])
            .filter(|[left, bottom, right, top]| left < right && bottom < top)
            .ok_or_else(|| invalid("Coverage grid has invalid projected bounds"))?;
        let size = job["effective_cell_size_m"]
            .as_f64()
            .filter(|size| *size > 0.0)
            .ok_or_else(|| invalid("Coverage grid has invalid spacing"))?;
        if job["job_id"].as_str().is_none() || job["terrain_crs"].as_str().is_none() {
            return Err(invalid(
                "Coverage run predates export metadata; recalculate before exporting",
            ));
        }

        let raw_ring = crate::rows(&job["settings"]["area_polygon_wgs84"]);
        let mut ring = Vec::new();
        let mut projected_ring = Vec::new();
        if job["settings"]["area_mode"] == "drawn" && raw_ring.len() >= 4 {
            for point in raw_ring {
                let point =
                    numeric(point).ok_or_else(|| invalid("Invalid drawn coverage coordinate"))?;
                ring.push(point);
                projected_ring.push(project(forward, point)?);
            }
        }
        Ok(Self {
            job,
            reverse,
            forward,
            bounds,
            rows,
            columns,
            requested,
            size,
            ring,
            projected_ring,
        })
    }

    /// Visit one stored or synthesized cell at a time. The flag identifies
    /// persisted rows, including persisted outside-area cells.
    pub fn visit(
        &self,
        cells: impl IntoIterator<Item = Cell>,
        mut visit: impl FnMut(&Value, bool) -> Result<(), ExportError>,
    ) -> Result<(), ExportError> {
        let mut cells = cells.into_iter();
        let mut current = cells.next().transpose()?;
        for index in 0..self.requested {
            let stored_index = current
                .as_ref()
                .map(|cell| {
                    cell["index"]
                        .as_u64()
                        .ok_or_else(|| invalid("Invalid coverage cell index"))
                })
                .transpose()?;
            if stored_index.is_some_and(|stored| stored < index as u64) {
                return Err(invalid("Coverage result cells are not strictly ordered"));
            }
            if stored_index == Some(index as u64) {
                if let Some(cell) = current.take() {
                    state(&cell)?;
                    visit(&cell, true)?;
                }
                current = cells.next().transpose()?;
            } else {
                let x =
                    self.bounds[0] + (index % self.columns) as f64 * self.size + self.size / 2.0;
                let y =
                    self.bounds[3] - (index / self.columns) as f64 * self.size - self.size / 2.0;
                let [longitude, latitude] = project(self.reverse, [x, y])?;
                let [left, bottom, right, top] = self.bounds;
                let inside = if self.projected_ring.is_empty() {
                    x >= left && x <= right && y >= bottom && y <= top
                } else {
                    intersects(&self.projected_ring, [x, y])
                };
                visit(
                    &json!({
                        "index": index, "x": x, "y": y, "longitude": longitude, "latitude": latitude,
                        "state": if inside { "not_evaluated" } else { "outside_area" },
                        "source_count": 0, "unknown_sources": 0, "unresolved_sources": 0,
                        "best_margin_db": null, "best_source_id": null, "sources": [],
                    }),
                    false,
                )?;
            }
        }
        if current.is_some() {
            return Err(invalid("Coverage result contains more cells than its grid"));
        }
        Ok(())
    }

    pub fn summarize(
        &self,
        cells: impl IntoIterator<Item = Cell>,
    ) -> Result<CoverageCounts, ExportError> {
        let mut counts = CoverageCounts {
            requested_cells: self.requested,
            ..CoverageCounts::default()
        };
        self.visit(cells, |cell, stored| {
            counts.stored_cells += usize::from(stored);
            let state = state(cell)?;
            counts.state_counts[state] += 1;
            counts.unknown_source_evaluations = counts
                .unknown_source_evaluations
                .checked_add(cell["unknown_sources"].as_u64().unwrap_or(0))
                .ok_or_else(|| invalid("Coverage evaluation count overflow"))?;
            counts.unresolved_source_evaluations = counts
                .unresolved_source_evaluations
                .checked_add(cell["unresolved_sources"].as_u64().unwrap_or(0))
                .ok_or_else(|| invalid("Coverage evaluation count overflow"))?;
            Ok(())
        })?;
        Ok(counts)
    }

    pub fn metadata(&self, counts: &CoverageCounts, options: MetadataOptions<'_>) -> Value {
        let MetadataOptions {
            stale,
            target_report,
            exported_at,
        } = options;
        let job = self.job;
        let empty = json!([]);
        let settings = &job["settings"];
        let fingerprint = job.get("terrain_fingerprint").unwrap_or(&empty);
        let terrain = rows(fingerprint).iter().filter_map(|entry| {
            let [path, size, modified] = <&[Value; 3]>::try_from(rows(entry)).ok()?;
            let path = path.as_str()?;
            let filename = path.rsplit('/').find(|part| !part.is_empty()).unwrap_or(path);
            Some(json!({
                "kind": if path.split('/').any(|part| part == "dom") { "surface" } else { "ground" },
                "filename": filename, "size_bytes": size, "modified_ns": modified,
            }))
        }).collect::<Vec<_>>();
        let partial = job["state"] != "complete" || counts.has_incomplete_cells();
        let mut metadata = json!({
            "schema": "repeaternet-coverage-export-v1", "job_id": job["job_id"],
            "job_state": job["state"], "complete": !partial && !stale, "partial": partial, "stale": stale,
            "terrain_fingerprint": fingerprint, "coordinate_reference_system": "OGC:CRS84",
            "terrain_inputs": terrain, "requested_cell_size_m": settings["cell_size_m"],
            "profile_step_m": settings["profile_step_m"], "maximum_profile_samples": settings["maximum_profile_samples"],
            "mode": settings["mode"], "coverage_model_version": job["model_version"],
            "source_ids": job.get("source_ids").unwrap_or(&empty),
            "source_sites": job.get("source_sites").unwrap_or(&empty),
            "router_ids": job.get("router_ids").unwrap_or(&empty),
            "network_sites": job.get("network_sites").or_else(|| job.get("source_sites")).unwrap_or(&empty),
            "network_links": job.get("report_links").or_else(|| job.get("network_links")).unwrap_or(&empty),
            "counts": counts, "prediction_warning": PREDICTION_WARNING,
        });
        for key in [
            "project_id",
            "alternative_id",
            "route_search_complete",
            "achieved_path_count",
            "requested_path_count",
            "snapshot_version",
            "input_revision",
            "created_at",
            "finished_at",
            "route_fingerprint",
            "terrain_crs",
            "area_bounds_wgs84",
            "grid_bounds_projected",
            "rows",
            "columns",
            "effective_cell_size_m",
            "surface_available",
            "settings",
            "radio_settings",
        ] {
            metadata[key] = job[key].clone();
        }
        if let Some(report) = target_report {
            metadata["target_report_id"] = report["report_id"].clone();
            metadata["target_count"] = json!(rows(&report["targets"]).len());
        }
        if let Some(exported_at) = exported_at {
            metadata["exported_at"] = json!(exported_at);
        }
        metadata
    }

    fn corners(&self, cell: &Value) -> Result<[[f64; 2]; 4], ExportError> {
        let x = cell["x"]
            .as_f64()
            .ok_or_else(|| invalid("Missing coverage cell x"))?;
        let y = cell["y"]
            .as_f64()
            .ok_or_else(|| invalid("Missing coverage cell y"))?;
        let [left, bottom, right, top] = self.bounds;
        let half = self.size / 2.0;
        let x0 = left.max(x - half);
        let x1 = right.min(x + half);
        let y0 = bottom.max(y - half);
        let y1 = top.min(y + half);
        Ok([
            project(self.reverse, [x0, y0])?,
            project(self.reverse, [x1, y0])?,
            project(self.reverse, [x1, y1])?,
            project(self.reverse, [x0, y1])?,
        ])
    }
}

/// The drawn area is a normalized simple ring. Include its boundary (as
/// intersects_xy does) and use even-odd crossings for concave rings.
fn intersects(ring: &[[f64; 2]], [x, y]: [f64; 2]) -> bool {
    let mut inside = false;
    for (&[ax, ay], &[bx, by]) in ring.iter().zip(ring.iter().cycle().skip(1)) {
        let cross = (x - ax) * (by - ay) - (y - ay) * (bx - ax);
        if cross == 0.0 && x >= ax.min(bx) && x <= ax.max(bx) && y >= ay.min(by) && y <= ay.max(by)
        {
            return true;
        }
        if (ay > y) != (by > y) && x < (bx - ax) * (y - ay) / (by - ay) + ax {
            inside = !inside;
        }
    }
    inside
}

fn state(cell: &Value) -> Result<usize, ExportError> {
    STATES
        .iter()
        .position(|state| cell["state"] == *state)
        .ok_or_else(|| invalid(format!("Unknown coverage cell state: {}", cell["state"])))
}

#[derive(Default, Debug)]
pub struct CoverageCounts {
    pub requested_cells: usize,
    pub stored_cells: usize,
    pub unknown_source_evaluations: u64,
    pub unresolved_source_evaluations: u64,
    state_counts: [usize; 6],
}

impl CoverageCounts {
    pub fn state_count(&self, state: &str) -> Option<usize> {
        STATES
            .iter()
            .position(|candidate| candidate == &state)
            .map(|i| self.state_counts[i])
    }

    pub fn has_incomplete_cells(&self) -> bool {
        self.state_counts[3] > 0 || self.state_counts[4] > 0
    }
}

impl Serialize for CoverageCounts {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let [covered, uncovered, unknown, unresolved, not_evaluated, outside] = self.state_counts;
        json!({
            "requested_cells": self.requested_cells, "stored_cells": self.stored_cells,
            "evaluated_cells": covered + uncovered, "covered_cells": covered,
            "unknown_cells": unknown, "unresolved_cells": unresolved,
            "not_evaluated_cells": not_evaluated, "outside_area_cells": outside,
            "unknown_source_evaluations": self.unknown_source_evaluations,
            "unresolved_source_evaluations": self.unresolved_source_evaluations,
            "state_counts": STATES.into_iter().zip(self.state_counts).collect::<HashMap<_, _>>(),
        })
        .serialize(serializer)
    }
}

#[derive(Default)]
pub struct MetadataOptions<'a> {
    pub stale: bool,
    pub target_report: Option<&'a Value>,
    pub exported_at: Option<f64>,
}

/// A compatible saved height-run comparison and an optional saved failure
/// scenario. Renderer preserves their supplied wire values.
#[derive(Default)]
pub struct CoverageAttachments<'a> {
    pub target_report: Option<&'a Value>,
    pub scenario_comparison: Option<&'a Value>,
    pub failure_scenario: Option<&'a Value>,
}

#[derive(Debug, Clone, Copy)]
pub enum CoverageFormat {
    GeoJson,
    Json,
    Html,
}

impl CoverageFormat {
    pub fn media_type(self) -> &'static str {
        match self {
            Self::GeoJson => "application/geo+json",
            Self::Json => "application/json",
            Self::Html => "text/html",
        }
    }

    pub fn filename(self, job_id: &str) -> String {
        let extension = match self {
            Self::GeoJson => "geojson",
            Self::Json => "json",
            Self::Html => "html",
        };
        format!("predicted-mesh-coverage-{job_id}.{extension}")
    }
}

pub fn write_coverage(
    output: impl Write,
    format: CoverageFormat,
    grid: &CoverageGrid<'_>,
    cells: impl IntoIterator<Item = Cell>,
    metadata: &Value,
    attachments: &CoverageAttachments<'_>,
) -> Result<(), ExportError> {
    limited(output, |out| match format {
        CoverageFormat::GeoJson => {
            write_geojson(out, grid, cells, metadata, attachments.target_report)
        }
        CoverageFormat::Json => write_json(out, grid, cells, metadata, attachments),
        CoverageFormat::Html => report::write_html(out, grid, cells, metadata, attachments),
    })
}

fn write_json(
    out: &mut dyn Write,
    grid: &CoverageGrid<'_>,
    cells: impl IntoIterator<Item = Cell>,
    metadata: &Value,
    attachments: &CoverageAttachments<'_>,
) -> Result<(), ExportError> {
    for (key, value) in [
        ("{\"metadata\":", Some(metadata)),
        (",\"target_report\":", attachments.target_report),
        (",\"scenario_comparison\":", attachments.scenario_comparison),
    ] {
        out.write_all(key.as_bytes())?;
        serde_json::to_writer(&mut *out, &value)?;
    }
    if let Some(scenario) = attachments.failure_scenario {
        write!(out, ",\"failure_scenario\":")?;
        serde_json::to_writer(&mut *out, scenario)?;
    }
    write!(out, ",\"cells\":[")?;
    grid.visit(cells, |cell, _| {
        if cell["index"] != 0 {
            write!(out, ",")?;
        }
        serde_json::to_writer(&mut *out, cell)?;
        Ok(())
    })?;
    write!(out, "]}}")?;
    Ok(())
}

fn outcomes(report: &Value) -> HashMap<&str, &Value> {
    rows(&report["targets"])
        .iter()
        .filter_map(|item| Some((item["id"].as_str()?, item)))
        .collect()
}

fn write_geojson(
    out: &mut dyn Write,
    grid: &CoverageGrid<'_>,
    cells: impl IntoIterator<Item = Cell>,
    metadata: &Value,
    targets: Option<&Value>,
) -> Result<(), ExportError> {
    write!(out, "{{\"type\":\"FeatureCollection\",\"name\":")?;
    serde_json::to_writer(
        &mut *out,
        &format!(
            "coverage-{}",
            grid.job["job_id"].as_str().unwrap_or_default()
        ),
    )?;
    write!(out, ",\"metadata\":")?;
    serde_json::to_writer(&mut *out, metadata)?;
    write!(out, ",\"features\":[")?;
    grid.visit(cells, |cell, _| {
        if cell["index"] != 0 { write!(out, ",")?; }
        let geometry = if cell["state"] == "outside_area" {
            Value::Null
        } else {
            let [sw, se, ne, nw] = grid.corners(cell)?;
            json!({"type": "Polygon", "coordinates": [[sw, se, ne, nw, sw]]})
        };
        serde_json::to_writer(&mut *out, &json!({
            "type": "Feature", "id": format!("cell-{}", cell["index"]), "geometry": geometry,
            "properties": {
                "feature_kind": "coverage_cell", "index": cell["index"], "state": cell["state"],
                "source_count": cell.get("source_count").unwrap_or(&json!(0)),
                "unknown_sources": cell.get("unknown_sources").unwrap_or(&json!(0)),
                "unresolved_sources": cell.get("unresolved_sources").unwrap_or(&json!(0)),
                "best_margin_db": cell["best_margin_db"], "best_source_id": cell["best_source_id"],
                "sources": cell.get("sources").unwrap_or(&json!([])),
            },
        }))?;
        Ok(())
    })?;
    if !grid.ring.is_empty() {
        write!(out, ",")?;
        serde_json::to_writer(
            &mut *out,
            &json!({
                "type": "Feature", "id": "coverage-analysis-area",
                "geometry": {"type": "Polygon", "coordinates": [grid.ring]},
                "properties": {"feature_kind": "analysis_area", "state": "requested_area"},
            }),
        )?;
    }
    if let Some(report) = targets {
        let outcomes = outcomes(report);
        for target in rows(&report["target_features"]) {
            let id = target["id"].as_str().unwrap_or_default();
            let outcome = outcomes.get(id).copied().unwrap_or(&Value::Null);
            let mut properties = target["properties"]
                .as_object()
                .cloned()
                .unwrap_or_default();
            properties.insert("feature_kind".into(), json!("coverage_target"));
            properties.insert("target_id".into(), target["id"].clone());
            properties.insert(
                "coverage_state".into(),
                outcome
                    .get("state")
                    .cloned()
                    .unwrap_or(json!("not_assessed")),
            );
            properties.insert("coverage_message".into(), outcome["message"].clone());
            write!(out, ",")?;
            serde_json::to_writer(
                &mut *out,
                &json!({
                    "type": "Feature", "id": format!("target-{id}"), "geometry": target["geometry"],
                    "properties": properties,
                }),
            )?;
        }
    }
    write!(out, "]}}")?;
    Ok(())
}
