# Agent guide

RepeaterNet is a self-hosted, terrain-aware RF mesh planner for Norway: a Rust
Cargo workspace under `rust/` that builds one `repeaternet` executable embedding a
vanilla-JS + Leaflet dashboard (no frontend build step).

- Architecture, source layout and verification commands: [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md)
- User-facing behaviour: [docs/USER_GUIDE.md](docs/USER_GUIDE.md)
- Open work: [docs/ROADMAP.md](docs/ROADMAP.md)

## UI work: follow the UX design rules

Any change to `rust/crates/web-server/assets/dashboard/` (HTML, CSS, `app.js`,
`coverage_inspection.js`) or to user-visible text **must** follow
[docs/UX_DESIGN_RULES.md](docs/UX_DESIGN_RULES.md). In short:

1. Put features where the information architecture says they live. Primary
   tasks are at most two clicks away and never hidden in a disclosure.
2. The map is an editor. Map and sidebar edits share one code path
   (`recordEdit` → `invalidate` → `scheduleCoverageCheck`).
3. Use the shared margin bands and colours, never colour alone, and show
   people's labels (`routeSiteLabel`), not IDs.
4. Validate raw strings before converting. Confirm destructive actions with
   their scope. Keep save, stale and job state visible.
5. Check 1440×900, 1280×720, 1024×768 and 390×844, plus keyboard and focus.
6. Done means you built it, drove it in a real browser with the shared test
   `--data` directory, **looked at the screenshots**, ran the Node and Cargo
   tests, and updated the user guide when you changed names or flows.

## Working conventions

- Run Cargo from `rust/` (toolchain pinned by `rust/rust-toolchain.toml`). On
  Windows, `cargo` may live in `~/.cargo/bin`.
- Run one server process per data directory. Never test against the user's real
  data directory (`%LOCALAPPDATA%\RepeaterNet\data`).
- Test with the persistent `%LOCALAPPDATA%\RepeaterNet\test-data` directory. Its
  `shared-cache` holds downloaded terrain tiles, so don't create a fresh data
  directory per test. If a test needs an empty workspace, use a new `--data`
  directory but set `RF_PLANNER_CACHE_DIR` to `test-data\shared-cache`.
- Dashboard assets are embedded at compile time, so rebuild to see changes.
  Register new asset files in `rust/crates/web-server/src/assets.rs`.
- Backwards compatibility with old saved data is not required while the project
  is pre-release, unless a task says otherwise.
