//! Native coverage evaluation and analysis over certified RepeaterNet routes.

mod cache;
mod compare;
mod engine;
mod grid;
pub mod live;
mod model;
mod network;
mod parallel;
mod point;
mod target_compare;
mod targets;

pub use cache::{CoverageCacheStats, CoverageMetricsCache};
pub use compare::*;
pub use engine::*;
pub use grid::*;
pub use model::*;
pub use network::*;
pub use point::*;
pub use target_compare::*;
pub use targets::*;

#[derive(Debug, thiserror::Error)]
pub enum CoverageError {
    #[error("{0}")]
    Invalid(String),
    #[error("Coverage calculation cancelled")]
    Cancelled,
    #[error(transparent)]
    Terrain(#[from] repeaternet_terrain::TerrainError),
    #[error(transparent)]
    Radio(#[from] repeaternet_rf_core::RfError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

fn invalid(message: impl Into<String>) -> CoverageError {
    CoverageError::Invalid(message.into())
}

pub const PYTHON_MODEL_VERSION: &str = "coverage-v3";
pub const MODEL_VERSION: &str = "coverage-rust-v1";
pub const MAX_CELLS: usize = 16_384;
pub const MAX_SOURCES: usize = 64;
pub const MAX_EVALUATIONS: usize = 250_000;
pub const MAX_PROFILE_SAMPLES: usize = 16_384;
