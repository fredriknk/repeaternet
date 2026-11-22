use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use repeaternet_rf_calibration::{extract, paths, predict, profiles, snapshot};

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
    /// Sample terrain profiles for every link into `<work>/profiles.jsonl`.
    Profiles,
    /// Evaluate Rust engines into `<work>/predictions/`.
    Predict,
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
        Command::Profiles => profiles::run(&work.join("dataset"), &work.join("profiles.jsonl"))?,
        Command::Predict => predict::run(
            &work.join("dataset"),
            &work.join("profiles.jsonl"),
            &work.join("predictions"),
        )?,
    }
    Ok(())
}
