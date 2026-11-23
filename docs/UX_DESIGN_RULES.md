# UX design rules

Binding rules for anyone (human or agent) changing the RepeaterNet dashboard in
`rust/crates/web-server/assets/dashboard/`. They come from the 2026-11-20 usability
review. When a change conflicts with a rule, change the rule here in the same
commit and say why.

The user is an RF hobbyist or planner placing MeshCore repeaters in Norway. They
think in places on a map, in routers and in dB of margin. They do not think in
job IDs, hashes or internal site IDs.

## 1. Information architecture

The layout has one owner per job. Put new features where users already look:

| Area | Purpose | Contents |
| --- | --- | --- |
| Top bar (56 px, one row) | Which project am I in? | Brand, project select, **＋ New**, **⋯** project menu, save state |
| **⋯** project menu | Rare project operations | Rename, duplicate, recover, import/export plan file, **Data sources…**, archive, delete, archived list, cache |
| Left panel tab **1 Route** | Define the problem | Accordion steps: Endpoints, Terrain, Routers, Radio, Search, then a sticky **Find repeater route** |
| Left panel tab **2 Coverage** | Live handheld coverage of every site on the map | Live on/off, per-site list, Display, Sources & range, Handheld, Saved runs (fixed-grid runs for scenarios, targets and exports), sticky live status and **Recalculate coverage** |
| Left panel tab **3 Scenarios** | What-ifs on a finished coverage run | Failure scenario, heights and run comparison, targets |
| **Map** view | See and edit geography | Toolbar, context menu, popups, legend |
| **Analysis** view | Judge the result | Verdict, key figures, alternatives, links, profile, exports |

- Primary tasks must be reachable in at most two clicks from the default screen,
  without opening a disclosure. These tasks are: start or switch a project, set
  A/B, add/move/remove a proposed router, set an existing router's policy, run a
  search, read the verdict, switch alternatives and export.
- Use `<details>` disclosures only for help text, advanced parameters and rarely
  used tools. Never hide a primary feature behind "Customize…" or "Advanced".
- A workspace must not show a wall of collapsed sections with no explanation. If
  its prerequisites are missing, show an empty-state card that names the missing
  prerequisite and has one button that goes there (see `#compare-empty-state`
  and `#coverage-no-route`).
- Exports sit next to the thing they export: route exports in Analysis, coverage
  exports beside the saved-run selector.
- Each workspace has exactly one primary (filled green) button, in its sticky
  action bar. **2 Coverage** is the exception: coverage is live, so its action
  bar shows live status and a secondary **Recalculate coverage** and has no
  primary button. (Changed 2026-11-22 when coverage stopped being a separate
  calculation step.)

## 2. The map is an editor

- Every map object that can be edited must be editable on the map (popup, drag or
  context menu) **and** in the sidebar. Both paths call the same function.
- A plan edit always runs `recordEdit()`, then `invalidate(reason)`, then
  `scheduleCoverageCheck()`. Autosave and undo come from `recordEdit()`. Never
  mutate plan state without this chain. Reuse helpers such as `editEndpoint()`,
  `addManualRouterAt()`, `moveManualRouter()` and `setRouterPolicy()`.
- Select a site or link to open matching read-only details on the map and in its
  details panel. Both offer **Show in analysis**. Open editing with right-click,
  touch long-press or Shift+F10 on the focused map object. A link offers editing
  for either of its sites.
- Respect the edit lock: check `mapEditsLocked()` and wrap new controls with
  `mapEditControl()` so they disable while jobs run or projects switch.
- The map toolbar is a single solid group (white background, shadow, clear
  disabled state). Do not float loose buttons over tiles. Text over map tiles is
  never acceptable without a solid background.
- Map-mode actions (place A/B/router/target, draw, inspect) show a pressed state
  and a hint pill, and Esc or **Cancel** exits them.
- Live coverage follows the map. A new draggable site marker calls
  `bindLiveCoverageDrag(marker, describe)` so its coverage previews while
  dragging; plan edits reach live coverage through `scheduleCoverageCheck()`.
  Never start live coverage requests from anywhere else.
- Fit the map once when a result appears or a project opens (`fitMapSites`).
  Never refit on routine re-renders such as coverage cells or autosave.

## 3. Visual encoding

