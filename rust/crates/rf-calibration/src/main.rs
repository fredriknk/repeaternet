use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use repeaternet_rf_calibration::{extract, paths, predict, profiles, snapshot, sondehub};

/// Measured-link calibration harness for RepeaterNet path-loss engines.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Calibration work directory (default `%LOCALAPPDATA%/RepeaterNet/calibration`
    /// or `RF_CALIBRATION_DIR`).
    #[arg(long, global = true)]
    work_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Copy capture databases into `<work>/snapshots` (read-only sources).
    Snapshot {
        /// Directory holding the capture databases (default the RepeaterNet data root).
        #[arg(long)]
        source_dir: Option<PathBuf>,
    },
    /// Build `<work>/dataset/{links,nodes}.jsonl` from the snapshots.
    Extract,
    /// Check air-to-ground reception against the SondeHub snapshot.
    Sondehub {
        /// Maximum estimated new terrain download in decimal GB (at most 20).
        #[arg(long, default_value_t = 20.0)]
        max_download_gb: f64,
        /// Estimate receiver terrain without downloading or writing exports.
        #[arg(long)]
        estimate_only: bool,
    },
    /// Sample terrain profiles for every link into `<work>/profiles.jsonl`.
    Profiles {
        /// Directed link JSONL (default <work>/dataset/links.jsonl).
        #[arg(long)]
        links: Option<PathBuf>,
        /// Maximum estimated new terrain download in decimal GB.
        #[arg(long, default_value_t = 5.0)]
        max_download_gb: f64,
    },
    /// Evaluate Rust engines into `<work>/predictions/`.
    Predict {
        /// Directed link JSONL (default <work>/dataset/links.jsonl).
        #[arg(long)]
        links: Option<PathBuf>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let work = paths::work_dir(cli.work_dir.as_deref())?;
    match cli.command {
        Command::Snapshot { source_dir } => {
            let source = match source_dir {
                Some(dir) => dir,
                None => paths::repeaternet_data_root()?,
            };
            for name in snapshot::snapshot_all(&source, &work.join("snapshots"))? {
                println!("snapshot {name}");
            }
        }
        Command::Extract => extract::run(&work.join("snapshots"), &work.join("dataset"))?,
        Command::Sondehub {
            max_download_gb,
            estimate_only,
        } => sondehub::run(&work, max_download_gb, estimate_only)?,
        Command::Profiles {
            links,
            max_download_gb,
        } => profiles::run(
            &links.unwrap_or_else(|| work.join("dataset/links.jsonl")),
            &work,
            max_download_gb,
        )?,
        Command::Predict { links } => predict::run(
            &links.unwrap_or_else(|| work.join("dataset/links.jsonl")),
            &work,
        )?,
    }
    Ok(())
}
