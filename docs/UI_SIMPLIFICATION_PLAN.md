# RepeaterNet UI simplification and direct-action plan

Status: implementation in progress; S0 source/API audit complete and S1 source implementation complete.
Baseline: `65f055e`. Scope: the existing self-hosted web app.

This is the next revision of [the UI workflow plan](UI_WORKFLOW_IMPROVEMENT_PLAN.md),
based on user feedback that explanatory text, scrolling, confirmation steps,
the pinned action panel, and the permanent map/results split obstruct everyday use.
Keep the existing visual identity, readable controls, and RF capabilities.

## 1. Outcomes and scope

- Find the major setup sections at a glance, without scrolling through tutorials.
- Reveal explanations explicitly through Help controls, while retaining clear labels and units.
- Start route and coverage work from one primary action, with at most one confirmation
  for the normal supported workflow. Confirmation starts the work immediately.
- Offer a full-height Map view and a separate full-height Analysis view.
- Replace the tall pinned action panel with one compact action row.
- Preserve project data, saved runs, history, job recovery, and truthful RF result states.

This revision supersedes the old plan's permanent map/results split, tall pinned
footers, and separate coverage-review checkbox. It does not mark outstanding U7
accessibility, responsive, performance, or concurrency checks complete.
No frontend framework migration or change to the RF model is required.

## 2. Current evidence

This revision is based on the current source and the user's reported experience.
The previous browser verification established project creation and validation
recovery; it did not measure this proposed layout or these new workflows.

Source inspection confirms that Kartverket preparation derives a bounded corridor
from the plan's endpoints and included router sites. The API has no requested
coverage polygon/map-bounds input, so it cannot safely promise to download terrain
for a coverage area outside that route corridor. The direct coverage flow will
proceed when its current area is supported by loaded terrain and otherwise give a
clear upload/adjust action; coverage-area terrain service work remains a separate
future extension. Route terrain preparation can use the existing estimate/prepare
endpoints.

Before this change, the source flow is: ready route = one primary click; route with
missing terrain = estimate terrain, confirm/download, then click Find route; coverage
= Review estimate, check the reviewed-input box, then Calculate (three interactions).
The plan sidebar contains four long Plan sections and one tall run panel. The main
view permanently splits map and results. No viewport measurements are claimed here.

| Current implementation | Effect | Planned change |
| --- | --- | --- |
| `index.html`: intro, readiness card, repeated `.hint` paragraphs, upload cards, router filters and explanatory text precede radio controls | Fields and section boundaries are difficult to scan | Compact section overview, short summaries, explicit Help disclosures |
| `workspace.css`: sticky `.run-panel` contains effort, explanatory text, primary button, cache statistics, maintenance, disclaimer | A large part of the sidebar is permanently occupied | One small action row; effort lives in Search settings; cache maintenance moves to Project tools |
| `estimateCoverage`, `updateCoverageEstimateActions`, `startCoverage`: estimate → checkbox → start | Routine calculation takes several separate interactions | Calculate → automatic estimate → one confirmation → execution |
| `estimateTerrain`, `pollTerrainJob`, optimize handler: download and route search are disconnected | User must find and start the next action after terrain finishes | Find route → offer Download and find route → continue automatically |
| `.workspace`: minimum 360 px map plus minimum 220 px results; main minimum height 520 px | Both views compete for height and can force outer-page scrolling | Mutually exclusive Map and Analysis panels in one bounded shell |
| Coverage inspection lives in the sidebar; route profile lives below the map | Results appear in different constrained places | Shared Analysis destination with explicit result and link/location context |
| Existing `role=status`, stale-state labels, error fields and coverage limitations share hint styling with tutorials | Blindly hiding all hints would hide operational information | Classify content by purpose before changing its visibility |

## 3. Information architecture

Use two independent navigation levels with distinct labels:

- **Setup workspace:** Plan, Coverage, Compare, retaining their existing meanings.
- **Main view:** Map, Analysis. These change the main content surface only.

