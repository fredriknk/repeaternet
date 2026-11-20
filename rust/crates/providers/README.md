# RepeaterNet providers

Native blocking adapters for Kartverket NHM WCS, OpenStreetMap Overpass, and CoreScope. The crate uses rustls with platform trust roots and the native terrain crate. The Kartverket service table is embedded at compile time. Python and an external configuration file are unnecessary at runtime.

## Integration

- **Kartverket.** Use `KartverketProvider::new` with `WcsConfig::default()` and `TileLimits::default()`. Defaults are 400 km² and four million pixels per tile, 256 tiles per plan, and a 50-million-total-pixel planning budget. Use `plan_download` for bounds, a corridor with optional fixed sites, or projected polygon geometry. `estimate` reports transfer and disk estimates, free space at the supplied existing directory, and expected cache hits. `plan_coverage_extension` accepts missing projected footprints per product and returns separate optional DTM and DOM plans. Use `fetch_plan` for ordered tile paths and per-tile progress, or `fetch` for a single tile and byte progress. Fetch validates both cached and downloaded rasters before returning them. Estimates count cache files by name so a corrupt entry can yield an estimated hit followed by a download.
- **Generation publication.** Pass the fetched paths to `StagedGeneration::copy_from_cache`. It copies the products in caller order into a temporary sibling directory. Drop the stage to discard it. Hold the project commit lock while rechecking job identity and calling `publish` with a fresh generation name. Both products become visible under that name in one rename. The caller owns workspace identity checks, catalog updates and generation cleanup. The server discovers published generations by directory name. A failed or cancelled preparation can leave reusable validated shared-cache tiles.
- **Overpass.** Construct `TiledRoadAccess` with the route CRS, cache directory, distance limit, and public or all-drivable classes. It reads `RF_PLANNER_OVERPASS_URL` and defaults to the public interpreter. Pass candidate points after corridor, terrain, and exclusion filtering to `allowed`. Reuse the same instance for batches and `overlay`. The overlay takes router locations per alternative and returns shared WGS84 polylines in Leaflet order, indices per alternative, and nearest distances in metres.
- **CoreScope.** Use `CoreScopeClient::new(DEFAULT_BASE_URL)` and call `fetch_repeaters` with `DEFAULT_MAXIMUM_PAGES` (20). The client uses `https://corescope.eth0.no` by default in the server and requests up to 500 records per paginated request. `filter_corridor` takes geographic endpoints, a half-width in metres, and a UTC clock value for deterministic freshness labels. It returns the router records and count for the dashboard.

`shared_cache_directory` resolves `RF_PLANNER_CACHE_DIR` or defaults to `<data>/shared-cache`. Kartverket and Overpass preserve the Python cache naming schemes. Each adapter offers `with_client` for an injected blocking reqwest client and supports a custom endpoint. Run network and raster operations on a blocking worker. Cancellation is cooperative between transport reads, retries, processing chunks, and tile operations. A blocked connection or read returns control when it completes or the HTTP timeout expires.

The terrain reader limits WCS downloads to supported single-band GeoTIFF encodings in EPSG:25832, 25833, or 25835. Rust geometry operations can produce different floating-point vertices and ordering from Shapely. The adapters expose provider results and errors for the server to map to its HTTP and job contracts. The web-server tests exercise the wired provider routes with local upstreams. Live upstream checks are tracked in the [roadmap](../../../docs/ROADMAP.md).

## Verification

From `rust/`:

```sh
cargo test --locked -p repeaternet-providers
cargo clippy --locked -p repeaternet-providers --all-targets -- -D warnings
```

Tests use loopback HTTP servers, temporary caches, and tiny synthetic GeoTIFFs. They check Python-compatible requests and cache keys, tile geometry, raster rejection, retries, cancellation and publication, road proximity and overlay reuse, and CoreScope pagination and corridor records.
