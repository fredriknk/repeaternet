//! Scalar link budget and evaluation results.

use crate::{require_positive, DiffractionObstacle, RfError, TerrainProfile};

/// Free-space path loss using the horizontal distance in metres and frequency in MHz.
pub fn free_space_path_loss_db(distance_m: f64, frequency_mhz: f64) -> Result<f64, RfError> {
    require_positive(distance_m, "Distance must be positive")?;
    require_positive(frequency_mhz, "Frequency must be positive")?;
    Ok(32.44 + 20.0 * (distance_m / 1000.0).log10() + 20.0 * frequency_mhz.log10())
}

/// Received power after antenna gains, feed losses, propagation and miscellaneous loss.
pub fn received_power_dbm(
    tx_power_dbm: f64,
    tx_gain_dbi: f64,
    tx_feed_loss_db: f64,
    propagation_loss_db: f64,
    rx_gain_dbi: f64,
    rx_feed_loss_db: f64,
    miscellaneous_loss_db: f64,
) -> f64 {
    tx_power_dbm + tx_gain_dbi - tx_feed_loss_db - propagation_loss_db + rx_gain_dbi
        - rx_feed_loss_db
        - miscellaneous_loss_db
}

/// Link budget and margin for one direction.
#[derive(Clone, Debug, PartialEq)]
pub struct DirectionResult {
    pub source_id: String,
    pub target_id: String,
    pub distance_m: f64,
    pub fspl_db: f64,
    pub diffraction_loss_db: f64,
    pub clutter_loss_db: f64,
    pub total_path_loss_db: f64,
    pub tx_gain_dbi: f64,
    pub rx_gain_dbi: f64,
    pub received_power_dbm: f64,
    pub sensitivity_dbm: f64,
    pub raw_margin_db: f64,
    pub usable_margin_db: f64,
    pub departure_angle_deg: f64,
    pub arrival_angle_deg: f64,
    /// Whether the usable RF margin is at least zero.
    pub valid: bool,
    pub tx_power_dbm: f64,
    pub tx_feed_loss_db: f64,
    pub rx_feed_loss_db: f64,
    pub miscellaneous_loss_db: f64,
}

/// Two-way RF budget, geometric clearance and optional sampled profile.
#[derive(Clone, Debug, PartialEq)]
pub struct LinkResult {
    pub source_id: String,
    pub target_id: String,
    pub distance_m: f64,
    pub forward: DirectionResult,
    pub reverse: DirectionResult,
    /// Two-way validity for the selected validation mode.
    pub valid: bool,
    pub los_clear: bool,
    pub fresnel_clear: bool,
    pub minimum_clearance_m: f64,
    pub minimum_fresnel_clearance_ratio: f64,
    /// Distance from the source to the minimum Fresnel clearance ratio.
    pub minimum_clearance_distance_m: f64,
    pub maximum_fresnel_radius_m: f64,
    pub diffraction_loss_db: f64,
    pub dominant_obstacles: Vec<DiffractionObstacle>,
    pub profile: Option<TerrainProfile>,
}

impl LinkResult {
    /// The lower usable margin of the two directions.
    pub fn worst_margin_db(&self) -> f64 {
        self.forward
            .usable_margin_db
            .min(self.reverse.usable_margin_db)
    }
}
