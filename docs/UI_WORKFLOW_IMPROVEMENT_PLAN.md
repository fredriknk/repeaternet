# RepeaterNet UI and workflow improvement plan

Created: 2026-09-28. Status: implementation in progress (U0–U6 implemented; U7 validation remains).
Review baseline: `228e770`. Scope: the self-hosted web interface.

Next revision: [UI simplification and direct-action plan](UI_SIMPLIFICATION_PLAN.md).
It addresses help on demand, reduced scrolling, compact action controls,
single-confirmation calculation workflows, and separate Map/Analysis views.
Its proposed interaction changes supersede the corresponding layout and review
steps below; remaining U7 validation gates stay open.

This plan builds on [the usability/performance work](USABILITY_PERFORMANCE_PLAN.md)
and [the mesh coverage roadmap](MESH_COVERAGE_PLAN.md). Those features already
exist; this work makes them easier to discover, understand and use together.
Their outstanding deployment, concurrency and field-validation gates remain open.

## Review basis and main recommendation

Reviewed the current HTML, all three application stylesheets, route/project/map
event handling in `app.js`, the active inspection handlers in
`coverage_inspection.js`, API paging metadata, existing frontend tests, and the
documented workflows. The computer-use inventory returned no apps or browsers.
This is a source-based interaction and layout review, not a completed visual or
usability test. Actual clipping, contrast, task times and rendering responsiveness
must be measured during implementation.

The main problem is information hierarchy. The sidebar presents route setup,
coverage calculation, failure analysis, height scenarios, targets and exports as
one long form. The route-search action sits after the coverage tools that depend
on its result. Improving this structure will provide more value than changing
colors alone.

Adopt a map-centered workspace with three task destinations: **Plan**, **Coverage**,
and **Compare**. Keep project identity, save status and job progress visible.
Give each destination a compact contextual control panel and one primary action.
Use a shared inspector for sites, links and coverage locations, plus a collapsible
results tray. Preserve the current Python services, Leaflet map and RF semantics.

## Findings from the current implementation

Priorities: P0 = correctness or loss of user context; P1 = core task friction;
P2 = supporting usability and polish. These are source observations; consequences
described as risks still need behavioral reproduction.

| ID | Priority | Evidence and finding | Planned response |
| --- | --- | --- | --- |
| F01 | P1 | `index.html`: the coverage section precedes `.run-panel`; route search is at the bottom of the entire sidebar. | Persistent Plan action footer; separate Coverage destination. |
| F02 | P1 | `style.css`: 340/290 px sidebar, many 9–11 px labels and hints; long coverage summaries share this narrow column. | Clearer type scale, wider/resizable desktop panel, concise summaries with expandable detail. Verify actual contrast. |
| F03 | P1 | `meshcore.css`: project bar exposes New, Rename, Recover, Duplicate, Archive and Delete together; header also says Save plan despite autosave. | Project switcher with overflow actions; explicit save status; rename portable actions Import/Export plan JSON. |
| F04 | P1 | `app.js`, `buildSettings`: infrastructure policy and objective are under Search & optimization, while low-level radio inputs are prominent. | Expose task intent, router policy and effort together; move solver and propagation tuning into Advanced. |
| F05 | P0 | `app.js`: `placing`, `inspectCoverage` and `drawingCoverageArea` are independent; map clicks choose a mode by branch order. | One mutually exclusive interaction mode, visible mode banner, Escape/cancel and deliberate transitions. |
| F06 | P0 | `coverage_inspection.js` replaces `coverage-area.onchange` assigned in `app.js`; the earlier handler includes drawing cleanup. Several other global functions are wrapped/reassigned. | Reproduce the area-switch transition; consolidate ownership and test before moving controls. |
| F07 | P1 | `syncCoverageLocks`: broad `aside input/select` locking also covers display mode and opacity. Locks are spread across render functions and wrappers. | Derive availability by operation; keep safe inspection/navigation/display actions usable during jobs. |
| F08 | P1 | `poll`, `pollCoverage`, `pollTerrainJob`: status is mainly long text; coverage polling has a wrapper to drain terminal-job pages. | Unified job presentation with distinct calculating/loading-results phases; preserve complete paging on replay and reload. |
| F09 | P0 | Coverage polling/preview use shared global state; project installation resets that state. Exact inspection already has dedicated race handling. | Extend project/job/revision guards and cancellation ownership to preview, paging, estimates and run selectors. Verify delayed-response behavior. |
| F10 | P1 | Route alternatives use one select; results and profiles live below a 52vh map; coverage inspection lives far away inside the sidebar. | Comparable alternative cards/table and shared inspector adjacent to the map. |
| F11 | P0 | Route `profile()` draws Fresnel geometry using live `rf.required_fresnel_clearance`; old results may remain visible after edits. | Render historical/stale profiles using their result assumptions; never mix live settings into a saved result. |
| F12 | P1 | Several selectors separately choose coverage runs for comparison, targets and exports; labels use alternative ID and timestamp. | One active run context, explicit pinned comparison baseline, richer run labels and visible compatibility reasons. |
| F13 | P1 | `renderCoverageLegend` lists only the first eight serving-router IDs; unknown/unresolved legend hatching is represented on map cells by dashed outlines and reduced opacity. | Accessible, consistent state styling and a complete scrollable serving-router legend. |
| F14 | P1 | Mobile CSS puts the map/results before the entire setup sidebar; the project bar scrolls horizontally and map hints use `white-space:nowrap`. | Dedicated narrow-screen navigation and sheets; verify 360 px, short landscape and zoomed desktop layouts. |
| F15 | P1 | File upload controls use labels around hidden inputs; map-only target placement lacks a coordinate-entry counterpart. Link rows do support Enter. | Audit keyboard access, replace inaccessible upload triggers, add coordinate-based actions, preserve existing row activation. |
| F16 | P2 | Native prompts/confirms handle project naming, target naming and calculation estimates; cache maintenance is next to the primary route action. | Accessible dialogs and contextual estimate cards; move maintenance into project/settings tools. |
| F17 | P1 | `app.js` creates one Leaflet rectangle/tooltip per coverage cell and recreates layers when changing views; existing tests focus on inspection ordering. | Measure rendering and retained memory at maximum grids; add bounded rendering/lifecycle work and behavior tests where measurements justify them. |
| F18 | P1 | Browser reproduction: `transitionProject` blocks creating a blank project when the current project's saved router settings are invalid; the generic warning did not reveal the collapsed field, and a rejected selector change could leave the dropdown showing a project that was not active. | Let blank-project creation autosave and preserve the current draft without validating it; for guarded switches, expand and scroll to the offending field, show inline feedback, and restore the selector to the active project. |

