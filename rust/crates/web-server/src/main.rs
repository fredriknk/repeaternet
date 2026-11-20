mod api;
mod assets;
mod catalog;
mod coverage_analysis;
mod coverage_history;
mod coverage_http;
mod coverage_inspect;
mod coverage_runs;
mod downloads;
mod provider_http;
mod route_http;
mod route_inputs;
mod route_output;
mod security;
mod server;
mod targets;
mod terrain_http;

use std::{
    env,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::PathBuf,
};

use anyhow::{Context, Result};
use clap::Parser;
use repeaternet_project_store::ProjectStore;
use tokio::net::TcpListener;

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
}

#[tokio::main]
async fn main() -> Result<()> {
    let options = Options::parse();
    let data = options.data.unwrap_or_else(default_data_dir);
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
    let store = tokio::task::spawn_blocking(move || {
        ProjectStore::new(data.join("projects.sqlite3"), &data)
            .with_context(|| format!("Could not open project catalog in {}", data.display()))
    })
    .await??;
    let app = server::router(
        store,
        &token,
        is_loopback(address.ip()),
        options.trusted_proxy,
    );
    println!("RepeaterNet dashboard preview is listening at {url}");
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

fn default_data_dir() -> PathBuf {
    if cfg!(windows) {
        if let Some(base) = env::var_os("LOCALAPPDATA").filter(|base| !base.is_empty()) {
            return PathBuf::from(base).join("RepeaterNet").join("rust-preview");
        }
    } else {
        if let Some(base) = env::var_os("XDG_DATA_HOME").filter(|base| !base.is_empty()) {
            return PathBuf::from(base).join("repeaternet").join("rust-preview");
        }
        if let Some(home) = env::var_os("HOME").filter(|home| !home.is_empty()) {
            return PathBuf::from(home).join(".local/share/repeaternet/rust-preview");
        }
    }
    PathBuf::from("web-data").join("rust-preview")
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
