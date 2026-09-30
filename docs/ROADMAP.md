# Current roadmap

Updated 2026-09-30. This is the active work list. Earlier implementation plans
are [historical records](history/README.md); their completed features and
unfinished acceptance checks have been consolidated here.

## Product baseline

The web app implements route planning with existing/proposed repeaters, bounded
RF search, streamed coverage, exact-location inspection, height and failure
scenarios, targets, comparison, exports, named projects, and archive/restore.
Map and Analysis have separate views; help and detailed diagnostics use
disclosures. Implementation is ahead of complete release acceptance.

The [2026-09-29 browser report](history/UI_USAGE_REPORT_2026-09-29.md) records real
Oslo terrain tests, including navigation, six route alternatives, valid height
presets, a 6 m/10 m coverage comparison, archive recovery, and two viewport sizes.
Its remaining gaps are tracked below. The old F7 release gate is still open.

## Release acceptance still required

- **Files and reports:** verify browser plan JSON import/export round trips,
  route CSV/GeoJSON, coverage JSON/GeoJSON/printable HTML, and valid/malformed
  target imports. Inspect actual file contents, coordinates, assumptions,
  run identities, stale/partial labels, and offline report rendering.
- **Job lifecycle:** cover ready/missing-terrain start, confirmation cancellation,
  duplicate clicks, queued/running cancellation, failure/retry, reconnect,
  server restart, project switching, and stale-result handling. Confirm view,
  alternative, and saved-run changes do not accidentally launch RF jobs.
- **Form boundaries and feedback:** verify minimum/maximum/blank/negative/
  non-finite/precision inputs, actionable focus and visible errors, unapplied
  scenario isolation, and status messages after view/project changes.
- **Project lifecycle:** exercise active/non-active archive/restore, repeated
  requests, no remaining active projects, revision recovery, reload, and
  workspace isolation. Verify deletion/cache clearing only with disposable QA
  projects, including confirmation cancellation.
- **Result consistency:** check selected alternative/link/profile/map/source
  synchronization across views; full results versus restored summaries;
  comparison rejection for changed/missing metadata, grid, terrain, or radio;
  compatibility after reload; visible unknown/partial/redundancy shortfalls.
- **Complete coverage workflow:** inspect all modes/layers, drawn areas, source
  policies, height apply/reset, failure scenarios, and point/road/area targets
  with cross-run reports on representative terrain.
- **Accessibility and layout:** verify focus order, keyboard and touch controls,
  screen readers, 200% zoom, and 1440×900, 1280×720, 1024×768, 768×1024, 390×844,
  360×640, and 844×390. The prior report covered 1280×720 and 390×844 only.
- **Deployment under load:** exercise two workspaces, container restart/recovery,
  storage pressure, saved-run replay over HTTP, max-budget jobs, cancellation
  latency, and aggregate server/child memory. Measure cold/warm latency and
  first-result time on the documented terrain fixture.

Mark each acceptance item pass, fail, blocked, or not tested with build revision
and evidence. Automated tests complement these checks; historical test counts
are not current release approval.

## Engineering follow-up

1. **Search quality:** candidate-density/adversarial bridge sweeps, exact-oracle
   comparisons for bounded beam search, successful long-distance non-anchored
   meshes, and miss-rate measurements. Avoid global-completeness claims.
2. **Terrain fidelity:** sub-cell narrow-ridge tests and conservative/adaptive
   final-profile sampling. Current final spacing does not guarantee every
   obstacle was sampled.
3. **RF reference validation:** compare Bullington LOS/single-/multi-ridge
   profiles to an independent implementation, then calibrate against measured
   links. Full terrain/climate models and richer antenna patterns are future
   work, not implemented capabilities.
4. **Resource management:** benchmark concurrent memory and cache behavior before
   tuning worker counts or batch sizes. A retention/storage-pressure policy
   remains open; present limits fail explicitly and do not automatically evict.
5. **Planning inputs:** project-specific installation costs, access/property/
   power/protected-area data, and offline basemaps are future capabilities.

## Cleanup milestones — 2026-09-30

| Milestone | Scope | Status |
| --- | --- | --- |
| C1 | Remove desktop UI, desktop files/tests/dependencies, alternate launcher, unused contour/logging code, duplicate configs, Deygout dispatch, old workspace adoption, and renamed tile-limit alias; resolve type ambiguities and refresh inspection test fixtures | Complete: `f0420d9` |
| C2 | Publish current web-only README/guide/development docs; consolidate open work here; archive dated plans and reports with corrected links | Complete: this documentation commit |

The current catalog, terrain data, and saved results are not reset by cleanup.
Removed source remains recoverable in Git. Desktop-specific tests are retired;
web/core tests and new startup/storage/model-validation checks cover the
remaining product. Cleanup does not close the release acceptance list above.

Verification for this cleanup:

- Python: **187 passed**, 19.85 s, 71 dependency deprecation warnings. Six tests
  exclusive to retired desktop behavior were removed; three checks were added
  for web startup, isolated new-project storage, and obsolete-model rejection.
- Frontend: **8 passed**, including navigation and delayed inspection races;
  JavaScript syntax checks passed for all three application scripts.
- Ruff passed; mypy passed for all **42 source files**. The project listing no
  longer shadows the `list` type, and preview limits/results have distinct names.
- Docker rebuilt and started with the persistent volume retained. The installed
  distribution contains no desktop/contour/project-serializer modules or
  Qt/Matplotlib dependencies, and exposes only the canonical web entry point.
  The page, application script, and bundled Leaflet returned HTTP 200.
- The native editable installation was refreshed; both package and console
  entry points expose the web server's host/port options.
- All local Markdown links in **14 documents** resolved. Historical plans keep
  their original evidence beneath an explicit archive notice.

No browser layout walkthrough or field measurements were repeated for this
maintenance change; those acceptance requirements remain listed above.
