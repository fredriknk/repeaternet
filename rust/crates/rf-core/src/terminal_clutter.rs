//! Per-site terminal clutter corrections.

use serde::{Deserialize, Serialize};

use crate::{
    calibration::{DEFAULT_TERMINAL_CLUTTER, P2108_STREET_WIDTH_M, TERMINAL_CLUTTER_RADIUS_M},
    knife_edge_loss_db, Point, RfError, TerrainLayer, TerrainSource,
};

/// Terminal clutter correction configured for a site.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalClutter {
    None,
    P2108Dom,
}

impl Default for TerminalClutter {
    fn default() -> Self {
        DEFAULT_TERMINAL_CLUTTER
    }
}

/// P.2108-1 §3.1 terminal height-gain correction, supported at 30–3000 MHz.
pub fn p2108_terminal_correction_db(
    freq_mhz: f64,
    antenna_height_agl_m: f64,
    clutter_height_m: f64,
) -> f64 {
    if !(30.0..=3000.0).contains(&freq_mhz) {
        return 0.0;
    }
    let gap = clutter_height_m - antenna_height_agl_m;
    if gap <= 0.0 {
        return 0.0;
    }
    // Both forest and urban use Eq. (2a) and the same default width.
    let theta = (gap / P2108_STREET_WIDTH_M).atan().to_degrees();
    let v = 0.342 * (freq_mhz / 1000.0 * gap * theta).sqrt();
    (knife_edge_loss_db(v) - 6.03).max(0.0)
}

/// Median positive surface excess on a grid centred on the terminal, including the centre.
/// The sampler returns (ground, surface). Missing and nonfinite pairs are skipped.
pub fn terminal_clutter_height_m(
    sample: impl Fn(Point) -> Option<(f64, f64)>,
    centre: Point,
    resolution_m: f64,
) -> Option<f64> {
    median_excess(
        clutter_points(centre, resolution_m)
            .into_iter()
            .filter_map(sample),
    )
}

/// Batch the two raster reads for a terminal neighbourhood.
pub(crate) fn sample_clutter_height(
    terrain: &(impl TerrainSource + ?Sized),
    centre: Point,
) -> Result<Option<f64>, RfError> {
    let points = clutter_points(centre, terrain.resolution_m());
    if points.is_empty() {
        return Ok(None);
    }
    let sample = |layer| {
        let values = terrain.sample(&points, layer)?;
        if values.len() != points.len() {
            return Err(RfError::TerrainSampleCount {
                expected: points.len(),
                actual: values.len(),
            });
        }
        Ok(values)
    };
    let ground = sample(TerrainLayer::GroundDtm)?;
    let surface = sample(TerrainLayer::SurfaceDom)?;
    Ok(median_excess(ground.into_iter().zip(surface)))
}

fn clutter_points(centre: Point, resolution_m: f64) -> Vec<Point> {
    if !resolution_m.is_finite() || resolution_m <= 0.0 {
        return Vec::new();
    }
    // Use a disc around the site. The calibration used samples along the path within 30 m.
    let steps = (TERMINAL_CLUTTER_RADIUS_M / resolution_m).floor() as i64;
    let mut points = Vec::new();
    for row in -steps..=steps {
        let dy = row as f64 * resolution_m;
        for col in -steps..=steps {
            let dx = col as f64 * resolution_m;
            if dx.hypot(dy) <= TERMINAL_CLUTTER_RADIUS_M {
                points.push(Point {
                    x: centre.x + dx,
                    y: centre.y + dy,
                });
            }
        }
    }
    points
}

fn median_excess(samples: impl Iterator<Item = (f64, f64)>) -> Option<f64> {
    let mut excesses: Vec<_> = samples
        .map(|(ground, surface)| surface - ground)
        .filter(|excess| excess.is_finite())
        .map(|excess| excess.max(0.0))
        .collect();
    excesses.sort_unstable_by(f64::total_cmp);
    let middle = excesses.len() / 2;
    let upper = *excesses.get(middle)?;
    Some(if excesses.len().is_multiple_of(2) {
        f64::midpoint(excesses[middle - 1], upper)
    } else {
        upper
    })
}
