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
| `terrain/` | Windowed raster access, profile sampling, candidates, Kartverket downloads |
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

Run one web process. Its bounded scheduler, workspace registry, and shared
scalar-metric caches are process-local. Route evaluation can use child workers.
Increasing server job concurrency multiplies active terrain/profile work;
benchmark total server-plus-child memory before changing deployment defaults.

Keep path containment, workspace isolation, stale-result checks, and job-identity
checks: these enforce current behavior, not compatibility with old releases.
Likewise, coverage model fingerprints prevent reuse of scientifically different
results and must remain authoritative.

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
