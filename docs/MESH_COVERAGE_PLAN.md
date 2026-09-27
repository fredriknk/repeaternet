# Mesh coverage and planning tools implementation plan

Created: 2026-09-27. Status: C0–C5 implemented; browser/accuracy validation remains open for C3–C5; C6–C8 planned; release validation open.

This is the next phase after [the usability and performance plan](USABILITY_PERFORMANCE_PLAN.md).
Deliver and commit each milestone independently. Update its status, actual commit,
verification evidence, benchmark results and remaining limitations before moving on.
Do not mark an unmet acceptance requirement complete by lowering its target after measurement;
record a justified scope revision separately from the original target.

## Outcome and delivery order

Help the planner answer three questions: where can a client use this mesh, what
stops working when a router fails, and which proposed change provides useful improvement?
The product name is **Predicted mesh coverage**, rather than radiation: this is
a radio-link prediction for specified equipment and terrain, not a measurement
of emitted energy or delivered packet reliability.

| Milestone | Deliverable | Depends on | Release |
| --- | --- | --- | --- |
| C0 | Coverage semantics, fixtures and baseline measurements | Existing engine | Foundation |
| C1 | Bounded area coverage engine and client radio model | C0 | Coverage |
| C2 | Background coverage jobs, revisions and persistence | C1 | Coverage |
| C3 | Coverage overlay, controls and legend | C2 | Coverage |
| C4 | Click-to-inspect client links and profiles | C1–C3 | Coverage |
| C5 | Node-failure scenarios and connectivity analysis | C2–C4 | Resilience |
| C6 | Compare alternatives and antenna-height scenarios | C2–C5 | Decisions |
| C7 | Coverage targets: places, areas and roads | C3, C4, C6 | Decisions |
| C8 | Shareable report, exports and deployment verification | C3–C7 | Reporting |

First useful release: C0–C4. C5–C8 are subsequent releases, not prerequisites
for using the basic map overlay. Implementation follows this order so each
feature uses the same radio assumptions, sampling coordinates and job lifecycle.

## Current implementation and constraints

- `gui/main_window.py:CoverageWorker` already uses `LinkEvaluator` and
  `evaluate_link_pairs`. It evaluates links to terrain candidate sites, collects
  pairs in memory and returns only valid links. Its display is useful as a desktop
  preview, but is not an area coverage engine and does not preserve unknown states.
- `web_assets/app.js` already uses Leaflet and has a terrain-availability layer.
  Radio coverage needs its own layer, controls and legend.
- `web.py` has per-browser workspaces, named projects, input revisions, certified
  route snapshots, a bounded shared job scheduler and cancellation. Extend these
  mechanisms; do not create an unconstrained second worker pool.
- `RFSettings` has global transmit power and sensitivity, with antenna settings
  for endpoint A, endpoint B and routers. A handheld with different radio hardware
  requires explicit per-direction budget support; merely changing antenna height
  would give an incorrect two-way coverage prediction.
- `optimization/cache.py` stores bounded scalar link metrics. Coverage must not
  retain a full terrain profile for every sampled cell or evict all route metrics.
- Previous benchmarks cover synthetic route searches. Real-raster I/O, combined
  worker memory and concurrent-workspace performance remain release checks; the
  prior plan does not establish those results for this feature.

## Prediction contract

### Equipment, direction and mesh membership

Default sources are enabled repeater nodes in the selected certified alternative.
Endpoints/clients are excluded by default and can be explicitly included as radio
sources. A route with no repeaters shows an explanatory empty state and the option
to include endpoints. Source inclusion is a coverage setting, not a route edit.

The client profile includes height above ground, transmit power, antenna gain,
feed loss, sensitivity or LoRa sensitivity inputs, and additional losses. Start
with a clearly labelled editable example profile at 1.5 m; show all assumed radio
values. Do not present this as a universal MeshCore hardware specification.
Copy network frequency, propagation model, fade margin and validation mode from
the certified route snapshot. Source radio settings come from that same snapshot.