## Intended workflows

### First useful route

1. Create or open a project. Show its name and autosave state in one header.
2. Choose endpoints by coordinates or map placement. Clearly label the initial
   Oslo coordinates as example values until the user chooses to use or edit them.
3. Review terrain readiness. Offer **Upload terrain** and **Get Kartverket terrain**
   as two clear paths; keep download estimate, effective resolution and download
   confirmation together. Show missing sites and the action that addresses them.
4. Optionally find existing MeshCore routers. Choose Optional/Required/Excluded,
   infrastructure policy and objective in the same context. Feed activity remains
   distinct from predicted RF suitability.
5. Review radio assumptions and search effort, then use the persistent **Find route**
   action. The readiness summary links directly to invalid or missing inputs.
6. See search phase and any certified early result. Choose **Stop and keep route**
   when supported, or let the search finish. Distinguish early, completed, stopped,
   budget-limited and no-route outcomes using the existing backend semantics.
7. Compare alternatives, select a link in the map or table, inspect its profile,
   then continue to Coverage or export the route.

### Coverage and exact inspection

1. Enter Coverage with the selected route and radio assumptions visible at the top.
   If prerequisites are missing, show the reason and a link back to Plan.
2. Choose area, source routers and client equipment. Start with a compact form;
   open advanced sampling controls only when needed.
3. View an inline estimate with requested/effective grid spacing, terrain coverage,
   calculation size and limits. Use **Preview** or **Calculate coverage**.
   Do not promise completion time from link count alone.
4. Keep the map and legend visible while progress streams. Show approximate,
   partial, unresolved and stale states independently from job completion.
5. Inspect a location through an explicit map mode or coordinate entry. Display
   same-router uplink/downlink results, terrain profile and remediation beside it.
6. Select a saved run to inspect or export. Opening an existing run loads its
   cells; it must not silently submit another RF calculation.

### Compare, decide and report

1. Pin a saved run as baseline in Compare. Give runs readable labels containing
   route alternative, creation time and distinguishing scenario parameters.
2. Choose a height or failure scenario, or another compatible saved run. Explain
   incompatible inputs before submission, while keeping the server authoritative.
3. Show gains/losses, unknown/unresolved area and local versus reference-connected
   coverage together. Keep baseline and scenario labels attached to the map.
4. Assess named places/areas/roads against an explicitly selected run. Show pass,
   fail and unassessed reasons, then focus a selected target on the map.
5. Applying scenario heights is an explicit, undoable plan edit that invalidates
   the route. Viewing/comparing a scenario does not edit the plan.
6. Export from the current result context with a preview of included data and
   assumptions. Require the existing opt-ins for partial/unresolved or stale data.

## Proposed layout and visual direction

Desktop layout, schematic rather than a finished mockup:

```text
+-----------------------------------------------------------------------+
| RepeaterNet | Project name v | Saved / Saving / Retry | Import | Export |
+-----------+-----------------------+-----------------------------------+
| Plan      | Contextual controls   | Map                               |
| Coverage  |                       | Tools + active mode + Cancel      |
| Compare   | Readiness / settings  | Layers + legend                   |
|           | Advanced (collapsed)  |                     Inspector     |
|           |                       |                     (on demand)  |
|           | Primary action footer|                                   |
+-----------+-----------------------+-----------------------------------+
| Job status / loading status | Results tray: Alternatives / Links      |
+-----------------------------------------------------------------------+
```

- Keep the existing forest-green identity; use neutral surfaces and a quieter
  header. Reserve strong color for primary actions and meaningful map states.
- Proposed desktop sizes: 56 px header, 72 px navigation rail, control panel
  320–400 px; show a separate 340 px inspector only when the map retains at least
  480 px usable width. Otherwise the inspector replaces the contextual panel.
  Validate these dimensions in U0/U1 before finalizing them.
- Use 14–16 px normal control/body text and at least 12 px secondary text.
  Align units consistently and use tabular numerals for comparable metrics.
- Define tokens for typography, spacing, surface, border, focus and semantic
  states. RF margin colors, comparison colors and UI validation states must have
  distinct names and meanings. Do not repurpose a color with contradictory labels.
- Provide icon + text or patterns for important states. Inspect computed text,
  control and focus contrast; do not infer accessibility from palette choice.
- On tablets use a collapsible panel; on phones use Plan/Coverage/Compare
  navigation with a Map/Controls switch and a details sheet. Keep active action
  and job state reachable without traversing the full document.
- Use visible buttons and numeric resize controls alongside optional dragging
  for panels. Respect reduced motion and avoid animated map flights by default.

## Implementation architecture and constraints

Use small native JavaScript modules and the existing static asset delivery. A
framework migration is not necessary for this scope. Introduce a build pipeline
only if a later measured need justifies it.

Proposed boundaries under `web_assets/` (filenames may be refined in U0):

- `app.js`: startup and composition only after incremental extraction.
- `state.js`: project identity, selected route/run, capabilities, revisions and
  the single map interaction mode. Keep persisted plan data separate from UI state.
- `api.js` / `jobs.js`: request errors, request ownership, polling/paging,
  cancellation, reconnect and superseded-response handling.
- `projects.js`, `plan_controls.js`, `coverage_controls.js`, `compare_controls.js`:
  feature controllers with one owner per event and one defined teardown path.
- `map_view.js`, `inspector.js`, `results_view.js`: rendering and selection shared
  across workflows. Consolidate the two profile drawing implementations where safe.
- `tokens.css`, `layout.css`, and scoped feature styles: remove cross-feature
  layout overrides currently living in `meshcore.css` as their owners migrate.

Rules for all milestones:

1. Preserve route certification, topology, bidirectional budgets, coverage bounds,
   profile-cap semantics, explicit unresolved states, workspace isolation and job
   concurrency. UI availability must reflect backend capability, not invent it.
2. Preserve `coverage-v3` identity and existing plan schema unless a milestone
   explicitly needs a versioned persistence change. Prefer existing IDs plus UI
   labels; do not invent a scenario-label API without a reviewed storage contract.
3. Attach async effects to project ID, job ID and input revision/generation.
   Dispose of polling and subscriptions on context changes. Aborting a job-start
   HTTP request must not orphan server computation; preserve the inspection queue.
4. Calculation inputs make relevant results stale. Display-only changes—opacity,
   layers, panel layout, metric mode—must not submit work or alter certification.
5. Busy, unavailable, invalid, stale, partial and unresolved are separate states.
   One capability selector decides each control's state and provides a reason.
6. Reuse the existing terminal-page draining behavior. Test completed-job reload
   and replay above one page; never show the total as fully loaded prematurely.
7. Keep route/coverage assumptions attached to displayed snapshots. A historical
   chart, legend, tooltip or export cannot take its radio values from a live edit.
8. First extract behavior without redesigning it; then move its UI. Retain a
   working app at each commit and remove superseded handlers once migrated.
9. Add behavior tests for consequential transitions, not tests that snapshot
   incidental markup or every spacing/color change.

## Milestones and delivery order

Effort is a planning estimate for implementation and review, not a completion-time
promise. A focused implementation session means roughly a day of engineering work;
environment access and review can extend elapsed time.

