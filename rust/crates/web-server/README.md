
# RepeaterNet web server

This crate builds the `repeaternet` executable. It hosts the embedded dashboard
and owns HTTP validation, workspace state, job admission, provider calls and
publication to the project and coverage stores. The
[application reference](../../README.md) lists all launcher options, settings
and HTTP routes.

## Main modules

| Module | Responsibility |
| --- | --- |
| [`main.rs`](src/main.rs) | CLI, data directory resolution, listener and shutdown signals |
| [`server.rs`](src/server.rs), [`security.rs`](src/security.rs), [`assets.rs`](src/assets.rs) | Axum router, token and workspace cookies, origin checks, embedded UI and discovery |
| [`api.rs`](src/api.rs), [`catalog.rs`](src/catalog.rs) | Workspace extraction and serialization, project operations, defaults and state |
| [`route_http.rs`](src/route_http.rs), [`route_inputs.rs`](src/route_inputs.rs), [`route_output.rs`](src/route_output.rs) | Scheduler, route input validation, job lifecycle and result serialization |
| [`terrain_http.rs`](src/terrain_http.rs), [`provider_http.rs`](src/provider_http.rs) | Terrain upload, readiness and generation publication, Kartverket and CoreScope |
| [`coverage_http.rs`](src/coverage_http.rs), [`coverage_runs.rs`](src/coverage_runs.rs), [`coverage_inspect.rs`](src/coverage_inspect.rs) | Coverage estimate, preview, persistence, paging and exact-point jobs |
| [`coverage_analysis.rs`](src/coverage_analysis.rs), [`targets.rs`](src/targets.rs), [`downloads.rs`](src/downloads.rs) | Scenarios, target validation and reports, and export responses |

## Contracts and limits

- Projects are shared by every browser by default; the workspace cookie holds
  each browser's active project. `--private-sessions` makes the cookie select a
  private workspace instead. The optional shared access token gates dashboard
  access. The server serves embedded assets with
  explicit media types. Rebuild to include asset changes.
- The shared planner scheduler defaults to one worker with room for two
  outstanding jobs including the running job. Route search, terrain
  preparation, saved coverage and exact-point inspection use job permits.
  Terrain uploads have separate admission limits.
- Workers release the workspace lock during computation and verify identity
  before publishing. Native route results and the selected alternative are
  persisted in SQLite and can be restored when their plan and terrain match.
  The coverage store keeps ordered JSONL cells and progress manifests.
- Terrain uploads accept up to 512 MiB per file. Candidate grids are bounded
  at two million points and route profiles at 65,536 samples. Coverage
  preview is bounded at 256 cells and 4,096 source evaluations. Saved cell
  pages return up to 256 rows. [Persistence layout](../../../docs/DEVELOPMENT.md#persistence-layout)

## Tests

From `rust/`, run `cargo test --locked -p repeaternet`. Router tests exercise
cookies, workspace and project operations, routing, coverage, cancellation and
provider adapters with local fixtures and mock upstreams. The
[executable test](tests/executable_route.rs) runs the binary on loopback with
temporary data, drives route and coverage endpoints, then checks restart.

From the repository root, run `node --test rust/crates/web-server/tests/js/`.
These tests use source extraction and stubs for dashboard events, terrain,
road overlays and coverage inspection. The
[development guide](../../../docs/DEVELOPMENT.md#dashboard-assets) describes
asset registration and browser verification.