- Downlink margin: source transmission received by the client.
- Uplink margin: client transmission received by the source.
- Two-way margin for source `s`: `min(downlink_s, uplink_s)`.
- Best two-way coverage: `max_s(min(downlink_s, uplink_s))`. Both directions must
  work with the same source; never combine the best uplink and downlink from
  different routers to claim a two-way link.
- A usable link must also pass the configured LOS/Fresnel/propagation validity
  rules. Positive power margin alone is insufficient in strict validation mode.
- Overlap counts distinct usable sources. It is not independent-path redundancy,
  guaranteed network access, airtime capacity or a packet-delivery probability.
- If a selected network has multiple components, show component membership.
  C5 adds coverage that remains connected to a user-selected reference node.

### Grid and unknown areas

Use a deterministic grid in the terrain's metric CRS, independent of router
candidates. Sample at cell centres and render clipped cells. The initial area is
the selected mesh extent plus a configurable buffer; users can instead choose
the current map extent or draw an area. Changing the viewport does not silently
start another calculation. Coverage never automatically downloads terrain.

Distinguish evaluated/usable, evaluated/unusable, unknown terrain, not evaluated,
and outside the requested area. Missing DTM or a failed terrain profile is unknown,
not an RF rejection. Mark whether DOM is available. A user-imposed distance cap
means outside the analysed range, not known uncovered. A mathematically valid
radio-budget bound may establish failure without a terrain read, but a two-way
bound must not discard a source when calculating downlink-only coverage.

Grid spacing and link-profile sample spacing are separate controls. A dense map
does not imply accurate terrain profiles. A coarse preview is labelled approximate;
it cannot certify fine-resolution failures or successes. Refined cells replace
their preview results, with their own calculation resolution recorded.

Cells represent samples, not guaranteed coverage everywhere inside their bounds.
Never smooth colour across nodata, unsampled cells or blocked valleys. Show
requested area, evaluated area, unknown area and estimated covered area separately.
Report coverage percentage over evaluated area alongside the evaluated fraction
of the requested area. Partial jobs cannot silently acquire a complete denominator.

## C0 — Contract, fixtures and baseline [implemented]

Implementation:

- Define coverage settings, source selection, client radio profile, cell states,
  aggregation rules and numerical tolerances in typed models.
- Add deterministic flat, ridge, valley and nodata fixtures, including unequal
  client/source transmit power and sensitivity. Include two sources where the
  strongest uplink and downlink belong to different nodes.
- Add a coverage benchmark tool: source count, grid dimensions, requested area,
  radio settings, cold/warm runs, elapsed time, first chunk, RF calls, cache hits,
  terrain reads/bytes, response bytes and peak combined process memory.
- Follow [the real-terrain fixture manifest](REAL_TERRAIN_FIXTURE.md). A local
  licensed raster can be used without committing the raster; record its checksum,
  source, settings and availability. CI remains independent of provider downloads.

Progress: typed settings/results, explicit coverage states, the default handheld
radio profile, deterministic flat/ridge/valley/nodata reference cases, and a
fresh-process synthetic benchmark tool are implemented. The reference cases cover
asymmetric radio inputs, the different-best-uplink/downlink trap, unknown terrain,
and cancellation without publishing a partial cell. They also found and fixed the
grid counter omission for cells whose centre has no ground data.

Acceptance: eight coverage contract/reference checks pass. A three-process
synthetic baseline was recorded on Windows 11 build 26200, Python 3.13.12, 24
logical CPUs: for 256 cells, two sources, 1 km spacing and a 256 km² flat area,
median completion was 0.0669 s, first chunk 0.0365 s, and peak working set 56.47
MiB; each run made 512 RF evaluations and estimated 145,348 result bytes. The
256 km² ridge and valley runs had similar timings. The 256 km² nodata run reported
256 unknown source evaluations while retaining an evaluated result per cell via
the second source. Cancellation was observed after four pair evaluations, with a
2.2–2.5 ms acknowledgement in these small synthetic cases. These are process-level
synthetic figures, not real-raster, concurrent-worker, or field-performance claims.
After C1 added caching, the same fresh-process flat case measured 0.0858 s cold,
0.0089 s warm, with 512/512 scalar-cache hits. The nodata case reused 256 valid
links and recomputed its 256 unknown paths; failed terrain evaluations are not
cached. No redistribution-approved real raster was available for this baseline.

