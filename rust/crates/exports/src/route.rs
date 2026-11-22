//! Active route data exports and printable Leaflet report.

mod profile;
mod report;

pub use profile::MAX_PROFILE_POINTS;
pub use report::{write_route_html, CoverageOmission, RouteReportOptions};

use crate::{invalid, project, rows, site_coordinate, ExportError, Transform};
use serde_json::{json, Value};
use std::{collections::HashMap, io::Write};

const CSV_FIELDS: [&str; 23] = [
    "hop",
    "from",
    "to",
    "distance_km",
    "fspl_db",
    "diffraction_loss_db",
    "total_path_loss_db",
    "rx_power_dbm",
    "sensitivity_dbm",
    "raw_margin_db",
    "fade_adjusted_margin_db",
    "reverse_margin_db",
    "worst_margin_db",
    "minimum_fresnel_clearance_percent",
    "los_clear",
    "valid",
    "diffraction_model",
    "forward_model_correction_db",
    "reverse_model_correction_db",
    "forward_terminal_clutter_db",
    "reverse_terminal_clutter_db",
    "forward_step_allowance_db",
    "reverse_step_allowance_db",
];

/// Stream the route DTO's active links in their persisted order.
pub fn write_route_csv(output: impl Write, route: &Value) -> Result<(), ExportError> {
    let mut csv = csv::WriterBuilder::new()
        .terminator(csv::Terminator::CRLF)
        .from_writer(output);
    csv.write_record(CSV_FIELDS)?;
    let decimal = |value: Option<f64>, precision| {
        value.map_or_else(String::new, |value| format!("{value:.precision$}"))
    };
    let boolean = |value: &Value| match value.as_bool() {
        Some(true) => "True",
        Some(false) => "False",
        None => "",
    };
    for (i, link) in rows(&route["links"]).iter().enumerate() {
        let f = &link["forward"];
        let r = &link["reverse"];
        csv.write_record([
            (i + 1).to_string(),
            id(link, "source_id").into(),
            id(link, "target_id").into(),
            decimal(link["distance_m"].as_f64().map(|n| n / 1000.0), 3),
            decimal(f["fspl_db"].as_f64(), 2),
            decimal(link["diffraction_loss_db"].as_f64(), 2),
            decimal(f["total_path_loss_db"].as_f64(), 2),
            decimal(f["received_power_dbm"].as_f64(), 2),
            decimal(f["sensitivity_dbm"].as_f64(), 2),
            decimal(f["raw_margin_db"].as_f64(), 2),
            decimal(f["usable_margin_db"].as_f64(), 2),
            decimal(link["reverse"]["usable_margin_db"].as_f64(), 2),
            decimal(worst_margin(link), 2),
            decimal(
                link["minimum_fresnel_clearance_ratio"]
                    .as_f64()
                    .map(|n| n * 100.0),
                1,
            ),
            boolean(&link["los_clear"]).into(),
            boolean(&link["valid"]).into(),
            id(link, "diffraction_model").into(),
            decimal(f["model_correction_db"].as_f64(), 2),
            decimal(r["model_correction_db"].as_f64(), 2),
            decimal(f["terminal_clutter_db"].as_f64(), 2),
            decimal(r["terminal_clutter_db"].as_f64(), 2),
            decimal(f["step_allowance_db"].as_f64(), 2),
            decimal(r["step_allowance_db"].as_f64(), 2),
        ])?;
    }
    csv.flush()?;
    Ok(())
}

fn id<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().unwrap_or_default()
}

fn worst_margin(link: &Value) -> Option<f64> {
    if let Some(margin) = link.get("worst_margin_db") {
        return margin.as_f64();
    }
    Some(
        link["forward"]["usable_margin_db"]
            .as_f64()?
            .min(link["reverse"]["usable_margin_db"].as_f64()?),
    )
}

