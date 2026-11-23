
# RepeaterNet application reference

The `repeaternet` executable embeds the dashboard, Leaflet, the Kartverket
service table and the terrain CRS catalog. The dashboard's basemap and live
providers need network access. Use the [user guide](../docs/USER_GUIDE.md)
for planning workflows and the [development guide](../docs/DEVELOPMENT.md)
for architecture, persistence and tests.

## Build and launch

Run Cargo from `rust/` with the pinned Rust 1.100.0 toolchain. Windows builds
need the MSVC C++ Build Tools and a Windows SDK. From the repository root,
`.\tools\build-rust.ps1` builds the Windows executable at
`rust\target\release\repeaternet.exe` using the workspace's static CRT
configuration. On Linux with a system linker, run
`cargo build --locked --release -p repeaternet` from `rust/` and launch
`./target/release/repeaternet`. The [Dockerfile](../Dockerfile) builds the
static x86-64 Linux musl target on Alpine and runs it in a `scratch` image.
Tagged releases package the Windows executable. [Release validation](../docs/RELEASE_VALIDATION.md)

Build the standalone CoreScope capture tool with
`cargo build --locked -p corescope-capture`. See the
[capture guide](crates/corescope-capture/README.md) for CLI usage and schema.

The launcher prints the URL after binding and opening the catalog. A wildcard
bind prints the corresponding loopback address for local use. Ctrl+C stops
the server. Windows also accepts Ctrl+Break, and Unix accepts SIGTERM.

### Command-line flags

| Flag | Default | Effect |
| --- | --- | --- |
| `--host <ip>` | `127.0.0.1` | Listen on an IPv4 or IPv6 address. `localhost` resolves to IPv4 loopback. Use `0.0.0.0` or `::` to bind the corresponding wildcard. |
| `--port <port>` | `8000` | TCP port from 0 through 65535. Port 0 asks the OS to assign a port. |
| `--data <directory>` | `RF_PLANNER_DATA` or the platform default below | Select the catalog and workspace data directory. The flag takes precedence. |
| `--trusted-proxy <ip>` | `RF_PLANNER_TRUSTED_PROXY` or unset | Trust forwarded host and scheme from one socket peer IP. The flag takes precedence. |
| `--no-browser` | Off | Serve without opening the default browser. |
| `--private-sessions` | `RF_PLANNER_PRIVATE_SESSIONS` or off | Give each browser profile its own private project workspace instead of the shared project list. The terrain cache is shared either way. |
| `-h`, `--help` | Off | Print usage and exit. |
| `-V`, `--version` | Off | Print the executable version and exit. |

### Environment variables

