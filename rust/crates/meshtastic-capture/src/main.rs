mod db;
mod model;
mod poll;
mod sources;
mod upstream;
mod viewer;

#[cfg(test)]
mod tests;

use std::{
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use clap::{Args, Parser, Subcommand, builder::PossibleValuesParser};
use sondehub_import::{DEFAULT_BUDGET_GB, default_dir, parse_gb};

#[derive(Parser)]
#[command(version, about = "Capture and view measured RF links")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Collect Meshtastic measurements from the enabled public hosts.
    Run(Run),
    /// View MeshCore, Meshtastic and SondeHub capture files.
    View(View),
}

#[derive(Args)]
struct Run {
    /// Total limit for SQLite capture files in the database directory (decimal GB).
    #[arg(long, default_value_t = DEFAULT_BUDGET_GB, value_parser = parse_gb)]
    total_budget_gb: f64,
    /// Directory for the per-host capture databases.
    #[arg(long)]
    db_dir: Option<PathBuf>,
    /// Stop after this duration (for example 12h or 20m).
    #[arg(long, value_parser = parse_duration)]
    duration: Option<Duration>,
    /// Run a single startup cycle per host.
    #[arg(long)]
    once: bool,
    /// Enable only this host. Repeat to enable multiple hosts.
    #[arg(long, value_parser = host_values())]
    only: Vec<String>,
    /// Disable this host. Repeat to disable multiple hosts.
    #[arg(long, value_parser = host_values())]
    skip: Vec<String>,
    /// Maximum requests per host in any rolling 24 hours.
    #[arg(long, default_value_t = 300, value_parser = clap::value_parser!(u32).range(1..))]
    daily_budget: u32,
    /// Shared minimum spacing between requests to the same host.
    #[arg(long, default_value_t = 2000, value_parser = clap::value_parser!(u64).range(2000..=3_600_000))]
    min_interval_ms: u64,
    /// Loopback port for the viewer.
    #[arg(long, default_value_t = 8795)]
    dashboard_port: u16,
    /// Run without opening a viewer server.
    #[arg(long)]
    no_dashboard: bool,
}

#[derive(Args)]
struct View {
    /// SQLite glob. Repeat to read from more than one directory.
    #[arg(long)]
    glob: Vec<String>,
    /// Directory to search for capture files.
    #[arg(long, conflicts_with = "glob")]
    db_dir: Option<PathBuf>,
    /// Total limit shown in the viewer (decimal GB).
    #[arg(long, default_value_t = DEFAULT_BUDGET_GB, value_parser = parse_gb)]
    total_budget_gb: f64,
    /// Loopback port for the viewer.
    #[arg(long, default_value_t = 8795)]
    port: u16,
}

fn host_values() -> PossibleValuesParser {
    PossibleValuesParser::new(poll::SOURCES.iter().map(|source| source.host))
}

fn parse_duration(raw: &str) -> Result<Duration> {
    let (number, unit) = raw.split_at(raw.char_indices().last().map_or(0, |(index, _)| index));
    let scale = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        _ => anyhow::bail!("Use a duration such as 12h, 20m or 45s"),
    };
    let seconds = number
        .parse::<u64>()?
        .checked_mul(scale)
        .context("Duration is too large")?;
    ensure!(
        (1..=31_536_000).contains(&seconds),
        "Duration must be from one second to one year"
    );
    Ok(Duration::from_secs(seconds))
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    static INTERRUPTED: AtomicBool = AtomicBool::new(false);
    ctrlc::set_handler(|| INTERRUPTED.store(true, Ordering::Relaxed))?;
    let interrupted = || INTERRUPTED.load(Ordering::Relaxed);
    match cli.command {
        Command::Run(args) => {
            let start = Instant::now();
            let stop =
                || interrupted() || args.duration.is_some_and(|limit| start.elapsed() >= limit);
            let dir = args.db_dir.clone().map_or_else(default_dir, Ok)?;
            std::fs::create_dir_all(&dir)?;
            let _viewer = if args.no_dashboard {
                None
            } else {
                Some(viewer::start(
                    viewer::directory_globs(&dir),
                    args.dashboard_port,
                    args.total_budget_gb,
                    Some(dir.clone()),
                )?)
            };
            poll::run(&args, &dir, &stop)
        }
        Command::View(args) => {
            let directory = if args.glob.is_empty() {
                Some(args.db_dir.map_or_else(default_dir, Ok)?)
            } else {
                None
            };
            let patterns = directory
                .as_deref()
                .map_or(args.glob, viewer::directory_globs);
            let _viewer = viewer::start(patterns, args.port, args.total_budget_gb, directory)?;
            while !interrupted() {
                thread::sleep(Duration::from_millis(200));
            }
            Ok(())
        }
    }
}