/// Stream active route sites and lines. A caller-provided projected-to-WGS84
/// transform fills missing coordinates. With no transform, use the engine's
/// synthetic XY fallback.
pub fn write_route_geojson(
    mut out: impl Write,
    route: &Value,
    reverse: Option<Transform<'_>>,
) -> Result<(), ExportError> {
    let mut coordinates = HashMap::new();
    write!(out, "{{\"type\":\"FeatureCollection\",\"features\":[")?;
    for (index, site) in rows(&route["route"]).iter().enumerate() {
        let coord = if let Some([lon, lat]) = site_coordinate(site) {
            [Some(lon), Some(lat)]
        } else {
            let x = site["x"].as_f64();
            let y = site["y"].as_f64();
            match reverse {
                Some(reverse) => {
                    let [lon, lat] = project(
                        reverse,
                        [
                            x.ok_or_else(|| invalid("Missing route site x"))?,
                            y.ok_or_else(|| invalid("Missing route site y"))?,
                        ],
                    )?;
                    [Some(lon), Some(lat)]
                }
                None => [x, y],
            }
        };
        coordinates.insert(id(site, "id"), coord);
        let base = if site["height_reference"] == "surface_dom"
            && !site["surface_elevation_m"].is_null()
        {
            &site["surface_elevation_m"]
        } else {
            &site["ground_elevation_m"]
        };
        let absolute = base
            .as_f64()
            .zip(site["antenna_height_m"].as_f64())
            .map(|(base, height)| base + height);
        if index > 0 {
            write!(out, ",")?;
        }
        serde_json::to_writer(
            &mut out,
            &json!({
                "type": "Feature", "geometry": {"type": "Point", "coordinates": coord},
                "properties": {
                    "id": site["id"], "kind": site["kind"], "dtm_elevation_m": site["ground_elevation_m"],
                    "dom_elevation_m": site["surface_elevation_m"], "antenna_height_m": site["antenna_height_m"],
                    "antenna_absolute_elevation_m": absolute, "terrain_slope": site["terrain_slope"],
                    "site_quality": site["site_quality"],
                },
            }),
        )?;
    }
    for link in rows(&route["links"]) {
        let coord = |key| {
            coordinates.get(id(link, key)).ok_or_else(|| {
                invalid(format!(
                    "Route link references missing site {}",
                    id(link, key)
                ))
            })
        };
        let endpoints = [coord("source_id")?, coord("target_id")?];
        let budget = |direction: &str| {
            let fields = &link[direction];
            json!({
                "model_correction_db": fields["model_correction_db"],
                "terminal_clutter_db": fields["terminal_clutter_db"],
                "step_allowance_db": fields["step_allowance_db"],
            })
        };
        write!(out, ",")?;
        serde_json::to_writer(
            &mut out,
            &json!({
                "type": "Feature", "geometry": {"type": "LineString", "coordinates": endpoints},
                "properties": {
                    "from": link["source_id"], "to": link["target_id"], "distance_m": link["distance_m"],
                    "worst_margin_db": worst_margin(link), "forward_margin_db": link["forward"]["usable_margin_db"],
                    "reverse_margin_db": link["reverse"]["usable_margin_db"],
                    "fresnel_clearance_percent": link["minimum_fresnel_clearance_ratio"].as_f64().map(|n| n * 100.0),
                    "diffraction_loss_db": link["diffraction_loss_db"], "los_clear": link["los_clear"], "valid": link["valid"],
                    "diffraction_model": link["diffraction_model"],
                    "forward": budget("forward"), "reverse": budget("reverse"),
                },
            }),
        )?;
    }
    write!(out, "]}}")?;
    Ok(())
}

fn number(value: Option<f64>, digits: usize, unit: &str) -> String {
    value
        .filter(|n| n.is_finite())
        .map_or_else(|| "—".into(), |n| format!("{n:.digits$}{unit}"))
}

fn field(value: &Value, key: &str, digits: usize, unit: &str) -> String {
    number(value[key].as_f64(), digits, unit)
}

fn km(meters: Option<f64>) -> String {
    number(meters.map(|m| m / 1_000.0), 3, "")
}

struct Hop<'a> {
    data: &'a Value,
    reversed: bool,
}

impl<'a> Hop<'a> {
    fn source(&self) -> &'a str {
        id(
            self.data,
            if self.reversed {
                "target_id"
            } else {
                "source_id"
            },
        )
    }
    fn target(&self) -> &'a str {
        id(
            self.data,
            if self.reversed {
                "source_id"
            } else {
                "target_id"
            },
        )
    }
    fn name(&self) -> String {
        format!("{} → {}", self.source(), self.target())
    }
    fn distance(&self) -> Option<f64> {
        self.data["distance_m"].as_f64()
    }
    fn clearance_distance(&self) -> Option<f64> {
        self.orient_distance(self.data["minimum_clearance_distance_m"].as_f64())
    }
    fn orient_distance(&self, distance: Option<f64>) -> Option<f64> {
        if self.reversed {
            Some(self.distance()? - distance?)
        } else {
            distance
        }
    }
    fn budgets(&self) -> [&'a Value; 2] {
        if self.reversed {
            [&self.data["reverse"], &self.data["forward"]]
        } else {
            [&self.data["forward"], &self.data["reverse"]]
        }
    }
}