Desktop layout: compact project/header row above a setup sidebar and the main view.
The sidebar contains a short readiness line, a section overview, the selected
section's controls, and a compact primary action row. The main view fills the
remaining available height. No results tray remains below the map.

### Plan section overview

| Section | Always-visible summary | Controls when opened |
| --- | --- | --- |
| Endpoints | A/B set or missing; concise location/coordinate summary | Coordinates, place-on-map, fit |
| Terrain | Ready / missing / partial; DTM and optional DOM counts | Download/upload actions, resolution, surface options |
| Routers | Included, required and proposed counts | Feed loading, searchable list, policies and proposed sites |
| Radio | Frequency and antenna-height summary | Common RF assumptions; advanced propagation disclosure |
| Search | Objective, infrastructure policy and effort | Objective, policy, effort; advanced search limits |

Use a compact section index above the editing area so every section remains
discoverable even when the current section is long. Initially open Endpoints for
a new project. Reopening an existing project restores the last section preference.
Section switches retain edited values and autosave behavior. Errors activate and
scroll to their section and field automatically.

Coverage uses the same pattern: Sources, Area & detail, Handheld, Saved runs.
Keep grid detail readily accessible; profile limits remain advanced. Layer opacity
and display mode belong in compact map display controls because changing them
does not request an RF calculation. Compare gets an explicit scenario selector
so height, failure, target and run-comparison controls do not form another long page.

## 4. Help on demand

Do not apply a blanket rule to hide `.hint` elements. Audit static HTML and dynamic
text generated in `app.js` and `coverage_inspection.js` first.

| Content | Default treatment |
| --- | --- |
| Field labels, units, current values, section summaries | Always visible |
| Tutorials, definitions, worked examples, general explanations | Collapsed Help disclosure beside the relevant section title |
| Optional detailed assumptions and radio-budget explanations | Expandable Details within Analysis |
| Validation errors, save failures, active job state and required next steps | Always visible while relevant |
| Stale, partial, approximate, unresolved and missing-terrain result states | Short visible labels; longer explanation under Details |
| RF prediction versus measured coverage | Concise visible result qualifier; full explanation in Help/report |
| Empty-state instructions | One actionable sentence, replaced by content when available |

Use text-labelled Help buttons/disclosures, not hover-only tooltips. Support keyboard,
touch, `aria-expanded`, and stable `aria-controls`. Opening help must not move focus
unexpectedly or change plan inputs. Avoid adding a separate help button to every field;
group related explanations into section help with links to specific advanced topics.
Remember section/help preferences locally, independently of exported RF plans.
Provide a Help menu entry to restore introductory guidance.

## 5. Direct route workflow

1. User presses **Find route** from the compact action row.
2. Validate only inputs relevant to the requested operation; open and focus any
   invalid field. Do not block route search on an unused coverage or router-entry form.
3. Save the current draft and capture the plan, project identity and input revision.
4. Check terrain readiness automatically.
5. If terrain is sufficient, start optimization directly. A ready route needs one click.
6. If supported terrain is missing, fetch its estimate and show one confirmation:
   concise area/resolution, tiles/cache reuse, estimated storage, and optional surface
   choice. Primary action: **Download and find route**. Cancel leaves the plan intact.
7. On confirmation, download, recheck readiness, then start optimization automatically.
   Show a single sequence: Preparing terrain → Finding route → Complete.

Do not add another confirmation after a successful download unless the original
approved scope materially changed. Terrain-only preparation remains available from
Terrain with **Download terrain** → estimate/confirm → download, without a route job.

If downloading is unsupported, terrain remains incomplete, or storage/limits reject
the request, show the specific issue and an Upload/Adjust action. Do not retry a
known failure in a loop. Optional DOM absence must remain distinguishable from
missing mandatory ground terrain.

## 6. Direct coverage workflow

