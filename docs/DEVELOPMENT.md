
# Development and architecture

RepeaterNet runs as one native `repeaternet` process. It serves an embedded
vanilla-JavaScript and Leaflet dashboard. The Kartverket service table and
terrain CRS catalog are embedded too. The executable runs without a frontend
build, Python, GDAL or PROJ. The retired Python implementation is available at
commit `18b7cee`. The [migration inventory](history/RUST_MIGRATION.md) records
the original compatibility contracts.

```mermaid
flowchart LR
    Browser["Browser with Leaflet"] <-->|HTTP and job polling| Server["Axum server"]
    Server <--> Workers["Scheduler and blocking workers"]
    Workers <--> Optimizer["Optimizer"]
    Workers <--> Coverage["Coverage engine"]
    Optimizer --> RF["RF core"]
    Coverage --> RF
    Workers <--> Terrain["Terrain and CRS transforms"]
    Terrain --> RF
    Server <--> Projects["Project store · SQLite"]
    Workers <--> Runs["Coverage store · JSON and JSONL"]
    Workers <--> Providers["Provider adapters"]
    Optimizer -.->|Road filter| Providers
    Providers <-->|Terrain tiles| Kartverket
    Providers <-->|Road tiles| Overpass
    Providers <-->|Repeaters| CoreScope
```

## Source layout

The Cargo workspace lives under [`rust/`](../rust). Paths in this table are
relative to the repository root.

| Path | Responsibility |
| --- | --- |
| [`rust/crates/web-server/`](../rust/crates/web-server/README.md) | Launcher, axum routes, workspace state, validation, jobs and persistence integration |
| [`rust/crates/web-server/assets/`](../rust/crates/web-server/assets) | Dashboard and Leaflet assets embedded at build time |
| [`rust/crates/web-server/tests/`](../rust/crates/web-server/tests) | Executable integration test and Node dashboard tests |
| [`rust/crates/project-store/`](../rust/crates/project-store/README.md) | SQLite projects, revisions, route snapshots and recovery |
| [`rust/crates/rf-core/`](../rust/crates/rf-core/README.md) | RF sites, radio budgets, profile sampling and propagation math |
| [`rust/crates/terrain/`](../rust/crates/terrain/README.md) | GeoTIFF decoding, CRS transforms, terrain mosaics and corridors |
| [`rust/crates/optimizer/`](../rust/crates/optimizer/README.md) | Candidate generation, graph search, RF certification and scalar cache |
| [`rust/crates/coverage/`](../rust/crates/coverage/README.md) | Area grids, exact-point evaluation, scenarios, comparisons and targets |
| [`rust/crates/coverage-store/`](../rust/crates/coverage-store/README.md) | Coverage manifests, result pages, target reports and identity checks |
| [`rust/crates/exports/`](../rust/crates/exports/README.md) | Route CSV, GeoJSON and HTML, plus coverage JSON, GeoJSON and HTML |
| [`rust/crates/providers/`](../rust/crates/providers/README.md) | Kartverket WCS, Overpass roads and CoreScope adapters |
| [`rust/crates/corescope-capture/`](../rust/crates/corescope-capture/README.md) | Standalone CoreScope RF reception and topology capture to SQLite |
| [`tools/`](../tools) | Windows build, executable verification and release packaging |
| [`.github/workflows/ci.yml`](../.github/workflows/ci.yml) | Windows PR checks and tagged Windows release publishing |

## Process model

