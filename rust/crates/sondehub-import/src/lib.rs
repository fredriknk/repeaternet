mod database;
mod http;
mod import;
mod records;
mod terrain;

pub use import::run;

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use clap::Args;

pub const SUSPECT_FLAGS: u32 =
    records::LONG_PATH | records::MOBILE_RECEIVER | records::BELOW_GROUND | records::HIGH_SNR;

pub const DATABASE: &str = "sondehub-import.sqlite3";
pub const DEFAULT_BUDGET_GB: f64 = 50.0;
pub const GB: f64 = 1_000_000_000.0;

#[derive(Args, Clone, Copy)]
pub struct Budget {
    /// Total limit for SQLite capture files in the database directory (decimal GB).
    #[arg(long, default_value_t = DEFAULT_BUDGET_GB, value_parser = parse_gb)]
    pub total_budget_gb: f64,
    /// Leave this much of the limit available for live capture growth (decimal GB).
    #[arg(long, default_value_t = 5.0, value_parser = parse_gb)]
    pub reserve_gb: f64,
}

impl Budget {
    pub fn ceiling(self) -> Result<u64> {
        ensure!(
            self.reserve_gb < self.total_budget_gb,
            "--reserve-gb must be smaller than --total-budget-gb"
        );
        Ok(((self.total_budget_gb - self.reserve_gb) * GB) as u64)
    }
}

pub fn parse_gb(value: &str) -> Result<f64> {
    let gb: f64 = value.parse()?;
    ensure!(
        gb.is_finite() && (0.0..=1_000_000.0).contains(&gb),
        "Size must be between 0 and 1,000,000 GB"
    );
    Ok(gb)
}

pub fn database_bytes(dir: &Path) -> io::Result<u64> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(err) => return Err(err),
    };
    let mut total = 0u64;
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
        if [".sqlite3", ".sqlite3-wal", ".sqlite3-shm"]
            .iter()
            .any(|suffix| name.ends_with(suffix))
        {
            match fs::metadata(entry.path()) {
                Ok(meta) if meta.is_file() => total = total.saturating_add(meta.len()),
                Ok(_) => {}
                // SQLite can remove sidecars as a writer closes.
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => return Err(err),
            }
        }
    }
    Ok(total)
}

pub fn default_dir() -> Result<PathBuf> {
    #[cfg(windows)]
    let home = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    #[cfg(not(windows))]
    let home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")));
    Ok(home
        .context("Set the user data directory or pass --db-dir")?
        .join("RepeaterNet"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_counts_database_files_and_sidecars() -> Result<()> {
        let dir = tempfile::tempdir()?;
        for name in [
            "sondehub-import.sqlite3",
            "corescope.sqlite3-wal",
            "mesh.SQLITE3-SHM",
            "archive.backup.sqlite3",
        ] {
            fs::write(dir.path().join(name), [0; 200])?;
        }
        fs::create_dir(dir.path().join("directory.sqlite3"))?;
        fs::write(dir.path().join("capture.sqlite3.lock"), [0; 300])?;
        assert_eq!(database_bytes(dir.path())?, 800);
        let budget = Budget {
            total_budget_gb: 0.000_002,
            reserve_gb: 0.000_001,
        };
        assert_eq!(budget.ceiling()?, 1_000);
        assert!(
            Budget {
                reserve_gb: 1.0,
                ..budget
            }
            .ceiling()
            .is_err()
        );
        Ok(())
    }
}
