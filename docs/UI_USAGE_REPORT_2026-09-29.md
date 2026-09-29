# RepeaterNet browser UI usage report

**Test date:** 2026-09-29  
**Build:** rebuilt and restarted with `docker compose up --build -d`; the existing Docker data volume was preserved.  
**Browser:** Chrome at `http://localhost:8000/`, tested at 1280 × 720 and 390 × 844.
**Project:** isolated test project `UI QA · Oslo map + coverage`; the existing `Untitled plan` was not edited.

**Regression checks:** the repository virtual environment is `.venv`. Running `.\.venv\Scripts\python.exe -m pytest` passed all 190 tests (19.75 s; 79 dependency deprecation warnings). `node --test tests/main_view_navigation.test.js` passed 2/2 tests. `node --check` passed for `app.js` and `main_view_navigation.js`.

## Test data and setup

I used the public Oslo sample route from 59.913900, 10.752200 to 59.950000, 10.850000. The live MeshCore lookup returned 39 nearby routers. I searched for `NO-0589 Oslo/Bjerke`, exercised the activity and policy filters, and used the bulk Optional/Exclude actions; the test plan ended with two included routers, one required. The planner estimated and downloaded four Kartverket DTM tiles at the requested 10 m resolution. Terrain occupied about 124.2 MiB in the workspace after preparation. No surface/DOM data was available, so every RF result below is terrain-only.

The route search used Balanced effort (798 generated candidates plus two existing routers, 12 neighbors per site). Searches completed in about 12 seconds initially and 11.5 seconds on the latest rebuilt-browser retest. Six alternatives were returned.

## What worked

| Workflow | Observed result |
| --- | --- |
| Router lookup and selection | Returned 39 nearby routers; text search found the Oslo/Bjerke device. Activity/policy filtering and bulk Optional/Exclude controls updated the visible/included counts. |
| Terrain estimate and download | Estimate showed four tiles and a bounded download. With terrain absent, Find route presented a download confirmation; the Kartverket download completed and the planner continued into route search. |
| Route calculation | Completed successfully. Six alternatives were returned. The shortest was one existing repeater, 6.83 km primary route, 4.9 dB bottleneck, but only 1/2 independent paths. The two-router alternative added one proposed repeater and met 2/2 paths at 7.58 km. |
| Route metrics and profile | The selected result exposed per-link distance, two-way margin, Fresnel clearance, LOS, validation, and forward/reverse budgets. For the first link the UI showed 4.52 km, 12.8 dB usable margin, and -94% minimum Fresnel clearance; the second showed 2.30 km, 4.9 dB, and -403%. The retested wording now reads “Passes configured validation criteria · LOS obstructed, Fresnel clearance not met,” distinguishing RF criteria from geometric clearance. |
| Coverage preview and full run | Preview asked for confirmation and returned 87/100 covered cells at 1,000 m, with zero unknown/unresolved/outside cells. Full calculation asked once, produced the same result, and later reused the identical saved run without redoing RF work. |
| Coverage modes and area tools | All five views (two-way, downlink, uplink, overlap, best source) could be selected. Selected mesh + buffer and current map extent were selectable. Drawn-polygon mode enabled Finish after three map vertices; clearing it returned to the default mesh area. The default two-way view and mesh area were restored afterward. |
| Coverage map and basemaps | The OSM map filled the available map panel, displayed the coverage overlay and legend, and the Kartverket topographic basemap could be selected and then restored to OSM. The map layer control exposed terrain, predicted coverage, coverage area, difference, and targets layers. Candidate visibility could be toggled. |
| Saved coverage and location inspection | A completed saved run loaded incrementally to 100/100 cells without starting a calculation. Inspecting a point at 59.92200, 10.79699 completed; the selected router was 2.25 km away, with 35.8 dB downlink and 33.8 dB uplink margin, reported usable. |
| Point target and assessment | A coordinate-entered point target (`QA Oslo point`, 59.932, 10.801) saved successfully. Assessment reported 1 pass, 0 fail, 0 unresolved, and 0 unknown/needs finer sampling. |
| Router failure scenario | Disabling the only selected repeater produced a result: 87 locally covered and 87 reference-connected cells lost, no unresolved backbone edges, and no surviving routers. |
| Coverage exports | The UI confirmed downloads for printable HTML, GeoJSON cells/targets, and JSON results, including the selected target report and assumptions. I verified the app’s completion messages, not the downloaded file contents. |
| Project copy and archive | Duplicate created a copy of the saved plan/terrain. The archived-project list exposed a Restore action; restoring returned the same project identity and four DTM tiles. Re-archiving the active QA copy selected the existing QA project rather than creating a blank project. |

## Post-fix retest

The following defects from the baseline were fixed and checked again in the rebuilt Docker UI:

- **Navigation:** the root cause was an overly broad `[data-mobile-destination]` listener that also bound to the `<main>` layout marker, so a bubbled click reset Analysis to Map. It is now scoped to `.mobile-destination-nav`. Pointer switching, View results, repeated Map/Analysis/Setup switching, and keyboard ArrowRight navigation worked; the bubbling case has a Node regression test.
- **Alternatives:** all six native selector options were available after a completed Balanced search, with correct one-/two-router labels. Selecting alternatives updated route metrics without a new search. In the expanded comparison only the selected card was disabled. Failure/cancel transitions remain untested.
- **Height:** browser native validity accepted the 3, 6, and 10 m presets with 0.1 m steps. An invalid 0 m unapplied scenario draft did not block project creation or switching. Minimum/maximum and backend-invalid cases remain untested.
- **Saved coverage comparison:** fresh same-terrain runs at 6 m (`b888b5b127625e26dcc30aec6e41fc46`) and 10 m (`197576c3ae12fed927ab9de7200e545e`) compared. The UI showed 100 common samples, 87→88 covered (+1), 0 lost, and no unknown/unresolved/outside cells; the difference report, legend, and map layer were visible.
- **Archive recovery:** a QA project restored with its identity and four DTM tiles. Re-archiving the active QA copy returned to the existing QA project, not a new blank project. Temporary QA projects remain archived.
- **Analysis layout:** route summary and link/profile controls were reachable at 1280×720. At 390×844 there was no horizontal overflow (390 px scroll width), alternatives reflowed to one column, and destination switching worked. 200% zoom and assistive-technology behavior were not tested.
- **RF wording:** links now say “Passes configured validation criteria · LOS obstructed, Fresnel clearance not met” where applicable. Geometric values remain visible; RF thresholds are unchanged.

