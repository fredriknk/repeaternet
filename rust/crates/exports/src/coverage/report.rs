use super::{outcomes, Cell, CoverageAttachments, CoverageGrid, STATES};
use crate::{html, invalid, numeric, rows, site_coordinate, ExportError};
use serde_json::{json, Value};
use std::{collections::HashMap, io::Write};

pub(super) const COLORS: [&str; 9] = [
    "#000000", "#287b64", "#61a88c", "#f0ca62", "#ce796d", "#a7afb0", "#9476b7", "#e2e5e3",
    "#f7f8f7",
];

pub(super) fn color_index(cell: &Value, mode: &str) -> u8 {
    match cell["state"].as_str() {
        Some("unknown_terrain") => return 5,
        Some("unresolved") => return 6,
        Some("not_evaluated") => return 7,
        Some("outside_area") => return 8,
        _ => {}
    }
    let sources = rows(&cell["sources"]);
    if mode == "overlap" {
        return match sources
            .iter()
            .filter(|source| source["valid_two_way"] == true)
            .count()
        {
            0 => 4,
            1 => 3,
            2 => 2,
            _ => 1,
        };
    }
    let (valid, margin) = match mode {
        "downlink" => ("valid_downlink", "downlink_margin_db"),
        "uplink" => ("valid_uplink", "uplink_margin_db"),
        _ => ("valid_two_way", "two_way_margin_db"),
    };
    let best = sources
        .iter()
        .filter(|source| source[valid] == true)
        .filter_map(|source| source[margin].as_f64())
        .reduce(f64::max);
    match best {
        Some(margin) if margin >= 10.0 => 1,
        Some(margin) if margin >= 5.0 => 2,
        Some(margin) if margin >= 0.0 => 3,
        _ => 4,
    }
}

pub(super) fn color_legend(mode: &str) -> impl Iterator<Item = (&'static str, &'static str)> {
    let good = if mode == "overlap" {
        ["3+ usable sources", "2 usable sources", "1 usable source"]
    } else {
        ["≥ 10 dB", "5–10 dB", "0–5 dB"]
    };
    good.into_iter()
        .chain([
            "Not usable",
            "Unknown terrain",
            "Unresolved profile limit",
            "Not evaluated",
        ])
        .zip(COLORS[1..8].iter().copied())
}

pub(super) fn target_table(out: &mut dyn Write, report: &Value) -> Result<(), ExportError> {
    write!(out, "<table><thead><tr><th>Target</th><th>Outcome</th><th>Margin / area / length detail</th></tr></thead><tbody>")?;
    for item in rows(&report["targets"]) {
        let detail = item
            .get("best_two_way_margin_db")
            .or_else(|| item.get("covered_area_m2"))
            .or_else(|| item.get("covered_length_m"))
            .filter(|value| !value.is_null())
            .unwrap_or(&item["message"]);
        write!(
            out,
            "<tr><td>{}</td><td>{}</td><td>{}</td></tr>",
            html(&item["name"]),
            html(&item["state"]),
            html(detail)
        )?;
    }
    write!(out, "</tbody></table>")?;
    Ok(())
}

struct Map([f64; 4]);

impl Map {
    fn point(&self, [longitude, latitude]: [f64; 2]) -> [f64; 2] {
        let [south, west, north, east] = self.0;
        [
            40.0 + (longitude - west) / (east - west).max(1e-9) * 920.0,
            40.0 + (north - latitude) / (north - south).max(1e-9) * 520.0,
        ]
    }

    fn points(
        &self,
        out: &mut dyn Write,
        points: impl IntoIterator<Item = [f64; 2]>,
    ) -> Result<(), ExportError> {
        for (index, point) in points.into_iter().enumerate() {
            if index != 0 {
                write!(out, " ")?;
            }
            let [x, y] = self.point(point);
            write!(out, "{x:.1},{y:.1}")?;
        }
        Ok(())
    }