| Milestone | Priority / dependency | Scope | Effort | Suggested milestone commit |
| --- | --- | --- | --- | --- |
| U0 | P0 / none | Reproducible UI fixtures, state audit, focused behavior extraction | 1–2 sessions | `refactor: establish UI state and workflow boundaries` |
| U1 | P1 / U0 | Map-centered shell, navigation, tokens, persistent action/status | 2–3 | `feat: introduce task-based planner workspace` |
| U2 | P1 / U1 | Guided setup, terrain readiness, radio intent, project saving | 2–3 | `feat: guide project setup and route readiness` |
| U3 | P0/P1 / U0–U2 | Unified map modes, router selection, accessible site editing | 1–2 | `feat: unify map editing and router selection` |
| U4 | P1 / U1–U3 | Job feedback, alternative comparison, shared link inspector | 2–3 | `feat: improve route review and job feedback` |
| U5 | P1 / U1, U3, U4 | Coverage workspace, saved-run context, legends and inspection | 2–3 | `feat: streamline coverage analysis workflow` |
| U6 | P1/P2 / U4–U5 | Scenarios, targets, compatibility and contextual export | 2–3 | `feat: unify scenario comparison and reporting` |
| U7 | P1 / U1–U6 | Responsive/accessibility review and measured browser performance | 2–3 | `test: verify planner UI workflows and rendering budgets` |

Accessibility, narrow-screen behavior and async correctness are checked in every
milestone; U7 is the cross-feature release review. First usable redesign: U0–U4.
Coverage/decision workflow release: U5–U7. Commit and update this document at each
milestone with actual scope, evidence and any unmet acceptance criteria.

### U0 — Baseline, fixtures and behavior ownership

- Inventory controls, API calls and persisted fields; map each to its intended
  destination so features do not disappear during relocation.
- Create deterministic UI fixtures for empty/loading projects, terrain gaps,
  route search with early result, no route within budget, stopped/cancelled/failed
  jobs, stale results, coverage with all states, direct routes with no repeaters,
  incompatible scenarios, server restart and connection loss.
- Use mocked public API responses for browser workflow tests and synthetic
  backend fixtures for contract tests. Avoid dependence on live terrain/router
  services or public map tiles for repeatable checks.
- Capture current screenshots/task paths at 1440×900, 1280×720, 768×1024,
  390×844 and 360×740 when a browser is available. Record unavailable checks.
- Extract mode/capability/request ownership helpers and consolidate duplicate
  handler definitions. Reproduce F06 and F11 before fixing; add narrow regression
  tests for the observed behaviors and protect existing inspection races.

Acceptance: fixtures cover the major states; source-audit risks are classified as
reproduced/resolved/not reproduced; current behavior tests pass; every control has
an assigned destination. Visual baseline remains explicitly open if unavailable.

### U1 — Workspace shell and visual foundation

- Add Plan/Coverage/Compare navigation, contextual panel, shared inspector slot
  and collapsible results tray around the existing Leaflet map instance.
- Keep the primary action and job status visible in each destination; show
  prerequisite explanations for Coverage/Compare instead of unexplained controls.
- Consolidate header/project actions and introduce design tokens and type scale.
- Persist display preferences separately from RF plan inputs; clear invalid
  selections on project switch. Panel/layout changes call `map.invalidateSize()`;
  observe chart containers for redraw without resetting user map position.

Acceptance: route action is reachable without scrolling through coverage settings;
destination switches preserve map, active job and selection; changing layout does
not invalidate results; project name and save status remain visible. No duplicate
Leaflet instance, resize listener or poller appears after repeated navigation.

### U2 — Project and planning setup

- Replace the long numbered form with compact Endpoints, Terrain, Routers and
  Radio/search sections and a readiness summary. Users can revisit any section;
  do not require a rigid wizard.
- Add coordinate validation next to each endpoint, place-on-map and fit actions,
  and a clear indication of example coordinates. Do not silently change stored
  endpoint values or require a new external geocoder.
- Present terrain upload/download as alternatives. Show loaded extent, effective
  resolution, missing-site reasons and DTM/DOM distinction in one readiness card.
- Surface infrastructure policy, objective, antenna heights and effort early.
  Keep advanced solver controls available with units/ranges; show resolved effort
  and radio assumptions before starting a calculation.
- Show Saving, Saved, Save failed/Retry states. Protect local edits from older
  save responses and prevent project switching from discarding pending edits.
- Put Rename/Duplicate/Archive/Delete in a project menu; use accessible dialogs
  with explicit project name, consequence, focus return and validation. Replace
  Save plan with Export plan JSON and explain terrain is separate.

Acceptance: a fresh user can follow the route task without opening Advanced;
invalid inputs identify their fields and preserve entered values; failed autosave
never shows Saved; out-of-order responses cannot regress the plan; import/export
and existing saved projects remain compatible. Terrain downloads retain explicit
request approval and their current bounds.

### U3 — Map interaction and router workflow

- Define `browse`, `place-a`, `place-b`, `place-router`, `place-target`,
  `draw-coverage-area` and `inspect-coverage` as mutually exclusive modes.
- Display mode-specific instructions and Cancel near the map; Escape cancels
  unfinished placement/drawing and restores normal map behavior. Finish drawing
  requires a valid polygon; offer Undo last vertex independently of plan undo.
