# RepeaterNet optimizer

`repeaternet-optimizer` searches for RF-certified networks in projected metre coordinates. It depends on `repeaternet-rf-core` for RF site geometry, antenna settings, link results, and the terrain sampling interface. It has no Python runtime dependency.

## API

```rust
use repeaternet_optimizer::{
    CandidateSettings, OptimizationError, OptimizationRequest, OptimizationResult,
    PlanningSite, RouteOptimizer, SearchEffort, SearchHooks,
};
use repeaternet_rf_core::{RfSettings, TerrainSource};

fn plan(
    terrain: &(dyn TerrainSource + Sync),
    bounds: [f64; 4],
    rf: &RfSettings,
    clients: Vec<PlanningSite>,
    fixed_routers: Vec<PlanningSite>,
) -> Result<OptimizationResult, OptimizationError> {
    let settings = SearchEffort::Balanced.resolve(&CandidateSettings::default());
    RouteOptimizer::new(terrain, rf, &settings).optimize(
        OptimizationRequest {
            clients,
            fixed_routers,
            terrain_bounds: Some(bounds),
            ..OptimizationRequest::default()
        },
        SearchHooks::default(),
    )
}
```

- Bounds use `[min_x, min_y, max_x, max_y]`. Supply each client and fixed router as a `PlanningSite` with projected coordinates and sampled ground elevation. Set the RF site kind, origin, router policy, and any explicit height override. Existing-only searches can run without generation bounds. An explicit `candidates` pool also bypasses generation and its cap on generated sites.
- `with_site_filter` accepts a fallible projected-point batch predicate that can enforce road proximity before generated-site scoring and thinning. Fixed sites bypass this predicate. The adapter may be called several times during one search.
- `RoutingGraph::select_path` ranks a single path. `solve_topologies` works on caller-supplied links and returns the best found topology at each attainable exact router count. Its input validity and resolution are the caller's responsibility. `RouteOptimizer::optimize` evaluates and certifies links itself. `NetworkSolution::sites` and `links` represent the induced mesh. `primary_route()` exposes the first representative client path. `select_solution` changes the active alternative by index.
- `SearchHooks` accepts progress, cancellation, and owned solution snapshots. `KeepCertified` returns the published snapshot with status `Stopped` and `search_complete = false`. `Discard` and a keep request before the first certification return `Cancelled` without an alternative. Snapshots omit the broad search graph. In a completed return, `search_links` can include unused coarse links. Use `OptimizationResult::links()` or the alternative's links for final-resolution results.
- Coarse RF screening uses up to `parallel_workers` scoped threads per optimization. `1` runs in the caller. `0` automatically uses one fewer than the available logical CPUs for at least 200 pairs, with a minimum of one. Explicit counts above one also apply to smaller batches. Terrain and custom `LinkEvaluation` adapters must be `Sync`. Site filters and search hooks run on the caller thread and can capture thread-local state.
- Coarse results and progress are consumed in input order, with at most one outstanding pair per worker. Cancellation stops dispatching pairs and releases cache waiters. Scoped workers finish any RF calls already in progress before `optimize` returns. The exact low-hop search, final retries, final certification, and profile preparation run in the caller.
- `with_cache` enables a shared scalar LRU with a global entry bound and single-flight computation for identical keys. Pass a project namespace and a revision that changes whenever RF settings, terrain, antenna patterns, or the evaluator implementation change. Keys also distinguish site identity, RF geometry, and sampling step. Detailed profiles bypass the cache. `stats` and `clear` are namespace-scoped. Clear a namespace when its jobs are idle. Capacity zero disables storage and coalescing.
- `refine_site` searches a disk around an unlocked, nonrequired generated candidate using medium-resolution adjacent links and the configured site filter. It ranks positions by minimum link margin and breaks ties by minimum Fresnel clearance. `reduce_mast_heights` tries ascending heights for each interior route site using final-resolution adjacent links and preserves the site's height if no setting qualifies. Both helpers are opt-in. The active optimizer uses coarse and final sample steps and initializes router heights from the configured maximum when height optimization is enabled and the site has no override. `refine_radius_m` still affects active candidate spacing.