    fn targets(&self, out: &mut dyn Write, report: &Value) -> Result<(), ExportError> {
        let outcomes = outcomes(report);
        for target in rows(&report["target_features"]) {
            let state = target["id"]
                .as_str()
                .and_then(|id| outcomes.get(id))
                .and_then(|item| item["state"].as_str())
                .unwrap_or("unknown");
            let color = match state {
                "pass" => "#287b64",
                "fail" => "#ce796d",
                _ => "#aa702f",
            };
            let title = format!(
                "{}: {}",
                html(&target["properties"]["name"]),
                crate::escape(state)
            );
            let geometry = &target["geometry"];
            let coordinates = &geometry["coordinates"];
            match geometry["type"].as_str() {
                Some("Point") => {
                    if let Some(point) = numeric(coordinates) {
                        let [x, y] = self.point(point);
                        write!(out, "<circle cx='{x:.1}' cy='{y:.1}' r='9' fill='{color}' stroke='#fff' stroke-width='2'><title>{title}</title></circle>")?;
                    }
                }
                Some("LineString") => {
                    write!(out, "<polyline points='")?;
                    self.points(out, rows(coordinates).iter().filter_map(numeric))?;
                    write!(out, "' fill='none' stroke='{color}' stroke-width='3'><title>{title}</title></polyline>")?;
                }
                Some("Polygon") => {
                    write!(out, "<path d='")?;
                    for ring in rows(coordinates) {
                        let mut points = rows(ring).iter().filter_map(numeric);
                        if let Some(first) = points.next() {
                            write!(out, "M")?;
                            self.points(out, std::iter::once(first).chain(points))?;
                            write!(out, "Z")?;
                        }
                    }
                    write!(out, "' fill='{color}' fill-rule='evenodd' fill-opacity='.18' stroke='{color}' stroke-width='3'><title>{title}</title></path>")?;
                }
                _ => {}
            }
        }
        Ok(())
    }
}

fn link_status(link: &Value) -> (&'static str, &'static str) {
    if link["unresolved"] == true || link["valid"].is_null() {
        ("unresolved: profile sample limit", "#9476b7")
    } else if link["valid"] == true {
        ("valid", "#1b4f73")
    } else {
        ("invalid", "#b44735")
    }
}