1. User presses **Calculate coverage**; estimation no longer requires a separate button.
2. Validate selected route/sources, area and coverage inputs. Resolve missing source
   choices visibly; never silently change which routers or endpoints are included.
3. Automatically obtain the estimate for an immutable request snapshot.
4. Show one confirmation containing area, source count, grid spacing, cells,
   evaluations, applicable limits and known terrain gaps. Do not invent runtime estimates.
5. **Calculate** immediately starts the approved request. Remove the separate review
   checkbox and the requirement to navigate back to a different Start button.
6. Offer **Quick preview** as a secondary action using the same estimate/confirmation
   mechanism and a clearly approximate result label. Preview is never a prerequisite.

When coverage needs additional terrain, offer **Download and calculate** only if the
terrain service can prepare the actual requested coverage area. Audit the current
route-corridor terrain API before promising this behavior. If necessary, add an
explicit bounded coverage-area preparation request with existing pixel/disk limits;
do not substitute the route corridor for a larger coverage polygon or viewport.
After download, revalidate and continue the approved calculation automatically.
If current services cannot supply the area, retain a clear Upload/Adjust path.

If there is no usable route, present **Find route first** with a clear return path;
do not silently choose a future route alternative or sources. Once the route is
ready, keep Coverage settings and offer the ordinary single-confirmation flow.
Loading an existing saved run requires no calculation confirmation and starts no job.

## 7. Workflow ownership and recovery

Introduce a small explicit operation coordinator rather than chaining button clicks.
Model states: idle, validating, estimating, awaiting confirmation, preparing terrain,
running, complete, cancelled, failed, disconnected.

- Bind every continuation to operation ID, project ID, input revision and payload.
- A confirmation approves the displayed payload; editing relevant inputs invalidates
  it. Freeze changes during committed work, or cancel/re-estimate before submission.
- Prevent repeated clicks from submitting duplicate jobs, including during preflight.
- Maintain separate display preferences: switching Map/Analysis or changing opacity
  must not invalidate an approved computation or restart work.
- Await the existing worker-capacity release semantics between terrain and calculation.
- Cancel stops the current phase and all pending continuation. Keep usable downloaded
  terrain and certified partial results according to existing backend rules.
- On a dropped connection, reconnect to the same job; never blindly resubmit a job
  whose acceptance is uncertain. Retry submission only after resolving job identity.
- On reload, restore active server jobs. Resume a multi-stage continuation only if its
  approved identity and snapshot can be proven; otherwise offer Resume explicitly.
- A changed project or newer operation makes old responses inert.
- Put errors beside the primary action and relevant field; keep detailed diagnostics
  expandable. Retain an always-reachable Cancel/Reconnect control in either main view.

## 8. Map and Analysis views

**Map:** use all main-panel height for Leaflet. Keep a small toolbar, compact layer
legend and concise active-job indicator. Add a Show/hide setup control to expand
the map horizontally. Hide the full results table and profile rather than leaving
an empty or collapsed results strip. Preserve map centre, zoom, layers and selection.

**Analysis:** use the full main panel for a short route summary, alternative selector,
readable hop table, selected-link profile and forward/reverse budget. Place the
table and profile beside each other where width allows, and stack them on smaller
screens. Detailed assumptions and raw diagnostics stay collapsed.

Keep a compact selected-route summary visible while navigating Analysis. Selecting
a map link opens Analysis with that link selected; **Show on map** returns to its
highlighted geometry. Coverage point inspection uses the same destination and
explicitly names the source run and location. Preserve the selected link/location
when changing views. Resize charts and call Leaflet `invalidateSize()` after showing
their panels; avoid recreating map layers or fetching RF data just to switch views.

Keep Map as the default for a new project. Job completion adds an Analysis badge and
**View results** action without forcibly taking the user away from the map. An empty
Analysis view gives one concise next action. Stale and partial labels remain visible
in both result summaries and full analysis.

## 9. Compact controls and responsive behavior

