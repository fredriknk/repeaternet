# RepeaterNet browser UI usage report

**Test date:** 2026-09-29  
**Build:** rebuilt and restarted with `docker compose up --build -d`; the existing Docker data volume was preserved.  
**Browser:** Chrome, `http://localhost:8000/`, tested at 1280 × 720.  
**Project:** isolated test project `UI QA · Oslo map + coverage`; the existing `Untitled plan` was not edited.

## Test data and setup

I used the public Oslo sample route from 59.913900, 10.752200 to 59.950000, 10.850000. The live MeshCore lookup returned 39 nearby routers. I searched for `NO-0589 Oslo/Bjerke`, exercised the activity and policy filters, and used the bulk Optional/Exclude actions; the test plan ended with two included routers, one required. The planner estimated and downloaded four Kartverket DTM tiles at the requested 10 m resolution. Terrain occupied about 124.2 MiB in the workspace after preparation. No surface/DOM data was available, so every RF result below is terrain-only.

The initial route search used Balanced effort (798 generated candidates plus two existing routers, 12 neighbors per site). The first search completed in about 12 seconds; a repeat after restoring the project completed in about 8 seconds.

## What worked

| Workflow | Observed result |
| --- | --- |
| Router lookup and selection | Returned 39 nearby routers; text search found the Oslo/Bjerke device. Activity/policy filtering and bulk Optional/Exclude controls updated the visible/included counts. |
| Terrain estimate and download | Estimate showed four tiles and a bounded download. With terrain absent, Find route presented a download confirmation; the Kartverket download completed and the planner continued into route search. |
| Route calculation | Completed successfully. Six alternatives were returned. The shortest was one existing repeater, 6.83 km primary route, 4.9 dB bottleneck, but only 1/2 independent paths. The two-router alternative added one proposed repeater and met 2/2 paths at 7.58 km. |
| Route metrics and profile | The selected result exposed per-link distance, two-way margin, Fresnel clearance, LOS, validity, and forward/reverse budgets. For the first link the UI showed 4.52 km, 12.8 dB usable margin, and -94% minimum Fresnel clearance; the second showed 2.30 km, 4.9 dB, and -403%. The “Valid” result beside “Obstructed” deserves clearer explanation. |
| Coverage preview and full run | Preview asked for confirmation and returned 87/100 covered cells at 1,000 m, with zero unknown/unresolved/outside cells. Full calculation asked once, produced the same result, and later reused the identical saved run without redoing RF work. |
| Coverage modes and area tools | All five views (two-way, downlink, uplink, overlap, best source) could be selected. Selected mesh + buffer and current map extent were selectable. Drawn-polygon mode enabled Finish after three map vertices; clearing it returned to the default mesh area. The default two-way view and mesh area were restored afterward. |
| Coverage map and basemaps | The OSM map filled the available map panel, displayed the coverage overlay and legend, and the Kartverket topographic basemap could be selected and then restored to OSM. The map layer control exposed terrain, predicted coverage, coverage area, difference, and targets layers. Candidate visibility could be toggled. |
| Saved coverage and location inspection | A completed saved run loaded incrementally to 100/100 cells without starting a calculation. Inspecting a point at 59.92200, 10.79699 completed; the selected router was 2.25 km away, with 35.8 dB downlink and 33.8 dB uplink margin, reported usable. |
| Point target and assessment | A coordinate-entered point target (`QA Oslo point`, 59.932, 10.801) saved successfully. Assessment reported 1 pass, 0 fail, 0 unresolved, and 0 unknown/needs finer sampling. |
| Router failure scenario | Disabling the only selected repeater produced a result: 87 locally covered and 87 reference-connected cells lost, no unresolved backbone edges, and no surviving routers. |
| Coverage exports | The UI confirmed downloads for printable HTML, GeoJSON cells/targets, and JSON results, including the selected target report and assumptions. I verified the app’s completion messages, not the downloaded file contents. |
| Project copy and archive | Duplicate created a copy of the saved plan/terrain. Archiving the temporary copy completed and the app switched to a new empty project. The archive dialog described the copy as recoverable. |

