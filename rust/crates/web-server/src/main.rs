mod api;
mod assets;
mod catalog;
mod coverage_analysis;
mod coverage_history;
mod coverage_http;
mod coverage_inspect;
mod coverage_runs;
mod downloads;
mod link_http;
mod live_coverage_http;
mod live_terrain;
mod mesh_paint;
mod placement_http;
mod provider_http;
mod route_http;
mod route_inputs;
mod route_output;
mod security;
mod server;
mod source_http;
mod targets;
mod terrain_http;

use std::{
    env,
    ffi::OsString,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use clap::Parser;
use repeaternet_project_store::ProjectStore;
use tokio::net::TcpListener;

const PROJECT_CATALOG: &str = "projects.sqlite3";

#[derive(Parser)]
#[command(version, about = "Run the native RepeaterNet dashboard")]
struct Options {
    /// IP address to listen on, or localhost.
    #[arg(long, default_value = "127.0.0.1", value_parser = parse_host)]
    host: IpAddr,
    /// TCP port to listen on. Use 0 to ask the OS for a free port.
    #[arg(long, default_value_t = 8000)]
    port: u16,
    /// Workspace data directory.
    #[arg(long, env = "RF_PLANNER_DATA")]
    data: Option<PathBuf>,
    /// Peer IP allowed to supply X-Forwarded-Host and X-Forwarded-Proto.
    #[arg(long, env = "RF_PLANNER_TRUSTED_PROXY")]
    trusted_proxy: Option<IpAddr>,
    /// Serve the dashboard without opening a browser.
    #[arg(long)]
    no_browser: bool,
    /// Give each browser its own private projects instead of one shared
    /// project list. Terrain downloads are shared either way.
    #[arg(
        long,
        env = "RF_PLANNER_PRIVATE_SESSIONS",
        value_parser = clap::builder::BoolishValueParser::new()
    )]
    private_sessions: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let options = Options::parse();
    let (data, using_legacy_data) = resolve_data_dir(
        options.data,
        cfg!(windows),
        |key| env::var_os(key),
        Path::is_file,
    );
    if using_legacy_data {
        eprintln!("Using existing RepeaterNet data at {}.", data.display());
    }
    let token = env::var("RF_PLANNER_TOKEN")
        .or_else(|error| match error {
            env::VarError::NotPresent => Ok(String::new()),
            error => Err(error),
        })
        .context("RF_PLANNER_TOKEN must be valid Unicode")?;
    let address = SocketAddr::new(options.host, options.port);
    let listener = TcpListener::bind(address)
        .await
        .with_context(|| format!("Could not listen on {address}"))?;
    let address = listener.local_addr()?;
    let url = format!("http://{}/", browser_address(address));
    let (store, data) = tokio::task::spawn_blocking(move || {
        let store = ProjectStore::new(data.join(PROJECT_CATALOG), &data)
            .with_context(|| format!("Could not open project catalog in {}", data.display()))?;
        Ok::<_, anyhow::Error>((store, data))
    })
    .await??;
    let app = server::router(
        store,
        &data,
        &token,
        if options.private_sessions {
            server::Sessions::Private
        } else {
            server::Sessions::Shared
        },
        is_loopback(address.ip()),
        options.trusted_proxy,
    );
    println!("RepeaterNet is listening at {url}");
    if options.private_sessions {
        println!("Private sessions: each browser has its own projects.");
    }
    if token.is_empty() && !is_loopback(address.ip()) && options.trusted_proxy.is_none() {
        eprintln!(
            "Warning: listening on {address} without RF_PLANNER_TOKEN. Anyone who can \
             reach this server can open, change and delete every project."
        );
    }
    if !options.no_browser
        && let Err(error) = open::that_detached(&url)
    {
        eprintln!("Could not open the browser: {error}. Open {url} manually.");
    }

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
    .context("Dashboard server failed")
}

fn parse_host(host: &str) -> Result<IpAddr, String> {
    if host.eq_ignore_ascii_case("localhost") {
        return Ok(Ipv4Addr::LOCALHOST.into());
    }
    host.parse()
        .map_err(|_| "expected an IPv4 or IPv6 address or localhost".to_owned())
}

fn resolve_data_dir(
    explicit: Option<PathBuf>,
    windows: bool,
    env: impl Fn(&str) -> Option<OsString>,
    is_file: impl Fn(&Path) -> bool,
) -> (PathBuf, bool) {
    if let Some(directory) = explicit {
        return (directory, false);
    }
    let env_path = |key| {
        env(key)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    };
    let (default, legacy) = if windows {
        if let Some(base) = env_path("LOCALAPPDATA") {
            let base = base.join("RepeaterNet");
            (base.join("data"), base.join("rust-preview"))
        } else {
            let base = PathBuf::from("web-data");
            (base.clone(), base.join("rust-preview"))
        }
    } else {
        let base = env_path("XDG_DATA_HOME")
            .or_else(|| env_path("HOME").map(|home| home.join(".local/share")))
            .map_or_else(
                || PathBuf::from("web-data"),
                |base| base.join("repeaternet"),
            );
        let legacy = base.join("rust-preview");
        (base, legacy)
    };
    if !is_file(&default.join(PROJECT_CATALOG)) && is_file(&legacy.join(PROJECT_CATALOG)) {
        (legacy, true)
    } else {
        (default, false)
    }
}

