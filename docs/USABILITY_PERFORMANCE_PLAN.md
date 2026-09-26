# Web usability and search performance implementation plan

Created: 2026-09-26. Status: P0–P4 implemented; P5–P7 remain planned.

This is the next development phase after the completed
[RF engine plan](RF_ENGINE_PLAN.md). Its milestones are independent of the
completed engine and memory-remediation work. Implement in the order below,
update this document with evidence, and commit each completed milestone.

## Intended outcome

Make ordinary planning a short loop: select endpoints, prepare terrain, choose
existing infrastructure, see a valid route, then adjust and compare it. Expensive
searches should provide useful intermediate results and reuse prior calculations.

First release: early certified results, calculation reuse, and planning that
minimizes new installations. Second release: automatic terrain preparation,
interactive comparison/editing, durable projects, and further measured speedups.

## Baseline and constraints

- Starting implementation: `e5b39b7`; 78 tests pass, Ruff and mypy pass.
- Web planning supports two endpoints, terrain uploads, optional CoreScope
  repeaters, objective/settings editing, cancellation, and plan/CSV/GeoJSON export.
- The web API exposes the active alternative. P1 adds certified route snapshots
  while a minimum-router search continues, plus final selected alternatives.
- Browser workspaces persist uploaded terrain and submitted inputs. Results and
  active jobs are in memory; the web service currently runs in one process.
- Desktop terrain retrieval/cache facilities exist in `terrain/kartverket.py`.
  Desktop behavior and old plan formats must continue to work.
- Single fresh-process, instrumented 100 km synthetic measurements: 800 candidates
  took 13.014 s / 94.0 MiB peak working set; 2,000 took 62.557 s / 135.2 MiB.
  At 2,000 candidates about 56 s was spent evaluating 214,080 frontier pairs.
  These measurements use optional seeded chain sites, flat terrain, and one
  worker. They are not realistic-terrain or statistical performance guarantees.
- Search-profile retention and duplicate fallback work are fixed. Scalar edge
  storage and exhaustive pair evaluation can still grow quadratically.

## Rules that apply to every milestone

1. Every displayed usable route must pass the existing final-resolution RF and
   topology checks. Preview may mean incomplete search; it must never mean
   uncertified links presented as valid.
2. Keep search effort, optimization objective, and RF validity separate. Quick
   search may miss alternatives; it must say so. A time limit without a route
   means "No route found within this search budget," not proof of infeasibility.
3. Every result belongs to an immutable input revision, terrain revision and job
   ID. Late job updates cannot replace a newer plan's result.
4. Preserve bidirectional budgets, deterministic tie-breaking, candidate limits,
   requested redundancy, final profile availability, and workspace isolation.
5. Bound cache disk/RAM use and worker concurrency. Never recreate the earlier
   unbounded retention of profile arrays through caching or result streaming.
6. Keep defaults compatible when new saved fields are absent. Add schema
   migrations and regression fixtures before changing persisted formats.
7. Do not promise a speedup before measurement. Record cold and warm runs,
   completed versus interrupted searches, and route quality alongside time.

## Delivery order and milestone commits

| Milestone | Deliverable | Depends on | Release |
| --- | --- | --- | --- |
| P0 | Repeatable performance and quality measurements | Current engine | Foundation |
| P1 | Early certified routes and useful progress | P0 | First |
| P2 | Bounded RF cache and selective recalculation | P0, P1 revision model | First |
| P3 | Existing-router policies and minimum new installations | P1, P2 | First |
| P4 | Automatic terrain preparation and coverage checks | P1, P2 invalidation | Second |
| P5 | Compare alternatives and edit routes on the map | P1–P3 | Second |
| P6 | Named projects, autosave and restart recovery | P1 revision model, P4–P5 schema | Second |
| P7 | Faster exhaustive evaluation with bounded resources | P0–P3; P4 real-terrain data | Second |

### P0 — Performance and quality contract [complete]

Implementation:

- Extend the existing benchmark tools to report time to first certified route,
  total time, peak working set, RF evaluation count, terrain I/O, cache hits and
  misses, topology states and worker count. Count operations separately from
  user-facing progress labels so phase changes cannot misattribute timing.