- Add coordinate-entry alternatives to placement and movement; keep Undo/Redo
  for committed plan edits. Distinguish undoable edits from downloads/deletions.
- Make map/list selection bidirectional. Give existing/proposed/required/excluded
  routers consistent badges and symbols. Explain dragging a proposed result site
  creates/pins a required site before committing that edit, with Undo available.
- Add router name/ID search, policy/activity filters and visible/selected counts.
  Bulk changes preview the affected count and operate only on the declared scope.
  Introduce list virtualization only if measured list sizes need it.

Acceptance: one click performs exactly the active mode's action; switching modes,
projects or area type removes prior handlers and temporary geometry; panning and
Escape work throughout. All policy changes and site edits have keyboard/numeric
paths; current filtering never silently changes router policy.

### U4 — Route results, jobs and shared inspector

- Present a job card with phase, known progress, elapsed time, queue/cancellation
  state and applicable actions. Show indeterminate progress when totals are not
  known; avoid converting RF-evaluation counts into invented completion estimates.
- Keep certified early results visible with explicit search-in-progress status.
  Honor backend eligibility for Stop and keep, coverage and exports after stopping.
- Show alternatives in a compact comparison table/cards: total/existing/new
  routers, bottleneck margin, route distance and requested/achieved independent
  paths. Highlight the current alternative and the ranking objective.
- Unify link/map/table selection and open the shared inspector. Present directional
  budgets as a readable table, RF rejection reasons and an expandable detailed
  profile with units, legend and a text equivalent. Draw from snapshot assumptions.
- Make stale status contextual: identify the input category that changed, keep
  the historical result labelled, and offer recalculation without losing edits.
- Separate network errors from calculation failures; offer retry/reconnect and
  resume polling the same job without submitting duplicates.

Acceptance: delayed old results cannot replace a new context; polling reconnects
without launching work; no-route/budget/cancel/failure messages retain their distinct
meanings; selected links are reachable by keyboard and remain selected on resize.
Live edits cannot change the displayed geometry of a historical profile.

### U5 — Coverage controls, loading and map interpretation

- Put active route/run identity, source count and client assumptions at the top.
  Direct routes with no repeaters offer Include endpoints with a clear explanation.
- Group area, sources and client radio as basic controls; advanced sampling shows
  separate grid and profile spacing, cap and requested/effective values.
- Replace the native calculation confirm with an inline estimate/review state.
  A changed input invalidates that estimate; Preview and Calculate use the exact
  reviewed input revision. Preserve explicit download/export opt-ins elsewhere.
- Add a saved-run picker with clear scenario labels and completeness/staleness.
  Show loading progress independently from compute completion and drain every page
  before claiming a saved/reused result is fully visible.
- Keep view, opacity, layer visibility and read-only results usable during work.
  Keep conflicting compute/input operations locked with a visible reason.
- Put the active legend on the map; support all serving-router identities through
  a scrollable/searchable list. Align map symbols with legend semantics for unknown,
  unresolved, approximate and outside-area states, including at changed opacity.
- Show denominator-aware coverage summaries: requested, evaluated, covered,
  unknown, unresolved and outside-area samples. Do not present samples as guaranteed
  geographic coverage or overlap as independent network paths.
- Review cell footprints against exported/projected grid geometry. The current
  degree-size approximation must not imply more precise alignment than it renders;
  share the existing grid/export transform if correction is necessary.

Acceptance: mode/opacity changes do not trigger RF work; a completed/replayed run
larger than 192 cells loads entirely once; aborted/obsolete previews do not replace
the active run; source selection survives valid context changes; unknown/unresolved
remain distinguishable without color. Saved-run client assumptions drive map
tooltips and route context. Exact-point inspection refuses stale/different route
contexts, temporary height scenarios, and mismatched client/source assumptions;
inspection of an explicitly selected historical scenario remains U6 work.

### U6 — Scenarios, targets and exports

- Introduce one active coverage-run context consumed by inspection, targets and
  export; pin comparison baseline separately. Preserve explicit scenario selection.
- Show compatible/incompatible run choices with reasons based on metadata, then
  retain authoritative server validation at submission.
- Present baseline/scenario metrics side by side and expose changed assumptions.
  Keep local RF access distinct from access to the selected reference router.
- Group height overrides and router-failure controls by scenario type. Separate
  Calculate scenario, Compare and Apply to plan with explicit effects.
- Use a target table with name, type, criterion, assessed run, outcome and reason;
  select a row to focus the geometry/inspector. Provide point naming/criteria
  editing in-app; retain GeoJSON import for areas/roads in this release.
- Consolidate export into a context-aware dialog: selected route/run, format,
  optional comparison/target reports, assumptions, completeness and file contents.
  Explain unresolved results in the partial-results opt-in, not only cancelled jobs.