Run one server process per data directory. The workspace registry, job
scheduler and scalar RF caches live in that process. The registry serializes
state access by workspace cookie. Handlers capture project and job identity
under the workspace lock, run blocking raster, network and search work on
workers, then recheck identity before publishing. Planner jobs share a
bounded scheduler. Route and coverage evaluation can use additional scoped
worker threads. Terrain uploads use separate admission limits.
[Configuration and limits](../rust/README.md#environment-variables)

### Route search

1. The dashboard saves pending edits and checks terrain readiness. It can
   estimate and queue a Kartverket corridor download, poll until the terrain
   is ready, then submit the route plan to `POST /api/optimize`.
2. The handler validates the plan and requested project revision, opens route
   terrain, projects site coordinates and checks required ground samples.
   It saves the plan and queues the route job.
3. The optimizer generates candidate sites when enabled, applies terrain and
   exclusion filters, and optionally filters by mapped Overpass roads.
   It screens links, searches topologies and certifies alternatives at the
   final profile resolution using RF core. It publishes certified snapshots
   as they become available.
4. The server persists the terminal native snapshot, summary and run state in
   SQLite. The dashboard polls `/api/state` for job progress and reads
   `/api/result` for alternatives and profiles. On reopening, the server
   restores a saved completed or stopped snapshot if the plan and route
   terrain fingerprints match.

### Coverage run

1. The dashboard sends coverage settings and the certified route snapshot and
   alternative. It can estimate or preview the grid, or request supplemental
   Kartverket terrain for the chosen area.
2. The handler resolves area, sources and grid, applies optional source-height
   overrides and builds the run identity. The coverage mosaic reads route
   terrain first and supplemental terrain second. The identity includes
   both sets of terrain files, settings, radio and model.
3. The coverage store checks for a compatible complete run. On a miss, a
   planner worker evaluates the grid and commits ordered JSONL chunks with
   progress metadata. The manifest records the committed byte watermark.
   Cancellation leaves committed chunks available for paging. Recovery marks
   unfinished runs interrupted and trims uncommitted native data.
4. The dashboard polls the manifest and pages the stored cells for the map.
   Exports and analyses read the saved grid. Exact-point inspection queues
   a separate evaluation and uses live settings or a compatible saved run.
   [HTTP job and export contracts](../rust/README.md#http-reference)

## Persistence layout

| Path relative to the selected data directory | Contents |
| --- | --- |
| `projects.sqlite3` | Catalog, workspace selection, current and previous plan, route summary and run state, and the native snapshot in `route_results`. SQLite can create WAL and SHM sidecars. |
| `projects.sqlite3.repeaternet-intent` | SQLite journal for staged project-store publication and reconciliation. |
| `<workspace-key>/projects/<directory-token>/` | Project files. Catalog project IDs and directory tokens are separate. Legacy catalog rows can refer to the workspace root. |
| `shared-cache/kartverket/`, `shared-cache/overpass/` | Shared downloads. `RF_PLANNER_CACHE_DIR` overrides the cache root. |

Paths below are relative to a project directory.

| Path | Contents |
| --- | --- |
| `plan.json` | Atomic mirror of the SQLite plan. SQLite is authoritative. |
| `dtm/`, `dom/` | Route ground and surface uploads. |
| `terrain-generations/generation-*/{dtm,dom}/` | Downloaded route terrain. |
| `coverage-terrain-generations/generation-*/{dtm,dom}/` | Downloaded supplemental terrain. |
| `coverage-job.json`, `coverage-jobs/<job-id>.json` | Latest manifest and prior manifests archived on rollover. |
| `coverage-reused-selection.json` | Optional pointer recording a reused run selected by the server. |
| `coverage-results/<job-id>.jsonl` | Ordered coverage result chunks, one array per line. |
| `coverage-target-reports/<job-id>-<report-id>.json` | Saved target assessments. |
| `.coverage-store.lock` | File lock for cooperating coverage-store readers and writers. |

New projects start with their own directory. Loose files in a workspace root
stay in place. Existing Python catalog rows and compatible historical
coverage artifacts can be read in place. See the
[project store](../rust/crates/project-store/README.md) and
[coverage store](../rust/crates/coverage-store/README.md) for recovery rules.

Terrain generations are staged beside their destination and published with a
rename after all requested products are ready. Upload and removal operations
can create `.terrain-upload-*` and `.terrain-removal-*` staging entries.
The project store uses `.repeaternet-stage-*` and `.repeaternet-owner-*`
entries during journaled publication. Route uploads, clearing or
successful route terrain preparation remove supplemental generations.
Changing the coverage area or selected alternative preserves them. The
supplemental estimate subtracts installed tile footprints by product. Raster
sampling still checks masked and missing pixels.

Shared provider caches sit outside project storage usage. Kartverket and
Overpass keys incorporate their upstream request or tile schema. The Overpass
grid uses 0.1° latitude by 0.2° longitude cells. Provider downloads can survive
a cancelled job and be reused. Shared caches have no automatic retention
policy. Stop the server before taking a backup of the full data directory or
removing the shared cache.

## Dashboard assets

[`index.html`](../rust/crates/web-server/assets/dashboard/index.html) loads
Leaflet CSS, then `style.css` (base), `meshcore.css`, `coverage.css`,
`workspace.css` (shell, panels and Analysis) and `map.css`. After Leaflet,
the deferred classic scripts load in order
`main_view_navigation.js`, `app.js` and `coverage_inspection.js`.
Put new styles in the layer that owns the feature.

Register a new asset and its content type in
[`src/assets.rs`](../rust/crates/web-server/src/assets.rs) and reference it
from the page as needed. Rebuild the executable to see embedded asset changes.
Follow the [UX design rules](UX_DESIGN_RULES.md) and use a throwaway data
directory outside the repository for browser checks.

## Verification

Use the [pinned toolchain](../rust/rust-toolchain.toml). Windows builds use the
MSVC C++ Build Tools and a Windows SDK. From `rust/` run:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

From the repository root run:

```sh
node --test rust/crates/web-server/tests/js/
git diff --check
```

Windows CI also reruns terrain and coverage tests with
`RF_PLANNER_TERRAIN_CACHE_MB=0`. The Rust suites include RF and terrain
fixtures, optimizer certification, storage recovery and HTTP workflows.
The executable integration test launches on loopback with temporary data,
uploads a synthetic GeoTIFF, searches, calculates coverage, verifies paging,
inspection and exports, then reopens the project after restart. Python-derived
fixtures are frozen oracles. Their generators remain at `18b7cee`.

Node uses the built-in test runner. Tests exercise the navigation helper and
evaluate slices of the classic dashboard scripts with stubbed browser, map
and network calls. They cover navigation event handling, autosave and
inspection races, terrain actions, and road overlays. When moving a function,
check the source boundaries used by the tests. Use a real browser to verify
layout, focus and downloads after UI changes.

Before release, run the [release workflow](RELEASE_VALIDATION.md) on the exact
candidate. For container changes, rebuild and check the supplied Compose
service with a disposable volume. Track open acceptance work in the
[roadmap](ROADMAP.md).

## Documentation screenshots

Before opening a PR for a major dashboard change, regenerate the user-guide
images with the [manual Playwright capture tool](../tools/docs-screenshots/README.md).

## Benchmarks

The [coverage cache benchmark](../rust/crates/coverage/README.md#cache-benchmark)
measures cold and warm coverage on synthetic terrain or a local raster.
Record the host, revision, settings, fixture checksum and repetitions. Cold
runs begin with empty application caches and use the ordinary OS file cache.
The [terrain fixture manifest](REAL_TERRAIN_FIXTURE.md) lists optional real
terrain uses. [Recorded Python measurements](benchmarks) are dated baselines.

Current instructions live in the application reference, the user guide and
this guide. [Historical records](history/README.md) preserve dated plans and
evidence. When changing the engine, preserve workspace isolation, path
containment, stale-result guards and model fingerprints.
