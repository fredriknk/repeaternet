# RF pathfinding improvement plan

Next phase: [Web usability and search performance implementation plan](USABILITY_PERFORMANCE_PLAN.md).
The milestones below describe the completed engine work; the linked plan tracks
the upcoming feature releases.

Owner: current implementation session. Started: 2026-09-26.

## Goal and completion criteria

Improve numerical stability, route completeness within an explicitly bounded
search, final-result correctness, objective fidelity and cancellation. Deliver
tested changes in separate milestone commits. Preserve the desktop and web APIs,
project compatibility and the existing network-alternative workflow. This is a
bounded engineering release, not a claim of field-calibrated RF accuracy or global
optimality over continuous terrain.

## Baseline

- `31dd3c0` snapshots the pre-existing desktop/network work and completed web app.
- Baseline suite: 46 passed, 1 failed (`test_selected_mesh_finally_checks_all_node_pairs`).
- On flat terrain at 869.5 MHz, 700 m separation and 3 m antennas, recursive
  Deygout reports about 77 dB loss at 10 m samples, despite clear geometric LOS.
  Repeated samples on smooth shoulders are treated as new edges.
- Two-repeater exhaustive search currently runs only after the sparse mesh fails.
  A successful longer mesh can therefore suppress a better summit connection.
- Mesh validation has a fixed round limit, then solves again with mixed coarse
  and final edges; cancellation is absent inside expensive topology enumeration.
- All exact-count topologies use the same reliability-first score, irrespective
  of selected objective. Infrastructure priority is effectively router count.
- Legacy route/refinement code after an unconditional return is unreachable.

## Milestones

### M1 — Stable diffraction model and numerical contracts [complete]

- Implement the Bullington component of ITU-R P.526-16 §4.5.1 using the existing
  curvature-adjusted metric profile, with explicit finite/order/shape validation.
- Make it the new default; retain selectable legacy Deygout for comparison and
  reproducibility, and identify the model in saved settings.
- Test clear paths, analytical knife-edge geometry, reversal symmetry, sampling
  stability, invalid inputs, and legacy project round trips.
- Boundary: this is not the complete delta-Bullington/spherical-Earth method in
  §4.5.2; do not label it as a full P.526 implementation.
- Commit after RF and integration regression tests pass.

### M2 — Complete low-hop search and final-only results [complete]

- Run the existing exact 0/1/2-repeater search for eligible two-client minimum
  router plans even when a sparse longer route exists; merge discovered edges
  into the mesh so alternatives remain available.
- Final-resolution decisions are authoritative; avoid losing links solely to
  inconsistent coarse/medium classification.
- Replace fixed-round validation termination with monotonic edge certification;
  never return an alternative containing an uncertified edge.
- Test a hidden long summit link, false-positive coarse edges and cancellation
  during validation. Guarantees apply to the generated candidate set and selected
  edge pool, not all possible sites or arbitrary larger meshes.

### M3 — Correct route and topology objectives [complete]

- Fix shortest-hop tie-breaking when a later bottleneck erases an earlier lead.
- Make topology ranking honor minimum repeaters, installation/mast cost and
  independent-path reliability as distinct objectives.
- Preserve stable site-ID tie-breaking and exact-count alternative ordering.
- Test adversarial converging paths, cheap mast alternatives, reliability and
  input-order invariance with exhaustive small-graph references.

### M4 — Responsive and bounded topology search [complete]

- Thread cancellation through exact enumeration, beam expansion and validation.
- Prune disconnected candidate components before combinatorial work.
- Report exact versus heuristic search in diagnostics; retain deterministic
  heuristic bounds and expose no unsupported global-optimum claims.
- Remove unreachable legacy flow only after active behavior has regression cover.
- Test pre-cancellation, cancellation during search and irrelevant-component pruning.

### M5 — Release verification and documentation [complete]

- Run all tests, targeted lint/type checks, packaging and source-diff review.
- Record counts, limitations and commit milestones here; update README model and
  search descriptions. Add repeatable synthetic regression scenarios, without
  machine-dependent timing thresholds.
- Finish with a clean working tree and one commit per completed milestone.

## Deferred work

Full delta-Bullington/Longley–Rice, climate variability and measured calibration;
sub-cell conservative raster traversal; access/power/ownership costs; globally
optimal arbitrary-size Steiner mesh search; persistent cross-run RF caches;
spatial-index screening and raster-window batching; automatic mesh-wide local
refinement/mast reduction. These need their own acceptance data or larger design
changes and are not prerequisites for this release.

