//! Phase 1: build `links.jsonl` and `nodes.jsonl` from capture snapshots.
//! See `docs/RF_CALIBRATION.md` § Dataset.

use std::path::Path;

use anyhow::{Result, bail};

/// Extract the measured-link dataset from `<work>/snapshots` into
/// `<work>/dataset`.
pub fn run(_snapshots: &Path, _dataset: &Path) -> Result<()> {
    bail!("extract is not implemented yet")
}