- Preserve deterministic direct, optional-relay and required-chain scenarios.
  Include blocked/no-route, cancellation, and small exhaustive reference graphs.
- Add a reproducible real-terrain fixture manifest: source/license, checksums,
  bounds, CRS and resolution. Do not commit large raster downloads. Keep public
  CI independent of live CoreScope/Kartverket availability.
- Record at least three fresh-process runs per timed comparison on the same
  machine; use medians and ranges. Separate instrumented from ordinary timings.
- A real-terrain fixture is not checked into the repository. Use the
  [fixture manifest](REAL_TERRAIN_FIXTURE.md) when a redistribution-approved
  raster is selected; synthetic baselines must remain labeled synthetic.

Completion evidence (Windows 11, Python 3.13.12, 24 logical CPUs; three fresh
single-worker processes per size; optional-anchor 100 km flat 25 m terrain):

| Candidates | Median time (range) | Peak working set (range) | RF evals, median | Sampled terrain points, median | Route |
| ---: | ---: | ---: | ---: | ---: | --- |
| 200 | 0.490 s (0.485–0.490) | 76.4 MiB (76.4–76.5) | 400 | 1,235,438 | Same four-anchor route, 4 routers |
| 800 | 2.110 s (2.099–2.124) | 96.2 MiB (96.1–96.5) | 1,536 | 4,831,757 | Same four-anchor route, 4 routers |

Cache telemetry is 0 hits and one miss per RF evaluation because caching has not
been implemented yet. These are successful anchored chain cases, not open-ended
subset search, and use in-memory terrain with zero raster I/O. Time to first
certified route is explicitly unavailable until P1 publishes solution snapshots.
The real-terrain manifest format is ready; a licensed fixture has not yet been
selected. The summary-only reproduction command is:

```console
python tools/benchmark_rf_search.py --topology long --distance-km 100 --sizes 200 800 --repetitions 3 --summary-only
```

Commit: `test: establish usability and search performance baselines`.

Acceptance:

- Baseline can be reproduced with documented commands and produces a structured
  report. Interrupted runs are not counted as completed-search timings.
- Route identity/metrics and search guarantees accompany every performance row.
- Set explicit performance targets for the selected real-terrain fixture before
  using it as a release gate. Synthetic targets below are initial engineering
  targets, to be reviewed against P0 evidence.

Primary areas: `tools/benchmark_rf_search.py`, `tools/profile_rf_search.py`, engine
diagnostics and benchmark fixtures. Commit: `test: establish usability and search performance baselines`.

### P1 — Early results, search presets and job snapshots [complete]

Implementation:

- Add a structured callback that publishes detached, certified solution
  snapshots. For the normal two-endpoint minimum-router objective, check a direct
  or low-hop route before broader topology search; then continue the configured
  search and replace the preview with the completed selected solution. Final RF
  validation and profile availability are unchanged.
- Keep the current thorough search available with its existing bounded-search
  guarantees. Add Quick/Balanced/Thorough effort presets separately from the
  objective. Persist resolved settings and display the actual search scope.
- Introduce job IDs, input revisions and explicit queued/running/complete/
  stopped/cancelled/failed states. Retain the existing cancellation action; add
  "Stop and keep result" when a certified snapshot exists.
- Extend polling with job/input revisions and snapshot versions so unchanged
  profiles are not repeatedly serialized. Show the current route, elapsed time,
  phase counts, resolved search limits and completeness/redundancy. Do not show
  an ETA until remaining work can be measured reliably.
- Establish a configurable server job limit and queue; begin with one active
  optimization per server by default to bound concurrent resource use.

Acceptance:

- At 800 sites on the baseline machine, certified-route smoke timings were
  1.25 s (4.21 s total) for a direct scenario and 2.49 s (2.49 s total) for the
  100 km required-chain scenario. The latter reaches its callback at completion;
  it confirms the target but not an early-preview gain for required-chain mode.
  RF checks were not weakened. These are one-run instrumented checks, not P0
  repeated-run statistics.
- Quick/Balanced resolve to at most 200/8 and 800/12 candidate/neighbor limits;
  Thorough uses configured limits. The UI warns that narrower searches can miss
  better routes, and the submitted plan records actual caps and selected
  existing-router count.
