# RepeaterNet Rust preview

The native launcher serves the existing dashboard and its bundled Leaflet assets from one executable. It opens the dashboard in the default browser. Python, a separate asset directory, and a development server are unnecessary at runtime. The map's remote basemap tiles still need network access.

The dashboard runs route planning from uploaded GeoTIFF or Kartverket terrain using the native terrain sampler, RF model, and optimizer. It supports terrain readiness, certified alternatives and link profiles, MeshCore repeater import, road access, saved coverage, exact-point inspection, failure and height scenarios, target assessments, and route and coverage downloads. It opens the Python application's v1 project catalog in place.

## Build and launch

Windows builds use Rust 1.100, the MSVC C++ Build Tools, and a Windows SDK. Run these commands in PowerShell from the repository root.

```powershell
.\tools\build-rust.ps1
.\rust\target\release\repeaternet.exe
```

The build script places the executable under `rust\target\release` and uses the workspace's static MSVC runtime configuration. You can copy the executable out of the checkout.

Linux builds use Rust 1.100 and a system linker. Start in the repository root.

```sh
cd rust
cargo build --locked --release -p repeaternet
./target/release/repeaternet
```

Run Cargo from `rust/` to pick up its Cargo configuration. Use `cargo test --workspace --locked` for the workspace tests and add `--release` to test the optimized binaries. The executable integration test starts the binary on loopback, uploads a checked-in terrain fixture, calculates a route and coverage, exercises paging, inspection and downloads, and reopens the project after restart.

The debug and release workspace suites ran on Windows x64 and in a `rust:1.100-slim` Linux x86-64 container. The Windows verifier copied the executable alone into a scratch directory and exercised headless launch, authentication, upload, routing, export, port contention, and restart. It reported only Windows system DLLs in the direct PE import table. [Windows verification workflow](../docs/RELEASE_VALIDATION.md)

The default address is `http://127.0.0.1:8000/`. The launcher prints the actual URL once it has bound the socket. Use `--host` for another listen address, `--port` to select a port, `--port 0` for an OS-assigned port, and `--no-browser` for a headless launch. Use Ctrl+C to stop the server gracefully. It also handles Ctrl+Break on Windows and SIGTERM on Linux.

Use `--data <directory>` or `RF_PLANNER_DATA` to select a data directory. The flag takes precedence. The default is `%LOCALAPPDATA%\RepeaterNet\rust-preview` on Windows and `${XDG_DATA_HOME:-$HOME/.local/share}/repeaternet/rust-preview` on Linux, with `web-data/rust-preview` as a fallback if the relevant environment variables are missing.

To reopen Python projects, stop the Python server, start the Rust executable with `--data` pointing at the same data directory, and use the same browser profile and host so it sends its existing `planner_workspace` cookie. The Rust server opens `projects.sqlite3` and its project directories directly. A new workspace cookie starts a new catalog workspace. Run one planner server at a time per data directory since project operations and active job state are coordinated within each process.

Upload a DTM or approve a Kartverket download before starting a route. A DOM is optional. The automatic preflight can estimate missing route terrain, prepare it after approval, recheck readiness, and continue into route search. Uploads accept single-band projected-metre GeoTIFFs in 450 named CRSs and supported custom GeoKey encodings. Mosaic tiles need a common CRS. Kartverket operates in its configured Norwegian zones. The dashboard checks endpoint and selected-router ground samples separately from gaps along the endpoint line. A route link needs valid terrain samples along its own RF profile. [Supported CRS, encodings, and masks](crates/terrain/README.md)

`RF_PLANNER_MAX_ACTIVE_JOBS` sets the number of concurrent planner workers and defaults to one. The shared scheduler allows up to twice that number of outstanding route, terrain, coverage, and inspection jobs. Coverage estimate, preview, target assessment, and CoreScope import use an available worker and can return HTTP 429. Terrain uploads separately admit up to two staging or decode operations server-wide and one per workspace. A route plan's `candidates.parallel_workers` can be 1–64, and zero lets the optimizer choose. `RF_PLANNER_RF_CACHE_ENTRIES` sets the process-local route RF cache capacity and defaults to 50,000.

`RF_PLANNER_COVERAGE_CACHE_ENTRIES` sets the capacity of a separate process-local coverage RF cache and defaults to 50,000 entries. It reuses scalar link evaluations across previews, saved runs, exact-point inspection, target assessment, and height-scenario backbone checks. Entries are scoped to the project, terrain file revision, RF settings, site geometry, client radio, and effective profile spacing. Terrain revisions follow the dashboard's paths, sizes, and modification timestamps, so out-of-band edits must change that metadata. The coverage cache excludes serving-link profiles and missing-terrain results. Saved-run replay remains a separate disk-backed operation.

## HTTP endpoints

