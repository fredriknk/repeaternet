//! ITU-R P.526 knife-edge and Bullington diffraction.

use crate::{RfError, SPEED_OF_LIGHT_M_S};

/// Dominant Bullington edge relative to the start of a profile.
#[derive(Clone, Debug, PartialEq)]
pub struct DiffractionObstacle {
    pub distance_m: f64,
    pub elevation_m: f64,
    /// Dimensionless knife-edge diffraction parameter.
    pub v: f64,
    pub loss_db: f64,
}

/// Diffraction loss and the dominant edge, if the path incurs loss.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DiffractionResult {
    pub loss_db: f64,
    pub obstacles: Vec<DiffractionObstacle>,
}

/// ITU-R P.526 single knife-edge loss approximation.
pub fn knife_edge_loss_db(v: f64) -> f64 {
    if v <= -0.78 {
        return 0.0;
    }
    6.9 + 20.0 * (((v - 0.1).powi(2) + 1.0).sqrt() + v - 0.1).log10()
}

/// Single edge parameter from obstruction height relative to line of sight.
/// A nonpositive endpoint distance returns negative infinity.
pub fn knife_edge_v(height_above_los_m: f64, d1_m: f64, d2_m: f64, frequency_mhz: f64) -> f64 {
    if d1_m <= 0.0 || d2_m <= 0.0 {
        return f64::NEG_INFINITY;
    }
    let wavelength = SPEED_OF_LIGHT_M_S / (frequency_mhz * 1_000_000.0);
    height_above_los_m * (2.0 * (d1_m + d2_m) / (wavelength * d1_m * d2_m)).sqrt()
}

/// P.526-16 §4.5.1 Bullington component. Obstruction elevations already include
/// Earth curvature. Distances and elevations are in metres. The endpoint LOS
/// elevations define the antenna-to-antenna chord.
pub fn bullington_loss_db(
    distances_m: &[f64],
    obstruction_elevations_m: &[f64],
    los_elevations_m: &[f64],
    frequency_mhz: f64,
) -> Result<DiffractionResult, RfError> {
    let count = distances_m.len();
    if count < 2
        || obstruction_elevations_m.len() != count
        || los_elevations_m.len() != count
        || distances_m.windows(2).any(|pair| pair[0] >= pair[1])
        || distances_m
            .iter()
            .chain(obstruction_elevations_m)
            .chain(los_elevations_m)
            .any(|value| !value.is_finite())
        || !frequency_mhz.is_finite()
        || frequency_mhz <= 0.0
    {
        return Err(RfError::InvalidInput(
            "Diffraction requires matching finite profiles, increasing distances and positive frequency",
        ));
    }
    if count == 2 {
        return Ok(DiffractionResult::default());
    }

    let start = distances_m[0];
    let length = distances_m[count - 1] - start;
    let tx = los_elevations_m[0];
    let rx = los_elevations_m[count - 1];
    let interior = distances_m[1..count - 1]
        .iter()
        .map(|distance| distance - start)
        .zip(obstruction_elevations_m[1..count - 1].iter().copied());
    let slope_tx = interior
        .clone()
        .map(|(x, elevation)| (elevation - tx) / x)
        .fold(f64::NEG_INFINITY, f64::max);
    let direct_slope = (rx - tx) / length;
    let (edge_distance, edge_height, v) = if slope_tx < direct_slope {
        let wavelength = SPEED_OF_LIGHT_M_S / (frequency_mhz * 1e6);
        let mut dominant = (0.0, 0.0, f64::NEG_INFINITY);
        for (x, elevation) in interior {
            let height = elevation - (tx + direct_slope * x);
            let v = height * (2.0 * length / (wavelength * x * (length - x))).sqrt();
            if v > dominant.2 {
                dominant = (x, elevation, v);
            }
        }
        dominant
    } else {
        let slope_rx = interior
            .map(|(x, elevation)| (elevation - rx) / (length - x))
            .fold(f64::NEG_INFINITY, f64::max);
        let denominator = slope_tx + slope_rx;
        // Tangency leaves the horizon rays coincident.
        let distance = if denominator > 0.0 {
            (rx - tx + slope_rx * length) / denominator
        } else {
            length / 2.0
        }
        .clamp(distances_m[1] - start, distances_m[count - 2] - start);
        let elevation = tx + slope_tx * distance;
        let height = elevation - (tx + direct_slope * distance);
        let v = knife_edge_v(height, distance, length - distance, frequency_mhz);
        (distance, elevation, v)
    };

    let knife_loss = knife_edge_loss_db(v);
    let loss_db = knife_loss + (-(-knife_loss / 6.0).exp_m1()) * (10.0 + 0.02 * length / 1000.0);
    let obstacles = if loss_db == 0.0 {
        Vec::new()
    } else {
        vec![DiffractionObstacle {
            distance_m: edge_distance,
            elevation_m: edge_height,
            v,
            loss_db,
        }]
    };
    Ok(DiffractionResult { loss_db, obstacles })
}