- Tests cover final-resolution snapshots, detached-snapshot mutation, API scope,
  queued jobs, stop-and-keep with profile/export, normal cancellation clearing
  the preview, and full-search route/export behavior. Full suite: 83 passed;
  Ruff, mypy and JavaScript syntax checks pass.
- Default queue is one active plus one queued job, configurable through
  `RF_PLANNER_MAX_ACTIVE_JOBS`. Job IDs/revisions reject stale controls/results;
  terrain updates invalidate snapshots. Browser reconnect re-fetches a retained
  snapshot by version.

Scope note: early snapshots currently cover the two-endpoint minimum-router
low-hop path; other priorities and required-router/network cases publish once an
exact alternative is available. Those cases do not yet stream every topology
improvement. The 100 km required-chain smoke measured first result at completion,
so incremental-result speed there remains a follow-up rather than a claimed win.

Primary areas: `optimization/optimizer.py`, `models/network.py`, `web.py`,
`web_assets/app.js`, `index.html`, styles and web/engine tests.
Commit: `feat: show certified routes while search continues`.

### P2 — Bounded cache and selective recalculation [complete: in-process phase]

Implementation:

- Add a versioned in-process LRU containing compact metrics for valid and invalid
  RF evaluations. Capacity defaults to 50,000 entries, is configurable with
  `RF_PLANNER_RF_CACHE_ENTRIES`, and evicts least-recently-used entries globally
  within the app process. Namespaces isolate hits and clear operations by browser
  workspace. Expose hit/miss/eviction counts and a workspace-scoped clear control.
- Keys must cover both sites' geometry, elevations, height references, antenna
  settings/pattern contents, RF settings/model version, sample spacing and terrain
  fingerprints (DTM/DOM content revisions, CRS, transform and resolution).
  Preserve directional results; do not accidentally reuse reversed antenna data.
- Include terrain file path/stat/geospatial metadata (or array content digest),
  canonical RF settings, antenna-pattern content digests, both ordered site
  fingerprints, sample spacing and cache schema in each key. Direction is
  preserved; setting, site and terrain changes safely miss.
- Cache only finished metrics. Process-pool evaluations are inserted in the
  parent cache, allowing warm runs to skip them while preserving parallel cold
  runs. Failed terrain reads are not cached. Display profiles and obstacle lists
  are omitted from entries and regenerated for selected routes.
- Objective-only edits reuse RF metrics; changed or newly generated sites miss by
  geometry. Cache access coalesces identical in-process computations, and the
  bounded LRU and explicit per-workspace clear limit RAM lifetime.

Scope decision: P2 ships the bounded in-process cache, not persistent on-disk
reuse. A disk cache would outlive app versions and terrain changes and needs a
separate migration, privacy, integrity and deletion policy. Candidate-generation
reuse is also deferred; it is a different data product and should be measured
independently rather than hidden in the RF-cache result.

Acceptance:

- An identical warm rerun and an objective-only web rerun reused cached RF
  metrics; the warm search regenerated only selected-route profiles (16 RF
  profile evaluations and 19,216 sampled terrain points in the 2,000-site run).
- Three-process, 100 km flat in-memory chain benchmark (cold → warm in the same
  process; medians across repetitions):

  | Candidates | Cold median (range) | Warm median (range) | Speedup median (range) | Peak RSS median (range) |
  | ---: | ---: | ---: | ---: | ---: |
  | 800 | 2.394 s (2.346–2.402) | 0.63 s (0.62–0.63) | 3.80× (3.77–3.80×) | 119.6 MiB (119.4–119.7) |
  | 2,000 | 6.935 s (6.872–7.067) | 2.57 s (2.54–2.57) | 2.71× (2.69–2.75×) | 190.6 MiB (190.1–191.2) |

  The 2,000-site target of 3× was missed. Warm runs still spend about 2.5 s in
  non-RF search/topology work; only 16 profile evaluations were repeated, so the
  residual is topology/screening dominated and is carried to P7. These are
  synthetic, instrumented measurements, not real-terrain release targets.
  P0's 800/2,000 peak RSS baselines were 96.2/135.2 MiB; the bounded cache adds
  about 23/55 MiB respectively, remaining far below the earlier 1.25 GiB issue.