| Method and path | Behavior |
| --- | --- |
| `GET /` | Serves the embedded dashboard and issues the isolated `planner_workspace` cookie when needed. Serves the sign-in page instead if a token is required and the browser has not signed in. |
| `GET /assets/…`, `GET /leaflet/…` | Serves the dashboard HTML, CSS, JavaScript, Leaflet CSS and JavaScript, and the three checked-in Leaflet marker PNGs. Supports HEAD for these assets. |
| `GET /openapi.json`, `GET /docs`, `GET /docs/oauth2-redirect`, `GET /redoc` | Serves the embedded Python-baseline OpenAPI contract and discovery pages. Swagger UI and ReDoc load their rendering scripts from external CDNs. |
| `POST /login` | Checks the `token` form field against `RF_PLANNER_TOKEN`. On success, issues the `planner_access` cookie and redirects to `/` with HTTP 303. An incorrect token returns 401. |
| `GET /health` | Returns `{"status":"ok","runtime":"rust","mode":"preview"}`. This liveness endpoint is available before sign-in and workspace initialization. |
| `GET /api/state` | Returns the active plan, default settings, active and archived projects, terrain filenames, workspace storage, and any saved route summary. On startup, queued, running, or preparing route work is shown as interrupted with fresh process-local route identity. |
| `POST /api/projects` | Creates and activates a named project with an optional plan. |
| `POST /api/projects/autosave` | Saves the active project's plan and retains one previous revision. The optional project ID must match the active project. Route input changes invalidate the live route and clear the saved summary. Coverage-only edits retain the route. |
| `POST /api/projects/recover-previous` | Swaps the current and previous plan and clears the saved route summary. |
| `POST /api/projects/{id}/activate` | Opens an unarchived project in the current workspace. |
| `POST /api/projects/{id}/duplicate` | Copies a saved plan and its route and coverage terrain, then opens the copy. |
| `PATCH /api/projects/{id}` | Renames an unarchived project. Names are unique within a workspace, including archived rows. |
| `POST /api/projects/{id}/archive` | Archives an inactive project and keeps its files. |
| `POST /api/projects/{id}/restore` | Restores an archived project under the requested name and opens it. |
| `DELETE /api/projects/{id}` | Permanently deletes an inactive project and its files. Protects projects stored at the workspace root. |
| `GET /api/export/project` | Downloads the current saved plan as `route.webplan.json`. The dashboard's file picker imports plans through autosave. |
| `POST /api/terrain/{dtm,dom}` | Streams one multipart `file` of up to 512 MiB, validates its metadata and every elevation and internal mask block, and publishes a generated filename and geographic bounds. |
| `DELETE /api/terrain/{dtm,dom}` | Clears the selected terrain product and supplemental coverage terrain, and invalidates the route. |
| `POST /api/terrain/estimate`, `POST /api/terrain/prepare` | Plan Kartverket DTM and optional DOM for the endpoints, corridor, and eligible fixed sites. Estimates include effective resolution, cache hits, and disk usage. Preparation queues a cancellable job and reports `terrain_ready` through `/api/state`. |
| `POST /api/meshcore` | Fetches CoreScope repeaters and returns nearby router records with relay activity and freshness for import. |
| `POST /api/terrain/coverage` | Returns required-site readiness, line gaps, DTM and DOM outlines, and surface availability. `route_only=true` checks route tiles alone. The default includes saved supplemental tiles. |
| `POST /api/optimize` | Checks RF and candidate settings, selected router policies, and DTM coverage, resolves the search effort, and queues native route search. Optional `project_id` and `input_revision` guard against stale submissions. |
| `POST /api/cancel`, `POST /api/stop-and-keep` | Cancel an active route job or retain its latest certified snapshot. Both accept an optional `job_id` and reject stale IDs. |
| `GET /api/result` | Returns live route geometry, directional link budgets, terrain profiles, and certified alternatives. A completed search with no viable route returns `found: false`. Full results remain available during the current project session. After restart, `/api/state` exposes the saved summary and its staleness, and a rerun reconstructs detailed profiles. |
| `POST /api/alternatives/{id}/select` | Selects an existing certified alternative and increments the snapshot version. |
| `DELETE /api/cache` | Clears the active project's route RF cache when the workspace is idle. |
| `GET /api/export/{csv,geojson,html}` | Downloads the active result's link CSV, site and link GeoJSON, or a standalone HTML route report with map, profiles, and eligible saved coverage. |
| `POST /api/coverage/estimate`, `POST /api/coverage/preview` | Use the current certified snapshot and selected alternative to estimate the grid or return an approximate coverage preview. Preview limits are 256 cells and 4,096 source evaluations. Both can read saved supplemental terrain. Neither creates a saved run. |
| `POST /api/coverage/terrain/estimate`, `POST /api/coverage/terrain/prepare` | Estimate or queue a supplemental Kartverket download for the certified alternative's coverage area. Installed DTM and DOM footprints are subtracted independently. Job status appears under `coverage_terrain_job` in `/api/state`. |
| `POST /api/coverage/terrain/{job_id}/cancel` | Cancels the active supplemental terrain job. |
| `POST /api/coverage/jobs`, `GET /api/coverage/jobs`, `GET /api/coverage/jobs/{job_id}` | Calculate or reuse coverage for the certified snapshot and saved settings, including optional source-height overrides. List the most recent 50 runs or fetch individual manifests. Interrupted work and stale identity are surfaced when reopening a project. |
| `GET /api/coverage/jobs/{job_id}/cells`, `POST /api/coverage/jobs/{job_id}/cancel` | Page up to 256 stored cells at a time and cancel the active run. Completed chunks remain available after cancellation or interruption. |
| `POST /api/coverage/inspect`, `GET /api/coverage/inspect/{job_id}`, `POST /api/coverage/inspect/{job_id}/cancel` | Queue and poll exact-point radio evaluation under current settings or a compatible saved run. A newer click supersedes the previous inspection. |
| `POST /api/coverage/jobs/{job_id}/scenario`, `POST /api/coverage/compare` | Evaluate router failure against the active saved grid or compare aligned saved runs with a shared reference router. |
| `POST /api/coverage/targets/import`, `PUT /api/coverage/targets` | Validate target GeoJSON for review or save it to the active plan. |
| `POST /api/coverage/jobs/{job_id}/targets`, `GET /api/coverage/target-reports`, `POST /api/coverage/target-reports/compare` | Assess saved point, road and polygon targets, list saved reports, and compare compatible assessments. |
| `GET /api/coverage/jobs/{job_id}/export.geojson`, `GET /api/coverage/jobs/{job_id}/export.json`, `GET /api/coverage/jobs/{job_id}/report.html` | Download coverage with optional target reports and scenario comparison on JSON and HTML. Partial and stale exports require explicit opt-ins. |

