# RepeaterNet coverage

`repeaternet-coverage` is the synchronous native coverage engine. It uses
`repeaternet-rf-core` for link evaluation and `repeaternet-terrain` for projected
geometry, profile limits and raster access. Callers can supply either the
array-backed or GeoTIFF-backed terrain via the shared `TerrainSource` trait. The
engine requires no Python runtime.

## Integration

Register `crates/coverage` in the Rust workspace and add a path dependency on
`repeaternet-coverage` where the server invokes it. Construct a
`CoverageEvaluator::new(&terrain, &rf_settings, &transformer)` with a terrain CRS
accepted by `repeaternet-terrain` and use the certified route's native RF sites.

For scalar reuse, attach a shared `CoverageMetricsCache` with `with_cache`.
Supply a revision specific to the project and its current terrain snapshot.
The evaluator adds the typed RF settings, native terrain resolution and
surface availability to that revision. Each entry also includes source and
target geometry, height reference and role, the client radio and effective
profile step. The evaluator checks the requested sample cap before looking up
a link. The LRU retains bounded scalar results for finite evaluated links.
Missing terrain, errors, cancellation, and full profiles bypass storage. A job
waiting for a concurrent fill uses 10 ms waits between cancellation checks.
In-flight terrain reads and profile evaluation are cooperative and finish the
current call before cancellation is observed.

1. Deserialize `CoverageSettings` and call `.normalized()` before saving or
   fingerprinting settings. Use `select_sources` with the active solution and
   its candidate sites. The function preserves route order for margin ties.
   For a height scenario, `apply_height_overrides` returns adjusted source
   clones. `revalidate_backbone` uses the active route and those clones to
   recheck affected certified router edges at the coverage profile step, as in
   the Python web handler.
2. Resolve the requested area with `resolve_area`. View bounds use
   `[south, west, north, east]`, drawn points use `[longitude, latitude]` and
   projected bounds use `[min_x, min_y, max_x, max_y]` in metres.
   `CoverageArea::geometry` exposes the actual masked region for terrain planning.
3. Use `estimate` for the bounded grid and ground-availability counts without RF.
   Use `calculate` for the full grid with `Sync` terrain. It uses scoped
   workers for larger runs. Use `with_parallel_workers(1)` to select one
   thread or `calculate_serial` for a non-`Sync` terrain backend. Pass
   `settings.preview_settings()` for
   the 256-cell and 4,096-pair preview limits while retaining the original
   settings for the HTTP response. `area_bounds_wgs84` and `cell_footprint`
   provide the rendering geometry. The caller supplies route identity, search
   status, elapsed time and preview metadata.
4. Set `RunOptions::on_chunk` for persistence or paging. The sink receives
   complete, ordered cell rows, and the returned grid has an empty `cells` list.
   Without a sink the rows are retained. `write_cell_chunk` and
   `read_cell_stream` use the Python JSONL format with one array per line.
   Supply a per-line byte limit when reading. A sink error propagates to the
   caller. A cancellation before evaluation returns `CoverageError::Cancelled`.
   During evaluation the engine returns the ordered prefix accepted before
   cancellation and flushes any pending complete cells on the calling thread.
   The caller owns job state and can choose whether to commit
   a final partial chunk. Cancellation is polled between sources and terrain
   batches, with one bounded RF profile evaluation per source call.
5. Use `analyze_node_failures` with a `CoverageNetwork` built from certified
   links and selected repeater IDs. Use `compare_runs` with the typed comparison
   fields of complete manifests, fallible cell streams and the current terrain
   fingerprint. The server authorizes the manifests and opens result files.
   Both operations use same-source two-way validity independently of the grid's
   display mode. Unresolved backbone edges are counted separately from failures.
6. Use `evaluate_point` for exact WGS84 inspection. Pass the candidate settings'
   final profile step and the saved coverage client's settings and adjusted
   sites when inspecting a historical height scenario. The optional native
   serving link can be encoded with the server's route-link DTO. Source site
   metadata and mesh component labels come from the caller's route snapshot.
7. Use `normalize_targets` on GeoJSON, `assess_targets` with the immutable
   coverage grid and rows, and `compare_target_reports` with typed saved-report
   metadata. Assessment uses `Sync` terrain to evaluate larger sets of
   independent road samples in parallel and accumulates their lengths in road
   order. It computes exact point and length-weighted road metrics
   and intersects polygon targets with clipped grid cells. It tracks passing,
   failed, unknown, unresolved and outside measures and enforces target,
   sample, evaluation and scan limits. As in Python, assessment clips to the
   grid rectangle except for drawn coverage, which also clips to the drawn
   polygon. The caller binds the report to its saved run and owns report IDs,
   persistence, staleness and quotas.

## Compatibility and remaining work

- Settings, cell rows, JSONL chunks and network/report result fields use the
  Python JSON names and nullable margins. `ComparisonRun` and
  `SavedTargetReport` decode the fields needed for comparisons. Keep the
  original manifest when preserving project data, provenance or route radio
  settings.
- Use `MODEL_VERSION` for new native runs. `PYTHON_MODEL_VERSION` identifies
  historical Python rows. Compatibility checks require model versions to
  match. The Rust comparison totals count lost cells toward the baseline and
  gained cells toward the scenario. Python coverage-v3 transposed those two
  aggregate totals and percentages.