- Differential tests cover terrain, frequency/settings, antenna-pattern content,
  site height, direction and sample spacing; invalid metrics are reusable, while
  exceptions are not. Raster process and sequential results remain equivalent.
  Full suite: 90 passed; Ruff, mypy and JavaScript syntax checks pass.
- Cache entries are bounded and explicitly clearable; workspace namespaces do
  not share hits. Restart intentionally clears all entries (no disk cache).
- Differential tests match cache-disabled routes and metrics. Test changes in
  terrain, antenna patterns, frequency, height, sample spacing and engine version.
- Cache limits remain enforced during long sessions. Eviction, corruption and
  restart yield safe misses; cancelled jobs cannot poison entries.

Primary areas: new optimization/cache module, propagation/sampling boundaries,
terrain metadata, web workspace state and targeted cache tests.
Commit: `perf: reuse bounded RF evaluations across planning edits`.

### P3 — Existing infrastructure as a planning policy [implemented]

Implementation:

- Add three candidate policies: proposed sites only; selected existing routers
  only; and selected existing routers plus proposed sites. Existing-only passes
  only endpoints and selected existing routers to the optimizer; it never calls
  terrain candidate generation. Disconnected networks return the ordinary
  no-route diagnostic.
- Add per-router Optional / Required / Excluded controls and list filters for
  active relays and routers heard within seven days. Refresh preserves previously
  saved sites missing from the current corridor response and marks them stale;
  filtering does not silently discard a saved choice. Feed activity is context,
  not proof of a usable RF link. The UI explains that missing antenna metadata
  uses the configured repeater antenna assumptions.
- Add a distinct "Fewest new installations" objective. Initial deterministic
  ordering: new installation count, total intermediate router count, then the
  established minimum-router RF tie-breakers. Retain current objectives unchanged.
  Any later monetary-cost mode needs separate explicit project cost inputs.
- Update graph ranking and optimizer dispatch as well as topology scoring.
  A longer existing-only path may beat a short path needing a new installation;
  the minimum-total-router low-hop shortcut must not decide this new objective.
- Count existing and proposed routers separately in results. The existing
  `maximum_solution_routers` hard limit applies to the combined topology and is
  shown before search. No separate new-site cap was added in this milestone.
- Save router selection, policy, required/excluded state and provenance in plans;
  handle stale/removed feed entries without silently deleting saved sites.

Acceptance:

- API fixture patches candidate generation to fail if existing-only invokes it;
  the search completes with only its required existing router. Another fixture
  confirms excluded routers do not enter the graph. A graph/topology fixture
  confirms a longer, all-existing path beats a shorter path needing installation.
- A paired cold-cache fixture confirms existing-only makes fewer RF metric
  misses than mixed mode. The full suite covers legacy optional-router payloads,
  version-one projects defaulting to mixed mode, and configured router limits.
- Full suite: 95 passed; Ruff, mypy and JavaScript syntax checks pass. P1 already
  covers early snapshots, stop-and-keep and result export; existing-router plans
  use the same versioned job/result/cache flow. Browser visual walkthrough and
  real CoreScope freshness behavior remain release checks, not claimed here.

Primary areas: `models/site.py`, `models/settings.py`, optimization graph/topology/
dispatch, web request/schema/UI and CoreScope integration tests.
Commit: `feat: plan routes around existing MeshCore infrastructure`.

### P4 — Terrain preparation and actionable coverage checks [implemented]

Implementation:

- Add `/api/terrain/estimate` and a one-click follow-up preparation action. Plans
  use the proper Norway UTM service, an A–B buffered corridor plus selected-site
  buffers, automatic resolution/pixel budgets and per-workspace content-addressed
  cache. The estimate reports resolution, area, tile/pixel counts, cache hits,
  approximate uncompressed cache-plus-workspace storage and available disk.
- Add `/api/terrain/coverage`: draw DTM/DOM tile outlines, mark selected endpoints
  or routers with missing ground data, and sample the direct corridor for nodata
  gaps. Optimization runs the same selected-site preflight synchronously before
  it queues work; missing site coverage produces an actionable 422 response.
