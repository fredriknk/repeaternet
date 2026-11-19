# Development and architecture

## Supported application

RepeaterNet has one runtime interface: the self-hosted web application.
Both `python -m rf_router_planner` and the installed `rf-router-planner` command
start it. Desktop Qt widgets, desktop project serialization, contour rendering,
the legacy Deygout implementation, and automatic adoption of pre-catalog workspace files
have been removed. No desktop dependency extra or compatibility launcher is
maintained.

Use Python 3.12+ and install `python -m pip install -e ".[dev]"` in a virtual
environment. Reactivate/reinstall the editable package after changing entry
points. Node is needed for frontend tests; the app itself uses bundled browser
JavaScript and CSS without a Node build step.

## Source layout

| Path | Responsibility |
| --- | --- |
| `web.py` | FastAPI routes, workspace state, validation, scheduler, persistence integration |
| `web_assets/` | Browser views, Leaflet interaction, responsive styles, inspection/navigation helpers |
| `project_store.py` | SQLite named-project catalog, revisions, archive/restore, isolated project directories |
| `models/` | Site, radio, candidate, network, and coverage contracts |
| `terrain/` | Windowed raster access, profile sampling, candidates, Kartverket downloads, Overpass road access |
| `rf/` | Directional budgets, antenna patterns, curvature, Fresnel, Bullington diffraction |
| `optimization/` | Screened graph, topology selection, staged validation, bounded metric caches, workers |
| `coverage/` | Area grids, profiles, scenarios, comparisons, target assessment |
| `export/` | Route CSV/GeoJSON and coverage JSON/GeoJSON/HTML |
| `integrations/` | CoreScope catalog client |
| `data/` | Packaged Leaflet assets and the authoritative WCS configuration |
| `tests/` and `tests/web/` | Python regression suite and Node event/race tests |
| `tools/` | Search, raster-coverage, and saved-result benchmarks |

Paths above are relative to `src/rf_router_planner` except tests/tools.
The RF and optimization core remains independent of HTTP and browser code.
Multi-client graph routines remain useful engine functionality even though the
current browser workflow has two endpoints.

## Persistence and process model

`RF_PLANNER_DATA/projects.sqlite3` stores the catalog. Workspaces contain isolated
`projects/<id>` directories with terrain, plan snapshots, and saved results.
New workspaces start with a new catalog project and do not adopt loose root-level
files. Existing catalog entries are read directly. Obsolete formats have no
migration guarantee during pre-release development.

`RF_PLANNER_OVERPASS_URL` overrides the road data endpoint (default
`https://overpass-api.de/api/interpreter`). The global WGS84 grid uses 0.1° of
latitude by 0.2° of longitude, about 11 × 11 km at 60° N. Each successful tile
response is cached in the shared `shared-cache/overpass` directory by endpoint,
highway class set, tile index, and query schema version.
The candidate filter loads tiles within the configured road distance of grid
points that pass the corridor, terrain, and exclusion masks. The route result
and its saved summary share a list of clipped road polylines across alternatives.
Each alternative stores indices into that list and distances to the nearest
mapped road within the display radius, keyed by router ID.

Run one web process. Its bounded scheduler, workspace registry, and shared
scalar-metric caches are process-local. Route evaluation can use child workers.
Increasing server job concurrency multiplies active terrain/profile work;
benchmark total server-plus-child memory before changing deployment defaults.

Keep path containment, workspace isolation, stale-result checks, and job-identity
checks: these enforce current behavior, not compatibility with old releases.
Likewise, coverage model fingerprints prevent reuse of scientifically different
results and must remain authoritative.

Coverage terrain is published atomically in project-local
`coverage-terrain-generations/generation-*/{dtm,dom}` directories. The coverage
terrain estimate and prepare endpoints use the same projected mesh buffer, map
extent, or polygon as the grid. They subtract installed raster footprints by
product and select aligned tiles within the existing Kartverket budget. The
estimate measures tile extents. Sampling still checks no-data values. DOM
follows the presence of route DOM. Set `RF_PLANNER_COVERAGE_TERRAIN_RESOLUTION_M` to override the
25 m starting resolution. The planner can coarsen it to fit the pixel budget.

