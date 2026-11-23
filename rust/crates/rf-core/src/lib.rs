//! RF propagation for projected metre coordinates. Raster access is supplied by
//! the caller through [TerrainSource].

pub mod antennas;
pub mod calibration;
pub mod curvature;
mod delta_bullington;
pub mod diffraction;
pub mod fresnel;
pub mod link;
mod multi_edge;
pub mod profile;
pub mod propagation;
pub mod settings;
pub mod site;
pub mod terminal_clutter;

pub use antennas::{elevation_angle_deg, AntennaPattern, ElevationPattern};
pub use curvature::{earth_bulge_m, EARTH_MEAN_RADIUS_M};
pub use delta_bullington::{
    delta_bullington_loss_db, smooth_earth_heights, spherical_earth_loss_db, DeltaBullington,
};
pub use diffraction::{
    bullington_edge_loss_db, bullington_loss_db, knife_edge_loss_db, knife_edge_v,
    DiffractionModel, DiffractionObstacle, DiffractionResult,
};
pub use fresnel::{first_fresnel_radius_m, SPEED_OF_LIGHT_M_S};
pub use link::{free_space_path_loss_db, received_power_dbm, DirectionResult, LinkResult};
pub use multi_edge::{deygout3_loss_db, Edge, MultiEdge};
pub use profile::{sample_profile, ObstacleSurface, TerrainLayer, TerrainProfile, TerrainSource};
pub use propagation::{EvaluationOptions, LinkEvaluator};
pub use settings::{
    AntennaSettings, LoRaSettings, RadioBudget, RfSettings, SiteRadioOverrides, ValidationMode,
};
pub use site::{HeightReference, Point, Site, SiteKind};
pub use terminal_clutter::{
    p2108_terminal_correction_db, terminal_clutter_height_m, TerminalClutter,
};

use std::{error, fmt};

/// Input, terrain sampling or antenna file error.
#[derive(Debug)]
pub enum RfError {
    InvalidInput(&'static str),
    MissingTerrain,
    TerrainSampleCount { expected: usize, actual: usize },
    Terrain(Box<dyn error::Error + Send + Sync>),
    AntennaCsv(csv::Error),
    InvalidAntennaCsvValue { line: u64, column: &'static str },
}

impl fmt::Display for RfError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInput(message) => f.write_str(message),
            Self::MissingTerrain => f.write_str("Terrain data does not cover the complete link"),
            Self::TerrainSampleCount { expected, actual } => {
                write!(f, "Expected {expected} terrain samples, received {actual}")
            }
            Self::Terrain(error) => write!(f, "Could not sample terrain: {error}"),
            Self::AntennaCsv(error) => write!(f, "Could not read antenna CSV: {error}"),
            Self::InvalidAntennaCsvValue { line, column } => {
                write!(
                    f,
                    "Invalid numeric value in antenna CSV column {column} on line {line}"
                )
            }
        }
    }
}

impl error::Error for RfError {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            Self::Terrain(error) => Some(error.as_ref()),
            Self::AntennaCsv(error) => Some(error),
            _ => None,
        }
    }
}

impl From<csv::Error> for RfError {
    fn from(error: csv::Error) -> Self {
        Self::AntennaCsv(error)
    }
}

pub(crate) fn require_positive(value: f64, message: &'static str) -> Result<(), RfError> {
    if !value.is_finite() || value <= 0.0 {
        return Err(RfError::InvalidInput(message));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