Areas: `models/coverage.py`, `tests/test_coverage.py`,
`tools/benchmark_mesh_coverage.py` and fixture documentation.
Commit: `test: add coverage reference fixtures and benchmark harness`.

## C1 — Area coverage engine and asymmetric client budgets [implemented; real-terrain validation open]

Implementation:

- Add a UI-independent coverage service with deterministic grid generation,
  bounded pair iteration, progress callbacks and cancellation checks.
- Extend the propagation boundary to accept source and client radio budgets
  explicitly. Preserve existing route-engine defaults and directional antenna
  behaviour. Include both budgets in versioned cache keys.
- Aggregate each cell's best source, directional and two-way margins, usable-source
  count, validity/rejection reason and completeness. Keep compact per-source scalar
  data within the pair budget for C5 scenarios; never store sampled profiles here.
- Use only valid optimistic bounds. Stream batches and release working arrays
  before advancing. Match exhaustive small-grid results before enabling pruning.
- Add a separately bounded coverage cache or quota so dense grids cannot evict the
  entire route cache. Key by source geometry/radio, client profile, terrain revision,
  model version, sample coordinates and profile spacing; keep workspace isolation.
- Refactor the desktop worker to use the shared service after web acceptance,
  or retain its explicitly labelled candidate preview until that separate migration.

Progress: deterministic metric-CRS grids, cell-centre terrain classification,
streamed chunks, pair/cell/source limits, two-way/downlink/uplink/overlap/best-source
aggregation and per-client radio budgets are implemented. Cancellation discards a
partially evaluated cell; profile failures are retained as unknown-source counts,
not negative links. C0 fixtures found and corrected omission of initial DTM nodata
from the grid's unknown-cell counter. A separate 50,000-entry coverage cache now
uses radio-budget-, route-, terrain-, model-, coordinate- and sample-spacing-aware
keys; it is namespace-isolated and cannot evict route metrics. Cached values retain
scalar link data only, never terrain profiles. The desktop preview still uses its
existing candidate-site workflow.

Synthetic acceptance passes: asymmetric client TX changes only the uplink budget;
strict-LOS flat/ridge/valley references pass; nodata remains unknown; streamed and
retained small-grid outputs match at chunk sizes 1 and 16; cancellation does not
publish a partially evaluated cell; and repeated 4,096-cell/eight-source coverage
reuses 32,768/32,768 scalar evaluations. That reference run completed in 5.17 s,
produced its first chunk in 0.169 s, and peaked at 162.54 MiB, below the 512 MiB
synthetic target. The default 50,000-entry cache fits that reference workload.
Real-raster I/O, concurrent workspaces, larger-than-cache repeated runs, and desktop
worker migration remain unverified or deferred; no real-raster fixture was
available in this workspace.

Areas: `rf/propagation.py`, `optimization/cache.py`, new
`coverage/engine.py`, `coverage/grid.py`, and engine regression tests.
Commits: `ef91b2c` (`feat: calculate bounded two-way mesh coverage grids`) and
`d13e77f` (`perf: cache bounded coverage link metrics`).

## C2 — Background jobs, stale results and saved settings [implemented; API validation passed]

Proposed API, using the existing workspace/authentication boundary:

| Endpoint | Purpose |
| --- | --- |
| `POST /api/coverage/estimate` | Resolve area, spacing, cell/pair count and limits without starting RF work |
| `POST /api/coverage/jobs` | Start a job against an explicit route snapshot and client profile |
| `GET /api/coverage/jobs/{id}` | State, progress, assumptions, completed extent and result revision |
| `GET /api/coverage/jobs/{id}/cells?cursor=…` | Bounded pages of immutable result chunks |
| `POST /api/coverage/jobs/{id}/cancel` | Cancel this workspace's matching job |

Implementation:

- Reuse the shared scheduler and outstanding-job cap. A workspace can have only
  one active coverage job; another workspace can use available global capacity.
  Coverage jobs have a separate manifest/state from route/terrain jobs.
- Freeze project ID, input revision, route/alternative content fingerprint,
  terrain fingerprint, source settings and client profile when dispatching.
  An alternative ID alone is insufficient when heights or geometry can change.
- Use worker-owned raster handles. Check cancellation between cells, pairs and
  bounded batches; release scheduler slots on success, failure, cancel and submit
  failure. Do not hold a workspace lock while computing or reading large rasters.
- Persist settings additively in each saved plan. Save a job manifest and stream
  completed chunks to workspace-local JSONL with a hard 100 MiB output limit.
  Exceeding the limit fails visibly (eviction/recomputation policy remains open).
  Restart marks queued/running jobs interrupted while retaining completed chunks.
- Server fingerprints reject stale route, alternative, terrain or settings
  revisions. Plan edits, project switching and terrain changes are locked while a
  coverage job runs; users can cancel before making those changes. Result reads
  and cancellation are scoped to the browser workspace. Late callbacks cannot
  publish into a replacement job.

Progress: estimate and bounded paging APIs, shared scheduler dispatch, project
settings persistence, atomic job manifests, restart recovery, cancellation, stale
revision checks, and a 100 MiB project-output guard are implemented. Route
fingerprints ignore client-only coverage settings and optimizer-generated
`resolved_search` metadata. API tests now cover estimates, paged output, stale saved
settings, browser/workspace isolation, reconnect without duplicate work, interrupted
manifest recovery with retained result pages, running and queued cancellation, shared
scheduler-slot release, project/terrain lifecycle locks, and quota failure. The quota
test lowers the byte cap in-process to exercise the same failure path without writing
a 100 MiB fixture.

Acceptance: all listed API lifecycle scenarios pass in `tests/test_coverage_jobs.py`.
The implementation fails visibly at the hard cap; quota eviction remains deferred
because no observed storage pressure justifies an eviction policy. No separate
fault-injected delayed-callback race test or two-live-workspace throughput benchmark
has been run; those remain C8 deployment checks.

Areas: `web.py`, `project_store.py`, a shared job helper if needed, web API tests.
Commits: `1ae3ba6` (`feat: run revision-safe coverage jobs and persist coverage settings`) and `1a1b905` (`test: validate coverage job recovery and isolation`).

## C3 — Coverage map and useful controls [implemented; visual validation open]

Implementation:

- Add a coverage panel with selected/all repeater sources, optional endpoints,
  editable client radio assumptions, area, detail, estimate, Calculate, Cancel,
  opacity and layer visibility. Require a completed certified route. Report missing
  ground data as unknown before or during calculation.
- Add modes for two-way margin (default), downlink, uplink, source overlap and best
  serving node. Implemented modes are two-way margin, downlink, uplink, overlap and
  best serving node. Margin bands are below 0, 0–5, 5–10 and at least 10 dB above
  the configured fade margin. They are labelled as planning bands, not reliability.
- Render with a Leaflet canvas/grid layer rather than one DOM marker per cell.
  Use a colour-blind-conscious palette, numeric tooltips and a text legend; unknown
  areas use a distinct gray fill and dashed outline. Keep route markers and links above.
- Show estimate counts, completed/total cells, effective grid, unknown cells, source
  count and client assumptions. Preserve pan/zoom while chunks arrive.
- Visibility/opacity/mode changes reuse stored results. Plan/source/client/area
  changes clear the layer and require recalculation; stale assumptions are never
  silently mixed. Changing the selected alternative invalidates the current result.

Progress: project-persisted coverage settings, all-or-selected repeater sources,
optional endpoint
sources, two-way/downlink/uplink/overlap/best-source views, estimate-before-confirm,
shared-job cancellation/progress, streamed JSONL result pages, Leaflet canvas cells,
opacity/layer control, and explicit unknown terrain rendering are implemented.
Mode and opacity changes reuse retained per-source data. Plan/alternative changes
clear the overlay; retaining a visibly stale prior result is deferred.

