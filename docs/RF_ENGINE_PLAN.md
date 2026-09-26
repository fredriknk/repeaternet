# RF pathfinding improvement plan

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

Reproduce with `python tools/benchmark_rf_search.py --repetitions 2` for the
direct case, or add `--topology mesh` for the anchored two-path screening case.
The mesh default is one repetition because its purpose is a quick scaling check.
Neither workload models rugged terrain, GDAL/DEM I/O, GUI responsiveness, or
open-ended multi-hop optimization. The next useful performance dataset should
use a representative DEM and report both successful route quality and search
states across increasing candidate counts.

## Reference

[ITU-R P.526-16](https://www.itu.int/rec/R-REC-P.526-16-202511-I/en),
§4.5.1 (Bullington component); §4.5.2 defines the additional complete-method work
explicitly deferred here. Existing Deygout is a legacy engineering approximation.