The optional `RF_PLANNER_TOKEN` gates the dashboard, assets, discovery, and API. Both cookies use `HttpOnly` and `SameSite=Strict`. The workspace cookie has a one-year lifetime. Mutating requests carrying an `Origin` header must match the effective request origin. A direct request to a loopback listener must use a loopback IP or localhost in `Host`.

For HTTPS, terminate TLS at the reverse proxy and set `--trusted-proxy <peer-IP>` or `RF_PLANNER_TRUSTED_PROXY` to the proxy's socket IP. Configure the proxy to overwrite `X-Forwarded-Host` with the public authority and `X-Forwarded-Proto` with the public scheme, using one value per header. Use this setting for a peer controlled by the operator. The server accepts those headers from the configured peer, checks writes against the forwarded origin, and adds `Secure` to both cookies when the scheme is HTTPS. Use `--no-browser` for a reverse-proxy deployment.

## Terrain and road providers

Kartverket and Overpass reuse validated downloads under `<data>/shared-cache` or `RF_PLANNER_CACHE_DIR`. Preparation stages both requested products before publishing a generation. Failed or cancelled jobs leave existing terrain available. Successful route terrain preparation adds a route generation and clears supplemental terrain. Clear existing route terrain before preparing a generation in a different CRS. Supplemental preparation retains the certified route and existing generations. `RF_PLANNER_COVERAGE_TERRAIN_RESOLUTION_M` defaults to 25 m before automatic coarsening.

Enable road access in candidate settings to limit generated sites by proximity to Overpass roads. Fixed sites remain eligible under their existing policy. Alternatives include indices into shared road polylines and nearest-road distances per router. Use `RF_PLANNER_OVERPASS_URL` to select an Overpass endpoint.

Provider cancellation is cooperative and may wait for an upstream read or timeout. Supplemental downloads require Kartverket service for the route terrain CRS whenever the selected area has missing terrain.

## Saved coverage compatibility and release limits

Native saved coverage becomes eligible for reuse after the server certifies the matching route again. Python coverage remains historical. It can be paged and exported with the applicable stale and partial opt-ins. Python-to-Python comparison requires complete compatible grids, verified embedded fingerprints, and an exact match to the current terrain paths, sizes, and nanosecond timestamps. Native and Python coverage models are compared within their respective formats. Paging retains saved footprints for custom CRSs. An export needing to transform custom coordinates requires the original fingerprint-matched terrain.

The terrain crate documents the supported GeoTIFF and CRS subset. Live route details and both RF caches are process-local. Project-store startup reconciles pending catalog publication through a journal beside the SQLite catalog. Live provider tests, clean-machine Windows launch, interactive browser workflows, and broader performance validation remain release gates. [Terrain support](crates/terrain/README.md) · [Coverage benchmark](crates/coverage/README.md#cache-benchmark) · [Project-store recovery](crates/project-store/README.md) · [Migration inventory](../docs/history/RUST_MIGRATION.md) · [Release validation](../docs/RELEASE_VALIDATION.md)

The server runs raster decoding and search outside the workspace lock. It verifies project, job, route input and terrain identity before publishing certified snapshots. It saves the certified summary and terminal run state through `ProjectStore` before exposing the terminal status. The server caps candidate grids at two million points and link profiles at 65,536 samples to bound allocations during search.

## Release notices

The embedded CRS catalog includes data from the EPSG Geodetic Parameter Dataset v12.029, maintained by the International Association of Oil & Gas Producers (IOGP), as distributed with PyProj 3.8.0 and PROJ 9.8.1. [IOGP EPSG Dataset Terms of Use](https://iogp.georepository.com/terms-of-use.html) · [IOGP EPSG Dataset overview](https://www.iogp.org/workstreams/engineering/geomatics/geodesy/)
