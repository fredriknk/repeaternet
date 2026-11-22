use std::collections::HashSet;

use repeaternet_rf_core::{AntennaPattern, AntennaSettings, Point, RadioBudget};
use serde::{Deserialize, Serialize};

use crate::{invalid, CoverageError, MAX_CELLS, MAX_EVALUATIONS, MAX_PROFILE_SAMPLES, MAX_SOURCES};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoverageMode {
    #[default]
    TwoWay,
    Downlink,
    Uplink,
    Overlap,
    BestSource,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AreaMode {
    #[default]
    Mesh,
    View,
    Drawn,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoverageState {
    #[default]
    NotEvaluated,
    UnknownTerrain,
    Unresolved,
    Covered,
    Uncovered,
    OutsideArea,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClientRadioProfile {
    pub name: String,
    pub height_agl_m: f64,
    pub tx_power_dbm: f64,
    pub gain_dbi: f64,
    pub feed_loss_db: f64,
    pub sensitivity_dbm: f64,
    pub miscellaneous_loss_db: f64,
}

impl Default for ClientRadioProfile {
    fn default() -> Self {
        Self {
            name: "Handheld · 1.5 m".into(),
            height_agl_m: 1.5,
            tx_power_dbm: 20.0,
            gain_dbi: 2.15,
            feed_loss_db: 0.0,
            sensitivity_dbm: -130.0,
            miscellaneous_loss_db: 0.0,
        }
    }
}

impl ClientRadioProfile {
    pub fn as_radio_budget(&self) -> RadioBudget {
        RadioBudget {
            antenna: AntennaSettings {
                pattern: AntennaPattern::ConstantGain(self.gain_dbi),
                feed_loss_db: self.feed_loss_db,
                height_agl_m: self.height_agl_m,
            },
            tx_power_dbm: self.tx_power_dbm,
            sensitivity_dbm: self.sensitivity_dbm,
            miscellaneous_loss_db: self.miscellaneous_loss_db,
        }
    }

    pub fn validate(&self) -> Result<(), CoverageError> {
        if !(1..=80).contains(&self.name.chars().count()) {
            return Err(invalid(
                "Client radio profile name must contain 1 to 80 characters",
            ));
        }
        for (key, value, min, max) in [
            ("height_agl_m", self.height_agl_m, 0.1, 1000.0),
            ("tx_power_dbm", self.tx_power_dbm, -20.0, 60.0),
            ("gain_dbi", self.gain_dbi, -30.0, 60.0),
            ("feed_loss_db", self.feed_loss_db, 0.0, 100.0),
            ("sensitivity_dbm", self.sensitivity_dbm, -200.0, 0.0),
            (
                "miscellaneous_loss_db",
                self.miscellaneous_loss_db,
                0.0,
                200.0,
            ),
        ] {
            if !(min..=max).contains(&value) {
                return Err(invalid(format!(
                    "Client {key} must be between {min} and {max}"
                )));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CoverageSettings {
    pub mode: CoverageMode,
    pub area_mode: AreaMode,
    pub area_polygon_wgs84: Option<Vec<[f64; 2]>>,
    pub cell_size_m: f64,
    pub area_buffer_m: f64,
    pub profile_step_m: f64,
    pub maximum_profile_samples: usize,
    pub maximum_cells: usize,
    pub maximum_sources: usize,
    pub maximum_evaluations: usize,
    pub include_endpoints: bool,
    pub area_includes_clients: bool,
    pub source_ids: Vec<String>,
    pub client: ClientRadioProfile,
}

impl Default for CoverageSettings {
    fn default() -> Self {
        Self {
            mode: CoverageMode::TwoWay,
            area_mode: AreaMode::Mesh,
            area_polygon_wgs84: None,
            cell_size_m: 1_000.0,
            area_buffer_m: 5_000.0,
            profile_step_m: 200.0,
            maximum_profile_samples: 4_096,
            maximum_cells: 4_096,
            maximum_sources: MAX_SOURCES,
            maximum_evaluations: MAX_EVALUATIONS,
            include_endpoints: false,
            area_includes_clients: true,
            source_ids: Vec::new(),
            client: ClientRadioProfile::default(),
        }
    }
}

impl CoverageSettings {
    /// Validate web settings and close the drawn WGS84 ring if necessary.
    pub fn normalized(mut self) -> Result<Self, CoverageError> {
        self.validate()?;
        if let Some(ring) = &mut self.area_polygon_wgs84 {
            if ring.first() != ring.last() {
                ring.push(ring[0]);
            }
        }
        Ok(self)
    }

    pub fn validate(&self) -> Result<(), CoverageError> {
        self.client.validate()?;
        for (key, value, max) in [
            ("cell_size_m", self.cell_size_m, 100_000.0),
            ("area_buffer_m", self.area_buffer_m, 500_000.0),
            ("profile_step_m", self.profile_step_m, 10_000.0),
        ] {
            if !value.is_finite() || value <= 0.0 || value > max {
                return Err(invalid(format!(
                    "Coverage {key} must be above zero and at most {max}"
                )));
            }
        }
        for (key, value, min, max) in [
            (
                "maximum_profile_samples",
                self.maximum_profile_samples,
                3,
                MAX_PROFILE_SAMPLES,
            ),
            ("maximum_cells", self.maximum_cells, 1, MAX_CELLS),
            ("maximum_sources", self.maximum_sources, 1, MAX_SOURCES),
            (
                "maximum_evaluations",
                self.maximum_evaluations,
                1,
                MAX_EVALUATIONS,
            ),
        ] {
            if !(min..=max).contains(&value) {
                return Err(invalid(format!(
                    "Coverage {key} must be between {min} and {max}"
                )));
            }
        }
        let mut ids = HashSet::new();
        if self.source_ids.len() > MAX_SOURCES
            || self.source_ids.iter().any(|id| {
                id.is_empty()
                    || id.chars().count() > 128
                    || id.chars().any(|c| c < ' ')
                    || !ids.insert(id)
            })
        {
            return Err(invalid(
                "Coverage source_ids must be a unique list of source IDs",
            ));
        }
        if let Some(ring) = &self.area_polygon_wgs84 {
            crate::grid::drawn_polygon(ring, None)?;
        } else if self.area_mode == AreaMode::Drawn {
            return Err(invalid("Draw and finish a coverage area polygon first"));
        }
        Ok(())
    }

    /// Python's preview uses its own evaluation cap and retains the requested settings separately.
    pub fn preview_settings(&self) -> Self {
        Self {
            maximum_cells: self.maximum_cells.min(256),
            maximum_evaluations: 4_096,
            ..self.clone()
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Rejection {
    UnknownTerrain,
    ProfileSampleLimit,
    Validation,
    #[serde(rename = "LOS/Fresnel validation")]
    LosFresnelValidation,
    #[serde(rename = "radio margin")]
    RadioMargin,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CoverageSourceResult {
    pub source_id: String,
    pub downlink_margin_db: Option<f64>,
    pub uplink_margin_db: Option<f64>,
    pub two_way_margin_db: Option<f64>,
    pub valid_downlink: bool,
    pub valid_uplink: bool,
    pub valid_two_way: bool,
    pub rejection: Option<Rejection>,
    pub rejection_detail: Option<String>,
}

impl CoverageSourceResult {
    pub fn usable_margin(&self, mode: CoverageMode) -> Option<f64> {
        match mode {
            CoverageMode::Downlink if self.valid_downlink => self.downlink_margin_db,
            CoverageMode::Uplink if self.valid_uplink => self.uplink_margin_db,
            CoverageMode::TwoWay | CoverageMode::Overlap | CoverageMode::BestSource
                if self.valid_two_way =>
            {
                self.two_way_margin_db
            }
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CoverageCell {
    pub index: usize,
    pub x: f64,
    pub y: f64,
    pub latitude: f64,
    pub longitude: f64,
    #[serde(default)]
    pub state: CoverageState,
    #[serde(default)]
    pub source_count: usize,
    #[serde(default)]
    pub unknown_sources: usize,
    pub best_margin_db: Option<f64>,
    pub best_source_id: Option<String>,
    #[serde(default)]
    pub sources: Vec<CoverageSourceResult>,
    #[serde(default)]
    pub unresolved_sources: usize,
}

impl CoverageCell {
    pub fn position(&self) -> Point {
        Point {
            x: self.x,
            y: self.y,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CoverageGrid {
    pub bounds: [f64; 4],
    pub requested_cell_size_m: f64,
    pub effective_cell_size_m: f64,
    pub rows: usize,
    pub columns: usize,
    pub requested_cells: usize,
    #[serde(default)]
    pub terrain_available_cells: usize,
    #[serde(default)]
    pub evaluated_cells: usize,
    #[serde(default)]
    pub unknown_cells: usize,
    #[serde(default)]
    pub unresolved_cells: usize,
    #[serde(default)]
    pub inside_area_cells: usize,
    #[serde(default)]
    pub outside_area_cells: usize,
    #[serde(default)]
    pub completed_cells: usize,
    #[serde(default)]
    pub cells: Vec<CoverageCell>,
    #[serde(default)]
    pub surface_available: bool,
    #[serde(default)]
    pub unresolved_source_evaluations: usize,
}
