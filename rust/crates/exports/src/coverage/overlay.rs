use super::{
    report::{color_index, color_legend, target_table, COLORS},
    Cell, CoverageGrid,
};
use crate::{html, invalid, project, ExportError};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::Value;
use std::{collections::HashMap, io::Write};

pub const MAX_COVERAGE_IMAGE_SIDE: usize = 384;
const EARTH_RADIUS: f64 = 6_378_137.0;

pub struct CoverageOverlay {
    image: String,
    bounds: [[f64; 2]; 2],
    metadata: Value,
    target_report: Option<Value>,
}

impl CoverageOverlay {
    pub fn image(&self) -> &str {
        &self.image
    }
    /// Southwest and northeast corners in Leaflet latitude/longitude order.
    pub fn bounds(&self) -> [[f64; 2]; 2] {
        self.bounds
    }
    pub fn metadata(&self) -> &Value {
        &self.metadata
    }
    pub(super) fn mode(&self) -> &str {
        self.metadata["mode"].as_str().unwrap_or("two_way")
    }

    pub(crate) fn legend(&self) -> impl Iterator<Item = (&'static str, &'static str)> {
        color_legend(self.mode())
    }

    pub(crate) fn write_section(&self, out: &mut dyn Write) -> Result<(), ExportError> {
        let counts = &self.metadata["counts"];
        let states = &counts["state_counts"];
        let area_cells = counts["requested_cells"]
            .as_u64()
            .unwrap_or(0)
            .saturating_sub(states["outside_area"].as_u64().unwrap_or(0));
        let covered = states["covered"].as_u64().unwrap_or(0);
        let size = self.metadata["effective_cell_size_m"]
            .as_f64()
            .unwrap_or(0.0);
        let client = &self.metadata["settings"]["client"];
        let mode = match self.mode() {
            "two_way" => "Two-way",
            "downlink" => "Downlink",
            "uplink" => "Uplink",
            "overlap" => "Overlap",
            "best_source" => "Best source",
            other => other,
        };
        write!(
            out,
            "<section id=\"coverage\"><h2>Predicted coverage · saved run</h2><p class=\"muted\">{}</p><dl>",
            html(&self.metadata["prediction_warning"])
        )?;
        for (label, value) in [
            ("Coverage mode", crate::escape(mode)),
            ("Cell size", format!("{} m", grouped(&format!("{size:.1}")))),
            (
                "Analyzed area",
                format!(
                    "{} km²",
                    grouped(&format!("{:.3}", area_cells as f64 * size.powi(2) / 1e6))
                ),
            ),
            (
                "Covered area",
                format!(
                    "{} km²",
                    grouped(&format!("{:.3}", covered as f64 * size.powi(2) / 1e6))
                ),
            ),
            ("Client profile", html(&client["name"])),
            ("Client height (m AGL)", html(&client["height_agl_m"])),
            ("Client TX power (dBm)", html(&client["tx_power_dbm"])),
            ("Client antenna gain (dBi)", html(&client["gain_dbi"])),
            ("Client sensitivity (dBm)", html(&client["sensitivity_dbm"])),
            ("Client feed loss (dB)", html(&client["feed_loss_db"])),
            (
                "Client miscellaneous loss (dB)",
                html(&client["miscellaneous_loss_db"]),
            ),
        ] {
            write!(out, "<div><dt>{label}</dt><dd>{value}</dd></div>")?;
        }
        write!(out, "</dl><table><thead><tr><th scope=\"col\">State</th><th scope=\"col\">Cells</th><th scope=\"col\">Share of area</th></tr></thead><tbody>")?;
        for (state, label) in [
            ("covered", "Covered"),
            ("uncovered", "Uncovered"),
            ("unknown_terrain", "Unknown terrain"),
            ("unresolved", "Unresolved"),
            ("not_evaluated", "Not evaluated"),
        ] {
            let count = states[state].as_u64().unwrap_or(0);
            let percent = if area_cells == 0 {
                "—".into()
            } else {
                format!("{:.1}%", 100.0 * count as f64 / area_cells as f64)
            };
            write!(
                out,
                "<tr><th scope=\"row\">{label}</th><td>{}</td><td>{percent}</td></tr>",
                grouped(&count.to_string())
            )?;
        }
        write!(out, "</tbody></table>")?;
        if let Some(targets) = &self.target_report {
            write!(out, "<h3>Coverage targets</h3>")?;
            target_table(out, targets)?;
        }
        write!(out, "</section>")?;
        Ok(())
    }
}

