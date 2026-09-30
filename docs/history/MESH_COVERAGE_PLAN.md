> Historical record, archived 2026-09-30. Implementation notes and test results describe their recorded revisions. For current behavior and remaining work, use the [user guide](../USER_GUIDE.md) and [roadmap](../ROADMAP.md).

# Mesh coverage and planning tools implementation plan

Created: 2026-09-27. Status: C0–C11 feature set delivered; C12 implementation corrections are delivered, with browser, deployment/concurrency, and field-accuracy release validation still open.

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
| C9 | Exact-result replay for unchanged large coverage runs | C2, C6, C8 | Performance hardening |
| C10 | Bounded approximate preview and seamless refinement | C3, C9 | Usability |
| C11 | User-drawn coverage areas and explicit outside-area cells | C3, C7, C10 | Coverage |
| C12 | Correct profile-limit semantics and real-raster sampling bottleneck found during acceptance audit | C1, C4, C6, C7 | Corrective validation |

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
- Follow [the real-terrain fixture manifest](../REAL_TERRAIN_FIXTURE.md). A local
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
At that initial implementation stage, real-raster I/O and larger-than-cache reuse
were unverified because no local fixture was available. The continuation evidence
below adds real-raster and cache-capacity measurements; aggregate web/worker
concurrency and desktop worker migration remain unverified or deferred.

Continuation evidence (2026-09-27): an existing local Kartverket DTM cache was
subsequently identified and its request identity/checksum verified. The
[fixture manifest](../fixtures/kartverket-nhm-25832-50m.md) records attribution,
provenance limitations, fixed coordinates and reproduction commands. Three fresh
processes per case used 50 m RF profiles over a 396.01 km² region:

| Real-raster core case | Cold median (range) | First chunk median | Warm median | Peak working set median (maximum) |
| --- | --- | --- | --- | --- |
| 256 cells × 2 sources | 3.6467 s (3.6366–3.6805) | 1.7743 s | 0.0247 s | 124.52 MiB (124.64) |
| 4,096 cells × 8 sources | 271.8822 s (268.8550–273.8899) | 11.3523 s | 0.7655 s | 191.38 MiB (191.59) |

Both cases produced byte-identical serialized cold/warm results, with 100% warm
cache hits (512 and 32,768). The standard case produced 3,349 covered and 747
uncovered cells, no unknown cells, and 8,181,630 serialized bytes. Raw measurements
are in [preview evidence](../benchmarks/coverage-raster-preview.json) and
[standard evidence](../benchmarks/coverage-raster-standard.json). The memory figure
is the engine child high-water working set, not concurrent web-server RSS. Cold
means an empty application cache, not flushed filesystem/GDAL/storage caches.
This closes the absence of real-raster core measurements; browser/concurrency,
field accuracy and the wider source/grid matrix remain open. A C1 maximum-budget
real-raster continuation measurement is recorded next.

Full-budget scaling evidence (2026-09-28): the 32-source × 7,744-cell workload
uses 247,808 source evaluations, just below the 250,000-per-job limit. Three fresh
processes on the same Kartverket fixture, bounds and 50 m terrain profiles completed
in a 96.5602 s median (96.5193–98.1734 s); the first chunk arrived in 1.3621 s
(1.3460–1.4477 s). Peak engine-child working set was 232.082 MiB median (maximum
232.586 MiB), below the 512 MiB single-job target. All 7,744 cells were evaluated
(6,963 covered, 781 uncovered, no unknown cells); the estimated serialized grid
was 62,625,475 bytes, below the 100 MiB persisted-result limit. Cold and warm
serialized results were identical across repetitions with checksum
`0e1785de64fdb17dc1de4a0cfe16ab73a4d60c34c7438bc7f6fdaa4ff61f8672`. The repeated
engine run took 96.5981 s median and had 0/247,808 cache hits: the default 50,000
scalar-metric cache cannot retain this full pair set, so the direct engine benchmark
does not meet a 90% cache-reuse target at this size. Exact saved-result replay is
implemented separately in C9; large-result HTTP replay and storage-pressure
behavior remain open. Full measurements are in the
[32-source maximum-budget artifact](../benchmarks/coverage-raster-large-32x7744-c12.json).
Reproduce with `python tools/benchmark_mesh_coverage.py --dtm
cache/wcs_large_smoke/kartverket_dtm_ee927a9a60cf36b4ef38.tif --sources 32
--cells 7744 --repeats 3 --output
docs/benchmarks/coverage-raster-large-32x7744-c12.json`.
These are engine-child figures only, not combined Uvicorn/worker RSS, concurrent
workspaces, browser latency or field-accuracy evidence. The harness cancellation
probe measures cancellation after an RF call returns and does not verify server
scheduler capacity release under concurrent load.

