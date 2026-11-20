//! Effective Earth curvature along the endpoint chord.

use crate::{require_positive, RfError};

/// Mean Earth radius in metres.
pub const EARTH_MEAN_RADIUS_M: f64 = 6_371_000.0;

/// Earth bulge above the endpoint chord at a distance from the start, in metres.
pub fn earth_bulge_m(
    distance_from_start_m: f64,
    total_distance_m: f64,
    k_factor: f64,
) -> Result<f64, RfError> {
    require_positive(total_distance_m, "Distance must be positive")?;
    require_positive(k_factor, "K-factor must be positive")?;
    Ok(
        distance_from_start_m * (total_distance_m - distance_from_start_m)
            / (2.0 * EARTH_MEAN_RADIUS_M * k_factor),
    )
}
