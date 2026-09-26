# Web usability and search performance implementation plan

Created: 2026-09-26. Status: planned; implementation has not started.

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
- Results are delivered at completion; the web API exposes the active alternative.
  The engine already produces alternatives and supports required router sites.
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

### P0 — Performance and quality contract [planned]

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

Acceptance:

- Baseline can be reproduced with documented commands and produces a structured
  report. Interrupted runs are not counted as completed-search timings.
- Route identity/metrics and search guarantees accompany every performance row.
- Set explicit performance targets for the selected real-terrain fixture before
  using it as a release gate. Synthetic targets below are initial engineering
  targets, to be reviewed against P0 evidence.

Primary areas: `tools/benchmark_rf_search.py`, `tools/profile_rf_search.py`, engine
diagnostics and benchmark fixtures. Commit: `test: establish usability and search performance baselines`.

### P1 — Early results, search presets and job snapshots [planned]

Implementation:

- Add a structured search callback that publishes immutable certified solution
  snapshots. First try the sparse graph and a small, geographically distributed
  candidate pool, then continue the configured broader search. Preserve the best
  feasible solution under the selected objective as search progresses.
- Keep the current thorough search available with its existing bounded-search
  guarantees. Add Quick/Balanced/Thorough effort presets separately from the
  objective. Persist resolved settings and display the actual search scope.
- Introduce job IDs, input revisions and explicit queued/running/complete/
  stopped/cancelled/failed states. Retain the existing cancellation action; add
  "Stop and keep result" when a certified snapshot exists.
- Extend the polling API first, with snapshot versions so unchanged profiles
  are not repeatedly serialized. Show the current route during improvement,
  elapsed time, phase counts and search-completeness status. Add ETA only when
  measurable remaining work supports it; otherwise show counts and elapsed time.
- Establish a configurable server job limit and queue; begin with one active
  optimization per server by default to bound concurrent resource use.

Acceptance:

- Initial synthetic targets: first certified route within 5 s at 800 candidates
  and 10 s at 2,000 on the baseline machine when the fixture is reachable.
  Investigate misses; do not meet targets by weakening final RF checks.
- Thorough final results remain equivalent to existing reference cases. Quick
  and Balanced reports explicitly identify omitted search and quality changes.
- Stop-and-keep retains a certified route and working profiles/exports. Cancel,
  no-result stop, worker failure, browser reconnect and obsolete-job updates have
  defined, tested behavior. Cancellation target: under 1 s between bounded tasks;
  measure and document any longer uninterruptible operation.
- Snapshots cannot mutate after publication or regress the chosen objective for
  an unchanged input revision. Required sites and redundancy remain visible.

Primary areas: `optimization/optimizer.py`, `models/network.py`, `web.py`,
`web_assets/app.js`, `index.html`, styles and web/engine tests.
Commit: `feat: show certified routes while search continues`.

### P2 — Bounded cache and selective recalculation [planned]

Implementation:

- Add a versioned link-evaluation cache containing compact RF metrics, including
  valid and invalid evaluated links. Start with a bounded in-process cache;
  add bounded on-disk reuse with explicit size limits, eviction and clear controls.
- Keys must cover both sites' geometry, elevations, height references, antenna
  settings/pattern contents, RF settings/model version, sample spacing and terrain
  fingerprints (DTM/DOM content revisions, CRS, transform and resolution).
  Preserve directional results; do not accidentally reuse reversed antenna data.
- Fingerprint terrain at ingestion and validate file changes. Cache only finished
  evaluations. Do not persist cancellation or missing-file errors as RF failures.
- Reuse candidate generation when its dependencies are unchanged. Objective-only
  edits reuse eligible RF metrics; position/height edits invalidate affected links
  and any dependent candidate generation. New candidate pairs still need checking.
- Keep display profiles in a separate small bounded cache or regenerate them.
  Isolate private workspace data and coordinate concurrent requests for a cache
  entry so the same computation is not launched twice.

Acceptance:

- Identical warm reruns and objective-only changes perform no repeat terrain/RF
  evaluations for already cached search links; displayed-profile regeneration
  and newly discovered links are counted separately.
- Initial target: identical warm rerun at least 3x faster than cold at 800 and
  2,000 candidates. Topology-only costs must be reported if they limit the gain.
- Differential tests match cache-disabled routes and metrics. Test changes in
  terrain, antenna patterns, frequency, height, sample spacing and engine version.
- Cache limits remain enforced during long sessions. Eviction, corruption and
  restart yield safe misses; cancelled jobs cannot poison entries.

