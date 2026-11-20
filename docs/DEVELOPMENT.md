# Development and architecture

## Supported application

RepeaterNet has one runtime interface: the self-hosted web dashboard served by
the native `repeaternet` executable. The executable embeds the browser assets,
the Kartverket service table and the CRS catalog; it needs no Python, GDAL, PROJ
or asset directory at runtime. The Python implementation was retired after the
Rust port reached parity; it remains in Git at baseline commit `18b7cee`, and the
[migration inventory](history/RUST_MIGRATION.md) records the contracts it fixed.
The Rust server still opens Python-era project catalogs and saved coverage.

Use the Rust toolchain pinned in `rust/rust-toolchain.toml` (1.100). Windows
builds need the MSVC C++ Build Tools and a Windows SDK. Node is needed only for
the frontend tests; the dashboard uses bundled JavaScript and CSS without a
build step.

## Source layout

All code lives in the Cargo workspace under `rust/`.

| Path | Responsibility |
| --- | --- |
| `crates/web-server` | The `repeaternet` binary: launcher, axum routes, workspace state, validation, scheduler, persistence integration |
| `crates/web-server/assets/` | Embedded dashboard HTML/CSS/JavaScript and Leaflet |
| `crates/web-server/tests/js/` | Node event/race tests for the dashboard scripts |
| `crates/project-store` | SQLite named-project catalog, revisions, archive/restore, startup recovery |
| `crates/rf-core` | Sites, settings, directional budgets, antenna patterns, curvature, Fresnel, Bullington diffraction |
| `crates/terrain` | GeoTIFF decoding, CRS transforms, windowed sampling, profiles, corridors |
| `crates/optimizer` | Candidate generation, screened graph, topology selection, staged validation, RF caches, workers |
| `crates/coverage` | Area grids, profiles, scenarios, comparisons, target assessment |
| `crates/coverage-store` | Saved coverage runs, result pages, target reports, legacy Python run identity |
| `crates/exports` | Route CSV/GeoJSON/HTML and coverage JSON/GeoJSON/HTML |
| `crates/providers` | Kartverket WCS, Overpass roads and CoreScope clients |
| `tools/` | Windows build, release verification and packaging scripts |
| `.github/workflows/` | CI and tagged release builds |

The RF, terrain and optimization crates are independent of HTTP and browser
code. Multi-client graph routines remain useful engine functionality even though
the current browser workflow has two endpoints. Each crate README documents its
contracts, compatibility limits and fixtures.

## Persistence and process model

`RF_PLANNER_DATA/projects.sqlite3` stores the catalog. Workspaces contain isolated
`projects/<id>` directories with terrain, plan snapshots, and saved results.
New workspaces start with a new catalog project and do not adopt loose root-level
files. Existing catalog entries are read directly. Catalogs written by the
Python implementation open in place.

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
scalar-metric caches are process-local. Route evaluation uses worker threads.
Increasing server job concurrency multiplies active terrain/profile work;
benchmark memory before changing deployment defaults.

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

From `rust/`:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cd .. && node --test rust/crates/web-server/tests/js/
git diff --check
```

CI runs the same checks on Windows and Linux. The Rust suites exercise RF math,
terrain decoding and CRS transforms, exact and heuristic search, cancellation,
caches, project isolation and recovery, coverage lifecycle, scenarios, targets,
exports and HTTP workflows. The executable integration test launches the binary,
uploads terrain, routes, calculates coverage and reopens the project after
restart. Fixtures captured from the Python implementation are frozen oracles;
their generators remain at commit `18b7cee`. Node tests cover navigation event
bubbling and delayed inspection/autosave races.

Before a release, run the [release validation](RELEASE_VALIDATION.md) workflow on
the exact executable. After packaging or dependency changes, also rebuild the
container with `docker compose up -d --build` and check startup, HTTP access and
a representative workflow. Automated assertions do not replace browser layout,
file-download or field-accuracy checks. Record unverified acceptance items in
[the roadmap](ROADMAP.md).

## Benchmarks

The coverage crate's [cache benchmark](../rust/crates/coverage/README.md#cache-benchmark)
measures cold and warm coverage evaluation on synthetic or local raster terrain.
Record host, settings, fixture checksum, cold/warm definition and repetitions.
The [terrain fixture manifest](REAL_TERRAIN_FIXTURE.md) explains provenance.
[Recorded artifacts](benchmarks) were measured with the removed Python tools and
are dated baseline measurements, not performance guarantees for other hardware
or the native engine. “Cold” clears application caches, not OS storage caches.
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
