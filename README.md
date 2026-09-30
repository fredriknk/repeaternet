# RepeaterNet

Self-hosted, terrain-aware RF mesh planning for Norway. Plan a connection between
two endpoints, include existing MeshCore repeaters, compare proposed routes, and
estimate mesh coverage using the configured radio and terrain data.

RepeaterNet is a pre-release web application. RF results are planning predictions;
validate important links with site surveys and field measurements.

## Start with Docker

```sh
docker compose up -d --build
```

Open **http://localhost:8000**. Docker stores projects, terrain, and saved coverage
in the persistent `planner-data` volume. Rebuilding the image preserves that volume.

The default bind address is localhost. Set `RF_PLANNER_BIND=0.0.0.0` in your
environment or `.env` for LAN access. Set `RF_PLANNER_TOKEN` to require a shared
access token. Use an HTTPS reverse proxy for access outside a trusted network.

## Install directly

Python 3.12 or newer is required. Install in a virtual environment:

```sh
python -m venv .venv
# Activate it: .venv\Scripts\Activate.ps1 on Windows;
# source .venv/bin/activate on Linux/macOS.
python -m pip install -e .
python -m rf_router_planner
```

The installed `rf-router-planner` command starts the same web server.
Use `--host 0.0.0.0 --port 8000` to change its listening address.

## Plan a connection

1. Set endpoints A and B on the map or enter coordinates.
2. Upload ground DTM GeoTIFFs, or let the planner prepare Kartverket corridor
   terrain. Add surface DOM tiles when available.
3. Optionally load nearby MeshCore routers and mark them Optional, Required, or
   Excluded. Add proposed sites and choose the search objective.
4. Select **Find repeater route**. Ready terrain starts the search directly;
   missing terrain presents a combined download-and-search confirmation.
5. Open **Analysis** to compare alternatives, inspect link profiles, and export
   results. Use **Map** for a full map view.
6. Calculate **Predicted mesh coverage**, inspect locations, assess targets, and
   compare saved height or router-failure scenarios.

## Documentation

- [User guide](docs/USER_GUIDE.md): projects, terrain, routing, coverage, exports,
  and interpretation of RF results.
- [Development and architecture](docs/DEVELOPMENT.md): source layout, tests,
  packaging, deployment, and benchmark reproduction.
- [Current roadmap](docs/ROADMAP.md): unfinished acceptance checks and engineering
  work, including the cleanup record.
- [Terrain fixture](docs/REAL_TERRAIN_FIXTURE.md) and
  [benchmark artifacts](docs/benchmarks): reproducible performance evidence.
- [Historical records](docs/history/README.md): dated plans and browser reports.

## Server configuration

Run **one server process**: workspace state, job scheduling, and caches live in
that process. RF evaluation may create child processes.

| Variable | Default | Purpose |
| --- | --- | --- |
| `RF_PLANNER_DATA` | `web-data` (Docker: `/data`) | Project catalog and workspace files |
| `RF_PLANNER_TOKEN` | unset | Optional shared access token |
| `RF_PLANNER_MAX_ACTIVE_JOBS` | `1` | Concurrent planner jobs; outstanding queue is bounded to twice this count |
| `RF_PLANNER_RF_CACHE_ENTRIES` | `50000` | Route scalar-metric cache capacity |
| `RF_PLANNER_COVERAGE_CACHE_ENTRIES` | `50000` | Separate coverage scalar-metric cache capacity |
| `RF_PLANNER_BIND` | `127.0.0.1` | Compose host-side port binding |

The supplied Compose file exposes the token and bind settings. To change worker
or cache settings in Docker, add them to the service's `environment` mapping.
Increasing job concurrency or cache sizes increases memory use.

Projects are separated by browser workspace cookies; the shared token does not
provide individual user accounts. Back up the complete data directory or Docker
volume with the server stopped so the SQLite catalog and project files agree.
