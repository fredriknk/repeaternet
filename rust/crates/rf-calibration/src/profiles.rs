//! Phase 3a: fetch terrain through the shared provider cache and sample one
//! profile per undirected endpoint pair into `<work>/profiles.jsonl`.

use std::path::Path;

use anyhow::{Result, bail};

pub fn run(_dataset: &Path, _out: &Path) -> Result<()> {
    bail!("profiles is not implemented yet")
}
