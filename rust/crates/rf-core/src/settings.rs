//! RF propagation settings and per-site radio budgets.

use std::{collections::BTreeMap, path::Path};

use crate::{
    calibration::{K_FACTOR, MODEL_CORRECTION_DB},
    DiffractionModel, ObstacleSurface, TerminalClutter,
};
use crate::{require_positive, AntennaPattern, ElevationPattern, RfError, SiteKind};

/// Structural condition required in addition to the usable RF margin.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ValidationMode {
    /// Validate the two-way RF budget including diffraction and clutter.
    #[default]
    Propagation,
    /// Also require unobstructed line of sight and the configured Fresnel clearance.
    StrictLos,
}

/// LoRa receiver sensitivity calculation.
#[derive(Clone, Debug, PartialEq)]
pub struct LoRaSettings {
    pub enabled: bool,
    pub bandwidth_hz: f64,
    pub spreading_factor: u8,
    pub coding_rate: String,
    pub noise_figure_db: f64,
    pub manual_sensitivity_override: bool,
    pub snr_thresholds_db: BTreeMap<u8, f64>,
}

impl Default for LoRaSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            bandwidth_hz: 125_000.0,
            spreading_factor: 12,
            coding_rate: "4/5".into(),
            noise_figure_db: 6.0,
            manual_sensitivity_override: true,
            snr_thresholds_db: BTreeMap::from([
                (7, -7.5),
                (8, -10.0),
                (9, -12.5),
                (10, -15.0),
                (11, -17.5),
                (12, -20.0),
            ]),
        }
    }
}

impl LoRaSettings {
    /// Thermal noise plus receiver noise figure and the selected SNR threshold, in dBm.
    pub fn sensitivity_dbm(&self) -> Result<f64, RfError> {
        let threshold =
            self.snr_thresholds_db
                .get(&self.spreading_factor)
                .ok_or(RfError::InvalidInput(
                    "LoRa spreading factor has no SNR threshold",
                ))?;
        require_positive(self.bandwidth_hz, "LoRa bandwidth must be positive")?;
        let thermal_noise = -174.0 + 10.0 * self.bandwidth_hz.log10();
        Ok(thermal_noise + self.noise_figure_db + threshold)
    }
}

/// Antenna pattern, feed loss and nominal mounting height for site construction.
#[derive(Clone, Debug, PartialEq)]
pub struct AntennaSettings {
    pub pattern: AntennaPattern,
    pub feed_loss_db: f64,
    pub height_agl_m: f64,
    /// Terminal clutter model for sites constructed from this role's antenna settings.
    pub terminal_clutter: TerminalClutter,
}

impl Default for AntennaSettings {
    fn default() -> Self {
        Self {
            pattern: AntennaPattern::ConstantGain(2.15),
            feed_loss_db: 1.0,
            height_agl_m: 3.0,
            terminal_clutter: TerminalClutter::default(),
        }
    }
}

impl AntennaSettings {
    /// Replace the gain curve with measurements loaded from an elevation CSV.
    pub fn with_pattern_csv(mut self, path: impl AsRef<Path>) -> Result<Self, RfError> {
        self.pattern = AntennaPattern::Elevation(ElevationPattern::from_csv(path)?);
        Ok(self)
    }
}

/// Per-endpoint hardware overrides for asymmetric links.
#[derive(Clone, Debug, PartialEq)]
pub struct RadioBudget {
    pub antenna: AntennaSettings,
    pub tx_power_dbm: f64,
    pub sensitivity_dbm: f64,
    /// Loss applied when this site is transmitting.
    pub miscellaneous_loss_db: f64,
}

/// Shared propagation settings. Defaults match the EU868 MeshCore configuration.
#[derive(Clone, Debug, PartialEq)]
pub struct RfSettings {
    pub frequency_mhz: f64,
    pub tx_power_dbm: f64,
    pub receiver_sensitivity_dbm: f64,
    pub fade_margin_db: f64,
    pub miscellaneous_loss_db: f64,
    pub k_factor: f64,
    pub diffraction_model: DiffractionModel,
    pub obstacle_surface: ObstacleSurface,
    pub model_correction_db: f64,
    pub required_fresnel_clearance: f64,
    pub validation_mode: ValidationMode,
    pub endpoint_a: AntennaSettings,
    pub endpoint_b: AntennaSettings,
    pub router: AntennaSettings,
    pub lora: LoRaSettings,
    pub clutter_loss_enabled: bool,
    pub clutter_loss_db_per_m: f64,
}

impl Default for RfSettings {
    fn default() -> Self {
        Self {
            frequency_mhz: 869.5,
            tx_power_dbm: 22.0,
            receiver_sensitivity_dbm: -130.0,
            fade_margin_db: 10.0,
            miscellaneous_loss_db: 0.0,
            k_factor: K_FACTOR,
            diffraction_model: DiffractionModel::default(),
            obstacle_surface: ObstacleSurface::default(),
            model_correction_db: MODEL_CORRECTION_DB,
            required_fresnel_clearance: 0.60,
            validation_mode: ValidationMode::Propagation,
            endpoint_a: AntennaSettings::default(),
            endpoint_b: AntennaSettings::default(),
            router: AntennaSettings::default(),
            lora: LoRaSettings::default(),
            clutter_loss_enabled: false,
            clutter_loss_db_per_m: 0.0,
        }
    }
}

impl RfSettings {
    /// Effective receiver sensitivity after applying the LoRa override settings.
    pub fn effective_sensitivity_dbm(&self) -> Result<f64, RfError> {
        if self.lora.enabled && !self.lora.manual_sensitivity_override {
            return self.lora.sensitivity_dbm();
        }
        Ok(self.receiver_sensitivity_dbm)
    }

    /// Default antenna for a site role. Clients share the endpoint A configuration.
    pub fn antenna_settings(&self, kind: SiteKind) -> &AntennaSettings {
        match kind {
            SiteKind::EndpointA | SiteKind::Client => &self.endpoint_a,
            SiteKind::EndpointB => &self.endpoint_b,
            SiteKind::Router | SiteKind::Candidate => &self.router,
        }
    }
}