- Consolidate branding/project identity/save state into a compact header where width permits.
- Move import/export, cache statistics and cache clearing into Project/tools menus.
- Replace each tall pinned footer with one primary button and optional compact state.
  Search effort belongs in Search, with its current value in the section summary.
- Target action-row height: 52–64 CSS px on desktop, at most 72 px on narrow screens.
  Keep 44 px touch targets; save space by removing prose and duplicate controls.
- Show detailed job progress only on request or in Analysis; retain phase and Cancel
  during work in the compact row. Avoid progress text increasing its height indefinitely.
- On narrow screens, use Setup, Map and Analysis destinations with a retained
  Plan/Coverage/Compare context inside Setup. Do not stack all three long surfaces.
- Use viewport-aware shell sizing rather than a fixed 520 px minimum main height.
  Each active pane owns its scrolling; opening help must not create page-wide overflow.
- At 200% zoom, reflow to the narrow layout with fully reachable dialogs and actions.
- Preserve explicit, visible field errors from the recent project-validation fix.

## 10. Milestones and commit boundaries

| Milestone | Work | Depends on | Completion evidence | Suggested commit |
| --- | --- | --- | --- | --- |
| S0 | Capture current viewport/scroll/click baselines; inventory help versus state text; document API support for coverage-area terrain | — | Reproducible baseline sheet; exact control mapping and backend gap list | `docs: record UI simplification baselines` |
| S1 | Help disclosures, compact section summaries/index, remove repeated intro prose, retain active warnings | S0 | All Plan/Coverage section headings visible in overview; keyboard help and error reveal verified | `feat: make planner help available on demand` |
| S2 | Compact header/action rows, move effort and maintenance, bounded shell and narrow navigation | S1 | Viewport measurements, no control obscured by footer, readable touch targets | `feat: reclaim planner workspace space` |
| S3 | Map/Analysis tabs, expanded results layout, shared inspection destination, preserve selections | S2 | Map fills panel; complete route/coverage analysis remains accessible; no RF work on view switches | `feat: separate map and analysis views` |
| S4 | Operation coordinator, automatic terrain preflight, one-confirmation download-and-route continuation | S1, S2 | Ready route one click; missing-terrain route two clicks; cancellation/stale-response/recovery checks | `feat: prepare terrain and find routes in one flow` |
| S5 | Automatic coverage estimates, single confirmation, preview alternative, coverage-area terrain support where needed | S4 | Calculate two clicks; approved payload preserved; actual area prepared; no review checkbox | `feat: simplify coverage calculation and preparation` |
| S6 | Apply consistent action behavior to scenarios; full responsive/accessibility/workflow regression; update help/docs | S3, S5 | Acceptance matrix below completed with measured results and remaining limitations | `test: verify simplified planner workflows` |

Keep changes reviewable by milestone; update this document and the original plan's
progress record with commit hashes and actual evidence after each milestone.
Do not introduce new interactions in the old hidden DOM while leaving duplicate
legacy action paths active. Preserve stable control IDs where practical and update
existing frontend harnesses when ownership changes.

## 11. Acceptance and validation

Click counts start with relevant inputs entered and the requested workspace open;
count the primary action and any confirmation, not data entry or route selection.

| Journey | Target |
| --- | --- |
| Find route with ready terrain | One click; job starts |
| Find route with supported missing terrain | Two clicks: Find route, Download and find route; no manual continuation |
| Calculate coverage with valid sources/area | Two clicks: Calculate coverage, Calculate |
| Calculate coverage requiring supported terrain preparation | Two clicks including combined confirmation; no second Start action |
| Cancel confirmation | No download/calculation starts; settings retained |
| Cancel during preparation | Download cancelled; queued calculation never starts |
| Double-click primary action | One operation and at most one server job per phase |
| Change inputs while estimate is loading | Old estimate cannot be confirmed for the new inputs |
| Switch main views during work | Job continues; progress and Cancel remain reachable |
| Click a route link / Show on map | Correct profile opens / correct link is highlighted; no new RF calculation |
| Invalid field inside a closed section | Section opens; field receives focus; visible actionable error |
| Reload/disconnect/project transition | No duplicate submission, wrong-project repaint or silent continuation using changed inputs |