- Run downloads as cancellable scheduler jobs with per-tile progress, two bounded
  retries, partial-file cleanup and CRS/data/bounds validation. A complete DTM
  plus optional DOM generation is staged and atomically published as one folder;
  incomplete generations never appear in terrain listings. Manual GeoTIFF upload
  and separate DTM/DOM clearing remain supported.
- Terrain changes increment the input revision and clear matching results.
  Missing DOM remains optional and is labeled as terrain-only RF analysis.
- Surface concise explanations for no-route cases: coverage gaps, rejected
  candidate links, RF margin deficits or search-budget exhaustion. Do not claim
  these are exhaustive diagnoses unless the search supports that conclusion.

Acceptance:

- A valid cached tile can be reopened without network; malformed responses retry
  only three times and leave no `.part`/cache tile. A two-product API test pauses
  between DTM and DOM and confirms neither is visible until the full generation
  is validated and published.
- Fixtures cover estimator budgets, tile outlines, interior nodata, outside-coverage
  sites, early preflight rejection, and generated terrain consumed by the RF path.
  Full suite: 104 passed; Ruff, mypy, JavaScript syntax and diff checks pass.
- No live Kartverket request or real DEM benchmark was run here; WCS download
  behavior is mocked in tests. Production WCS compatibility and real-raster I/O
  remain deployment smoke checks. Nodata map warnings sample the straight A–B
  line at no more than 2,001 points (spacing at least 100 m or four raster cells),
  not a complete pixel-level nodata heatmap.

Primary areas: `terrain/kartverket.py`, raster metadata, web background jobs,
map coverage layers and mocked provider tests.
Commit: `1ecd0f3 feat: prepare and validate terrain from the web planner`.

### P5 — Alternative comparison and map editing [implemented]

Implementation:

- Expose stable alternative IDs and summary metrics in the API. Compare new/
  existing router counts, worst link margin, achieved/requested redundancy,
  total link distance and search completeness. Select one active alternative.
- Allow adding a proposed router, changing its antenna height, requiring or
  excluding a site, and dragging a proposed site. Existing physical sites stay
  fixed unless the user explicitly corrects their saved coordinates.
- Add undo/redo and explicit stale-result marking. Feed revisions through P2
  selective recalculation; never display old green links as current validation.
- Keep map, table, profile, selected alternative and exports synchronized.
  Load larger profiles on demand if payload measurements justify it.

Implementation notes:

- `/api/result` now carries topology-derived alternative IDs and concise metrics
  (existing/proposed counts, weakest link margin, total network-link and
  primary-path distance, achieved/requested independent paths). `/api/alternatives/{id}/select` switches
  the active `NetworkSolution` without starting a search or changing the input
  revision; CSV/GeoJSON exports read that selected solution.
- Plan files accept additive `manual_routers` entries. These can be placed,
  moved, assigned optional/required/excluded policy, and given a per-site antenna
  height in the web UI. Dragging a generated proposed route site creates a
  required manual site; MeshCore physical sites are not draggable. Existing
  P2 RF-link cache reuse limits recalculation to changed link inputs.
- A 50-step plan undo/redo stack restores settings and site edits. Existing
  certified results remain visible but are muted and explicitly marked stale;
  stale exports and alternative switching are disabled until a new search
  certifies the current inputs.

Acceptance:

- Switching alternatives leaves RF cache misses and the input revision unchanged;
  the API regression test verifies that a different topology becomes active.
- Manual-router integration verifies required-site inclusion and per-site antenna
  height preservation. Selected-route serialization drives the map/table and
  exports; Undo/Redo restore plan state and explicitly require revalidation.
- API regression suite, Ruff, mypy and JavaScript syntax checks pass. A visual
  browser walkthrough remains outstanding because no browser provider/window was
  available in this environment; narrow-screen and keyboard checks are not claimed.

Primary areas: web alternative/edit APIs, map UI, plan revisions and export tests.
Commit: `feat: compare and edit route alternatives interactively`.

### P6 — Named projects, autosave and restart recovery [planned]