## Validation log

- Baseline verified during web release: 46 passed, 1 mesh failure.
- Plan committed before engine implementation.
- M1: Bullington component implemented and made default for new settings;
  old desktop project files without a model explicitly retain legacy Deygout.
  Desktop selector and API settings support both. Full suite: 54 passed before
  adding the explicit legacy/new serialization regression; targeted rerun below.
  The original mesh failure is resolved by the stable RF model.
- M1 commit: `68103d6`. Legacy/new serialization plus diffraction tests: 10 passed.
- M2: final-resolution certification replaces unsafe coarse rejection; coarse
  failures are retried at final resolution. Validation now advances monotonically
  over previously uncertified pairs with cancellation checks. Low-hop discovery
  runs even when the sparse graph connects. Antenna screening uses pattern peak
  gain so a configured directional pattern cannot be underestimated.
  Full suite before new scenarios: 55 passed. Eight screening tests pass,
  including hidden two-router vs three-router, 29 rejected alternatives, coarse
  rejection recovery and cancellation during validation.
- M2 commit: `f82b4db`.
- M3: shortest-hop routing solves margin and Fresnel thresholds before additive
  tie-breakers, avoiding invalid prefix dominance. Widest routing uses a maximum
  spanning-tree bottleneck followed by constrained shortest-hop optimization.
  Topology objectives now distinguish infrastructure cost (100 installation units
  plus mast metres), minimum count and reliability. Site ordering is deterministic.
  Twelve graph/topology tests pass, including 40 seeded exhaustive graph oracles,
  later-bottleneck regression, mast-cost tradeoff and reordered inputs.
- M3 commit: `bd4d11d`. Combined graph, topology and screening tests: 20 passed.
- M4: cancellation now reaches exact subset enumeration and heuristic beam
  expansion; partial alternatives are discarded on cancellation. Disconnected
  candidate islands are pruned before enumeration. Diagnostics label exhaustive
  subset search versus bounded beam search. Removed 150+ lines of unreachable
  route flow while retaining local-edit helpers used by the desktop UI.
  Twenty topology/screening tests pass, including cancellation inside both search
  modes and a 40-site irrelevant island reduced to two subset evaluations.
- M4 commit: `5c7b279`.
- M5: full suite **68 passed**, up from baseline 46 passed / 1 failed. Ruff passes
  across `src` and `tests`; mypy passes across all 40 source files. Wheel builds
  successfully (`rf_router_planner-0.1.0-py3-none-any.whl`). Source whitespace
  validation passes. Dependency deprecation warnings and headless Qt GPU messages
  remain non-failing. Default-height browser upload/optimize/export integration
  passes with the new model; directional-pattern peak-gain regression passes.
- Numerical reproduction: flat 700 m path, 3 m antennas, 869.5 MHz, k=4/3.

  | Profile samples | Legacy Deygout loss (dB) | Bullington component loss (dB) |
  | --- | ---: | ---: |
  | 15 | 41.147 | 4.008 |
  | 71 | 77.331 | 4.008 |
  | 701 | 83.574 | 4.008 |

  This checks sampling stability, not agreement with field measurements.
- Final milestone commit includes verification fixes, consistent formatting,
  README changes and this completed plan. Use `git log --oneline` to locate
  `test: verify and document RF engine improvement milestones` (its own hash
  cannot be embedded in the committed document without changing that hash).

## Release boundaries and operational notes

All five implementation milestones are complete. Restart a running desktop/web
process to load the changed engine. Saved desktop plans missing a model field
intentionally retain Deygout; choose Bullington to compare recalculated predictions.

Final certification guarantees validity at the configured final sample spacing,
not raster sub-cell obstacle coverage. Minimum 0/1/2-hop discovery applies only to
two-client minimum-router plans with no required intermediate sites. Wider search
still uses the screened graph; final reevaluation of selected edges does not prove
that an unselected coarse edge could not rank better. Beam search is heuristic.
Cancellation is cooperative between operations, not an interruption of an active
NumPy, GDAL or graph-flow call. No full delta-Bullington, field-calibration or
automatic mesh-wide refinement claims are made by this release.

## Post-implementation review — 2026-09-26

### Assessment

The implementation is a sound step up for planning workflows: the previous
sample-count-driven loss on a smooth profile is gone, path tie-breaks agree with
an independent exhaustive oracle on seeded small graphs, the selected solution
edges are rechecked at the configured final spacing, and failure/cancellation
cases have explicit regression coverage. The full suite was rerun for this review:
**68 passed**. Ruff and mypy pass. `git status` was clean before this review note.