Measure at 1440×900, 1280×720, 1024×768, 768×1024, 390×844,
360×640, short landscape 844×390, and 200% browser zoom.
At 1280×720 with help closed, all five Plan section entries and the primary action
must be visible without scrolling the overview. At 360×640, section entries and
the action remain reachable without scrolling through explanatory paragraphs.
Map view reserves no height for route-analysis content. Analysis view must show
useful table/profile content without clipping controls or requiring a narrow tray.

Record before/after action-row height, map area, scroll distance to common controls,
click counts and task failures. Do not achieve density by shrinking text or touch targets.
Check keyboard order, tab semantics, focus return, screen-reader labels, live
announcements, reduced motion, dialog overflow and hidden-panel tab exclusion.

Add meaningful automated coverage for operation sequencing, exact approved payloads,
duplicate prevention, cancellation, obsolete responses and reload reconciliation.
Reuse backend tests for job lifecycle and terrain limits. Browser checks cover
geometry, click counts and focus; screenshots alone do not prove workflow correctness.
Compare view-switch responsiveness and retained map layers on the same existing
large-run fixture; investigate regressions without claiming a new RF benchmark.

## 12. Implementation entry points and open checks

- `web_assets/index.html`: shell, section navigation, help, action rows, main-view tabs,
  confirmation content and result destinations.
- `web_assets/workspace.css`, `style.css`, `coverage.css`, `meshcore.css`: remove conflicting
  old split/sticky/min-height rules and define responsive pane sizing centrally.
- `web_assets/app.js`: dynamic settings/help, layout preferences, project cleanup,
  terrain/route/coverage handlers, immutable preflight and operation ownership.
- `web_assets/coverage_inspection.js`: shared inspection destination and focus/selection.
- Prefer a small dedicated workflow module for the coordinator; give it one owner
  instead of adding more function wrappers or DOM-handler overrides.
- `web.py` and terrain preparation modules: inspect actual area support, estimate
  fidelity and operation recovery APIs; extend only where the approved flow requires it.
- `tests/web`, `tests/test_web.py`, terrain/job tests: lifecycle and behavior regressions.

S0 must settle the terrain-area API gap, actual baseline dimensions, and which
job metadata can support reload-safe continuation. These are implementation checks,
not reasons to defer the help, compact layout or Map/Analysis work.

## Progress

| Milestone | Status | Evidence |
| --- | --- | --- |
| S0 | Source/API audit complete; viewport metrics pending | Current handlers and terrain-area limitation recorded above. Before/after viewport, map-area and scroll measurements remain for S6. |
| S1 | Implemented; interactive evidence pending | Commit `d01c357`. Added a persistent, compact five-section setup index; section visibility/persistence; live endpoint/terrain/router summaries; separated Search settings from Radio; converted major instructional paragraphs into keyboard-operable Help disclosures. Project-transition validation opens the containing section. No browser or automated checks run. |
| S2 | Implemented; responsive evidence pending | Reduced the Plan, Coverage and Compare pinned actions to compact rows; moved RF cache maintenance into Project actions; bounded the desktop shell to the available viewport height. CSS targets a 60 px row; actual dimensions and narrow-screen reflow remain unmeasured. |
| S3 | Implemented; browser acceptance pending | Added independent Map/Analysis tabs, a full-height single main pane, a shared job card with persistent cancellation/reconnect, an Analysis result badge and View results action, side-by-side route table/profile layout, and a shared coverage-inspection destination with Show on map. Route-link selection opens Analysis and view changes only redraw local charts; browser interaction remains unverified. |
| S4–S6 | Pending | Implementation and interaction acceptance remain. |