| Variable | Default | Effect |
| --- | --- | --- |
| `RF_PLANNER_DATA` | Platform default below | Select the data directory when `--data` is absent. |
| `LOCALAPPDATA` | Windows account environment, otherwise unset | With no explicit data directory, use `%LOCALAPPDATA%/RepeaterNet/data` on Windows. An unset or empty value falls back to `web-data` relative to the working directory. |
| `XDG_DATA_HOME` | Unset by RepeaterNet | On non-Windows systems, a nonempty value selects `$XDG_DATA_HOME/repeaternet` when the data directory is implicit. |
| `HOME` | Account environment, otherwise unset | On non-Windows systems, use `$HOME/.local/share/repeaternet` if `XDG_DATA_HOME` is absent or empty. If neither is set and nonempty, use `web-data`. |
| `RF_PLANNER_TOKEN` | Unset or empty | Require a shared token when nonempty. An invalid Unicode value prevents startup. |
| `RF_PLANNER_PRIVATE_SESSIONS` | Unset | `true`, `1`, `yes` or `on` enables `--private-sessions`. |
| `RF_PLANNER_TRUSTED_PROXY` | Unset | Accept `X-Forwarded-Host` and `X-Forwarded-Proto` from this peer IP when `--trusted-proxy` is absent. |
| `RF_PLANNER_MAX_ACTIVE_JOBS` | `1` | Concurrent planner workers. The outstanding job bound is twice this value and includes active jobs. The server clamps the worker count to at least 1 and within the semaphore limit. |
| `RF_PLANNER_RF_CACHE_ENTRIES` | `50000` | Process-wide route scalar-metric cache capacity. The server uses at least one entry. |
| `RF_PLANNER_COVERAGE_CACHE_ENTRIES` | `50000` | Separate process-wide coverage scalar-metric cache capacity. The server uses at least one entry. |
| `RF_PLANNER_TERRAIN_CACHE_MB` | Half of detected physical memory, reduced by the Linux cgroup limit when lower, or 1024 MiB if unavailable | Budget in MiB for resident decoded terrain windows shared by open rasters. Zero disables window retention. Reads still use the decoder's per-file block cache. |
| `RF_PLANNER_LIVE_COVERAGE_CACHE_MB` | `512` | Process-wide retention budget in MiB for live coverage elevation tiles. Zero disables tile retention between requests. In-flight requests hold the tiles they use. |
| `RF_PLANNER_COVERAGE_TERRAIN_RESOLUTION_M` | `25` | Starting resolution for supplemental Kartverket terrain. Values must be 1 through 1,000 m. An invalid supplied number gives HTTP 422 from the coverage terrain endpoints. The tile planner can coarsen this spacing. |
| `RF_PLANNER_CACHE_DIR` | `<data>/shared-cache` | Shared Kartverket and Overpass download cache root. Projects hard-link prepared terrain from it, so keep it on the same volume as the data directory to avoid per-project copies. |
| `RF_PLANNER_OVERPASS_URL` | `https://overpass-api.de/api/interpreter` | Overpass interpreter used for route road access and overlays. |
| `RF_PLANNER_BIND` | `127.0.0.1` | Compose host-side binding for port 8000. Set it in the shell or Compose `.env` file. The container listens on `0.0.0.0:8000`. |

If the data directory is implicit and its catalog is absent, the launcher uses
the preview directory if it contains `projects.sqlite3`. With
`LOCALAPPDATA` on Windows, that is the adjacent `RepeaterNet/rust-preview`.
With the non-Windows default or the `web-data` fallback, it is a
`rust-preview` subdirectory of the default. Explicit data paths skip preview
detection.

The supplied [Compose service](../compose.yaml) passes through
`RF_PLANNER_TOKEN`. Add other server overrides to the service environment.
The amd64 image runs as UID and GID 1000, sets `RF_PLANNER_DATA` and `TMPDIR`
to `/data`, and sets `SSL_CERT_FILE` to
`/etc/ssl/certs/ca-certificates.crt` for the bundled CA certificates.
Compose mounts the named `planner-data` volume there. Rebuilding preserves
that volume. Run one server process per data directory. For a consistent
backup, stop the server and copy the complete directory or volume.

## HTTP reference

All `/api/` routes, sign-in and UI assets are internal dashboard interfaces.
Download endpoints produce the file formats described below. `/health` is
the operator liveness check. The embedded OpenAPI document is a Python-era
reference. Consult the Rust handlers for the current behavior.

API writes use JSON objects unless a row specifies a form, an empty body or a
different shape. Dashboard geographic endpoint pairs use `[latitude, longitude]`.
GeoJSON coordinates use longitude first. Application errors usually return
`{"detail":"message"}`. JSON object validation can return a list in `detail`.
Expect 404 for missing resources, 409 for stale identities or ineligible
operations, 422 for invalid inputs and 429 for planner capacity limits.
Axum also handles HEAD for GET routes, with an empty response body.

Queued jobs and their progress are polled over HTTP. Cancel requests signal the
worker and return an acknowledgement. Poll for the terminal state. Running
work stops at cancellation checkpoints, including after a blocking provider
read returns. A full job queue returns 429. The standard API request body limit is 16 MiB. File and sign-in handlers use the limits listed below. Coverage estimate, preview, target
assessment and CoreScope import take an immediately available worker and return
429 if all workers are busy. Uploads have separate limits of two active uploads
server-wide and one per workspace.

### State and projects