## Issues found

### High priority

1. **Analysis is not reachable by pointer.** Clicking the Analysis tab, including a coordinate click on its visible tab, did not switch views. Clicking “View results” also left Map selected. Keyboard navigation from Map (`ArrowRight`) did open Analysis, and “Show route on map” returned to Map. This makes the main map/analysis workflow unreliable for mouse users.
2. **Route alternatives are disabled after a completed search.** All six cards had the DOM `disabled` property set even while the status said “Select an alternative or link to inspect it.” The alternatives cannot be changed, so the main benefit of generating them is inaccessible in this session.
3. **Antenna-height presets violate their own input step.** The input is `min=0.1`, `step=0.5`, but the 10 m preset writes `10.0`. Chrome rejected it and reported the nearest valid values as 9.6 and 10.1. This blocked a project action until I manually changed the value to 10.1. The 3 m and 6 m whole-number presets have the same step mismatch. Make the input step/presets consistent (for example, `step=0.1` or valid preset values).
4. **Scenario comparison could not run.** Saved 6 m and 10 m height runs were present, but the comparison UI marked them “Terrain differs” and disabled “Compare runs on map.” I had not changed the ground-terrain selection or requested resolution between those runs; only the antenna-height scenario changed. The compatibility check needs investigation.

### Medium priority

5. **Correction: route confirmation behavior matches the approved plan.** With ready terrain, Find route immediately started validating the search. When terrain was missing, the app confirmed the combined download-and-search action and then automatically ran the route search. This is the intended low-click workflow in the [UI simplification plan](UI_SIMPLIFICATION_PLAN.md), not a defect; no additional confirmation should be added. Full coverage and quick preview correctly presented one confirmation before running.
6. **Archive recovery is unclear.** The archive confirmation promised recovery, but after archiving, “Recover previous” remained hidden and the copy disappeared from the active project selector. Archiving the active copy also created `Untitled 1790700185239` as a new blank project. The recovery entry point should be discoverable, and the automatic blank-project creation should be made explicit or avoided.
7. **Analysis is still a long scroll.** The dedicated Analysis tab is a useful separation, and the content is readable, but at 1280 × 720 the route alternatives consume much of the initial view and the link/profile/budget details are farther down. A compact alternative summary with a clear link/profile selector would help users reach the important diagnostics sooner.

## Overall impression

Implementation follow-up: [prioritized fix implementation checklist](UI_FIX_IMPLEMENTATION_PLAN.md), including evidence qualifications, milestone commits, and acceptance gates. “Recover previous” concerns a saved plan revision; discoverable archived-project restoration is a separate requirement.

The rebuilt UI is a substantial improvement over a dense single-pane workflow: the plan sections, collapsed help, coverage tabs, map/analysis separation, confirmation dialogs, and map legend make the tool easier to scan. The coverage layer is especially compelling over real terrain, and saved-run reuse makes iteration feel fast. The most serious friction is not visual polish but interaction state: the Analysis tab and route alternatives look available yet do not work, while the height presets can leave an invalid field that blocks unrelated project actions. Those should be fixed before calling the workflow dependable.

## Not verified / limits

- GeoJSON target import was attempted with a small test FeatureCollection, but Chrome blocked file selection because the Codex extension lacks “Allow access to file URLs.” Enabling that permission would broaden extension access to local files, so I did not change it. No test fixture remains in the repository.
- Route CSV/GeoJSON exports, plan JSON import/export, permanent project deletion, cache clearing, mobile breakpoints, and target-comparison-across-runs were not fully verified. Permanent deletion was intentionally not confirmed. Coverage comparison was blocked by the observed terrain-compatibility message.
- The test project now contains the Oslo terrain/router/coverage test state and the `QA Oslo point` target. `UI QA temporary copy` is archived. The archive flow also left the blank fallback project `Untitled 1790700185239` in the project selector; `Untitled plan` remains untouched. The workspace indicator reached 186.5 MiB after copying/archiving terrain data.
