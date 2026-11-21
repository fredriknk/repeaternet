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
    /// View MeshCore and Meshtastic capture files.
    View(View),
}

#[derive(Args)]
struct Run {
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

fn default_dir() -> Result<PathBuf> {
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
                    vec![viewer::directory_glob(&dir)],
                    args.dashboard_port,
                )?)
            };
            poll::run(&args, &dir, &stop)
        }
        Command::View(args) => {
            let patterns = if args.glob.is_empty() {
                vec![viewer::directory_glob(
                    &args.db_dir.map_or_else(default_dir, Ok)?,
                )]
            } else {
                args.glob
            };
            let _viewer = viewer::start(patterns, args.port)?;
            while !interrupted() {
                thread::sleep(Duration::from_millis(200));
            }
            Ok(())
        }
    }
}