fn grouped(number: &str) -> String {
    let length = number.split('.').next().unwrap_or(number).len();
    let mut grouped = String::with_capacity(number.len());
    for (index, ch) in number.char_indices() {
        if index > 0 && index < length && (length - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(ch);
    }
    grouped
}

fn mercator([longitude, latitude]: [f64; 2]) -> [f64; 2] {
    [
        EARTH_RADIUS * longitude.to_radians(),
        EARTH_RADIUS * latitude.to_radians().tan().asinh(),
    ]
}

fn lon_lat([x, y]: [f64; 2]) -> [f64; 2] {
    [
        (x / EARTH_RADIUS).to_degrees(),
        (y / EARTH_RADIUS).sinh().atan().to_degrees(),
    ]
}

/// Rasterize the eligible complete, current coverage run for a Leaflet map.
/// Projected edges are densified as in the Python transform_bounds operation.
pub fn build_route_overlay(
    grid: &CoverageGrid<'_>,
    cells: impl IntoIterator<Item = Cell>,
    metadata: &Value,
    target_report: Option<&Value>,
) -> Result<CoverageOverlay, ExportError> {
    if metadata["complete"] != true || metadata["partial"] == true || metadata["stale"] == true {
        return Err(invalid(
            "Coverage is omitted because the run is incomplete, unresolved or stale.",
        ));
    }
    let [left, bottom, right, top] = grid.bounds;
    let [mut west, mut south] = [f64::INFINITY; 2];
    let [mut east, mut north] = [f64::NEG_INFINITY; 2];
    for i in 0..=22 {
        let fraction = f64::from(i) / 22.0;
        let x = left + (right - left) * fraction;
        let y = bottom + (top - bottom) * fraction;
        for point in [[x, bottom], [x, top], [left, y], [right, y]] {
            let [x, y] = mercator(project(grid.reverse, point)?);
            west = west.min(x);
            south = south.min(y);
            east = east.max(x);
            north = north.max(y);
        }
    }
    let (extent_x, extent_y) = (east - west, north - south);
    if ![west, south, east, north]
        .iter()
        .all(|value| value.is_finite())
        || extent_x.min(extent_y) <= 0.0
    {
        return Err(invalid("Coverage grid has invalid map bounds"));
    }
    let longest = MAX_COVERAGE_IMAGE_SIDE.min(grid.rows.max(grid.columns).saturating_mul(2));
    let scale = longest as f64 / extent_x.max(extent_y);
    let width = (extent_x * scale).round_ties_even().max(1.0) as usize;
    let height = (extent_y * scale).round_ties_even().max(1.0) as usize;
    let mut sampled_indices = Vec::with_capacity(width * height);
    let mut colors = HashMap::new();
    for row in 0..height {
        let y = north - (row as f64 + 0.5) * extent_y / height as f64;
        for column in 0..width {
            let x = west + (column as f64 + 0.5) * extent_x / width as f64;
            let [x, y] = project(grid.forward, lon_lat([x, y]))?;
            let index = if x >= left && x < right && y > bottom && y <= top {
                let column = ((x - left) / grid.size) as usize;
                let row = ((top - y) / grid.size) as usize;
                (row < grid.rows && column < grid.columns).then_some(row * grid.columns + column)
            } else {
                None
            };
            if let Some(index) = index {
                colors.insert(index, 0);
            }
            sampled_indices.push(index);
        }
    }
    let mode = metadata["mode"].as_str().unwrap_or("two_way");
    grid.visit(cells, |cell, _| {
        if cell["state"] == "not_evaluated" || cell["state"] == "unresolved" {
            return Err(invalid(
                "Coverage is omitted because the run is incomplete or has unresolved cells.",
            ));
        }
        if let Some(color) = colors.get_mut(&(cell["index"].as_u64().unwrap_or_default() as usize))
        {
            if cell["state"] != "outside_area" {
                *color = color_index(cell, mode);
            }
        }
        Ok(())
    })?;
    let pixels: Vec<u8> = sampled_indices
        .into_iter()
        .map(|index| index.and_then(|i| colors.get(&i).copied()).unwrap_or(0))
        .collect();
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, width as u32, height as u32);
        encoder.set_color(png::ColorType::Indexed);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_palette(
            COLORS
                .iter()
                .flat_map(|color| {
                    let rgb = u32::from_str_radix(&color[1..], 16).expect("static palette");
                    [(rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8]
                })
                .collect::<Vec<_>>(),
        );
        encoder.set_trns(vec![0]);
        let mut writer = encoder.write_header()?;
        writer.write_image_data(&pixels)?;
        writer.finish()?;
    }
    let [west, south] = lon_lat([west, south]);
    let [east, north] = lon_lat([east, north]);
    Ok(CoverageOverlay {
        image: format!("data:image/png;base64,{}", STANDARD.encode(bytes)),
        bounds: [[south, west], [north, east]],
        metadata: metadata.clone(),
        target_report: target_report.cloned(),
    })
}
