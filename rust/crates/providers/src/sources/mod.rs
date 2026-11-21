//! Provider definitions, geographic selection, and terrain downloads.

mod downloader;
mod range;
mod registry;
mod schema;
#[cfg(test)]
mod tests;
mod wcs;

pub use crate::tile_grid::{
    DownloadArea, DownloadPlan, DownloadTile, PlanOptions, Product, TileLimits,
};
pub use downloader::*;
pub use registry::*;
pub use schema::*;

use std::{fs, io::Write, path::Path};

use sha2::{Digest, Sha256};

use crate::{lower_hex, Result};

fn hash(bytes: &[u8]) -> String {
    lower_hex(&Sha256::digest(bytes))
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let mut staged = tempfile::NamedTempFile::new_in(parent)?;
    staged.write_all(bytes)?;
    staged.as_file().sync_all()?;
    staged.persist(path).map_err(|error| error.error)?;
    Ok(())
}
