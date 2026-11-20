# RepeaterNet

Self-hosted, terrain-aware RF mesh planning for Norway. Plan a connection between
two endpoints, include existing MeshCore repeaters, compare proposed routes, and
estimate mesh coverage using the configured radio and terrain data.

RepeaterNet is a pre-release web application. RF results are planning predictions;
validate important links with site surveys and field measurements.

## Run the executable

RepeaterNet is a single self-contained executable. It embeds the dashboard and
needs no Python, GIS runtime or installer. Download `repeaternet-windows-x64.zip`
from the [GitHub releases](https://github.com/fredriknk/repeaternet/releases),
check it against `SHA256SUMS.txt`, and double-click `repeaternet.exe`. The
dashboard opens in your default browser at **http://127.0.0.1:8000/**.

Releases ship only the Windows executable. On Linux, [build from source](#build-from-source)
or use Docker.

Use `--no-browser` for headless operation, `--host`/`--port` to change the listen
address and `--data <dir>` to select a data directory. Projects default to
`%LOCALAPPDATA%\RepeaterNet\data` on Windows and
`${XDG_DATA_HOME:-~/.local/share}/repeaternet` on Linux. Earlier preview data in
a `rust-preview` subfolder is used automatically when no newer catalog exists.
Preserve the browser origin and workspace cookie when reopening existing projects.
Stop the server with Ctrl+C.

## Start with Docker

```sh
docker compose up -d --build
```

Open **http://localhost:8000**. The image contains only the static binary and CA
certificates, and runs as UID 1000. Docker stores projects, terrain and saved
coverage in the persistent `planner-data` volume. Rebuilding the image preserves
that volume, including volumes created by the earlier Python image.

The default bind address is localhost. Set `RF_PLANNER_BIND=0.0.0.0` in your
environment or `.env` for LAN access. Set `RF_PLANNER_TOKEN` to require a shared
access token. Use an HTTPS reverse proxy for access outside a trusted network.

## Build from source

Install Rust with rustup; `rust/rust-toolchain.toml` selects 1.100.0. Windows
builds need the MSVC C++ Build Tools and a Windows SDK.

```powershell
.\tools\build-rust.ps1
.\rust\target\release\repeaternet.exe
```

```sh
cd rust
cargo build --locked --release -p repeaternet
./target/release/repeaternet
```

See the [application guide](rust/README.md) for configuration, HTTP endpoints,
provider integrations, supported terrain formats and compatibility limits, and
[release validation](docs/RELEASE_VALIDATION.md) for verification and packaging.

## Plan a connection

1. Set endpoints A and B with the map toolbar, the map's right-click menu, or
   coordinates.
2. Upload ground DTM GeoTIFFs, or let the planner prepare Kartverket corridor
   terrain. Add surface DOM tiles when available.
3. Optionally load nearby MeshCore routers and mark them Optional, Required, or
   Excluded (from the list or by clicking them on the map). Add proposed sites and
   choose the search objective.
4. Select **Find repeater route**. Ready terrain starts the search directly;
   missing terrain presents a combined download-and-search confirmation.
5. Open **Analysis** to compare alternatives, inspect link profiles, and export
   results. Use **Map** for a full map view.
6. In **Coverage**, calculate predicted handheld coverage and inspect locations.
   In **Scenarios**, assess targets and compare height or router-failure scenarios.

## Documentation

- [User guide](docs/USER_GUIDE.md): projects, terrain, routing, coverage, exports,
  and interpretation of RF results.
- [Application guide](rust/README.md): launcher, configuration, HTTP endpoints,
  providers and compatibility limits.
- [Development and architecture](docs/DEVELOPMENT.md): source layout, tests,
  CI, packaging, deployment, and benchmark reproduction.
- [UX design rules](docs/UX_DESIGN_RULES.md): binding rules for dashboard and
  user-facing changes; agents start from [AGENTS.md](AGENTS.md).
- [Release validation](docs/RELEASE_VALIDATION.md): executable verification,
  packaging and the clean-machine test.
- [Current roadmap](docs/ROADMAP.md): unfinished acceptance checks and engineering
  work, including the cleanup record.
- [Terrain fixture](docs/REAL_TERRAIN_FIXTURE.md) and
  [benchmark artifacts](docs/benchmarks): reproducible performance evidence.
- [Historical records](docs/history/README.md): dated plans and browser reports.

## Server configuration

Run **one server process** per data directory: workspace state, job scheduling,
and caches live in that process. RF evaluation uses worker threads.

| Variable | Default | Purpose |
| --- | --- | --- |
| `RF_PLANNER_DATA` | per-user data directory (Docker: `/data`) | Project catalog and workspace files; `--data` takes precedence |
| `RF_PLANNER_TOKEN` | unset | Optional shared access token |
| `RF_PLANNER_MAX_ACTIVE_JOBS` | `1` | Concurrent planner jobs; outstanding queue is bounded to twice this count |
| `RF_PLANNER_RF_CACHE_ENTRIES` | `50000` | Route scalar-metric cache capacity |
| `RF_PLANNER_COVERAGE_CACHE_ENTRIES` | `50000` | Separate coverage scalar-metric cache capacity |
| `RF_PLANNER_COVERAGE_TERRAIN_RESOLUTION_M` | `25` | Starting resolution for supplemental coverage terrain |
| `RF_PLANNER_TERRAIN_CACHE_MB` | half of physical or container memory | Decoded terrain kept in memory across sampling calls; beyond it, terrain is read from disk per batch. `0` disables retention |
| `RF_PLANNER_CACHE_DIR` | `<data>/shared-cache` | Shared Kartverket and Overpass download cache |
| `RF_PLANNER_OVERPASS_URL` | public Overpass API | Road data endpoint |
| `RF_PLANNER_TRUSTED_PROXY` | unset | Reverse-proxy peer IP allowed to set forwarded host and scheme |
| `RF_PLANNER_BIND` | `127.0.0.1` | Compose host-side port binding |

The supplied Compose file exposes the token and bind settings. To change worker
or cache settings in Docker, add them to the service's `environment` mapping.
Increasing job concurrency or cache sizes increases memory use.

Projects are separated by browser workspace cookies; the shared token does not
provide individual user accounts. Back up the complete data directory or Docker
volume with the server stopped so the SQLite catalog and project files agree.