## Implemented coverage

| Area | Behavior |
| --- | --- |
| Candidate selection | Square-ended corridors, rectangular multi-client search, exclusions, ground and surface sampling, slope and obstruction penalties, per-cell thinning, and round-robin route bands. Fixed candidates consume budget first. Proposed-only, existing-only, and combined policies honor optional, required, and excluded router choices. Quick and balanced effort caps match the web planner. |
| Pair screening | Direct and terminal-to-candidate coverage, distance and optimistic margin checks, indexed nearest neighbors with stable index ties, and a principal-axis backbone with a budget for known sites. |
| RF certification | Exact final-resolution zero-, one-, and two-router checks for minimum-router two-terminal searches. The two-router frontier is streamed. Sparse coarse results guide larger searches. Coarse rejections get another check at final resolution. Selected subsets are repeatedly checked at final resolution, including previously unscreened pairs and subsets up to two routers above the return limit. Profiles are restored for all returned induced links. |
| Graph and topology | Minimum routers, fewest new installations, minimum infrastructure, and maximum reliability with their respective tie-break stages. Enabled clients and required routers must form one connected component. Exact subset comparison through 100,000 combinations per count, then deterministic width-64 beam search with shortest-path, disjoint-path, hub, and metric-closure seeds. Alternatives report the search method used. Vertex-capacitated flow supplies representative internally node-disjoint paths and the minimum achieved count across client pairs. |
| Lifecycle | Cooperative cancellation during generation, screening, subset search, validation, and profile preparation. Detached certified snapshots can be retained while the search continues. Scalar RF metrics can be reused across runs within a namespace and revision. |

## Verification

```powershell
& "$env:USERPROFILE/.cargo/bin/cargo.exe" test --offline --locked --manifest-path rust/crates/optimizer/Cargo.toml
& "$env:USERPROFILE/.cargo/bin/cargo.exe" clippy --offline --locked --all-targets --manifest-path rust/crates/optimizer/Cargo.toml -- -D warnings
```

The tests adapt scenarios from Python's graph, topology, screening, synthetic optimization, road access, parallel evaluation, and cache suites. They cover ordered results under out-of-order evaluation, equivalent serial and parallel RF meshes and progress, bounded cache storage, concurrent cache misses and retry, cancellation with active workers, stopping with a certified snapshot, and the opt-in helpers.

## Remaining parity and integration

- Register the crate in the parent workspace and remove its temporary `[workspace]` section. The host still owns CRS transforms, terrain readiness checks, projection of fixed sites, request and result DTOs, topology-derived web IDs, job persistence, and the road filter and overlay adapters.
- Python uses child processes only for reopenable raster terrain and batches of at least 200 pairs. Rust shares terrain with threads and reports coarse progress for each consumed pair. Workers can populate the shared cache out of order, so LRU residency and hit counts may vary with scheduling. Search metrics, alternatives, and progress order are deterministic for the same RF inputs.
- Rust orders optimizer link endpoints by site ID and relies on the caller's RF and terrain revision for cache invalidation. Missing terrain is treated as an unavailable link and can be retried at final resolution. Other typed RF errors propagate, while Python skips `ValueError` in search pair evaluation. The opt-in mast helper propagates sampling errors and retains the failing site's last committed height.
- The native representative-flow and Steiner seed algorithms can select different equal-quality paths or bounded heuristic winners from NetworkX. The minimum-router low-hop pass runs before broad screening and retains the final-valid links it discovers. Python runs it afterward and merges the winning route, so exact-count alternatives can differ if Rust's pass adds more edges to the search graph. For fewest-new installations, Rust's additional paths avoid the preferred path's internal nodes and cannot count a preferred direct edge again.
- Full HTTP payload, raster-backed performance, and large-scale Python-to-Rust parity still need integration coverage.
