mod capture;
mod dashboard;
mod db;
mod model;
mod watch;

use std::{
    env, fs, io,
    num::NonZeroUsize,
    path::{Component, Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use clap::{Parser, ValueEnum};
use repeaternet_providers::corescope::{DEFAULT_BASE_URL, capture::CaptureClient};

#[derive(Parser)]
#[command(
    version,
    about = "Capture CoreScope RF measurements and topology to SQLite"
)]
struct Cli {
    #[arg(value_enum)]
    mode: Mode,
    /// SQLite file outside the RepeaterNet repo and planner data directory.
    #[arg(long, value_name = "FILE")]
    db: Option<PathBuf>,
    /// CoreScope server URL.
    #[arg(long, default_value = DEFAULT_BASE_URL)]
    base_url: String,
    /// Maximum HTTP requests per second, including retries (at most 1).
    #[arg(long, default_value = "0.5", value_parser = parse_rate)]
    rate: Duration,
    /// Page cap per endpoint per cycle (packets in watch: 8, otherwise: 40).
    #[arg(long)]
    max_pages: Option<NonZeroUsize>,
    /// Per-node neighbor bounds as south,north,west,east in WGS84 degrees.
    #[arg(long, default_value = "54,71.5,4,31.7", allow_hyphen_values = true)]
    bbox: Bbox,
    /// Packet poll interval in minutes in watch mode.
    #[arg(long, default_value_t = 15, value_parser = clap::value_parser!(u64).range(5..=525_600))]
    packet_minutes: u64,
    /// Topology snapshot interval in minutes in watch mode.
    #[arg(long, default_value_t = 360, value_parser = clap::value_parser!(u64).range(60..=525_600))]
    snapshot_minutes: u64,
    /// Maximum per-node neighbor requests per packet cycle.
    #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u32).range(0..=20))]
    neighbor_batch: u32,
    /// Hours before a node's neighbors are fetched again.
    #[arg(long, default_value_t = 168, value_parser = clap::value_parser!(u64).range(24..=8_760))]
    neighbor_refresh_hours: u64,
    /// Loopback port for the local watch dashboard.
    #[arg(long, default_value_t = 8790)]
    dashboard_port: u16,
    /// Run without the local watch dashboard.
    #[arg(long)]
    no_dashboard: bool,
}

impl Cli {
    fn neighbor_refresh_ms(&self) -> i64 {
        self.neighbor_refresh_hours as i64 * 3_600_000
    }
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Mode {
    /// Capture nodes, observers, the full graph, and one batch of local node neighbors.
    Snapshot,
    /// Resume capturing packet receptions from all regions.
    Packets,
    /// Capture packet receptions and take a topology snapshot.
    Run,
    /// Poll CoreScope continuously and serve a local live map.
    Watch,
}

#[derive(Clone, Copy, serde::Serialize)]
struct Bbox {
    south: f64,
    north: f64,
    west: f64,
    east: f64,
}

impl std::str::FromStr for Bbox {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        let coordinates = value
            .split(',')
            .map(|part| part.trim().parse::<f64>())
            .collect::<std::result::Result<Vec<_>, _>>()
            .context("Enter numeric bounds as south,north,west,east")?;
        let [south, north, west, east] = coordinates[..] else {
            anyhow::bail!("Enter four bounds as south,north,west,east");
        };
        ensure!(
            (-90.0..=90.0).contains(&south)
                && (-90.0..=90.0).contains(&north)
                && (-180.0..=180.0).contains(&west)
                && (-180.0..=180.0).contains(&east)
                && south <= north
                && west <= east,
            "Enter ordered latitude and longitude bounds within WGS84"
        );
        Ok(Self {
            south,
            north,
            west,
            east,
        })
    }
}

fn parse_rate(value: &str) -> Result<Duration> {
    let rate: f64 = value
        .parse()
        .context("Enter a number of requests per second")?;
    ensure!(
        (0.001..=1.0).contains(&rate),
        "Enter a rate from 0.001 to 1 request per second"
    );
    Ok(Duration::from_secs_f64(rate.recip()))
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut client = CaptureClient::new(&cli.base_url, cli.rate)?;
    let path = database_path(cli.db.clone())?;
    fs::create_dir_all(path.parent().context("Select a SQLite file path")?)?;
    let mut db =
        db::open(&path).with_context(|| format!("Open capture database {}", path.display()))?;
    let interrupted = Arc::new(AtomicBool::new(false));
    let interrupted_by_signal = Arc::clone(&interrupted);
    ctrlc::set_handler(move || interrupted_by_signal.store(true, Ordering::Relaxed))
        .context("Install Ctrl+C handler")?;
    let stopped = || interrupted.load(Ordering::Relaxed);

    println!("Capture database · {}", path.display());
    if cli.mode == Mode::Watch {
        let _dashboard = if cli.no_dashboard {
            None
        } else {
            Some(dashboard::start(
                path,
                client.base_url(),
                cli.bbox,
                cli.dashboard_port,
            )?)
        };
        watch::run(&mut db, &mut client, &cli, &stopped)
    } else {
        watch::once(&mut db, &mut client, &cli, &stopped)
    }
}

fn database_path(explicit: Option<PathBuf>) -> Result<PathBuf> {
    let path = match explicit {
        Some(path) => path,
        None => default_data_home()?
            .join("RepeaterNet")
            .join("corescope-capture.sqlite3"),
    };
    ensure!(path.file_name().is_some(), "Select a SQLite file path");
    let path = resolve_path(&path)?;
    if let Some(local) = env_path("LOCALAPPDATA") {
        let planner = resolve_path(&local.join("RepeaterNet").join("data"))?;
        #[cfg(windows)]
        let inside_planner = path
            .to_string_lossy()
            .to_lowercase()
            .starts_with(&(planner.to_string_lossy().to_lowercase() + "\\"))
            || path
                .to_string_lossy()
                .eq_ignore_ascii_case(&planner.to_string_lossy());
        #[cfg(not(windows))]
        let inside_planner = path.starts_with(planner);
        ensure!(
            !inside_planner,
            "Select a capture file outside the RepeaterNet planner data directory"
        );
    }
    ensure!(
        !path.ancestors().any(|ancestor| {
            ancestor.join("rust/crates/web-server/Cargo.toml").is_file()
                && ancestor.join("AGENTS.md").is_file()
        }),
        "Select a capture file outside the RepeaterNet repository"
    );
    Ok(path)
}

fn default_data_home() -> Result<PathBuf> {
    #[cfg(windows)]
    let data_home = env_path("LOCALAPPDATA")
        .or_else(|| env_path("USERPROFILE").map(|path| path.join("AppData").join("Local")));
    #[cfg(not(windows))]
    let data_home = env_path("XDG_DATA_HOME")
        .or_else(|| env_path("HOME").map(|path| path.join(".local").join("share")));
    data_home.context("No user data directory is available. Select a capture file with --db.")
}

fn env_path(name: &str) -> Option<PathBuf> {
    env::var_os(name)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

// Resolve symlinks and junctions before testing containment, including when
// the requested database or some of its parent directories do not yet exist.
fn resolve_path(path: &Path) -> io::Result<PathBuf> {
    let absolute = env::current_dir()?.join(path);
    let mut resolved = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Normal(part) => {
                resolved.push(part);
                match fs::symlink_metadata(&resolved) {
                    Ok(_) => resolved = resolved.canonicalize()?,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
            }
            Component::ParentDir => {
                resolved.pop();
            }
            Component::CurDir => {}
            prefix => resolved.push(prefix),
        }
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests;