| Method and path | Request and response |
| --- | --- |
| `GET /api/state` | Returns defaults, saved plan, active and archived project lists, storage bytes, historical route summary, route and supplemental terrain filenames, RF cache statistics, and `job`, `coverage_job` and `coverage_terrain_job` status. |
| `POST /api/projects` | `{name, plan?}` creates and opens a project. Returns `{id, name, plan}`. |
| `POST /api/projects/autosave` | `{plan, project_id?}` saves the active plan and keeps one previous revision when the plan changes. Returns `ok`, `project_id`, `updated_at` and `has_previous_revision`. Route input changes invalidate the result. Coverage-only edits keep it. |
| `POST /api/projects/recover-previous` | Swaps current and previous plans and clears the route result. Returns `{id, name, plan}`. |
| `POST /api/projects/{id}/activate` | Opens an unarchived project and returns `{id, name, plan}`. |
| `POST /api/projects/{id}/duplicate` | `{name}` copies the current plan and route and coverage terrain trees. Opens the copy and returns `{id, name, plan}`. |
| `PATCH /api/projects/{id}` | `{name}` renames an unarchived project. Returns `{id, name}`. Names are unique within a workspace across active and archived projects. |
| `POST /api/projects/{id}/archive` | Archives an inactive project, retains its files and returns `{ok:true}`. |
| `POST /api/projects/{id}/restore` | `{name}` restores an archived project, opens it and returns `{id, name, plan}`. |
| `DELETE /api/projects/{id}` | Deletes an inactive project and its files and returns `{ok:true}`. Legacy projects stored at the workspace root are protected. |

### Terrain

| Method and path | Request and response |
| --- | --- |
| `POST /api/terrain/{kind}` | One multipart `file` of up to 512 MiB. `kind` is `dtm` or `dom`. Validates metadata, elevation blocks and internal mask blocks. Returns `{name, bounds}`, with bounds in latitude/longitude order. Invalidates the route and clears supplemental terrain on success. |
| `DELETE /api/terrain/{kind}` | For `dtm` or `dom`, clears that product from route terrain and clears supplemental terrain. Invalidates the route and returns an acknowledgement. |
| `POST /api/terrain/coverage` | Takes endpoints, candidate settings and router inputs for readiness. Returns `covered`, `uncovered_sites`, `line_gaps`, DTM and DOM outlines, and `surface_available`. Query `route_only=true` excludes supplemental terrain. |
| `POST /api/terrain/estimate` | Takes endpoints, eligible fixed sites, candidate settings and optional `terrain: {requested_resolution_m, auto_resolution, include_dom}` (defaults 10 m, true, true). Returns Kartverket tile counts, effective resolution, expected cache hits, transfer and disk estimates. |
| `POST /api/terrain/prepare` | Same inputs as the estimate. Returns job status with a terrain estimate. Poll `job` in `/api/state` for progress and `terrain_ready`. Use `POST /api/cancel` to cancel. The dashboard submits route search separately after readiness. |
| `POST /api/coverage/terrain/estimate` | Takes the coverage request below or `{live: {sites: [{latitude, longitude}, …], radius_m}, terrain?, registry_fingerprint?}`. The live form uses the union of circles around 1 to 256 sites in the installed route terrain CRS, with a radius of 1,000 to 80,000 m. Returns supplemental tile and storage estimates after subtracting installed footprints independently for DTM and DOM. Includes DOM when route DOM is installed. |
| `POST /api/coverage/terrain/prepare` | Same inputs. Returns a job to poll under `coverage_terrain_job` in `/api/state`, or a complete status with a null job ID if no download is needed. |
| `POST /api/coverage/terrain/{id}/cancel` | Signals cancellation of the matching active supplemental download and returns `{ok:true}`. |

The [terrain crate guide](crates/terrain/README.md) lists GeoTIFF and CRS
support. Tiles in a mosaic use one CRS. Route readiness samples required sites
separately from gaps along the endpoint line. Certification checks the terrain
along each RF link. Coverage reads route terrain first, followed by
supplemental terrain. Kartverket preparation stages requested products before
publishing a generation. Failed or cancelled transfers can leave reusable
validated tiles in the shared cache.