- The terrain crate documents supported projected CRSs and GeoTIFF encodings.
  The native projection and polygon engine may differ numerically from PROJ
  and GEOS near geometry boundaries. Fixture comparisons specify coordinate
  and RF margin tolerances. [Terrain support](../terrain/README.md)
- The HTTP job lifecycle, route certification and lookup, persistent reuse and
  fingerprints, paging quotas, inspection supersession, supplemental terrain
  downloads and exports belong to the server and storage integration. Broader
  coverage performance remains unverified.

## Verification

Run `cargo test -p repeaternet-coverage` from `rust`. The fixture in `tests/fixtures/coverage-python-v3.json` freezes Python
defaults, flat/ridge/valley results, asymmetric margins, missing ground and
missing path samples, and profile-limit rejections. The Rust tests also cover
grid orientation and masking, preview limits, interrupted streaming, failed
routers, capped backbone revalidation, saved-run compatibility, inspection and
point, road and polygon assessments. The fixture is a frozen oracle; its generator remains available at commit `18b7cee`.

## Cache benchmark

Run these commands from the repository root with the same Cargo toolchain used
for the Windows release. Each timed run evaluates 8 sources over 1,024 cells
and streams hashed JSON chunks. Run the example three times in fresh
processes for each fixture. The Python rows below were measured with
`tools/benchmark_mesh_coverage.py` at commit `18b7cee` and are historical.

```powershell
Push-Location rust
cargo build --locked --release -p repeaternet-coverage --example benchmark_cache
Pop-Location
$tile = 'cache/wcs_tiled_smoke/kartverket_dtm_03a40f293cb87254cb21.tif'
1..3 | ForEach-Object { .\rust\target\release\examples\benchmark_cache.exe }
1..3 | ForEach-Object { .\rust\target\release\examples\benchmark_cache.exe $tile }
```

The flat synthetic terrain is generated by each harness. The raster run used
a local 400 × 400 Kartverket DTM with 128 × 128 TIFF blocks in EPSG:25832
at 50 m resolution, with
SHA-256 `FD8D0D1CEA07A743D79C072061C7FDC7C5ABFE5A4A896958EB7C056290C13A5B`.
The raster is an optional local cache fixture.

Medians of three fresh processes per row on Windows 11 x64, Ryzen 9 7900X,
Rust 1.100.0 release and Python 3.13.12. The Rust baseline uses the preserved
benchmark executable from before the sampling change. The candidate and the
Python reference were rerun on the same machine.

| Fixture | Engine | Cold | Warm | Peak working set | Cancel acknowledgement |
| --- | --- | ---: | ---: | ---: | ---: |
| Flat array | Rust baseline | 38.0 ms | 4.80 ms | 8.0 MiB | 11 µs |
| Flat array | Rust optimized | 32.8 ms | 4.28 ms | 8.1 MiB | 33 µs |
| Flat array | Python | 1.56 s | 161 ms | 116.1 MiB | 23 µs |
| Kartverket DTM | Rust baseline | 12.49 s | 9.41 ms | 10.7 MiB | 34 µs |
| Kartverket DTM | Rust optimized | 119 ms | 4.63 ms | 10.1 MiB | 15 µs |
| Kartverket DTM | Python | 3.47 s | 173 ms | 139.6 MiB | 26 µs |

The raster cold runs ranged from 12.44 to 16.17 s in baseline Rust, 117 to
143 ms in optimized Rust, and 3.45 to 3.58 s in Python. The optimized median
was 105× faster than the Rust baseline and 29× faster than the Python
reference on this tile. Rust median time to first chunk fell from 1.48 s to
30.6 ms. Both engines reused all 8,192 links and reproduced their own cold
output byte-for-byte on the warm run. The Rust hashes also matched before
and after on both fixtures. All cases reported 1,024 covered cells on flat
terrain and 1,013 covered cells with 11 uncovered cells on the raster.
Numerical cross-engine parity relies on the tolerance-based tests above.

For a separate profiling run, a local instrumented copy of the baseline and
`tiff-reader` measured cumulative elapsed time around each sampling stage
using `Instant`. On the 12.95 s cold run, 14,782 window reads took 12.66 s
in total. Collecting the typed windows into `Vec<f64>` via the ndarray iterator
took 11.66 s. Inside the typed reads, row copies took 599 ms, byte-to-type
conversion took 193 ms, and output allocation took 139 ms. The 59,128
decoded-block cache lookups took 4.79 ms including lock acquisition. The 15
misses after the open-time read spent 0.32 ms reading file payloads and
0.24 ms decoding them. These timers describe the single-threaded benchmark.

Cold starts with an empty application cache. The OS file cache is left in
its ordinary state. Each harness opens the terrain before timing, including
the Rust first-block validation read. Times include per-engine grid preparation,
sampling, and JSON chunk serialization. Peak working set includes the entire
benchmark process and its dependencies. The cancellation probe signals at the
end of the 16th profile terrain sample in Rust and after the 16th RF evaluation
in Python. Rust committed one complete cell and Python committed two. The probes
measure engine-level acknowledgement after those boundaries. HTTP, job
scheduling, concurrent read contention, and cancellation during an in-flight
raster read are outside this benchmark.