- **Margin bands** are shared everywhere: links on the map, margin pills,
  metric tiles and the coverage legend.

  | Band | Usable margin | Meaning |
  | --- | --- | --- |
  | strong | ≥ 10 dB | green |
  | good | 5 to < 10 dB | light green / teal |
  | marginal | 0 to < 5 dB | amber |
  | fail | < 0 dB or not valid | red, dashed on the map |
  | stale | result for old inputs | grey, dashed |

  Use `analysisMarginBand()` / `routeLinkMapStyle()`. Do not invent new
  thresholds or colours.
- The live coverage layer uses a user-selectable perceptual colour map
  (Viridis by default; Plasma, Turbo, Cividis, Magma, or **Margin bands** with
  the shared 0, 5 and 10 dB orange, yellow and green ramp) from 0 dB to the
  chosen scale top. Only usable margin (≥ 0 dB) is coloured. Fade the weakest
  few dB so terrain shadows and the basemap stay readable. Overlap highlighting
  uses one user-chosen colour for two sites and a darker shade for three or
  more. Hatch unknown terrain. The legend names every colour and pattern, has dB
  ticks, the colour map choice and an opacity control, and the hover tooltip
  lists each site's margin in dB, so colour never stands alone. (Changed
  2026-11-22: users asked for a prettier, selectable colour map; the margin-band
  ramp remains one of the choices.)
- Saved-run rasters keep the 0, 5 and 10 dB orange, yellow and green ramp, hatch
  unknown terrain and unresolved profiles, and label both patterns in the legend.
  Only one coverage layer (live or a saved run) is shown at a time.
- Never encode meaning by colour alone. Pair colour with text (the pill shows the
  number), shape (marker shapes differ by kind) or line style.
- Show people's labels, not IDs: route sites are `R1, R2…` or the router's name
  via `routeSiteLabel(id)`, endpoints are `A`/`B`. Raw IDs and hashes belong in
  tooltips, popups and exports only.
- One marker per place. Shape says the kind of site: endpoints are circles,
  your proposed routers are hexagonal tags named `P1, P2…`, sites found by the
  search are squares `R1, R2…`, MeshCore routers are round. **Required** is a
  pin badge, never just a thicker border. A proposed router keeps its brown
  hexagonal tag when used in a route. An underline marks route membership.
  Never set `position` on a Leaflet marker icon class; Leaflet positions icons
  absolutely, and overriding it makes markers drift off their coordinates.
  (Added 2026-11-22 after users could not tell which router belonged to which
  point.)
- Lead with the judgement, then the evidence. Analysis order is verdict line →
  four key figures → alternatives → links → profile → detailed budgets. Saved-run
  labels lead with time, grid and heights, never a hash.
- Every map symbol and chart line must be explained by a visible legend (the
  **Map legend** control, `.chart-legend`).
- Show a coverage legend only when coverage data is on the map
  (`syncCoverageLegend`, `renderLiveCoverageLegend`).

## 4. Feedback, state and safety

- Results that no longer match the inputs are marked **stale** in three places:
  the Analysis badge, the verdict and the map style. Exports of stale data
  require explicit opt-in.
- Long jobs use the shared job card with progress and cancel. Once finished it
  becomes a small dismissible toast (bottom-right). Never leave a finished job
  occupying layout space or showing a running clock. Don't show the toast over
  the view it points to (Analysis), and never over navigation on mobile.
- Validate raw input **strings** before converting (`parseCoordinates`,
  `validEndpointCoords`). `Number('')` is `0`; a blank field must never silently
  become a coordinate or a value.
- Show errors next to the field (`aria-invalid`, `.field-error`) and move focus to
  it. Don't throw uncaught errors from click handlers.
- Destructive or expensive actions (delete, clear terrain, large downloads, long
  calculations) go through `askActionDialog` and state **what** and **how much**,
  for example "Remove 6 DTM tiles (183 MiB)". Plan edits are not confirmed; they
  are undoable instead.
- Save state is always visible in the top bar, and a failed save offers
  **Retry save**. Flush pending autosaves before starting a job that locks
  inputs.
- Switching project, alternative or view must never discard computed results or
  start an RF job by itself. Switching to a project without a result must clear
  every trace of the previous project's result (verdict, figures, badge, map
  layers, exports) via `resetRouteResultView()`.

## 5. Layout and sizing

- Required viewports: 1440×900, 1280×720, 1024×768 and 390×844. At each one:
  - no horizontal page overflow;
  - the workspace's primary action is visible without scrolling the page;
  - the map is at least 320 px tall.