Cache sizing probe (single run, same case): increasing capacity to 300,000 entries
gave 247,808/247,808 warm hits and a 4.3422 s warm rerun, but peak child working
set reached 594.6289 MiB (cold peak 592.5273 MiB), exceeding the 512 MiB target.
The cold run took 99.2803 s and its first chunk arrived in 1.3502 s. This one-run
diagnostic confirms that simply raising the default cache is not a safe fix; retain
the bounded 50,000-entry default and use C9 exact saved-result replay for identical
requests. The 300,000-entry result is a sizing probe, not a three-repetition
acceptance benchmark.

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
and responsiveness at the maximum grid. C10 adds a bounded approximate preview and
keeps it visible until the first refined result cells arrive. C11 adds user-drawn
areas and explicit outside-area semantics; browser verification remains open.

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
  old one. Use the separate coverage cache with both radio budgets included in its
  key. Retain one profile only.

Progress: exact point terrain sampling, capped final-resolution profiles, directional
link margins, deterministic best same-router two-way ranking, rejection reasons,
selected network-component identity, bounded scheduler dispatch and stale-revision
guards are implemented. Browser cancellation aborts/supersedes late requests and
the server checks cancellation between source evaluations. Missing terrain along
one candidate path is retained as an unknown link; if every path is unknown, the
inspection reports unknown rather than an RF failure. The on-demand terrain profile
is labelled, and a profile-only terrain gap does not discard a valid scalar result.

Acceptance open: hand-check at grid centres, test asymmetric budgets and strict LOS,
exercise nodata/blocked links and project edits, and verify chart/map presentation.
Rapid-click ordering now has the regression evidence below; a live browser review
is still required. The coverage scalar cache keys already include both budgets.

C4 continuation audit (2026-09-27): delayed autosaves could dispatch obsolete
clicks after newer results, and a start response arriving after inspection mode
was closed could leave an orphan server job. Three Node regression checks
reproduced these failures before the fix. Job-start requests are now serialized,
each click retains its own cancellation controller, and obsolete start responses
are cancelled before the next click dispatches. Five JavaScript tests verify
delayed autosave, pending start, exiting inspection, failed-start recovery and a
late polling response. A deterministic API test holds the older RF evaluation
until the newer inspection completes, then releases it and confirms the newer
result is unchanged and the older ID is unavailable. All six targeted checks pass.
This is automated request-ordering verification, not a browser visual sign-off.
Commit: `415a148` (`fix: preserve newest mesh coverage inspection across request races`).

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

## C6 — Compare alternatives and antenna-height scenarios [implemented; visual validation open]

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

Progress: completed coverage jobs are now archived as project-local immutable
snapshots, with a bounded recent-run selector. Comparison requires the same project
terrain revision, radio/model version, handheld profile, sample spacing, area and
cell grid; it rejects different or incomplete grids. The comparison streams saved
JSONL chunks (rather than retaining both cell matrices), computes local and
reference-connected gained/lost/retained cells separately, excludes unknown cells
from both denominators, and exposes an overlay plus a common evaluated-area summary.
Height scenarios accept per-router overrides with 3/6/10 m shortcuts. They rerun
client links, revalidate affected certified router-to-router links under the shared
bounded worker, and never invent new backbone edges. Applying a scenario height is
an explicit plan edit: optimized routers are pinned as required proposed sites,
existing MeshCore/manual site values are updated, and normal undo restores the
baseline. The original route snapshot remains unchanged until that edit.

Verification: identical runs yield zero deltas; swapped runs reverse gained/lost;
unknown and misaligned coordinates are handled conservatively. API tests verify
history persistence, same-grid comparison, changed terrain/profile rejection,
separate height snapshots, height bounds, and a height change that invalidates a
certified bridge before reference-connected comparison. Full suite: 138 passed; Ruff, mypy,
JavaScript syntax and diff checks pass. Synthetic rasters only; browser visual review
and comparative real-terrain performance remain open.