pub(super) fn write_html(
    out: &mut dyn Write,
    grid: &CoverageGrid<'_>,
    cells: impl IntoIterator<Item = Cell>,
    metadata: &Value,
    attachments: &CoverageAttachments<'_>,
) -> Result<(), ExportError> {
    let bounds = numeric(&metadata["area_bounds_wgs84"])
        .ok_or_else(|| invalid("Coverage report has invalid WGS84 bounds"))?;
    let map = Map(bounds);
    let mode = grid.job["mode"]
        .as_str()
        .or_else(|| metadata["mode"].as_str())
        .unwrap_or("two_way");
    let stale = metadata["stale"] == true;
    let partial = metadata["partial"] == true;
    let status = if stale {
        "STALE"
    } else if partial {
        "PARTIAL"
    } else {
        "COMPLETE"
    };
    let counts = &metadata["counts"];
    out.write_all(include_bytes!("report_head.html"))?;
    write!(out, "<header><div><h1>Predicted mesh coverage</h1><div class='sub'>Project {} · alternative {} · job {}</div></div><span class='badge {status}'>{status}</span></header><p class='warning'>{}",
        html(&metadata["project_id"]), html(&metadata["alternative_id"]), html(&metadata["job_id"]), html(&metadata["prediction_warning"]))?;
    if metadata["route_search_complete"] == false {
        write!(out, " Coverage describes the selected certified route only. The route search stopped with {}/{} requested independent paths; unexplored alternatives are not included.",
            html(&metadata["achieved_path_count"]), html(&metadata["requested_path_count"]))?;
    }
    if partial {
        write!(out, " This export includes partial or unresolved cells; inspect the state counts and profile-limit settings.")?;
    }
    if stale {
        write!(
            out,
            " Inputs changed since this run; treat it as a historical snapshot."
        )?;
    }
    write!(out, "</p><section class='metrics'>")?;
    for (label, key) in [
        ("Covered cells", "covered_cells"),
        ("Evaluated cells", "evaluated_cells"),
        ("Unknown terrain", "unknown_cells"),
        ("Unresolved profile limits", "unresolved_cells"),
        ("Not evaluated", "not_evaluated_cells"),
        ("Outside requested area", "outside_area_cells"),
    ] {
        write!(
            out,
            "<div class='metric'><strong>{}</strong><span>{label}</span></div>",
            html(&counts[key])
        )?;
    }

    write!(out, "</section><h2>Coverage and selected network</h2><svg viewBox='0 0 1000 600' role='img' aria-label='Offline map of saved coverage grid, network links and target locations'>")?;
    grid.visit(cells, |cell, _| {
        if cell["state"] != "outside_area" {
            write!(out, "<polygon points='")?;
            map.points(out, grid.corners(cell)?)?;
            let color = COLORS[usize::from(color_index(cell, mode))];
            write!(out, "' fill='{color}' stroke='#ffffff' stroke-width='.7'><title>Cell {}: {}</title></polygon>", html(&cell["index"]), html(&cell["state"]))?;
        }
        Ok(())
    })?;
    if !grid.ring.is_empty() {
        write!(out, "<polygon points='")?;
        map.points(out, grid.ring.iter().copied())?;
        write!(out, "' fill='none' stroke='#19372d' stroke-width='2' stroke-dasharray='8 5'><title>Requested analysis area</title></polygon>")?;
    }
    let sites = rows(&metadata["network_sites"]);
    let links = rows(&metadata["network_links"]);
    let site_positions: HashMap<_, _> = sites
        .iter()
        .filter_map(|site| Some((site["id"].as_str()?, site_coordinate(site)?)))
        .collect();
    for link in links {
        let source = link["source_id"]
            .as_str()
            .and_then(|id| site_positions.get(id));
        let target = link["target_id"]
            .as_str()
            .and_then(|id| site_positions.get(id));
        if let (Some(&source), Some(&target)) = (source, target) {
            let [x1, y1] = map.point(source);
            let [x2, y2] = map.point(target);
            let (status, color) = link_status(link);
            let dash = if link["valid"] == true { "" } else { "7 5" };
            write!(out, "<line x1='{x1:.1}' y1='{y1:.1}' x2='{x2:.1}' y2='{y2:.1}' stroke='{color}' stroke-width='3' stroke-dasharray='{dash}'><title>{} to {}: {status}</title></line>",
                html(&link["source_id"]), html(&link["target_id"]))?;
        }
    }
    for site in sites {
        if let Some(point) = site_coordinate(site) {
            let [x, y] = map.point(point);
            write!(out, "<circle cx='{x:.1}' cy='{y:.1}' r='6' fill='#fff' stroke='#19372d' stroke-width='3'><title>{}</title></circle>", html(&site["id"]))?;
        }
    }
    if let Some(report) = attachments.target_report {
        map.targets(out, report)?;
    }
    write!(out, "</svg><div class='legend'>")?;
    for (label, color) in color_legend(mode).chain([("Outside requested area", COLORS[8])]) {
        write!(
            out,
            "<span><i class='swatch' style='background:{color}'></i> {label}</span>"
        )?;
    }
    write!(out, "</div><h2>Coverage states</h2><table><thead><tr><th>State</th><th>Cells</th></tr></thead><tbody>")?;
    for state in STATES {
        write!(
            out,
            "<tr><td>{state}</td><td>{}</td></tr>",
            html(&counts["state_counts"][state])
        )?;
    }

    write!(out, "</tbody></table><h2>Coverage targets</h2>")?;
    if let Some(report) = attachments.target_report {
        target_table(out, report)?;
    } else {
        write!(out, "<p class='muted'>No saved target-assessment report is attached to this coverage run.</p>")?;
    }
    write!(out, "<h2>Scenario comparison</h2>")?;
    if let Some(scenario) = attachments.scenario_comparison {
        write!(
            out,
            "<p>{} → {}</p>",
            html(&scenario["baseline_job_id"]),
            html(&scenario["scenario_job_id"])
        )?;
        scenario_table(
            out,
            scenario,
            &[
                ("Reference", "reference_id"),
                (
                    "Baseline reference available",
                    "baseline_reference_available",
                ),
                (
                    "Scenario reference available",
                    "scenario_reference_available",
                ),
                (
                    "Baseline connected sources",
                    "baseline_connected_source_ids",
                ),
                (
                    "Scenario connected sources",
                    "scenario_connected_source_ids",
                ),
                ("Baseline unresolved links", "baseline_unresolved_links"),
                ("Scenario unresolved links", "scenario_unresolved_links"),
                ("Effective cell size (m)", "effective_cell_size_m"),
                ("Approximate area", "approximate_cell_area_m2"),
            ],
        )?;
        if let Some(changes) = scenario["counts"].as_object() {
            write!(
                out,
                "<table><thead><tr><th>Change</th><th>Cells</th></tr></thead><tbody>"
            )?;
            for (key, value) in changes {
                write!(
                    out,
                    "<tr><td>{}</td><td>{}</td></tr>",
                    crate::escape(key),
                    html(value)
                )?;
            }
            write!(out, "</tbody></table>")?;
        }
    } else {
        write!(
            out,
            "<p class='muted'>No compatible scenario comparison was selected for this report.</p>"
        )?;
    }
    if let Some(failure) = attachments.failure_scenario {
        write!(out, "<h2>Router failure scenario</h2>")?;
        scenario_table(
            out,
            failure,
            &[
                ("Reference", "reference_id"),
                ("Reference available", "reference_available"),
                (
                    "Baseline reference available",
                    "baseline_reference_available",
                ),
                ("Failed routers", "failed_ids"),
                ("Connected sources", "connected_source_ids"),
                ("Disconnected sources", "disconnected_source_ids"),
                ("Components", "components"),
                ("Isolated routers", "isolated_router_ids"),
                ("Unresolved links", "unresolved_links"),
                ("Unknown cells", "unknown_cells"),
                ("Unresolved cells", "unresolved_cells"),
                ("Outside area", "outside_area_cells"),
                ("Lost local cells", "lost_local_cells"),
                (
                    "Lost reference-connected cells",
                    "lost_reference_connected_cells",
                ),
                ("Newly uncovered cells", "newly_uncovered_cells"),
            ],
        )?;
    }
    weakest_links(out, links)?;
    write!(out, "<h2>Terrain provenance and model assumptions</h2><p>CRS: {} · grid: {} × {} at {} m · profile step: {} m · surface data: {}</p>",
        html(&metadata["terrain_crs"]), html(&metadata["rows"]), html(&metadata["columns"]),
        html(&metadata["effective_cell_size_m"]), html(&metadata["profile_step_m"]), html(&metadata["surface_available"]))?;
    let provenance = json!({
        "terrain_inputs": metadata["terrain_inputs"], "terrain_fingerprint": metadata["terrain_fingerprint"],
        "settings": metadata["settings"], "radio_settings": metadata["radio_settings"], "source_ids": metadata["source_ids"],
        "coverage_model_version": metadata["coverage_model_version"],
    });
    write!(out, "<pre>{}</pre><footer class='muted'>Generated from saved, project-scoped results. No basemap or remote assets are embedded.</footer></main></body></html>",
        crate::escape(&serde_json::to_string_pretty(&provenance)?))?;
    Ok(())
}

