# Browser findings: fix implementation plan

Date: 2026-09-29. Status: planned; no application fixes implemented by this document.

Evidence: [real-map browser usage report](UI_USAGE_REPORT_2026-09-29.md). This is the acceptance follow-up to [the UI simplification plan](UI_SIMPLIFICATION_PLAN.md), not a replacement roadmap.

## Scope and evidence corrections

- Fix blocked interaction and incorrect state before visual polish. Preserve saved projects, terrain, results, and existing RF rules.
- Report item 5 is **expected behavior**, not a defect: ready-terrain route search starts with one click; missing terrain gets one combined download-and-search confirmation, then continues automatically. Coverage gets one estimate/confirmation. Do not add a second route confirmation.
- Analysis pointer failure was observed, but its cause is unconfirmed. Reproduce with overlays closed and fresh DOM targets before changing event handling; distinguish an application defect from automation targeting problems.
- Height validation is confirmed in source: `min=0.1`, `step=0.5` rejects the whole-number presets. Disabled alternative cards and terrain comparison have strong source-level leads, but still need regression reproductions.
- “Recover previous” restores a previous plan revision, not an archived project. Treat archive discovery/restoration as a separate workflow.
- Export success messages do not prove file correctness. The file-upload permission failure is a test-environment limitation, not an application defect. Initial delayed map tiles and summary-only project restoration do not establish data loss or a persistent rendering bug.

## Milestones and tracking

Execute F0–F7 in order. Each milestone should end with its own focused commit, automated checks, and an update here recording the commit, test results, remaining limitations, and browser evidence. Do not mark a milestone complete from code changes alone.

| Milestone | Priority | Status | Commit / evidence |
| --- | --- | --- | --- |
| F0: Reproduce and establish regression cases | P0 | Complete | Browser baseline and repeatable procedures are recorded in the usage report; pointer tab activation was also rechecked against the live app. |
| F1: Restore dependable result navigation | P0 | In progress | Clicks to Analysis/View results did not change the selected pane; workspace navigation by click did work. Alternative cards were observed disabled after completion while the native selector remained available. |
| F2: Correct height validation and error visibility | P0 | Planned | — |
| F3: Fix compatible-run comparison | P0 | Planned | — |
| F4: Make archive recovery discoverable | P1 | Planned | — |
| F5: Reduce Analysis scrolling | P1 | Planned | — |
| F6: Clarify RF results and transient status | P1 | Planned | — |
| F7: Complete real-browser acceptance and usage report | Release gate | Planned | — |

### F0 — Reproduce against the current build

- [ ] Check the running Docker build against the checkout; rebuild/restart if stale, preserving the data volume.
- [ ] Use an isolated QA project and the Oslo endpoints from the report. Record build revision, viewport, terrain inputs, route settings, and saved-run IDs. Never alter the user's existing project for testing.
- [ ] Capture pointer and keyboard navigation with menus/dialogs closed, then with each relevant overlay open. Record the actual pointer target, active pane, focus, and console errors.
- [ ] Capture alternative-card disabled states before, during, and after a route job, including failure/cancel paths.
- [ ] Capture native validity for 3, 6, and 10 m presets and inspect the actual comparison payloads for the saved height runs.
- [ ] Add minimal regression fixtures/tests using the repository's existing test conventions. If browser-event tests are missing, add a small browser test harness rather than relying only on string/source assertions.

Acceptance: each confirmed defect has a failing test or repeatable browser procedure; hypotheses remain labeled as hypotheses. Suggested commit: `test: reproduce browser workflow regressions`.

### F1 — Result navigation and alternative state

Primary files: `web_assets/app.js`, `web_assets/index.html`, `web_assets/workspace.css` under `src/rf_router_planner/`.

- [ ] Trace `setMainView`, tab click handlers, View results, and overlay hit testing. Fix the demonstrated cause without layering duplicate click handlers or arbitrary z-index overrides.
- [ ] Centralize/reuse alternative availability rules across cards and the native selector. Investigate cards rendered while busy that are never unlocked by `setBusy(false)` or coverage/project transitions.
- [ ] Keep stale results and genuinely locked operations protected; leave other valid alternatives selectable after completion. Represent the selected alternative clearly without disabling all choices.
- [ ] Keep alternative, map, link table, profile, and coverage-source selection synchronized. Changing a selection must not launch a new route search.