I would use it for comparing candidate routes and planning field checks. I would
not use this release alone to assert that a real link will work. The terrain loss
is the Bullington component, not the complete Bullington/spherical-Earth method;
there is no independent field-measurement validation, so the lower example loss
means stable numerical behavior, not proven higher real-world accuracy. See the
[current ITU-R P.526 recommendation](https://www.itu.int/rec/R-REC-P.526-16-202511-I/en)
for the complete method definition.

### Remaining engineering risks

1. **Candidate and edge coverage:** two-client, zero/one/two-repeater checks are
   exhaustive only over generated candidate sites. Their possible links still
   pass distance and optimistic link-budget screening. Longer routes use the
   sparse neighbor graph. Add candidate-density sweeps and adversarial bridge
   sites before making stronger route-completeness claims.
2. **Larger meshes:** beam search is deterministic and bounded (width 64), but
   a feasible router subset may be dropped before exact scoring. Compare it to an
   exact oracle over thousands of seeded small instances, then measure miss rate
   and explored states on representative Norwegian terrain before tuning the
   beam or claiming practical completeness.
3. **Terrain sampling:** certification means every selected edge passed at the
   configured point spacing. A narrow ridge between samples can still be missed.
   Next, test sub-cell peak injection at several orientations and implement a
   conservative raster traversal or adaptive sampling before treating final
   validation as a terrain-coverage guarantee.
4. **Diffraction scope:** the Bullington component changes defaults for new plans,
   while legacy projects keep Deygout. Keep both labeled in results and exports;
   test representative LOS, single-ridge and multi-ridge profiles against an
   independent reference implementation, then calibrate only with field data.
5. **Operational cancellation and scale:** cancellation cannot interrupt an
   in-flight GDAL/NumPy/NetworkX call; process batches can contain up to 32 links.
   Synthetic 200/800/2,000-candidate measurements are recorded below. They do
   not measure real-terrain profile costs or unrestricted large-mesh search.
   Before tuning batch size, worker count or progress latency, benchmark actual
   DEMs and successful non-anchored meshes as well.
6. **Cost model:** the 100-installation-unit plus mast-metre score is a proxy.
   Keep that assumption visible, and permit project-specific install/mast costs
   before users interpret the infrastructure objective as monetary cost.

### Review verification

- `pytest`: **68 passed**, 12 dependency deprecation warnings; no failures.
- `ruff check src tests`: passed.
- `mypy src/rf_router_planner --ignore-missing-imports`: passed (40 source files).
- The headless Qt test process emitted GPU-context fallback diagnostics after
  tests completed; process exit was successful. The initial review did not
  measure performance; a synthetic candidate sweep is recorded below.
- At the time of the initial review, only the plan document changed; the later
  performance sweep adds the benchmark utility described below.

### Larger-search benchmark — 2026-09-26

Added [`tools/benchmark_rf_search.py`](../tools/benchmark_rf_search.py) to make
the size sweep repeatable. It runs each measurement in a fresh process, reports
`RouteOptimizer.optimize` wall time and peak working set, and uses deterministic
random seeds. Timings have no pass/fail threshold. Environment: Windows 11,
Python 3.13.12, 24 logical CPUs; flat 0 m `ArrayTerrain` (401 x 1,001 cells at
25 m spacing), single worker, 16 neighbors per site, and a 24 km endpoint gap.
These are synthetic load tests rather than timings over an imported DEM.

The direct case uses 100 m endpoint antennas and a clear direct route. It shows
the cost of screening and optimization as candidate alternatives grow, but the
winning solution uses no repeaters. Each table entry is median wall time and
peak working set from fresh processes; link counts are the observed run range.

| Candidates | Repetitions | Median optimize time | Median peak RSS | Valid screened links | Result |
| ---: | ---: | ---: | ---: | ---: | --- |
| 200 | 2 | 14.814 s | 69.6 MiB | 2,205–2,216 | direct, 0 routers |
| 800 | 3 | 3.805 s | 85.1 MiB | 8,802–8,847 | direct, 0 routers |
| 2,000 | 2 | 10.885 s | 118.7 MiB | 21,916 | direct, 0 routers |

The 200-site result is slower than 800, so candidate count alone is not a useful
runtime predictor here. The topology solver's 100,000-subset exact-search limit
provides a plausible explanation: at 200 candidates, the two-router layer has
19,900 subsets and is exact; at 800, its 319,600 subsets exceed the limit and
that layer uses the bounded heuristic. This is consistent with the timings, but
the benchmark does not emit per-layer explored-state counts, so it is not a
causal attribution. At 2,000 sites, peak memory is about 1.7x the 200-site
median; screening/evaluation and final validation are the largest measured
phases in the worker output.

A second, strict-LOS case checks a real multi-hop result while scaling candidate
screening. Four required 100 m relays form two parallel paths between 3 m
endpoints; randomized sites fill out the candidate pool. To keep this focused on
candidate-screening cost, the solution limit is exactly four routers, so this
does **not** exercise size-scaled router-subset search. Each size ran once:

| Candidates | Optimize time | Peak RSS | Valid screened links | Result |
| ---: | ---: | ---: | ---: | --- |
| 200 | 0.419 s | 60.9 MiB | 2,671 | 4 routers, 2 independent paths |
| 800 | 1.810 s | 82.1 MiB | 10,967 | 4 routers, 2 independent paths |
| 2,000 | 5.686 s | 122.5 MiB | 27,360 | 4 routers, 2 independent paths |

### Long-distance corridor sweep

The benchmark also supports `--topology long --distance-km D`. These cases use
flat 25 m terrain with a 10 km corridor, strict LOS/Fresnel validation, 10 m
endpoint antennas and 100 m relays. Four required relays span 100 km; nine span
200 km, at roughly 20 km per hop. Random candidates are added around the route.
Each measurement ran twice in a fresh process:

| Distance | Candidates | Median optimize time | Median peak RSS | Valid screened links | Result |
| ---: | ---: | ---: | ---: | ---: | --- |
| 100 km | 200 | 0.482 s | 72.9 MiB | 2,546–2,578 | 4 relays, 1 path |
| 100 km | 800 | 2.077 s | 93.4 MiB | 10,476–10,576 | 4 relays, 1 path |
| 100 km | 2,000 | 6.261 s | 133.3 MiB | 26,066–26,166 | 4 relays, 1 path |
| 200 km | 200 | 0.841 s | 89.7 MiB | 2,634–2,638 | 9 relays, 1 path |
| 200 km | 800 | 3.671 s | 116.9 MiB | 11,308–11,313 | 9 relays, 1 path |
| 200 km | 2,000 | 10.208 s | 169.2 MiB | 28,247–28,265 | 9 relays, 1 path |

These long cases pass the chain sites as required network routers. This avoids
the specialized two-client, at-most-two-repeater prepass, and the maximum router
count equals the required-chain size. As a result, they test long-distance link
evaluation, final certification and candidate-screening scale with a successful
route, but **not** selection among alternative relay subsets or discovery of an
unseeded long route.

That distinction matters: an exploratory 100 km run using ordinary two-client
minimum-router mode at 2,000 candidates reached about 1.25 GB (1.17 GiB) working
set before I stopped it; it produced no timing result. In that mode,
`_find_low_hop_route` builds a list and deduplication set for the Cartesian
product of endpoint-visible candidate sets before checking two-repeater links.
That allocation is a plausible contributor, though the interrupted run did not
profile memory by operation. Treat this as a concrete scale risk to investigate:
count and stream those pairs (preserving exact-search semantics), add a
cancellation/memory regression, and compare against a bounded alternative.
The subsequent scaling investigation below identifies retained terrain-profile
arrays as the dominant allocation in a reproducible optional-relay workload;
the pair containers are a smaller, independently measurable contribution.

Reproduce with `python tools/benchmark_rf_search.py --repetitions 2` for the
direct case, or add `--topology mesh` for the anchored two-path screening case.
The mesh default is one repetition because its purpose is a quick scaling check.
For long cases, use `--topology long --distance-km 100` (or `200`); add
`--sizes 200 800 2000 --repetitions 2` for the recorded sweep. None of these
synthetic workloads models rugged terrain, GDAL/DEM I/O, GUI responsiveness, or
open-ended multi-hop subset optimization. The next useful performance dataset
should use a representative DEM and report successful route quality, peak memory
and explored search states across increasing candidate counts.

### Optional existing MeshCore routers — 2026-09-26

The self-hosted web workflow can now load nearby repeaters from CoreScope,
filtered to a configurable-width corridor around the endpoint segment. The user
can select all or individual routers from the list or map. Selections are sent
as optional, locked `KNOWN` router sites: they are available to the optimizer,
but are not required and only appear in a route when they improve the selected
objective. Generated candidates are reduced by the number selected so the
combined candidate budget stays within `Max candidates`; selected IDs are saved
in exported plans and restored when reopened. Router elevation is sampled from
the loaded terrain, and out-of-coverage sites fail with a clear validation
error.

Regression coverage verifies corridor filtering, optional (not forced) solver
behavior and plan export. CoreScope calls are mocked in tests; a live feed fetch
was not part of this verification. This is a web-app feature and does not yet
add the selection workflow to the desktop UI.

### Scaling investigation — 2026-09-26

Investigation complete; production remediation was open at this point and is
completed in the subsequent remediation section below. Added
[`tools/profile_rf_search.py`](../tools/profile_rf_search.py), which instruments
the existing benchmark in a separate process, counts live profile-array payloads
using weak references, reports peak process working set and pair-container sizes,
and supports cooperative time and retained-profile limits. These limits are
diagnostic guards, not hard process memory limits. Experimental flags temporarily
omit search profiles or reuse a low-hop result within this one fixed search;
the application engine is unchanged.

Workload: 100 km flat 25 m terrain, 10 m endpoints, 100 m routers, strict
LOS/Fresnel, one requested path, maximum four routers, seed 20260926. Four chain
sites remain in the candidate pool but are **optional**, alongside randomized
sites. This exercises subset selection and the exhaustive two-router prepass;
it is still synthetic and includes seed sites. Each row is one fresh process
with diagnostic overhead, not a repeated performance estimate.

| Candidates | Mode | Time | Peak working set | Outcome |
| ---: | --- | ---: | ---: | --- |
| 200 | Current engine | 7.604 s | 118.3 MiB | Complete, two routers |
| 800 | Current engine, 600 MiB profile limit | 13.062 s | 566.5 MiB | Complete, two routers |
| 800 | Omit search profiles experimentally | 12.683 s | 98.9 MiB | Same route, margins and alternative router lists |
| 2,000 | Current engine, 384 MiB profile limit | 14.169 s | 544.2 MiB | Cancelled by diagnostic guard during first pair scan |
| 2,000 | Omit search profiles experimentally | 90.032 s | 171.4 MiB | Cancelled by time guard during repeated pair scan |
| 2,000 | Omit profiles and reuse low-hop result experimentally | 63.262 s | 171.2 MiB | Complete, two routers |

Findings:

1. **Retained terrain profiles dominate memory.** `LinkEvaluator.evaluate`
   attaches eleven NumPy arrays to every result. The initial bulk evaluator
   drops them, but `_find_low_hop_route` retains them in `endpoint_links` and
   `cross_links`. At 800 candidates, 382 endpoint links plus 2,254 cross-links
   retained 459.76 MiB of arrays at the end of the pair scan; the pair list and
   set occupied only 6.19 MiB. Profile payloads are released when that search
   returns, so this is excessive intermediate retention rather than evidence
   of a cross-request leak.
2. **Pair storage is secondary but still quadratic.** At 2,000 candidates,
   the endpoint-visible sets contained 446 and 480 sites: 214,080 pairs. The
   pair list occupied 13.18 MiB and its deduplication set 19.43 MiB (including
   their tuples, excluding referenced sites/strings). Streaming these pairs
   would save about 33 MiB in this case, but would not address the main profile
   allocation. The profile guard stopped the baseline after about 15% of the
   scan, already at 384.22 MiB of live array payloads.
3. **Fallback repeats expensive work.** With profiles omitted, the 2,000-site
   scan completed all 214,080 pairs at 62.013 s. The topology stage produced
   no alternatives and the fallback restarted the same low-hop search at
   63.162 s. At 90 s it was still repeating the scan. The engine should retain
   the already computed result for fallback and preserve known feasible routes
   when the bounded topology heuristic does not retain their subset. A separate
   experiment reused that result and finished in 63.262 s at 171.2 MiB, returning
   the already discovered `A -> C0201 -> C1803 -> B` route with no second RF scan.
   This also shows the fallback's existing "graph was disconnected" diagnostic
   is misleading in this case: a feasible route was already known.
4. **Memory improvement alone does not remove quadratic runtime.** The
   800-site experiment evaluated exactly the same 48,271 RF links as baseline
   and retained the same route IDs, margins and alternative router lists.
   Peak memory fell about 83%, but total time remained around 13 s. At 2,000
   sites the first pair scan alone took roughly 56 s.

Recommended remediation order (not implemented in the application yet):

- Keep compact metrics for search links and materialize terrain profiles only
  for returned/displayed links. Explicitly restore profiles for low-hop winners
  and fallback links so browser charts and exports remain intact; the diagnostic
  omission flag intentionally does not do that and is not a deployable fix.
- Reuse the low-hop result during fallback and seed/preserve that feasible
  topology through heuristic selection.
- Stream frontier pairs with cancellation during enumeration; investigate
  lossless pruning and compact best-route selection to reduce further work.
- Add memory/cancellation regressions and route/metric equivalence checks,
  including charts/exports, then rerun this sweep and representative real DEMs.

Reproduce the complete 800-site baseline with:

```console
python tools/profile_rf_search.py --topology long --optional-anchors --candidates 800 --seconds 45 --profile-limit-mib 600
```

Add `--omit-search-profiles` for the controlled memory experiment. For the
2,000-site diagnostic use `--candidates 2000 --seconds 90`; the default profile
limit is 384 MiB. `--reuse-low-hop` enables the separate fallback experiment.
All recorded runs used the real RF evaluator. The completed baseline/experimental
800-site pair performed the same 48,271 evaluations; the 2,000-site reuse
experiment performed 243,211. The diagnostic script passes Ruff; application
source was unchanged, so this investigation did not rerun the application suite.

## Scaling remediation — complete, 2026-09-26

- [x] Release terrain-profile arrays after search evaluation; restore profiles
  only for final returned alternatives, including fallback routes.
- [x] Stream disjoint endpoint-frontier pairs and reuse a completed low-hop search.
- [x] Score known feasible low-hop subsets alongside the topology heuristic.
- [x] Verify dense-search memory, cancellation, seed preservation, fallback
  reuse and charts/exports; run full regressions and repeat scaling measurements.
- [x] Record results and commit the completed remediation milestone.

The application now retains scalar metrics during both low-hop exploration and
iterative final certification. After selecting alternatives, it rebuilds profiles
once per distinct returned link at final resolution, checks validity again, and
honors cancellation while preparing those profiles. All alternatives receive
profiles, not just the active route. Fallback retains the requested reliability
count instead of silently reporting a one-path request.

Endpoint-visible sets are disjoint whenever no zero/one-router route exists, so
their Cartesian product can be streamed without deduplication storage. A low-hop
result (including no route) is reused within the current optimize call. Known
feasible low-hop router subsets are also scored under the normal topology
constraints and objective, so a beam miss cannot silently discard that solution.

Repeated the same 100 km optional-anchor workload in fresh processes, with all
experimental omission/reuse flags **off**:

| Candidates | Production optimize time | Peak working set | Peak live profile arrays | Result |
| ---: | ---: | ---: | ---: | --- |
| 800 | 13.014 s | 94.0 MiB | 0.58 MiB | Two routers, 10 distinct profiles across alternatives |
| 2,000 | 62.557 s | 135.2 MiB | 0.34 MiB | Two routers, 3 returned profiles; one low-hop scan |

At 800 sites, the route IDs, margins and alternative router lists match the
566.5 MiB baseline exactly: about 83% less peak memory, with ten extra RF
evaluations to restore the displayed profiles. At 2,000 sites, the route remains
`A -> C0201 -> C1803 -> B`, with the same per-edge metrics as the earlier reuse
experiment. It now survives topology selection without needing fallback. Link
ordering follows the normal topology representation. The 214,080-pair scan still
takes about 56 seconds: this fixes memory retention and repeated work, not the
quadratic worst-case number of RF evaluations. Retained scalar edge metrics also
still grow with the number of valid links. Timings are single-run observations.

Verification includes eight new cases covering dense-search profile lifetime
and graph-reference route equivalence; cancellation before enumerating a 160,000
pair frontier with a 4 MiB allocation ceiling; cached success/failure for both
minimum-router and infrastructure modes; profile restoration for all alternatives
and fallback; cancellation during restoration; and preserving a feasible seed
when the heuristic beam misses it. Existing browser upload/optimize/profile/CSV/
GeoJSON integration tests pass. Full suite: **78 passed** (13 non-failing
dependency warnings); Ruff passes across `src`, `tests` and `tools`; mypy passes
across all 40 source files; whitespace checks pass. The completed milestone commit is named
`fix: bound RF search memory and retain feasible low-hop routes`.

## Reference

[ITU-R P.526-16](https://www.itu.int/rec/R-REC-P.526-16-202511-I/en),
§4.5.1 (Bullington component); §4.5.2 defines the additional complete-method work
explicitly deferred here. Existing Deygout is a legacy engineering approximation.