Primary areas: new optimization/cache module, propagation/sampling boundaries,
terrain metadata, web workspace state and targeted cache tests.
Commit: `perf: reuse bounded RF evaluations across planning edits`.

### P3 — Existing infrastructure as a planning policy [planned]

Implementation:

- Add three candidate policies: proposed sites only; selected existing routers
  only; and selected existing routers plus proposed sites. Existing-only bypasses
  terrain candidate generation and clearly reports disconnected existing networks.
- Add per-router Optional / Required / Excluded controls and filters for feed
  freshness/activity. Treat feed activity as context, not proof of a usable RF
  link. Show the antenna/RF assumptions used when feed metadata is incomplete.
- Add a distinct "Fewest new installations" objective. Initial deterministic
  ordering: new installation count, total intermediate router count, then the
  established minimum-router RF tie-breakers. Retain current objectives unchanged.
  Any later monetary-cost mode needs separate explicit project cost inputs.
- Update graph ranking and optimizer dispatch as well as topology scoring.
  A longer existing-only path may beat a short path needing a new installation;
  the minimum-total-router low-hop shortcut must not decide this new objective.
- Count existing and proposed routers separately in results. The total-router
  hard limit still applies to both; optionally allow a separate cap on new sites.
  Explain this limit before starting a search.
- Save router selection, policy, required/excluded state and provenance in plans;
  handle stale/removed feed entries without silently deleting saved sites.

Acceptance:

- Fixtures prove existing-only never invents a site, required routers always
  appear, excluded routers never appear, and a route with fewer new installations
  wins according to the documented objective even when it has more total hops.
- Mixed-mode candidate limits, no feasible route, missing metadata and imported
  older plans behave predictably. Compare existing-only and mixed-mode RF counts.
- First-release end-to-end flow works: select endpoints and terrain, choose
  infrastructure, receive an early certified route, edit the objective, reuse
  calculations, stop-and-keep, and export the displayed result.

Primary areas: `models/site.py`, `models/settings.py`, optimization graph/topology/
dispatch,
web request/schema/UI and CoreScope integration tests.
Commit: `feat: plan routes around existing MeshCore infrastructure`.

### P4 — Terrain preparation and actionable coverage checks [planned]

Implementation:

- Expose the existing Kartverket planning/download/cache integration to the web.
  Estimate required tiles, disk space and resolution for the corridor plus all
  selected/required sites. Offer one clear preparation action and reuse cached tiles.
- Show ground/surface coverage outlines and nodata gaps. Validate endpoint/site
  coverage before optimization, including non-contiguous raster coverage.
- Put preparation in the job workflow with download progress, cancellation,
  bounded retries and atomic completion. Keep manual GeoTIFF uploads supported.
- Distinguish missing ground terrain from optional absent surface data. Changing
  terrain creates a new revision and invalidates affected results/cache entries.
- Surface concise explanations for no-route cases: coverage gaps, rejected
  candidate links, RF margin deficits or search-budget exhaustion. Do not claim
  these are exhaustive diagnoses unless the search supports that conclusion.

Acceptance:

- Cached preparation works offline. Partial downloads never appear as complete
  terrain. Provider errors leave projects usable and downloaded valid tiles intact.
- API tests use fixtures; separately record a live-provider smoke test when the
  service is available. Real DEM benchmarks include cold and warm I/O costs.
- The UI identifies uncovered sites before an expensive search begins.

Primary areas: `terrain/kartverket.py`, raster metadata, web background jobs,
map coverage layers and mocked provider tests.
Commit: `feat: prepare and validate terrain from the web planner`.

### P5 — Alternative comparison and map editing [planned]

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

Acceptance:

- Switching alternatives performs no unnecessary RF search. Edits trigger only
  the required recomputation, measured with P0/P2 counters.
- Selected-alternative exports match the map and table. Undo restores settings
  and a valid corresponding snapshot or correctly triggers revalidation.
- Keyboard operation, readable narrow-screen layout and comprehensible loading/
  stale/error states receive a browser walkthrough as well as API regression tests.

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

- Planning baseline: all P0–P7 milestones are planned. No feature code changed.
- First-release gate: P0–P3 accepted; profiles/exports, legacy plans and cancellation
  work throughout the early-result/cache/existing-router flow.
- Second-release gate: P4–P7 accepted; terrain preparation, editing, recovery and
  resource limits work together on a representative self-hosted deployment.

Out of scope for these releases: field calibration or a new propagation model,
global optimality over arbitrary terrain, mobile-native clients, multi-host job
distribution, multi-user account management and automated deployment to a server.