Acceptance: pointer, touch, and keyboard can switch Map/Analysis and use View results/Show route on map repeatedly. Every available alternative can be selected after a completed search; cancel/error/project-switch paths restore correct states. Verify with the six real Oslo alternatives and event-driven tests. Suggested commit: `fix: restore analysis navigation and route alternative selection`.

### F2 — Height validation without hidden blockers

Primary files: `web_assets/app.js` (`renderHeightScenarioControls`, project transition validation), associated HTML/styles, backend height schema/tests.

- [ ] Align height input precision with the supported schema, using `step=0.1` if tenths are supported; keep exact 3, 6, and 10 m presets. Do not silently round a user's height to bypass validation.
- [ ] Test minimum, maximum, blank, negative, non-finite, and unsupported-precision values in both UI and backend.
- [ ] Separate transient scenario-form validation from saved-plan validation. An unapplied invalid scenario must not block unrelated project actions; invalid saved plan edits must not be silently discarded or saved.
- [ ] When an operation genuinely requires an invalid field, show a visible action-level error, reveal its section, and focus/scroll to the field. Error text must not be hidden in optional help.

Acceptance: all presets satisfy native validity and can run a scenario. Project create/switch/duplicate works with valid saved data regardless of an unrelated scenario draft. Required invalid fields are visible and actionable at desktop and mobile sizes. Suggested commit: `fix: align height presets and scope form validation`.

### F3 — Value-based comparison compatibility

Primary files: `web_assets/app.js` comparison helpers, `web.py` coverage listing/comparison endpoints and `_terrain_fingerprint`, coverage comparison tests.

- [ ] Inspect the current frontend `baseline.terrain_fingerprint !== scenario.terrain_fingerprint` check: array/object reference inequality is not content inequality. Confirm the API payload shape and reproduce separately parsed equal fingerprints.
- [ ] Use a shared, schema-aware value comparison for compatibility metadata. Normalize only ordering/representation that is explicitly non-semantic; preserve meaningful terrain differences.
- [ ] Keep the backend authoritative. Ensure UI reasons agree with backend requirements for terrain, grid, client, radio, and model compatibility while allowing supported height scenarios.
- [ ] Add equal-value/different-object, changed terrain, stale terrain, incompatible grid, missing metadata, and reload tests. Preserve safeguards rather than enabling every comparison.
- [ ] Exercise difference overlay, legend, target-report comparison, and selecting/reloading saved runs without starting RF work.

Acceptance: same-terrain 6 m and 10 m runs compare successfully; genuinely incompatible runs remain blocked with accurate reasons. Reloading does not change compatibility. Suggested commit: `fix: compare saved coverage compatibility by value`.

### F4 — Archive and restore lifecycle

Primary files: `project_store.py`, project endpoints in `web.py`, project controls in `web_assets/app.js` and `index.html`.

- [ ] Audit archive/list/active-project semantics and retained files before adding restoration. Add workspace-scoped archived listing and restore operations if absent, with cross-workspace and busy-project safeguards.
- [ ] Add an explicit Archived projects entry with name/date and Restore action. Label previous-revision recovery distinctly so it cannot be confused with archive restoration.
- [ ] After archiving the active project, select another existing active project deterministically. If none remain, show a deliberate empty/new-project state; do not silently create timestamp-named clutter.
- [ ] Explain retained storage and distinguish archive, restore, cache clearing, and permanent deletion. Restoring must retain project identity and the data that archive promises to preserve.
- [ ] Test active/non-active archive, restore, repeated requests, no remaining active projects, reload, and workspace isolation.

Acceptance: a QA copy can be archived, found, restored, and opened with saved plan/terrain intact. Another active project is selected without creating an unwanted blank. No user project is deleted. Suggested commit: `feat: add discoverable archived project recovery`.

### F5 — Compact Analysis without losing diagnostics

Primary files: `web_assets/index.html`, `workspace.css`, `style.css`, `app.js`, and profile rendering/inspection code.