### MeshCore and road providers

| Method and path | Request and response |
| --- | --- |
| `POST /api/meshcore` | Takes `{a, b, corridor_width_m?}`. Filters CoreScope repeaters by corridor half-width in metres (1 through 500,000, default 15,000) and returns `{routers, count}` with relay activity and freshness on each router. The dashboard imports selected records into the plan. |

Road access is part of route search. It queries Overpass tiles to filter
generated candidate sites by distance to mapped roads. Fixed sites follow
their router policy. Route alternatives can reference shared road polylines
and carry nearest-road distances for display.
[Provider APIs and limits](crates/providers/README.md)

### Route jobs

| Method and path | Request and response |
| --- | --- |
| `POST /api/optimize` | Takes a route plan with `a`, `b`, and optional `rf`, `candidates`, `known_routers`, `manual_routers` and `search_effort`. Optional `project_id` and `input_revision` guard against stale submissions. Validates readiness, saves the plan and returns queued or running job status. Poll `job` in `/api/state`. |
| `POST /api/link/evaluate` | Evaluates one two-way link using the active project's saved radio settings and route terrain. Send `source` and `target` objects with WGS84 `latitude`, `longitude`, ground-relative `antenna_height_m`, `kind` (`endpoint_a`, `endpoint_b`, `router`, `candidate` or `client`) and optional `id`. Optional `project_id` guards against a project switch. Set `include_profile: true` to include the sampled profile. Returns a route link object plus `required_fresnel_clearance`, or HTTP 422 with `detail` for invalid sites or missing ground terrain. |
| `POST /api/cancel` | Empty body or `{job_id?}`. Cancels route search or route terrain preparation. A supplied ID must match the current job. Returns `{ok:true}`. |
| `POST /api/stop-and-keep` | Same input and response as cancel. Requires a running search with a certified snapshot. Retains that snapshot and marks the search stopped and incomplete. |
| `GET /api/result` | Latest certified snapshot or terminal result, including route geometry, directional link budgets, profiles, alternatives and snapshot version. A search with no viable route returns `found:false`. A matching completed or stopped native snapshot can be restored after reopening the project or restarting the server. |
| `POST /api/alternatives/{id}/select` | Selects a certified alternative while idle, increments the snapshot version and returns the updated route result. Persists the selection. |
| `DELETE /api/cache` | Clears this project's route RF metric cache while idle. Returns `{ok:true, deleted_entries}`. |

The server bounds route candidate grids at two million points and link profiles
at 65,536 samples. `candidates.parallel_workers` accepts 0 through 64.
Zero selects the optimizer's automatic worker count. See the
[optimizer guide](crates/optimizer/README.md) for certification and cache details.

### Coverage

#### Live per-site map

`POST /api/live-coverage` evaluates one site using the active project's saved RF
settings and the route and supplemental terrain. Send `site` with `id` (at most
64 characters), `kind` (`endpoint_a`, `endpoint_b` or `router`), WGS84
`latitude` and `longitude`, and ground-relative `antenna_height_m` (above 0
through 1,000 m). Optional fields include `project_id`, the standard coverage
`client` radio profile, and `mode` (`two_way` by default, `downlink` or
`uplink`). The ranges and defaults are 1,000 to 80,000 m (20,000) for
`radius_m`, 5 to 500 m (30) for `resolution_m`, and −60 to 0 dB (−20) for
`floor_db`. Invalid inputs return HTTP 400. A project mismatch, unavailable
terrain or missing ground at the site returns HTTP 409 with `detail`.