fn scenario_table(
    out: &mut dyn Write,
    scenario: &Value,
    fields: &[(&str, &str)],
) -> Result<(), ExportError> {
    write!(
        out,
        "<table><thead><tr><th>Scenario detail</th><th>Value</th></tr></thead><tbody>"
    )?;
    for &(label, key) in fields {
        if let Some(value) = scenario.get(key) {
            write!(out, "<tr><td>{label}</td><td>{}</td></tr>", html(value))?;
        }
    }
    write!(out, "</tbody></table>")?;
    Ok(())
}

fn weakest_links(out: &mut dyn Write, links: &[Value]) -> Result<(), ExportError> {
    write!(out, "<h2>Selected network and weakest links</h2><table><thead><tr><th>Link</th><th>Distance</th><th>Worst margin</th><th>Validity</th></tr></thead><tbody>")?;
    let mut weakest: Vec<_> = links.iter().collect();
    let margin = |link: &Value| link["worst_margin_db"].as_f64().unwrap_or(f64::INFINITY);
    weakest.sort_by(|a, b| margin(a).total_cmp(&margin(b)));
    for link in weakest.iter().take(10) {
        let margin = if link["worst_margin_db"].is_null() {
            "—".into()
        } else {
            format!("{} dB", html(&link["worst_margin_db"]))
        };
        let detail = link["rejection_detail"]
            .as_str()
            .filter(|detail| !detail.is_empty())
            .map(|detail| format!(" · {}", crate::escape(detail)))
            .unwrap_or_default();
        let (status, _) = link_status(link);
        write!(
            out,
            "<tr><td>{} → {}</td><td>{:.0} m</td><td>{margin}</td><td>{status}{detail}</td></tr>",
            html(&link["source_id"]),
            html(&link["target_id"]),
            link["distance_m"].as_f64().unwrap_or(0.0)
        )?;
    }
    if weakest.is_empty() {
        write!(out, "<tr><td colspan='4'>Certified link metrics were not retained in this historical coverage snapshot.</td></tr>")?;
    }
    write!(out, "</tbody></table>")?;
    Ok(())
}