Acceptance: compare/apply/export always show the intended run identities; incompatible
reports cannot appear comparable; previewing a scenario does not mutate the plan;
applying heights is undoable and makes the route stale; exported counts, selected
targets and state labels match the chosen snapshot.

### U7 — Responsive, accessible and performance validation

- Review the complete task flows at all U0 viewports, short landscape and 200%
  zoom. Verify no page-level horizontal overflow; allow labelled table scrolling.
- Verify keyboard order, visible focus, dialog return focus, upload access, numeric
  placement alternatives, Escape, selected-state announcements and screen-reader
  status updates. Avoid reading every polling count aloud; announce phase changes
  and meaningful milestones. Aim for 4.5:1 normal-text contrast and 44 px primary
  touch controls, and measure the actual delivered styles.
- Benchmark local rendering with deterministic 256, 4,096 and 16,384 cell fixtures,
  plus the actual 32-source/7,744-cell result shape and supported router/target
  counts. Use small valid source counts for the 16,384-cell fixture to respect the
  250,000-pair cap. Include mixed unknown/unresolved states and a missing basemap.
- Record JSON parse time, first meaningful paint, full layer load, view switching,
  input latency, long tasks and retained browser heap separately from server RF
  time. Capture cold load and repeat navigation on a named browser/device.
- Before tuning, adopt provisional local-fixture targets: interaction feedback
  within 100 ms, 4,096-cell display-mode change within 300 ms, first visible result
  page within 500 ms after bytes arrive, and no synchronous rendering task above
  100 ms during large-grid loading. Measure p95 over at least 20 interactions;
  record misses and fixes instead of weakening targets after measurement.
- Use animation-frame batching, event delegation, bounded lists and compact
  display models where measurements identify bottlenecks. Do not duplicate full
  per-source arrays across view models; retain exact inspection on demand.
- Repeat load/switch/close cycles 20 times and check for retained layers, requests,
  listeners and monotonic post-cleanup heap growth. Record browser memory baseline
  and peak; do not confuse it with the existing 512 MiB server/engine target.

Acceptance: scripted core workflows and relevant backend/Node tests pass; human
visual and keyboard review is recorded; the performance matrix has actual numbers
and documented gaps. Missing browser access leaves U7 open even if unit tests pass.

## Validation journeys and release checklist

| Journey | Required outcome |
| --- | --- |
| New project → endpoints → terrain → route | Primary next action visible; missing inputs actionable; no advanced solver knowledge required. |
| Existing routers → policy filters → route | Bulk scope explicit; activity never represented as proven RF suitability. |
| Search → early route → stop/continue | Correct certification/search status; actions match server capability. |
| Select alternative → inspect link → resize | Map/table/inspector remain synchronized; snapshot profile unchanged. |
| Edit settings → stale result → undo/recalculate | Old result clearly identified; no accidental export as current. |
| Coverage preview → full run → inspect | Approximation replaced deliberately; page loading complete; unknown/unresolved visible. |
| Reload/reuse a large completed run | Every saved cell loads; no additional RF job; user sees loading progress. |
| Switch project during delayed responses | Old results cannot repaint the new project or alter its saved state. |
| Compare height/failure runs → apply → undo | Comparison read-only until Apply; plan invalidation and history correct. |
| Assess targets → partial/stale export | Run identity/criteria clear; required opt-in and report labels preserved. |
| Connection loss → reconnect / server restart | Saved edits protected; honest recovery state; no duplicate calculation. |
| Phone / keyboard / missing basemap | Core tasks remain reachable and usable through available alternatives. |

For each milestone record: commit, affected files, fixture/state coverage,
screenshots at reviewed widths, keyboard/visual findings, relevant test commands,
performance evidence if applicable, API/schema changes and unresolved acceptance
items. Use the existing Python, Ruff/mypy and Node checks for code changes; add
browser automation when available for real interaction/layout behavior.

Do not call this redesign finished until U0–U7 acceptance is met. Engine field
validation and concurrent deployment validation remain tracked in the coverage
plan; this UI plan neither closes nor redefines those requirements.

## Deferred additions

Address the existing workflow before adding address search, external geocoding,
dark mode, public sharing, multi-user collaboration, custom dashboard layouts,
mobile GPS tracking or new RF models. A later release may add named reusable
equipment/scenario presets with an explicit persistence contract. No extra
frontend framework, tile provider or analytics service is required by this plan.

## Progress record

