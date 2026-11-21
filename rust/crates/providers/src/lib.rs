//! Native HTTP and shared-cache adapters for RepeaterNet's external providers.
pub mod corescope;
pub mod kartverket;
pub mod roads;

mod transport;
pub use transport::user_agent;

use repeaternet_terrain::TerrainError;
use std::{
    io,
    path::{Path, PathBuf},
};

pub type Result<T> = std::result::Result<T, ProviderError>;

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("{0}")]
    InvalidInput(String),
    #[error("Provider operation cancelled")]
    Cancelled,
    #[error("{provider} HTTP {status}")]
    Http { provider: &'static str, status: u16 },
    #[error("{provider} returned an invalid response: {detail}")]
    InvalidResponse {
        provider: &'static str,
        detail: String,
    },
    #[error("{provider} connection or request failed: {source}")]
    Transport {
        provider: &'static str,
        #[source]
        source: reqwest::Error,
    },
    #[error(
        "Could not load road network: 1 of {total} tiles failed (tile ({row}, {col}): {source})"
    )]
    RoadTile {
        total: usize,
        row: i32,
        col: i32,
        #[source]
        source: Box<ProviderError>,
    },
    #[error(transparent)]
    Terrain(#[from] TerrainError),
    #[error(transparent)]
    Io(#[from] io::Error),
}

pub fn shared_cache_directory(data: &Path) -> PathBuf {
    std::env::var_os("RF_PLANNER_CACHE_DIR")
        .map_or_else(|| data.join("shared-cache"), PathBuf::from)
}

fn invalid(message: impl Into<String>) -> ProviderError {
    ProviderError::InvalidInput(message.into())
}

fn response_error(provider: &'static str, detail: impl Into<String>) -> ProviderError {
    ProviderError::InvalidResponse {
        provider,
        detail: detail.into(),
    }
}

/// Lowercase hexadecimal encoding of a digest.
pub(crate) fn lower_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}