Commit: `afd7db8` (`feat: compare coverage and antenna-height planning scenarios`).

## C7 — Places and routes that need coverage [implemented; visual/terrain validation open]

Implementation:

- Add named point targets, drawn polygons and polylines; support GeoJSON import
  with size, geometry, coordinate and vertex limits. Store targets with the project.
- Inspect points and distance-sampled road locations exactly with the same client
  radio budget, profile cap and LOS/Fresnel policy as C4. Use distance-weighted
  intervals so endpoints and short final segments are represented correctly.
- Estimate polygon outcomes from clipped coverage-grid cell footprints in the
  terrain's projected metre CRS. A clipped footprint without a sample centre inside
  the target, area outside the grid, or nodata is unknown; a polygon with no centre
  sample asks for a finer grid instead of passing.
- Let targets specify minimum two-way margin and optional reference connectivity.
  Summarise pass/fail/unknown and compare saved outcomes across compatible C6 runs.
- First version assesses plans; a new coverage-maximising routing objective is a
  separate future engine milestone with its own search guarantees.

Progress: named point, polygon and line targets can be imported as bounded GeoJSON
or placed on the map and saved in the project plan. Imports and autosaves validate
geometry type, unique identifiers, coordinate ranges, polygon closure, minimum
margin, 5 MiB file size, 500 features and 50,000 vertices. Exact point/road work is
capped at 50,000 router evaluations and 20,000 samples per line; polygon assessment
is capped at 32 areas and 250,000 cell scans. Target work reserves capacity from
the same global planner slot counter and returns 429 when all worker capacity is
occupied. Per-target reference routers restrict the eligible set to the certified
connected component. Completed reports are project-local, bounded to 20 per
coverage run and 5 MiB per report. Compatible reports from different alternatives
or height scenarios can be compared for improved, regressed, unchanged and
uncertain target outcomes.

Verification: the exact point's best two-way margin matches C4 inspection at the
same coordinate; a short road passes with represented lengths summing to its full
projected length; an area with no in-area grid sample requests finer resolution.
Tests also cover GeoJSON validation/byte, vertex, sample and grid-scan limits,
project/workspace isolation, target persistence, reference connectivity, stale-run
handling and compatible C6 report comparison. Full suite: 148 passed; Ruff, mypy
and JavaScript syntax checks pass. No browser visual review or licensed real-raster
accuracy/performance check was available; these remain explicit open checks.

Commit: `b75384b` (`feat: assess targets against predicted mesh coverage`).

## C8 — Reports, exports and release verification [implementation complete; release checks open]

Implementation:

- Add GeoJSON cell/target export and compact JSON settings/results export with
  job/revision, CRS, terrain provenance, grid/profile spacing, completeness and
  radio assumptions. Export unknown states explicitly. Respect result-size limits.
- Add a printable HTML report containing the selected network, an embedded offline SVG map,
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

Commit: `cf2d6a3` (`feat: export mesh coverage reports`).

## C9 — Large-run result reuse [implemented; large-file validator measured]

The C0–C8 benchmark matrix showed that an unchanged full-grid rerun could miss
the entire 50,000-entry LRU when the scan itself was larger than capacity. The
8-source/16,384-cell flat case made 131,072 misses on its warm pass; the
32-source/evaluation-capped case made 247,808. Increasing the cache to 250,000
entries produced 100% warm hits, but peaked at 569.94 MiB for that 32-source
case, above the original 512 MiB engineering target. Prefer verified project-local
snapshot replay for an exact repeated calculation; keep the scalar cache bounded
for partial overlap across changed plans.

Implementation:

- Fingerprint project, certified route and alternative, terrain revision/CRS,
  propagation/client settings, selected source geometry and heights, and the
  resolved projected grid (bounds, dimensions and spacing).
- Search a bounded recent manifest window for a matching complete run. Validate
  all stored cell indexes/states before reuse. Return the existing immutable job
  and result file without reserving a worker slot or running RF evaluations.