The response is `application/octet-stream` with `Cache-Control: no-store`.
Read the ASCII magic `RNLC`, a little-endian 32-bit JSON byte length, the UTF-8
JSON header, and `width * height` unsigned bytes in north-to-south row order.
The header includes `site_id`, `width`, `height`, `origin_x` (west edge),
`origin_y` (north edge), `pixel_size`, `floor_db`, `step_db` (0.5),
`radius_m`, `resolution_m`, `rays`, `samples_per_ray`, `site_ground_m`,
`antenna_elevation_m`, `unknown_fraction`, `covered_fraction`,
`bounds_wgs84` (south, west, north, east), and `elapsed_ms`. Origins and
pixel size use EPSG:3857 metres. Fractions divide by the number of pixels
inside the radius, and covered pixels have a margin of at least 0 dB.
Pixel codes 0, 1 and 2 indicate outside, unknown terrain and below-floor
margin, respectively. Codes 3 through 255 encode
`floor_db + (code - 3) * step_db`. Strict LOS mode puts obstructed paths below
the floor and uses geometric clearance.

#### Certified-route coverage

A coverage request supplies `snapshot_version`, the selected `alternative_id`
and optional `settings` (defaults to coverage engine settings). `map_bounds` supplies `[south, west, north, east]` for the
map-extent area mode. The server ties the request to the certified route
and effective terrain. To start a saved run, save matching coverage settings
to the plan first.

| Method and path | Request and response |
| --- | --- |
| `POST /api/coverage/estimate` | Coverage request. Returns grid, source and terrain-availability counts and effective grid size without RF evaluation. |
| `POST /api/coverage/preview` | Coverage request. Returns approximate cells and counts with a limit of 256 cells and 4,096 source evaluations. Estimate and preview leave saved runs unchanged. |
| `POST /api/coverage/jobs` | Coverage request with optional `source_height_overrides` keyed by site ID. Returns job status and a `reused` flag when replaying a matching saved run. New runs stream ordered result chunks to disk. |
| `GET /api/coverage/jobs` | Returns `{jobs}` with up to 50 saved run summaries for this project, including staleness and terrain compatibility. |
| `GET /api/coverage/jobs/{job_id}` | Returns the saved manifest and current status, including stale reasons where applicable. |
| `GET /api/coverage/jobs/{job_id}/cells` | Query `cursor` defaults to 0 and counts stored rows. `limit` defaults to 128 and accepts 1 through 256. Returns ordered cells, the next cursor, counts, and run state and staleness. |
| `POST /api/coverage/jobs/{job_id}/cancel` | Cancels the matching active run and returns `{ok:true}`. Committed rows can still be paged after cancellation or interruption. |
| `POST /api/coverage/inspect` | Takes `latitude`, `longitude` and either live coverage settings and route identity or `coverage_job_id` for a compatible completed run. Returns an inspection job. An accepted newer inspection supersedes the previous job. |
| `GET /api/coverage/inspect/{job_id}` | Returns inspection status and, when complete, exact-point evaluation including source results and the serving-link profile when available. The server retains only the latest inspection in the workspace session. |
| `POST /api/coverage/inspect/{job_id}/cancel` | Cancels the matching active inspection and returns `{ok:true}`. |

Native saved coverage can be reused when a live or restored certified route
matches its saved identity, including model, snapshot, terrain, settings and
radio. Python coverage remains historical and can be paged and exported.
Python-to-Python comparisons require matching live terrain provenance and
compatible historical fingerprints, settings, grid and radio. Custom CRS
pages can use saved footprints. Exports that need a custom transform require
the original fingerprint-matched terrain.
[Coverage store format and compatibility](crates/coverage-store/README.md)

### Scenarios and targets

| Method and path | Request and response |
| --- | --- |
| `POST /api/coverage/jobs/{job_id}/scenario` | `{failed_ids?, reference_id}` for the selected current, complete grid. Returns a router-failure connectivity summary and per-cell results. |
| `POST /api/coverage/compare` | `{baseline_job_id, scenario_job_id, reference_id}`. Requires compatible complete runs and a shared reference router. Returns cell changes and aggregate counts. |
| `POST /api/coverage/targets/import` | Multipart `file` of up to 5 MiB. Validates and returns normalized target GeoJSON for review. |
| `PUT /api/coverage/targets` | Takes a GeoJSON FeatureCollection of point, road and polygon targets. Saves it to the active plan and returns `{ok, count, targets}`. |
| `POST /api/coverage/jobs/{job_id}/targets` | Uses the saved targets and a current, complete run. Takes optional `road_spacing_m` (default 100, range 25 through 5,000). Returns and saves an assessment of target outcomes. |
| `GET /api/coverage/target-reports` | Returns `{reports}` with up to 100 project report summaries including staleness. |
| `POST /api/coverage/target-reports/compare` | `{baseline_report_id, scenario_report_id}` for two distinct compatible reports. Returns changes by target. |