Acceptance open: this session exposes no browser surface (`cua` reported no apps
or browsers), and the Windows computer-use bridge returned an access-denied error
before browser launch. Therefore no visual walkthrough is claimed. The existing
automated coverage-job/API checks pass, but do not substitute for the required
browser review. When browser access is available, walk through calculate,
hide/show, mode switches, cancel, alternative change and project switch; verify
desktop/narrow-screen layout, keyboard controls, accessible legend, nodata rendering,
and responsiveness at the maximum grid. A distinct approximate preview and seamless
preview replacement are not implemented.

Areas: `web_assets/app.js`, `index.html`, `meshcore.css`, browser/API integration tests.
Commit: `61a8fb6` (`feat: add predicted mesh coverage map and point inspection`).

## C4 — Inspect any location [implemented; accuracy validation open]

Implementation:

- Add an explicit Inspect coverage map mode so clicks do not move endpoints or
  place routers. Show selected location and client height.
- Evaluate the exact clicked point, independently of its cell centre, using final
  profile resolution. Rank sources by valid two-way margin, then stable source ID.
  Display uplink/downlink, serving-node/component identity and terrain availability.
- Fetch the selected source's terrain/Fresnel profile on demand in the inspection
  chart. Return rejection reasons and distinguish missing data from failure.
- Run through the shared bounded job scheduler; a newer click cancels/supersedes an
  old one. Share scalar route cache only where radio budgets match; the asymmetric
  client override currently bypasses the scalar cache. Retain one profile only.

Progress: exact point terrain sampling, capped final-resolution profiles, directional
link margins, deterministic best same-router two-way ranking, rejection reasons,
selected network-component identity, bounded scheduler dispatch and stale-revision
guards are implemented. Browser cancellation aborts/supersedes late requests and
the server checks cancellation between source evaluations. Missing terrain along
one candidate path is retained as an unknown link; if every path is unknown, the
inspection reports unknown rather than an RF failure. The on-demand terrain profile
is labelled, and a profile-only terrain gap does not discard a valid scalar result.

Acceptance open: hand-check at grid centres, test asymmetric budgets and strict LOS,
exercise nodata/blocked links and rapid clicks/project edits, and verify chart/map
presentation. The route-scalar cache cannot be reused with the alternate client
budget, so cache sharing is deferred until cache keys support those budgets.

Commit: `61a8fb6` (`feat: add predicted mesh coverage map and point inspection`).

## C5 — Node-failure scenarios [implemented; visual validation open]

Implementation:

- Allow temporary disabling of one or more routers within an analysis scenario.
  Keep the saved route untouched; provide Reset scenario.
- Remove failed sources and their incident certified graph edges, then recompute
  connected components and reachability to a selected surviving reference node.
  If the reference fails, show reference unavailable and offer another selection.
- Display lost local coverage, lost reference-connected coverage, isolated routers
  and affected client routes separately. Reuse compact cell/source metrics where
  complete; recompute explicitly when retained data is insufficient.
- Optional independent-path counts must be calculated on the surviving graph.
  Never infer them from overlapping RF footprints or automatically invent replacement
  links. An explicit re-optimize action creates a separate planning operation.

Acceptance: bridge, ring and disconnected-component fixtures give exact expected
losses; overlapping but isolated routers do not count as reference access; reset
restores the baseline without RF work when the matrix is available.

Progress: completed-coverage scenarios can disable selected repeater sources and
recompute surviving-router components from the immutable certified route graph.
The server reports local RF coverage separately from coverage served by sources
still connected to a chosen reference; a failed reference is explicitly unavailable.
The UI provides temporary failure checkboxes, reference selection, reset, component
membership and lost-cell summaries. Stale, partial, cross-workspace and non-source
requests are rejected. Scenario evaluation reuses stored same-source directional
metrics and does not start RF work or edit the saved route.