Ready-terrain Find route starts with one click. Missing terrain uses one combined download-and-search confirmation and then continues automatically. Coverage preview/full calculation uses one confirmation. This is intended per the [UI simplification plan](UI_SIMPLIFICATION_PLAN.md), not a defect.

## Original baseline findings (historical; superseded by the retest above)

These describe the initial session before implementation. They are retained to show what was reproduced; current status is in “Post-fix retest.”

### High priority

1. **Analysis was not reachable by pointer.** In the baseline, the click bubbled to a listener accidentally attached to `<main>` because it shared the `[data-mobile-destination]` attribute used for layout state; that listener reset the view to Map. Resolution and rebuilt-browser verification are recorded in “Post-fix retest.”
2. **Route alternatives are disabled after a completed search.** All six cards had the DOM `disabled` property set even while the status said “Select an alternative or link to inspect it.” The alternatives cannot be changed, so the main benefit of generating them is inaccessible in this session.
3. **Antenna-height presets violate their own input step.** The input is `min=0.1`, `step=0.5`, but the 10 m preset writes `10.0`. Chrome rejected it and reported the nearest valid values as 9.6 and 10.1. This blocked a project action until I manually changed the value to 10.1. The 3 m and 6 m whole-number presets have the same step mismatch. Make the input step/presets consistent (for example, `step=0.1` or valid preset values).
4. **Scenario comparison could not run.** Saved 6 m and 10 m height runs were present, but the comparison UI marked them “Terrain differs” and disabled “Compare runs on map.” I had not changed the ground-terrain selection or requested resolution between those runs; only the antenna-height scenario changed. The compatibility check needs investigation.

### Medium priority

5. **Correction: route confirmation behavior matches the approved plan.** With ready terrain, Find route immediately started validating the search. When terrain was missing, the app confirmed the combined download-and-search action and then automatically ran the route search. This is the intended low-click workflow in the [UI simplification plan](UI_SIMPLIFICATION_PLAN.md), not a defect; no additional confirmation should be added. Full coverage and quick preview correctly presented one confirmation before running.
6. **Archive recovery is unclear.** The archive confirmation promised recovery, but after archiving, “Recover previous” remained hidden and the copy disappeared from the active project selector. Archiving the active copy also created `Untitled 1790700185239` as a new blank project. The recovery entry point should be discoverable, and the automatic blank-project creation should be made explicit or avoided.
7. **Analysis is still a long scroll.** The dedicated Analysis tab is a useful separation, and the content is readable, but at 1280 × 720 the route alternatives consume much of the initial view and the link/profile/budget details are farther down. A compact alternative summary with a clear link/profile selector would help users reach the important diagnostics sooner.

## Overall impression

Implementation follow-up: [prioritized fix implementation checklist](UI_FIX_IMPLEMENTATION_PLAN.md), including evidence qualifications, milestone commits, and acceptance gates. “Recover previous” concerns a saved plan revision; discoverable archived-project restoration is a separate requirement.

The retest addressed the largest usability defects: result navigation works, generated alternatives are usable, height presets no longer create hidden blockers, comparable height runs display their differences, and archived projects can be found and restored. The compact Analysis view is easier to scan without sacrificing the map panel. Terrain-only predictions and negative clearance still need contextual judgment, but the updated wording avoids presenting RF validation as geometric clearance. This is not a claim that every function has been tested; the remaining list below is the release gate.

## Not verified / limits

- GeoJSON target import was attempted with a small test FeatureCollection, but Chrome blocked file selection because the Codex extension lacks “Allow access to file URLs.” Enabling that permission would broaden extension access to local files, so it was not changed. No test fixture remains in the repository.
- Actual contents of route CSV/GeoJSON, coverage JSON/GeoJSON/HTML, and plan JSON import/export were not inspected; completion notifications alone are not proof. Target comparison across separate runs, malformed import fixtures, permanent deletion, and cache clearing remain unverified. Permanent deletion was intentionally not confirmed.
- Cancel/retry and injected failure for route/terrain/coverage jobs, reconnect/reload behavior, stale-result paths, duplicate-click protection, no-active-project archive behavior, height bounds, and full point/road/area workflow inventory remain unverified. These must pass or be explicitly resolved before F7 can be marked complete.
- Responsive browser testing covered 1280 × 720 and 390 × 844 only. 1440 × 900, 1024 × 768, tablet/landscape sizes, 360 × 640, keyboard focus order beyond navigation, screen readers, and 200% zoom remain unverified.
- QA state left in the app: `UI QA · Oslo map + coverage` retains the Oslo terrain/router/coverage test data and `QA Oslo point`; the two temporary QA projects are archived. `Untitled plan` was not edited. Workspace data indicator reached about 186.5 MiB after testing/copying terrain. No permanent project or cache deletion was performed.