- A stale, partial, malformed, differently sampled or different-project result
  must never match. Same-snapshot comparisons are valid and produce zero deltas.
  The UI must tell the user when saved results were reused.
- Tests cover the exact-match path (same job ID, zero further `calculate_coverage`
  calls), changed antenna-height invalidation, stale terrain rejection, malformed
  JSONL rejection, every fingerprint field, and same-snapshot zero-delta comparison.
  The POST response includes `reuse_validation_seconds`; the UI reports that no RF
  recalculation was needed. Existing result retention remains capped by the prior
  100 MiB per-run output limit and 50-manifest lookup window.

Verification: all 157 tests pass; Ruff, mypy, JavaScript syntax, and `git diff
--check` pass. The synthetic benchmark matrix used three fresh-process repetitions
on Windows 11 build 26200, Python 3.13.12, 24 logical CPUs, 1 km cells, a 200 m
terrain fixture, 200 m RF profile step, and 50,000 cache entries. The flat
16,384-cell/eight-source case took 23.56 s median (0.179 s first chunk, 209.91 MiB
peak) and had zero cache hits on its 131,072-evaluation warm scan. The
16,384-cell/32-source request was evaluation-capped to 247,808 evaluations; flat
terrain took 44.64 s median (0.684 s first chunk, 211.14 MiB peak, 54.89 MB
serialized-result estimate) and also had zero warm hits. Nodata variants likewise
had zero warm hits because failed paths are deliberately not cached.

A one-repetition capacity check at 250,000 cache entries gave 100% warm hits for
both large cases, but peak working set rose to 352.54 MiB for 8×16,384 and
569.94 MiB for 32×16,384. Production cache capacity therefore remains 50,000;
verified persisted-grid replay avoids this memory increase for exact reruns. The
benchmark harness exposes `--cache-entries` for reproducing the tradeoff. The
validation timer is exposed by the API. The continuation measurements below add
large-file validation timings; retention/storage-pressure policy remains deferred.

Continuation evidence (2026-09-27): `tools/benchmark_coverage_replay.py` generates
bounded production-shaped JSONL files, then calls the production replay validator.
Each case uses three fresh processes and searches 50 manifests with the matching
one last. Eight sources × 16,384 cells produces 27,973,272 bytes: current-job
validation median 0.1634 s (0.1598–0.1635), archive lookup 0.1879 s
(0.1852–0.3267), peak working set median 112.75 MiB. The 32-source case is capped
at 7,744 cells / 247,808 pairs and produces 48,027,449 bytes: current-job validation
0.2594 s (0.2573–0.2600), archive lookup 0.2857 s (0.2771–0.4333), peak median
117.63 MiB. Every repetition also rejects an extra out-of-grid cell appended at
EOF. Exact fixture checksums, individual runs and commands are captured by
[eight-source evidence](../benchmarks/coverage-replay-8-sources.json) and
[32-source evidence](../benchmarks/coverage-replay-32-sources.json). These isolate
large-file parsing/integrity and manifest selection using generated scalar data;
they exclude RF computation, browser/HTTP latency and cold storage caches. The
existing API regression separately proves identical requests do not rerun RF.

Commit: `5a9ee78` (`perf: reuse verified coverage snapshots`).

## C12 — Acceptance-audit corrections [implementation delivered; release validation open]

The audit found two concrete issues behind previously open acceptance criteria.
Both implementation corrections are delivered as C12a and C12b. C12 remains
open for browser, deployment/concurrency, and field-accuracy release validation.

1. Grid, exact inspection, target and changed-backbone evaluation had silently
   enlarged profile spacing at the sample cap. C12b preserves the requested final
   resolution, reports over-limit links as unresolved with an actionable reason,
   propagates that state through scenarios, targets, exports and UI, and invalidates
   old reusable results. Long-link cap regressions now verify that capped links do
   not pass because a narrow obstruction may have been skipped.
2. The real-raster baseline spent most of its cold time in point-at-a-time
   sampling. A profiled 256-cell/two-source cold/warm/cancel probe spent 8.85 s of
   9.81 s in `RasterTerrain.sample`, including 85,925 Rasterio sampling-generator
   calls (profiling adds overhead; use the unprofiled timings above for performance).
   C12a batches bounded raster windows while preserving nearest-pixel sampling,
   nodata, mosaic precedence, DOM behavior and edge semantics. Independent Rasterio
   sample comparisons and the same-checksum/settings benchmark pass; see C12a below.