Verification: bridge, ring, disconnected/overlapping-router and unknown-cell
fixtures pass in `tests/test_coverage_scenarios.py`; workspace-scoped completed-job
API, failed-reference, invalid-source and cross-workspace checks pass in
`tests/test_coverage_jobs.py`. Full suite: 130 passed; Ruff, mypy and JavaScript
syntax checks pass. UI visual review remains open because no browser surface is
available in this session (same C3 limitation).

Commit: `feat: simulate router failures and surviving mesh coverage`.

## C6 — Compare alternatives and antenna-height scenarios [planned]

Implementation:

- Capture two immutable snapshots from alternatives or saved projects within the
  same workspace. Compare on the same area, client profile, grid and radio model;
  resolve incompatible settings visibly before producing area deltas.
- Add baseline/scenario toggles and a difference overlay: newly covered, lost,
  still covered, and unknown in either run. Compare margin only on jointly evaluated
  cells; report the common evaluated area beside percentage changes.
- Offer editable source-height scenarios, initially 3/6/10 m examples. Revalidate
  both affected mesh links and client links. A scenario with a broken backbone must
  not appear better merely because one node's local RF footprint increased.
- Reuse unchanged per-source calculations. Make applying a scenario an explicit
  plan edit with undo; retain baseline snapshots for comparison.

Acceptance: identical snapshots give zero change; swapping baseline/scenario
reverses gained/lost areas; height/radio changes invalidate only applicable cache
entries; missing terrain is excluded from claims of improvement; source edits
cannot leak into the baseline.

Commit: `feat: compare coverage and antenna-height planning scenarios`.

## C7 — Places and routes that need coverage [planned]

Implementation:

- Add named point targets, drawn polygons and polylines; support GeoJSON import
  with size, geometry, coordinate and vertex limits. Store targets with the project.
- Inspect points exactly; estimate polygon area from clipped grid-cell areas in a
  suitable metric/equal-area CRS; sample roads at a stated distance interval.
  Report evaluated, covered, failed and unknown area/length separately, including
  small targets that need finer sampling. Do not treat a road as a list of vertices.
- Let targets specify minimum two-way margin and optional reference connectivity.
  Summarise pass/fail/unknown and compare target outcomes across C6 scenarios.
- First version assesses plans; a new coverage-maximising routing objective is a
  separate future engine milestone with its own search guarantees.

Acceptance: point targets agree with C4; area/length denominators match reference
geometry cases; imports cannot create unbounded sampling work; nodata and partially
evaluated targets cannot pass; saved targets round-trip without geometry changes.

Commit: `feat: assess coverage for target locations areas and roads`.

## C8 — Reports, exports and release verification [planned]

Implementation:

- Add GeoJSON cell/target export and compact JSON settings/results export with
  job/revision, CRS, terrain provenance, grid/profile spacing, completeness and
  radio assumptions. Export unknown states explicitly. Respect result-size limits.
- Add a printable HTML report containing the selected network, a local map image,
  coverage legend, target results, scenario deltas and weakest links. It must work
  without a live basemap. Clearly label partial/stale reports; require explicit
  selection to export them. PDF generation and public share hosting can follow later.
- Update README and this plan with actual evidence. Test the self-hosted container
  with two browser workspaces and interrupted/restarted jobs. Review the browser UI
  visually, including unknown terrain, empty source sets and failed jobs.

Acceptance: exported cells reproduce displayed counts and states, client/source
settings are included, project data stays workspace-scoped, report printing is
legible, and cold/warm benchmarks meet the recorded release targets or remain
explicitly open with a corrective milestone.

Commit: `feat: export mesh coverage reports and verify self-hosted operation`.

## Resource limits and benchmark gates

Current defaults and remaining release gates:

| Limit | Initial proposal | Behaviour at the limit |
| --- | --- | --- |
| Preview grid | Not implemented | A separate 256-cell approximate preview remains planned |
| Standard grid | 4,096 cells by default | Estimate resolves effective spacing against cell/evaluation caps |
| Detailed grid | 16,384 cells maximum in saved settings | Still bounded by the cell/source evaluation cap |
| Selected radio sources | 64 maximum | Return actionable validation error |
| Cell/source evaluations | 250,000 per job maximum | Estimate includes all requested passes/scenarios |
| Stream chunks | 128 cells per chunk | Write JSONL and release worker cell buffers |
| Coverage scalar cache | 50,000 compact metrics by default | Separate from route metrics; configurable with `RF_PLANNER_COVERAGE_CACHE_ENTRIES` |
| Persisted results | 100 MiB hard output limit | Fail visibly at the limit; no automatic eviction yet |
| Concurrent work | Existing server-wide active/queued limits | One active compute job per workspace initially |