fn is_loopback(ip: IpAddr) -> bool {
    ip.to_canonical().is_loopback()
}

fn browser_address(address: SocketAddr) -> SocketAddr {
    let loopback = match address.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => Ipv4Addr::LOCALHOST.into(),
        IpAddr::V6(ip) if ip.is_unspecified() => Ipv6Addr::LOCALHOST.into(),
        ip => ip,
    };
    SocketAddr::new(loopback, address.port())
}

async fn shutdown_signal() {
    let interrupt = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            eprintln!("Could not listen for Ctrl+C: {error}");
            std::future::pending::<()>().await;
        }
    };
    #[cfg(windows)]
    {
        let console_break = async {
            match tokio::signal::windows::ctrl_break() {
                Ok(mut signal) => {
                    signal.recv().await;
                }
                Err(error) => {
                    eprintln!("Could not listen for Ctrl+Break: {error}");
                    std::future::pending::<()>().await;
                }
            }
        };
        tokio::select! {
            _ = interrupt => {},
            _ = console_break => {},
        }
    }
    #[cfg(unix)]
    {
        let terminate = async {
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(mut signal) => {
                    signal.recv().await;
                }
                Err(error) => {
                    eprintln!("Could not listen for SIGTERM: {error}");
                    std::future::pending::<()>().await;
                }
            }
        };
        tokio::select! {
            _ = interrupt => {},
            _ = terminate => {},
        }
    }
    #[cfg(not(any(unix, windows)))]
    interrupt.await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projects_are_shared_unless_private_sessions_is_given() {
        if env::var_os("RF_PLANNER_PRIVATE_SESSIONS").is_some() {
            return; // The environment overrides the default under test.
        }
        let parse = |args: &[&str]| Options::try_parse_from(args).unwrap().private_sessions;
        assert!(!parse(&["repeaternet"]));
        assert!(parse(&["repeaternet", "--private-sessions"]));
    }

    #[test]
    fn explicit_data_bypasses_default_resolution() {
        let explicit = PathBuf::from("custom");
        assert_eq!(
            resolve_data_dir(
                Some(explicit.clone()),
                true,
                |_| panic!("explicit data should bypass environment lookup"),
                |_| panic!("explicit data should bypass catalog lookup"),
            ),
            (explicit, false)
        );
    }

    #[test]
    fn default_data_dir_uses_platform_environment_and_fallbacks() {
        for (windows, environment, expected) in [
            (
                true,
                &[("LOCALAPPDATA", "appdata")][..],
                "appdata/RepeaterNet/data",
            ),
            (
                true,
                &[("LOCALAPPDATA", ""), ("HOME", "home")][..],
                "web-data",
            ),
            (
                false,
                &[("XDG_DATA_HOME", "xdg"), ("HOME", "home")][..],
                "xdg/repeaternet",
            ),
            (
                false,
                &[("XDG_DATA_HOME", ""), ("HOME", "home")][..],
                "home/.local/share/repeaternet",
            ),
            (false, &[][..], "web-data"),
        ] {
            let env = |key: &str| {
                environment
                    .iter()
                    .find(|(name, _)| *name == key)
                    .map(|(_, value)| OsString::from(value))
            };
            assert_eq!(
                resolve_data_dir(None, windows, env, |_| false),
                (PathBuf::from(expected), false)
            );
        }
    }

    #[test]
    fn uses_legacy_data_only_when_its_catalog_is_the_only_one_present() {
        for (windows, key, default, legacy) in [
            (
                true,
                "LOCALAPPDATA",
                "base/RepeaterNet/data",
                "base/RepeaterNet/rust-preview",
            ),
            (
                false,
                "XDG_DATA_HOME",
                "base/repeaternet",
                "base/repeaternet/rust-preview",
            ),
            (false, "", "web-data", "web-data/rust-preview"),
        ] {
            let env = |name: &str| (name == key).then(|| OsString::from("base"));
            let default = PathBuf::from(default);
            let legacy = PathBuf::from(legacy);
            let legacy_catalog = legacy.join(PROJECT_CATALOG);
            let default_catalog = default.join(PROJECT_CATALOG);
            assert_eq!(
                resolve_data_dir(None, windows, env, |path| path == legacy_catalog),
                (legacy, true)
            );
            assert_eq!(
                resolve_data_dir(None, windows, env, |path| {
                    path == legacy_catalog || path == default_catalog
                }),
                (default, false)
            );
        }
    }
}