- [ ] Replace the tall alternative-card stack with a compact alternative picker and selected-route summary. Make a fuller comparison an explicit disclosure rather than the initial wall of content.
- [ ] Put route distance, bottleneck margin, repeater composition, and achieved/requested independent paths near the top. Keep shortfall warnings visible.
- [ ] Provide a clear link selector and immediately accessible profile; move detailed directional budgets and secondary metadata into labeled disclosures or a compact details view.
- [ ] Preserve selected route/link when switching to Map and back. Clearly distinguish a restored summary from a full inspectable result, with a direct rerun action when needed.
- [ ] Fix panel overflow and action-row clipping without adding another large sticky block. Support touch targets, keyboard focus, screen readers, and 200% zoom.

Acceptance: at 1280 × 720 the selected-route summary and link/profile controls are reachable without scrolling past all alternatives. The map retains its full dedicated panel. At narrow widths, content reflows without inaccessible controls or page-wide horizontal overflow. Suggested commit: `refactor: compact route analysis and expose primary diagnostics`.

### F6 — Truthful labels and contextual feedback

- [ ] Replace ambiguous standalone “Valid” labels with concise RF-feasibility wording distinct from geometric LOS/Fresnel obstruction. Explain the propagation model in optional help; do not change RF thresholds or hide negative clearance values.
- [ ] Keep terrain-only assumptions, stale/partial/unknown results, and redundancy shortfalls visible next to affected results. Never imply a prediction is measured coverage or a guarantee.
- [ ] Audit status/toast ownership and clear obsolete operation messages after completion or project/view changes. Persistent errors must remain accessible until resolved/dismissed appropriately.
- [ ] Keep normal help collapsed while action errors and essential scientific caveats remain visible. Preserve one-click ready route and single combined download confirmation.

Acceptance: the reported obstructed-but-RF-feasible Oslo links are understandable without contradictory labels; current action feedback is visible and never obscures primary controls. Suggested commit: `fix: clarify RF feasibility and contextual workflow feedback`.

### F7 — Close acceptance gaps with real data

- [ ] Rebuild if needed and repeat all report workflows through the actual browser, including every changed control. Record pass/fail/blocked/not-tested explicitly.
- [ ] Exercise route/terrain/coverage completion, cancel, retry, reconnect/reload, stale-result handling, duplicate-click prevention, saved-run reuse, and project changes. Confirm display-only changes trigger no RF jobs.
- [ ] Test ready route (one click), missing terrain (one combined confirmation), coverage preview/full (one confirmation), and dialog cancel (no job starts).
- [ ] Verify plan JSON import/export round-trip, route CSV/GeoJSON, coverage JSON/GeoJSON/HTML, and target GeoJSON import with valid and malformed fixtures. Inspect actual file contents, coordinates, assumptions, IDs, and partial/stale labeling—not only download notifications.
- [ ] If browser file access remains blocked, request explicit permission or a user-assisted upload; document the limitation and use backend fixture tests as complementary evidence, not a claim of browser completion.
- [ ] Verify point/road/area targets and cross-run reports, height apply/reset, failure scenarios, coverage modes/layers/area drawing, router policies, and project revision recovery.
- [ ] Test destructive workflows only on disposable QA data: cancellation first, then authorized QA-only deletion/cache clearing. Never clear shared/user terrain as a test shortcut.
- [ ] Check 1440 × 900, 1280 × 720, 1024 × 768, 768 × 1024, 390 × 844, 360 × 640, 844 × 390, and 200% zoom; include pointer, keyboard, focus order, help disclosures, and error visibility.
- [ ] Run the relevant frontend/backend regression suites, then update the usage report with build/commit, screenshots or precise observations, timing, remaining limits, and a full function inventory. Update the original simplification plan's runtime-acceptance status only where evidence supports it.

Acceptance: all P0 fixes verified in the real UI and regression tests; all other checklist items pass or carry an explicit unresolved limitation. Any untested function prevents a claim that “every function” was verified. Suggested commit: `test: verify real-map workflows and document UI acceptance`.

## Completion criteria

The fix list is complete only when navigation, presets, comparison, and archive recovery work end to end; Analysis meets the density/accessibility checks; exports/imports have evidence beyond notifications; and both this checklist and the usage report accurately distinguish completed checks from unresolved limitations. Do not expand this effort into RF-model changes, automatic arbitrary-area terrain downloading, or a new general UI rewrite.