| Item | Status | Evidence / next action |
| --- | --- | --- |
| Source/workflow audit | Complete | Findings F01–F18 above, reviewed at `228e770` plus the 2026-09-29 project-validation browser reproduction; visual/accessibility acceptance remains tracked under U7. |
| U0 | Implemented; acceptance evidence pending | Consolidated the map tool into one mode, added Escape/cancel, removed the coverage-area handler overwrite and lifecycle wrappers, kept display-only coverage controls usable during jobs, and attached the RF snapshot to route results. Automated tests were not run; live browser review is unavailable. Commit recorded after staging. |
| U1 | Implemented; acceptance pending | Added Plan/Coverage/Compare views with a pinned action per workspace, a persistent Leaflet map/results workspace, collapsible results, project-action menu, autosave/export labels, workspace preference persistence, and a visual token/layout layer. U6 will make the Compare action scenario-aware; live browser review and viewport/keyboard evidence remain unavailable. Commit `57c4c1b`. |
| U2 | Implemented; acceptance evidence pending | Added endpoint validation and terrain readiness links, visible infrastructure/objective controls, grouped advanced settings, explicit import/export actions, truthful save/retry state, and project-transition guards that flush edits and reject invalid coordinates. Browser testing verified creating a clean project from an invalid saved repeater-height draft and field-focused feedback on a blocked switch; responsive/keyboard/accessibility acceptance remains open. |
| U3 | Implemented; acceptance evidence pending | Added coordinate-based router/target creation and router movement, name/ID plus activity/policy filters, scoped bulk-action counts, policy badges, and bidirectional router map/list focus. Map mode, keyboard and browser behavior remain unverified; commit subject `feat: improve router selection and coordinate editing`. |
| U4 | Implemented; acceptance evidence pending | Added a shared job card, reconnect-to-same-job behavior, alternative comparison cards, and a directional RF inspector using the result snapshot. Matching result identity/revision is required before final render. No automated tests or browser review were run. |
| U5 | Implemented; acceptance evidence pending | Added reviewed estimate gating for preview/full runs, paged saved-run loading and status/progress, a searchable map legend, projected cell footprints, and explicit guards against interpreting current exact-point inspection as a stale or scenario run. Syntax/whitespace checks passed; regression test added but not run; browser review remains pending. |
| U6 | Implemented; acceptance evidence pending | Unified inspection, target assessment, router-failure analysis and exports around the Coverage workspace's selected run. Exact inspection resolves saved client/source-height assumptions. Comparison choices surface incompatibility reasons; target points can be renamed/re-criterioned and focused; exports can include a saved target report. Added regression test but did not run it. Static syntax/whitespace checks passed; browser review remains open. |
| U7 | In progress; browser now connected | Keyboard upload access and dialog focus restoration implemented. Terrain-download journey verified in Docker/browser. 47 backend workflow tests and six Node inspection tests pass. Full viewport, keyboard-flow, screen-reader, and rendering/memory evidence remains outstanding. |

Implementation is active. Do not call the redesign complete until each milestone
acceptance is met and the outstanding browser-only evidence is explicitly recorded.

### Implementation log

- U0 commit `2e64ada`: removed overlapping map-mode booleans and handler
  overwrites; added Escape/cancel cleanup and left display-only coverage controls
  available during jobs. Route API results now include the RF settings snapshot
  used to calculate their profiles. Tests were not run; browser behavior remains
  unverified.
- U1 commit `57c4c1b`: three task workspaces reuse the same
  map and results DOM, keep Plan/Coverage actions reachable, collapse the result
  tray, and store workspace/display preferences outside the plan. Plan, Coverage
  and target assessment in Compare each have a pinned action. No new RF
  calculation is triggered by navigation or result-tray layout. U6 will make
  Compare's action follow the selected scenario; no visual/browser validation is
  claimed.
- U2 commit `feat: guide project setup and route readiness`: added a live
  readiness card with field-level endpoint errors and links to terrain setup;
  surfaced route intent and collapsed solver/propagation controls; clarified
  import/export labels; and added truthful queued/saved/failed autosave feedback
  with retry. Project changes now flush pending saves, reject malformed endpoint
  values without discarding edits, and apply a replacement project before
  archiving/deleting the previous one. Removed dead duplicate coverage-inspection
  handlers so the extracted inspection module remains the sole owner. Automated
  and browser validation remain pending; the accepted browser inventory was
  empty during this session.
- U3 commit `feat: improve router selection and coordinate editing`: added
  keyboard-operable coordinate entry for proposed routers and point targets,
  coordinate editing for proposed sites, and visible map/list focus in both
  directions for saved and proposed routers. Existing router search now combines
  name/ID, activity and policy filters; visible/included/required counts and
  bulk-action labels make the affected scope explicit. Map clicks select the
  matching router rather than silently changing its policy; policy badges and
  map styles distinguish excluded, optional and required choices. The existing
  mutually exclusive modes, Escape cancellation, and polygon finish/cancel
  controls remain the mode owner. No automated tests or live browser review were
  run; acceptance evidence remains pending.
