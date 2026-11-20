//! First Fresnel zone geometry.

use crate::{require_positive, RfError};

/// Speed of light in metres per second.
pub const SPEED_OF_LIGHT_M_S: f64 = 299_792_458.0;

/// First Fresnel zone radius in metres at a point between the transmitter and receiver.
pub fn first_fresnel_radius_m(
    distance_from_tx_m: f64,
    distance_to_rx_m: f64,
    frequency_mhz: f64,
) -> Result<f64, RfError> {
    require_positive(frequency_mhz, "Frequency must be positive")?;
    if [distance_from_tx_m, distance_to_rx_m]
        .iter()
        .any(|&distance| !distance.is_finite() || distance < 0.0)
    {
        return Err(RfError::InvalidInput(
            "Distances must be finite and nonnegative",
        ));
    }
    let denominator = distance_from_tx_m + distance_to_rx_m;
    let wavelength = SPEED_OF_LIGHT_M_S / (frequency_mhz * 1_000_000.0);
    if denominator == 0.0 {
        return Ok(0.0);
    }
    Ok((wavelength * distance_from_tx_m * distance_to_rx_m / denominator).sqrt())
}
