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

## Reference

[ITU-R P.526-16](https://www.itu.int/rec/R-REC-P.526-16-202511-I/en),
§4.5.1 (Bullington component); §4.5.2 defines the additional complete-method work
explicitly deferred here. Existing Deygout is a legacy engineering approximation.