- U4 (implementation ready for commit): added the persistent job card with
  elapsed/phase/indeterminate-or-known progress, cancellation and eligible
  stop/keep actions; reconnect resumes polling the same server job and retains
  the busy state after a network error. Final route snapshots must match both
  job ID and input revision before rendering, and the project summary no longer
  overwrites a restored result's status. Alternative cards compare router
  counts, distance, bottleneck margin and independent-path outcome. Map lines
  and keyboard-operable table rows share selection; the inspector explains
  LOS/Fresnel/directional-budget checks and presents forward/reverse budgets
  plus the saved RF assumptions. Diff review only; automated tests and live
  browser review were not run, so U4 acceptance evidence is still pending.
- U5 (implementation ready for commit): coverage now starts with an inline,
  input-bound estimate review; Preview and Calculate submit the exact reviewed
  payload. Saved runs can be loaded without recalculation through all stored
  pages, with loading separated from compute progress and explicit stale/partial
  states. The map legend is searchable for all serving routers, unknown and
  unresolved cells retain distinct patterns when opacity changes, and cell
  polygons use projected grid corners transformed to WGS84. Preview responses
  carry route identity. Exact-point inspection is blocked when the displayed
  context is stale/different, uses temporary source-height overrides, or has
  mismatched client/source assumptions. Added an archived-run footprint
  regression assertion; it was not run. `git diff --check`, `node --check` and
  `python -m py_compile` passed. Browser/accessibility review is unavailable;
  U6 will complete scenario-aware inspection and report context.
- U6 (implementation ready for commit): a single selected saved run now drives
  exact-point inspection, target assessment, router-failure analysis and all
  export formats; inspection uses the selected run's saved client and source
  heights when its route and terrain remain current, even if it is historical
  relative to the edited Coverage form. Baseline/scenario selectors remain
  separately explicit, with client/radio/grid/terrain/model/shared-reference
  compatibility reasons in their choices and a side-by-side run summary plus
  changed source/height assumptions after comparison. Exports can include a
  saved target report. Point target name and margin criteria are editable
  in-app, and Focus actions synchronize list selection with map geometry. Added
  a backend regression test for saved-run inspection after a client-setting
  change; tests were not run. `git diff --check`, JavaScript syntax checks and
  `python -m py_compile` passed. Browser/accessibility and measured rendering
  evidence remain U7 gates.
- U7 source-audit milestone: ground and surface terrain file inputs remain in
  the accessibility tree and keyboard tab order while visually clipped; their
  upload cards show a focus ring when the input is focused. Shared confirmation
  dialogs now explicitly return focus to the invoking control after confirm,
  cancel, or Escape, when that control still exists and is available. Static
  source checks only; tests and browser interaction were not run. Viewport,
  assistive-technology, and performance measurements remain unverified, so U7
  is still open.
- 2026-09-29 Docker/browser verification: browser access became available after
  the local Docker app was started. Terrain preparation initially failed because
  Rasterio's GDAL dependency could not load `libexpat.so.1` in `python:3.12-slim`.
  The Dockerfile now installs `libexpat1` and imports Rasterio during image build,
  so this dependency failure stops the build instead of appearing during a user
  download. Rebuilt and restarted the planner with its existing data volume;
  confirmed Rasterio 1.5.1 loads in the running container. Retried the existing
  four-tile, 10 m terrain request through the browser: all four DTM tiles loaded,
  the job reached **Terrain ready**, and readiness changed to **Ready to search**
  (17 s). This verifies the terrain-download journey, not the full U7 viewport,
  accessibility or performance matrix.
- U7 workflow regression milestone, 2026-09-29: route completion is now published
  after result persistence and worker-slot release. Previously the UI could see
  **complete** and immediately receive HTTP 429 from coverage preview while that
  route still held the only worker slot. Added a deterministic delayed-persistence
  regression; persistence failures also release capacity. Updated the inspection
  test harness to use the unified interaction mode. Corrected an overbroad
  footprint assertion: outside-area cells intentionally have no polygon; added
  a non-vacuous polygon check to the four evaluated-cell pagination test.
  Validation: `python -m pytest tests/test_coverage_jobs.py tests/test_web.py`
  passed **47 tests**; `node --test tests/web/*.test.cjs` passed **6 tests**;
  Ruff passed for the changed Python files. No browser performance numbers are
  claimed by these checks.
- 2026-09-29 project-validation recovery: reproduced that an invalid saved
  repeater height blocked **New project** and that the generic warning could be
  separated from the collapsed control. New-project creation now waits for the
  current draft autosave but skips validation of that draft, so the new project
  starts clean while the old project's values remain saved. Other guarded
  transitions expand the containing settings section, focus/scroll to the
  invalid control, associate an inline error for assistive technology, and show
  a header-level warning; a rejected project-selector change restores the
  current active name. Docker/browser verification created a clean project from
  a saved 0 m repeater-height draft and reproduced the field focus and native
  validation message. A screenshot confirmed the inline error appears beside
  the field and the selector returns to the actual active project; corrected
  the test draft back to its default 3 m height and saved it. Broad U7 viewport
  and accessibility checks remain open.