Preparation uses the shared bounded planner queue and publishes a generation
only after all requested products are ready. Job status is available in
`/api/state` under `coverage_terrain_job` and cancellation uses
`POST /api/coverage/terrain/{job_id}/cancel`. The shared `shared-cache/kartverket` tile cache
retains completed tiles after cancellation. Changing area or route selection
preserves installed coverage generations. Route terrain uploads, preparation,
and clearing remove installed coverage generations and preserve the tile cache.

Coverage jobs, inspection, and target assessment all pass route paths first to
`RasterTerrain`, whose first unmasked sample wins for each product. Coverage
reuse and staleness checks fingerprint both sets of files. Route certification,
readiness checks, and the route RF cache use route terrain exclusively.

## Verification

From an activated virtual environment:

```sh
python -m pytest
python -m ruff check src tests
python -m mypy src/rf_router_planner --ignore-missing-imports
node --test tests/main_view_navigation.test.js tests/web/coverage_inspection.test.cjs
node --check src/rf_router_planner/web_assets/app.js
node --check src/rf_router_planner/web_assets/main_view_navigation.js
node --check src/rf_router_planner/web_assets/coverage_inspection.js
git diff --check
```

The Python suite exercises RF math, terrain/nodata behavior, exact and heuristic
search, process/sequential equivalence, cancellation, caches, project isolation,
coverage lifecycle/scenarios/targets, exports, and web workflows. Node tests cover
navigation event bubbling and delayed inspection/autosave races.

After packaging/dependency changes, rebuild with `docker compose up -d --build`.
Check startup, HTTP access, and a representative workflow. Automated assertions
do not replace browser layout, file-download, or field-accuracy checks. Record
unverified acceptance items in [the roadmap](ROADMAP.md).

## Benchmarks

Use `--help` on each script for its current options:

```sh
python tools/benchmark_rf_search.py --help
python tools/profile_rf_search.py --help
python tools/benchmark_mesh_coverage.py --help
python tools/benchmark_coverage_replay.py --help
```

For a locally available raster:

```sh
python tools/benchmark_mesh_coverage.py --dtm path/to/ground.tif --sources 8 --cells 4096 --repeats 3 --output report.json
python tools/benchmark_coverage_replay.py --sources 32 --repeats 3
```

Record host, settings, fixture checksum, cold/warm definition, and repetitions.
The [terrain fixture manifest](REAL_TERRAIN_FIXTURE.md) explains provenance.
[Recorded artifacts](benchmarks) include pre/post windowed sampling and large
saved-result replay measurements. They are dated measurements, not performance
guarantees for other hardware or current concurrency.

The core raster benchmark reports child-process high-water memory, including
setup. It does not measure combined concurrent server/worker memory or HTTP/UI
latency. Its “cold” run clears application caches, not OS storage caches.
A larger scalar cache can consume substantial memory; preserve bounded defaults.

## Maintenance policy

Implement the current product directly. Remove replaced paths and their
exclusive tests rather than carrying parallel implementations or silent format
fallbacks. Retain current validation and scientific metadata. Keep one source for
runtime defaults/configuration and update callers together.

Current instructions belong in README, the user guide, this document, and the
roadmap. Dated plans and reports under [history](history/README.md) are evidence,
not current requirements. Changes that remove features should identify the
removed behavior and verification performed. Git retains removed source.

## Shared download cache

Kartverket terrain tiles and OpenStreetMap road tiles are cached once for the
whole server under `$RF_PLANNER_DATA/shared-cache/` (override with
`RF_PLANNER_CACHE_DIR`). File names are hashes of the exact upstream request,
so every project and workspace reuses them, and writes are atomic. The cache is
not counted in per-workspace storage usage and is never pruned automatically;
delete the directory to reclaim space. Kartverket downloads send a descriptive
User-Agent, pause briefly between consecutive network tiles, and retry
transient errors with exponential backoff.