- On mobile (≤ 650 px) there is one destination nav (Setup / Map / Analysis). Do
  not show a second tab bar.
- Minimum text sizes:
  - 12 px for hints and table text;
  - 13 px for controls and summaries;
  - 14 px for body copy and step titles.
- Minimum targets: 36 px for click targets, 44 px on mobile.
- Hint text must meet WCAG AA contrast (use `var(--muted)` or darker, never the
  legacy `#849087`).
- Styles are layered: `style.css` (legacy base) → `meshcore.css` → `coverage.css`
  → `workspace.css` (shell, panels, analysis) → `map.css` (map only). Add rules to
  the most specific layer that owns the area. Don't add new rules to `style.css`.

## 6. Keyboard and accessibility

- Every control is reachable and operable by keyboard, with visible
  `:focus-visible` outlines.
- Re-rendering a list or popup must restore focus to the equivalent control (see
  the MeshCore policy select and map policy buttons). Focus must never fall back to
  `<body>` after an action. Dialogs return focus to their opener, or to the menu
  that contained it. Use `element.checkVisibility()` to test whether a target can
  take focus, because content inside a closed `<details>` still reports layout boxes.
- Esc closes menus, popups and map modes. Ctrl+Z / Ctrl+Shift+Z / Ctrl+Y undo and
  redo, but are ignored while focus is in a text field or a dialog is open.
- Accordions use `aria-expanded` and `aria-controls`. Tabs use `role="tab"` and
  `aria-selected`. Toggle buttons use `aria-pressed`. Status text uses
  `role="status"` / `aria-live="polite"`.
- When a result appears outside the viewport (for example a location
  inspection), scroll it into view and focus its heading.

## 7. Copy

- Use sentence case and plain language. Buttons are verbs ("Find repeater route",
  "Download terrain for coverage area").
- Put units in labels with a middle dot: `Antenna height · m`. Give numbers fixed
  precision: dB to 0.1, km to 0.01 in tables and 0.1 in tiles, coordinates to
  6 decimals.
- Explain RF jargon (Fresnel, k-factor, DOM) once in a help disclosure, not inline
  on every row.
- State uncertainty honestly. These are predictions; "Pass" means it meets the
  configured validation mode, not that it will work.
- Don't use marketing copy or badges that carry no information.

## 8. Code conventions for UI work

- `app.js` is a classic script with shared globals and dense lines. Make surgical
  edits and add new behaviour as new named functions near related code.
- A function name must be declared once. Duplicate declarations silently shadow
  each other. Check with:
  `node -e "const s=require('fs').readFileSync('rust/crates/web-server/assets/dashboard/app.js','utf8');const m=[...s.matchAll(/^(?:async )?function (\w+)/gm)].map(x=>x[1]);console.log(m.filter((x,i)=>m.indexOf(x)!==i))"`
- Keep element IDs stable; JS and tests address them directly. If you rename one,
  grep `app.js`, `coverage_inspection.js` and `tests/js/`.
- New asset files must be registered in `rust/crates/web-server/src/assets.rs`.
  Assets are embedded at build time, so rebuild after every asset change.
- Node tests in `rust/crates/web-server/tests/js/` extract functions from
  `app.js` by source boundaries. When you refactor a function they cover, update
  their stubs.

## 9. Definition of done for a UI change

1. `cargo build --release -p repeaternet`. If a running server locks the
   executable, use `--target-dir target/<name>`.
2. Run the binary with the shared test data directory
   (`--data %LOCALAPPDATA%\RepeaterNet\test-data`) and a `--port`. Reuse it so
   terrain tiles in its `shared-cache` aren't downloaded again. Never point tests
   at the user's real data directory.
3. Drive the changed workflow in a real browser (Playwright with the installed
   Chrome is fine). Take screenshots at the viewports in §5 and **look at them**.
   Check the console for errors.
4. `node --test rust/crates/web-server/tests/js/`, plus `cargo fmt --all --check`,
   `cargo clippy --workspace --all-targets --locked -- -D warnings` and
   `cargo test --workspace --locked` from `rust/`.
5. Update `docs/USER_GUIDE.md` when names, locations or workflows change.
6. For major UI changes, regenerate the [guide screenshots](../tools/docs-screenshots/README.md) and update the guide.