Also bound samples per RF profile, chunk bytes, inspection profiles and total job
runtime after C0 measurements. Over-limit profiles become unresolved with an
actionable message; do not silently lower terrain resolution and label them final.
Cancellation is checked inside long sampling/evaluation stages where possible.

Benchmark matrix: 2/8/32 sources; 256/4,096/16,384 cells subject to the pair cap;
flat/ridge/nodata and real-raster cases; one/two concurrent workspaces; cold/warm
cache; fully covered/no coverage; queued and mid-job cancellation. Exercise
1/2/4 worker counts only if process parallelism is added; aggregate their RSS.
Run three fresh-process repetitions on a recorded host and report median/range.

Initial engineering targets: first 256-cell preview within 5 s on the recorded
two-source synthetic case; at least 90% reuse of eligible scalar evaluations on
an unchanged rerun; cancel acknowledgement within 1 s and released compute capacity
within 2 s for the reference bounded jobs; combined peak process memory below
512 MiB for the 4,096-cell/eight-source raster fixture with one active job; no
unbounded growth when repeatedly switching alternatives. These are targets, not
measured claims. Set real-terrain completion-time targets from C0 before C1 tuning.

## Validation and progress record

Run tests appropriate to each milestone, the full suite at release boundaries,
Ruff/mypy and JavaScript checks when relevant. Include human-visible browser checks
for C3/C4/C6/C8. Do not substitute synthetic tests for field measurements or mark
unavailable browser/real-terrain checks as passed.

For each C0–C8 entry append: status; commit; implemented scope; tests and commands;
timings/RSS and fixture identity; schema/default changes; visual review; unresolved
acceptance criteria and their next action.

Current implementation record (2026-09-27):

| Milestone | Status / commit | Verification and remaining work |
| --- | --- | --- |
| C0 | Implemented · `061f0df` | Eight coverage contract checks pass; flat/ridge/valley/nodata and cancellation benchmark captured; cold/warm comparison measured. |
| C1 | Implemented · `ef91b2c`, `d13e77f` | Directional budgets, bounded separate cache, streamed/retained reference and 4,096×8 benchmark pass; real-raster/concurrent validation open. |
| C2 | Implemented · `1ae3ba6`, `1a1b905` | API tests cover workspace isolation, lifecycle locks, paging, reconnect/restart, running/queued cancellation, scheduler release, stale settings and quota failure. |
| C3 | Implemented · `61a8fb6`; visual review open | Browser access unavailable in this session; calculate/hide/show/mode/cancel/project-switch, narrow-screen, keyboard, nodata and max-grid visual checks remain open. |
| C4 | Implemented · `61a8fb6` | Mypy (ignoring missing third-party stubs) passes; no terrain-reference, click-race or profile visual check. |
| C5 | Implemented · commit pending | Bridge/ring/disconnected graph analysis, same-matrix local/reference coverage deltas and workspace-scoped API tests pass; visual review remains open with C3. |
| C6–C8 | Planned | No implementation started. |

Validation recorded so far: full suite `python -m pytest` passes 126 tests;
`python -m ruff check src tests tools` and `python -m mypy src/rf_router_planner
--ignore-missing-imports` pass. JavaScript syntax checks passed in the previous C3/C4
implementation pass. No real-raster timing, concurrent-workspace RSS, field
validation or browser visual review has been measured; these remain release gates,
not implied successes.

Deferred beyond this plan: live packet/RSSI ingestion and calibration, traffic or
airtime simulation, automatic optimisation for area coverage, mobile GPS tracking,
new propagation models, hosted public sharing and authentication beyond the
existing self-hosted workspace model.
