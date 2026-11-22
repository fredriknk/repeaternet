//! Phase 3b: evaluate Rust engines on `profiles.jsonl` and write
//! `<work>/predictions/<engine>.jsonl`.

use std::path::Path;

use anyhow::{Result, bail};

pub fn run(_dataset: &Path, _profiles: &Path, _out: &Path) -> Result<()> {
    bail!("predict is not implemented yet")
}