Targets set **before tuning**: standard 4,096×8 real-raster cold completion at most
60 s, first 128-cell chunk at most 5 s, below 512 MiB engine peak working set;
unchanged cold/warm serialized results away from corrected invalid raster edges.
The existing two-source synthetic preview, cache-reuse and cancellation targets
remain unchanged. Deployment-level memory and cancellation checks remain separate.

### C12a — Window-batched raster sampling [implemented · `8720a29`]

`RasterTerrain.sample` now groups requested pixel coordinates by 256×256 raster
windows, reads each touched window once, and preserves request order and the first
unmasked tile's mosaic precedence. DOM sampling still falls back to DTM only when
no DOM is configured; masked DOM samples remain unknown. Independent tests compare
rotated and north-up transforms to Rasterio's nearest-pixel sampler and cover
nodata, overlap, edges, nonfinite coordinates, empty arrays, and the bounded-window
read contract.

On the same local Kartverket DTM, bounds, 50 m profile floor, 4,096×8 settings,
and three fresh-process repetitions as the committed point-sampling baseline,
cold median fell from 271.88 s to 12.3352 s (22.04×); first 128-cell chunk fell
from 11.35 s to 0.3436 s; peak engine working set was 188.54 MiB versus 191.38 MiB.
Warm median was 0.6398 s. All three cold/warm output checksums matched the baseline
`3a11ded81997c61a22beee3692c0bd4ec29d8253360e4486d47a489aacfac227`. Preview
256×2 cold median was 0.1885 s (22.04× versus the baseline), first chunk 0.0915 s,
and peak 125.06 MiB. Full per-run data is in the
[standard real-raster benchmark](../benchmarks/coverage-raster-standard-windowed.json)
and [preview benchmark](../benchmarks/coverage-raster-preview-windowed.json).
These are local-raster timings, not server/browser latency or field accuracy.

### C12b — Preserve final-profile sample limits [implemented · `5fa4b55`]

Grid, exact-point inspection, road/polygon targets and changed-backbone
revalidation now use the requested profile spacing (subject to terrain-resolution
floor) without automatically coarsening it to fit the sample cap. A link over the
configured maximum is not sent to the RF evaluator and carries an explicit
`profile_sample_limit` rejection with its required sample count and remediation.
Coverage and target assessment distinguish unresolved from covered, uncovered,
terrain-unknown and outside-area states. If a cell has a valid source as well as a
capped source, it remains proven covered while retaining the unresolved-source
count. Comparisons exclude unresolved cells from gained/lost counts and their
denominator; height scenarios report unresolved cells/links; backbone reachability
is conservative for unresolved links. JSON/GeoJSON/printable exports retain the
state and details, identify capped reports as partial, and require explicit
partial-export opt-in. The map exposes a 3–16,384 maximum-samples control and a
separate unresolved legend/pattern. Coverage model identity is now coverage-v3 so
prior v2 snapshots are not reused under the corrected semantics.

Regression tests cover the exact cap boundary and rejection, grid/inspection
short- and long-distance behavior, target assessment, comparisons, scenario and
backbone analysis, saved jobs and exports. The default maximum remains 4,096
samples per path; users may set the cap from 3 through 16,384 or explicitly raise
profile spacing. The full suite passed 187 tests; Ruff and mypy passed; JavaScript
syntax checks and all six inspection interaction tests passed. No fresh real-raster
benchmark was required because this milestone changes correctness and does not
change the C12a sampler. Human-visible map/print review remains open because no
browser surface was available.

## Resource limits and benchmark gates

Current defaults and remaining release gates:

| Limit | Initial proposal | Behaviour at the limit |
| --- | --- | --- |
| Preview grid | 256 cells / 4,096 source evaluations | Separate approximate endpoint reserves a shared planner slot; refined cells replace the preview when available |
| Standard grid | 4,096 cells by default | Estimate resolves effective spacing against cell/evaluation caps |
| Detailed grid | 16,384 cells maximum in saved settings | Still bounded by the cell/source evaluation cap |
| Selected radio sources | 64 maximum | Return actionable validation error |
| Cell/source evaluations | 250,000 per job maximum | Estimate includes all requested passes/scenarios |
| Stream chunks | 128 cells per chunk | Write JSONL and release worker cell buffers |
| Coverage scalar cache | 50,000 compact metrics by default | Separate from route metrics; configurable with `RF_PLANNER_COVERAGE_CACHE_ENTRIES` |
| Persisted grid results | 100 MiB hard output limit | Fail visibly at the limit; no automatic eviction yet |
| Target GeoJSON | 5 MiB / 500 features / 50,000 vertices | Reject with actionable validation error |
| Exact point/road target RF work | 50,000 source evaluations / request; 20,000 samples / road | Reject; increase road spacing or assess fewer targets |
| Polygon target work | 32 areas / 250,000 grid-cell scans / request | Assess fewer areas at once |
| Saved target reports | 20 / coverage run; 5 MiB / report | Reject additional reports visibly |
| Concurrent work | Existing server-wide active/queued limits | Target assessment reserves a global planner slot; one coverage job per workspace |

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

For each C0–C12 entry append: status; commit; implemented scope; tests and commands;
timings/RSS and fixture identity; schema/default changes; visual review; unresolved
acceptance criteria and their next action.

Current implementation record (2026-09-28):

| Milestone | Status / commit | Verification and remaining work |
| --- | --- | --- |
| C0 | Implemented · `061f0df` | Eight coverage contract checks pass; flat/ridge/valley/nodata and cancellation benchmark captured; cold/warm comparison measured. |
| C1 | Implemented · `ef91b2c`, `d13e77f`; C12 corrections delivered | Directional budgets, bounded cache and real-raster cold/warm measurements recorded above, including the 32 × 7,744 max-budget case. It takes 96.56 s median / 232.08 MiB median child peak and has no direct cache hits on replay because the 50,000-entry cache is smaller than the 247,808-pair workload; a 300,000-entry sizing probe reached 594.63 MiB, above target. Keep the bound and use C9 exact replay for identical requests. Sampler and sample-cap fixes are in C12. Aggregate concurrency and field verification remain open. |
| C2 | Implemented · `1ae3ba6`, `1a1b905` | API tests cover workspace isolation, lifecycle locks, paging, reconnect/restart, running/queued cancellation, scheduler release, stale settings and quota failure. |
| C3 | Implemented · `61a8fb6`; visual review open | Browser access unavailable in this session; calculate/hide/show/mode/cancel/project-switch, narrow-screen, keyboard, nodata and max-grid visual checks remain open. |
| C4 | Implemented · `61a8fb6`; race correction recorded above | Five JavaScript race regressions and a delayed-worker API regression pass. Terrain-reference and profile visual review remain open. |
| C5 | Implemented · `6b5f168` | Bridge/ring/disconnected graph analysis, same-matrix local/reference coverage deltas and workspace-scoped API tests pass; visual review remains open with C3. |
| C6 | Implemented · `afd7db8` | Compatible snapshot comparisons, streaming deltas, difference overlay, separate height runs and backbone-edge revalidation pass; same-project synthetic tests, visual/real-raster review open. |
| C7 | Implemented · `b75384b`, extended in C11 | Project-scoped targets, exact points/roads, conservative area checks, drawn-analysis-area boundaries and saved-report comparisons pass synthetic API and hard-limit tests; browser/real-raster review open. |
| C8 | Implemented · `cf2d6a3` · release checks open | GeoJSON/JSON/printable offline SVG report and API/UI controls implemented; 157-test suite, Ruff, mypy, JS syntax and diff checks pass; Docker/browser visual checks remain unavailable. |
| C9 | Implemented · `5a9ee78`; large-file measurements recorded above | Production validator measured against 28/48 MB generated JSONL fixtures with 50-manifest lookup and EOF corruption rejection. Browser/HTTP replay latency and storage-pressure policy remain open. |
| C10 | Implemented · `8ae53bd` | `POST /api/coverage/preview` is bounded to 256 cells and 4,096 router evaluations, participates in the shared scheduler, is labelled approximate in the UI, and yields to the full calculation on its first chunk. API test verifies the cap and that previews do not create saved jobs. The 2-source/256-cell flat synthetic core benchmark (3 fresh-process repetitions, Python 3.13.12, Windows 11) took 0.0847 s median (0.0842–0.0848), first chunk 0.0454 s median, and 98.23 MiB median peak working set. This is below the 5 s synthetic engineering target; it is not a live browser or real-raster measurement. Browser visual review remains open. |
| C11 | Implemented · `3446d91` | Draw polygon in the map and persist its WGS84 ring in plan settings; validate finite, non-self-intersecting polygons; mask projected sample centres without RF/terrain reads outside scope; report in-area/outside counts; persist `outside_area` cells; preserve the state in incomplete-grid reconstruction, GeoJSON/JSON/printable report, scenario comparisons, node-failure analysis and exact/area target assessments. Bump model identity to coverage-v2 and reject prior-version result reuse. Tests cover sample/evaluation masking, estimate and saved-job counts, corrupt/missing polygons, exports, comparisons, failures and target points outside the analysis area. Full 166-test suite, Ruff, mypy, JavaScript syntax and diff checks pass. Browser interaction and visual review remain open. |
| C12a | Implemented · `8720a29` | Window-batched raster reads; Rasterio equivalence and nodata/mosaic/edge/window tests pass. Same Kartverket 50 m DTM fixture as baseline: 4,096×8 cold median 12.3352 s (22.04× faster), first chunk 0.3436 s, peak 188.54 MiB; checksum unchanged. Preview 256×2 cold median 0.1885 s. Artifacts linked above. |
| C12b | Implemented · `5fa4b55` | Final-profile caps now produce unresolved results rather than hidden coarsening across grids, inspection, targets, height scenarios and backbone validation; model identity bumped to coverage-v3; partial exports/UI semantics updated. Full 187-test suite, Ruff, mypy, JS syntax and six Node inspection checks pass. Browser review remains open. |