Implementation:

- Persist named projects, input revisions, terrain references and compact certified
  result summaries transactionally, using a versioned local store such as SQLite.
  Debounce autosave; retain a recoverable previous revision and explicit exports.
- Persist the selected alternative and its input/terrain fingerprints. Restore
  profiles lazily from matching terrain; mark results stale if dependencies differ.
- On restart, mark interrupted jobs accurately and offer rerun. Reopening a saved
  project must not require finding and uploading unchanged terrain again.
- Provide duplicate/rename/archive, storage usage and explicit deletion controls.
  Shared terrain references must not be deleted while another project uses them.
- Preserve the current access-token/workspace isolation model. This milestone
  does not add multi-user accounts or a distributed job system.

Acceptance:

- Crash/restart and failed-write tests recover the last committed revision;
  autosave cannot associate a result with the wrong inputs.
- Existing `.webplan.json` files import with documented defaults. Export/import
  round trips preserve new fields. Missing terrain produces a repairable state.
- Multiple browser workspaces cannot enumerate or overwrite each other's projects.

Primary areas: new project store, web workspace/project APIs, schema migrations,
project picker/autosave UI and persistence tests.
Commit: `feat: persist projects and recover planning sessions`.

### P7 — Reduce exhaustive-search CPU and terrain I/O [planned]

Implementation:

- Use P0 measurements to identify repeated RF arithmetic, raster reads and
  scheduling overhead. First implement admissible pair rejection/bounds and
  reuse of unchanged calculations; verify pruning against exhaustive small cases.
- Batch terrain-window reads with bounded reuse for real rasters. Cap samples,
  bytes and outstanding work explicitly rather than materializing all pairs.
- Extend bounded process-worker evaluation to expensive frontier scans. Workers
  open their own read-only raster handles and return compact metrics. Preserve
  deterministic aggregation and stop queued work promptly on cancellation.
- Coordinate worker and job limits across browser workspaces to avoid multiplying
  CPU/RAM demand. Keep a single-worker mode for modest hosts and reproducibility.
- Consider spatial indexing for neighbor selection only after comparing its
  measured contribution with RF costs. Preserve endpoint completeness and tie
  ordering; do not let nearest-neighbor filtering silently weaken thorough search.

Acceptance:

- Initial target: at least 2x faster cold 2,000-candidate thorough search on the
  reference multicore host, equivalent route/metrics, and at most 256 MiB peak
  combined parent/worker working set for the synthetic fixture. Record measured
  host limits and revise targets explicitly if evidence warrants it.
- Include 1/2/4-worker and concurrent-workspace measurements, real DEM I/O, no-route
  and cancellation cases. Measure aggregate worker memory, not just the parent.
- Lossless optimizations match exact reference results; any optional heuristic
  belongs to an explicitly labeled effort mode and gets separate quality reports.

Primary areas: `optimization/optimizer.py`, `parallel.py`, propagation/sampling,
raster access and benchmark/reference tests.
Commit: `perf: accelerate bounded RF evaluation and terrain access`.

## Release verification and progress log

For each milestone, record the commit, completed acceptance checks, changed
defaults/schema, benchmark deltas and remaining limitations here. Use targeted
tests during development, then the full suite, Ruff, mypy and relevant browser
walkthrough before each release. Rebuild the web container when dependencies or
packaging change. Update README feature coverage and user instructions with each
release. Keep every milestone independently reviewable and the working
tree clean after its commit.

- Planning baseline: P0–P5 are committed; P6–P7 remain planned. P3 adds the
  `infrastructure_policy` setting and router
  `policy`/`provenance` plan fields; missing fields in older plans retain their
  previous behavior (mixed mode, selected known routers optional).
- First-release gate: P0–P3 accepted; profiles/exports, legacy plans and cancellation
  work throughout the early-result/cache/existing-router flow.
- Second-release gate: P4–P7 accepted; terrain preparation, editing, recovery and
  resource limits work together on a representative self-hosted deployment.

Out of scope for these releases: field calibration or a new propagation model,
global optimality over arbitrary terrain, mobile-native clients, multi-host job
distribution, multi-user account management and automated deployment to a server.