### Exports

| Method and path | Request and response |
| --- | --- |
| `GET /api/export/{kind}` | `project` downloads the saved plan as `route.webplan.json`. The dashboard imports a plan through autosave. `csv`, `geojson` and `html` download link CSV, site and link GeoJSON or a standalone route report with profiles and eligible saved coverage. |
| `GET /api/coverage/jobs/{job_id}/export.geojson` | Downloads grid and context features, with an optional target report. |
| `GET /api/coverage/jobs/{job_id}/export.json` | Downloads manifest metadata and cells with optional target and comparison data. |
| `GET /api/coverage/jobs/{job_id}/report.html` | Downloads a standalone printable coverage report with optional target and comparison data. |

Coverage downloads accept `include_partial=true` for partial or unresolved
data and `include_stale=true` for stale runs. Active runs must finish or stop
before download. `target_report_id` selects an attached report. JSON and HTML
accept `scenario_job_id` together with `reference_id` for an aligned run
comparison. [Export formats and limits](crates/exports/README.md)

### Auth, dashboard and discovery

| Method and path | Request and response |
| --- | --- |
| `GET /` | Serves the dashboard and issues `planner_workspace` when needed. A visitor without the required access cookie receives the sign-in page. |
| `POST /login` | URL-encoded or multipart `token` form, up to 16 KiB. Success sets `planner_access` and sends HTTP 303 to `/`. The optional `remember=1` field gives the access cookie a 30-day lifetime. An incorrect token returns 401. |
| `GET /health` | Returns `{"status":"ok","runtime":"rust"}` before sign-in or workspace initialization. |
| `GET /openapi.json`, `GET /docs`, `GET /docs/oauth2-redirect`, `GET /redoc` | Embedded discovery resources. Swagger UI and ReDoc load their rendering scripts from external CDNs. |
| `GET /assets/{file}` | Registered dashboard files. Names are `index.html`, `login.html`, `style.css`, `meshcore.css`, `coverage.css`, `workspace.css`, `map.css`, `main_view_navigation.js`, `app.js` and `coverage_inspection.js`. |
| `GET /leaflet/{file}` | Registered Leaflet files. Names are `leaflet.css`, `leaflet.js`, `images/marker-icon.png`, `images/marker-icon-2x.png` and `images/marker-shadow.png`. |

Asset and discovery paths are matched in the router fallback. Both cookies use
`HttpOnly` and `SameSite=Strict`. The workspace cookie lasts one year. By
default all clients (signed in, when a token is set) share the catalog,
including projects in legacy workspace directories, and the workspace cookie
identifies each browser profile's active project. With `--private-sessions` the
cookie instead identifies a private project workspace. In shared mode, plan writes (autosave, recovery, route terrain
preparation, optimization and saving targets) require `X-RepeaterNet-Project`
and `X-RepeaterNet-Plan-Revision`. Get both values as `projects.active_id` and
`projects.plan_revision` from `/api/state`. A stale project or revision returns
409. The server requires a loopback
`Host` for direct requests to a loopback listener. Writes that carry
`Origin` must match the effective host and scheme.

For HTTPS, terminate TLS at a reverse proxy and configure
`--trusted-proxy <peer-ip>`. Have the proxy overwrite
`X-Forwarded-Host` and `X-Forwarded-Proto` with single values for the public
authority and scheme. Forwarded HTTPS adds `Secure` to both cookies.

## Notices

The embedded CRS catalog includes data from the IOGP EPSG Geodetic Parameter
Dataset v12.029 as distributed with PyProj 3.8.0 and PROJ 9.8.1.
[IOGP dataset terms](https://iogp.georepository.com/terms-of-use.html)