Latest validation (2026-09-27): `.venv/Scripts/pytest.exe -q` passes all 166
tests; Ruff, mypy (`--ignore-missing-imports`), `node --check` and `git diff
--check` pass. C8 tests verify GeoJSON/JSON/HTML content, explicit unknown and
not-evaluated states, target escaping/identity, compatible scenario comparisons,
workspace isolation, stale/partial opt-in and size-limit cleanup. The native
Uvicorn process reached application startup. Docker’s Linux engine pipe remained
unavailable; the app exposed no browser surfaces, and Chrome headless exited on
GPU initialization before producing a screenshot. Therefore container-based
two-workspace/restart and visual-print review are still open. No real-raster
timing, concurrent-workspace RSS or field validation has been measured; these
remain release gates, not implied successes. This paragraph records the earlier
166-test release boundary; the subsequent C1/C4/C9/C12 continuation evidence
above supersedes its absence-of-raster, click-race and sample-cap findings.

The continuation audit's sample-cap finding is resolved by C12b. Remaining
release gates are human-visible browser review for the map, inspection, comparison,
target and export flows; deployment/restart and concurrent-workspace verification;
combined server/worker memory and cancellation checks under concurrent load; and
field validation against measured links. These remain open and must be completed
before claiming the full roadmap complete.

C12b validation (2026-09-27): `.venv/Scripts/pytest.exe -o addopts='' -q`
passes 187 tests (77 deprecation/pending-deprecation warnings); targeted C12b
coverage/job/export/scenario tests pass 41 tests; Ruff and mypy (`--ignore-missing-imports`)
pass; `node --check` passes for both coverage scripts, all six Node inspection
tests pass, and `git diff --check` passes. Browser visual attempt (2026-09-28):
the available computer-use inventory returned no apps or browser surfaces. A
temporary loopback Uvicorn server started, but opening the local page failed with
`Browser is not available: iab`; the server was then stopped. No screenshot was
available, so the unresolved-pattern legend, inspector messages, compare/target
views and print export have not been visually signed off. C12b did not alter the
sampler, so its C12a real-raster benchmark remains applicable; concurrent service
RSS, cancellation under concurrent load, and field accuracy remain unmeasured.

Deferred beyond this plan: live packet/RSSI ingestion and calibration, traffic or
airtime simulation, automatic optimisation for area coverage, mobile GPS tracking,
new propagation models, hosted public sharing and authentication beyond the
existing self-hosted workspace model.
