mod capture;
mod db;
mod model;

use std::{
    env, fs, io,
    num::NonZeroUsize,
    path::{Component, Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
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
    /// Maximum HTTP requests per second, including retries.
    #[arg(long, default_value = "1", value_parser = parse_rate)]
    rate: Duration,
    /// Maximum pages per paginated endpoint in one capture.
    #[arg(long, default_value = "40")]
    max_pages: NonZeroUsize,
    /// Per-node neighbor bounds as south,north,west,east in WGS84 degrees.
    #[arg(long, default_value = "57.5,71.5,4,31.5", allow_hyphen_values = true)]
    bbox: Bbox,
    /// Minutes to wait between captures in run mode.
    #[arg(long = "loop", value_name = "MINUTES", value_parser = clap::value_parser!(u64).range(5..=u64::MAX / 60))]
    loop_minutes: Option<u64>,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Mode {
    /// Capture all nodes and observers, the full graph, and local node neighbors.
    Snapshot,
    /// Resume capturing packet receptions from all regions.
    Packets,
    /// Capture packet receptions and take a topology snapshot.
    Run,
}

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Snapshot => "snapshot",
            Self::Packets => "packets",
            Self::Run => "run",
        }
    }
}

#[derive(Clone, Copy)]
struct Bbox {
    south: f64,
    north: f64,
    west: f64,
    east: f64,
}

impl Bbox {
    fn contains(self, lat: f64, lon: f64) -> bool {
        (self.south..=self.north).contains(&lat) && (self.west..=self.east).contains(&lon)
    }
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
        (0.001..=1000.0).contains(&rate),
        "Enter a rate from 0.001 to 1000 requests per second"
    );
    Ok(Duration::from_secs_f64(rate.recip()))
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    ensure!(
        cli.loop_minutes.is_none() || cli.mode == Mode::Run,
        "Use --loop with run mode"
    );
    let mut client = CaptureClient::new(&cli.base_url, cli.rate)?;
    let path = database_path(cli.db)?;
    fs::create_dir_all(path.parent().context("Select a SQLite file path")?)?;
    let mut db =
        db::open(&path).with_context(|| format!("Open capture database {}", path.display()))?;
    let interrupted = Arc::new(AtomicBool::new(false));
    let interrupted_by_signal = Arc::clone(&interrupted);
    ctrlc::set_handler(move || interrupted_by_signal.store(true, Ordering::Relaxed))
        .context("Install Ctrl+C handler")?;
    let stopped = || interrupted.load(Ordering::Relaxed);

    while !stopped() {
        let ok = capture::run_once(
            &mut db,
            &mut client,
            cli.mode,
            cli.bbox,
            cli.max_pages.get(),
            &stopped,
        )?;
        let Some(minutes) = cli.loop_minutes else {
            ensure!(
                ok || stopped(),
                "Capture had errors. See capture_runs for details."
            );
            break;
        };
        let start = Instant::now();
        let interval = Duration::from_secs(minutes * 60);
        while !stopped() && start.elapsed() < interval {
            thread::sleep(
                interval
                    .saturating_sub(start.elapsed())
                    .min(Duration::from_millis(100)),
            );
        }
    }
    Ok(())
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
