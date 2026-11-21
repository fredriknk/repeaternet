# RepeaterNet providers

Native blocking adapters for worldwide elevation sources, OpenStreetMap
Overpass, and CoreScope. The crate uses rustls with platform trust roots and
the native terrain crate.

## Integration

- **Elevation sources.** `sources::ProviderRegistry` merges the embedded
  Kartverket, USGS and Copernicus providers and map layers with
  `<data>/providers.json`. The file can add definitions or disable built-ins.
  `RF_PLANNER_PROVIDERS_FILE` selects a different path. Definitions are
  validated with field paths and saved by atomic replacement. The web server
  reloads on new requests and gives each job a registry snapshot.
- **Terrain downloads.** `sources::TerrainDownloader` ranks enabled
  providers per tile and product, supports a user pin with optional fallback,
  and preserves existing terrain CRS for supplemental coverage. The shared
  grid defaults to 400 km² and four million pixels per tile, 256 tiles, and an
  automatic 50-million-pixel planning budget. Estimates include effective
  resolution, primary sources, disk space and composed cache hits.
- **Transport.** WCS supports GetCoverage 1.0.0 and 2.0.1. Single and tiled
  COGs read only the TIFF metadata and byte chunks needed for the chosen
  overview and output pixels. Upstream elevations are resampled into the
  projected target grid. Later sources fill gaps, and composed GeoTIFFs are
  opened and decoded before publication.
- **Generation publication.** Pass the fetched paths to `StagedGeneration::copy_from_cache`. It copies the products in caller order into a temporary sibling directory. Drop the stage to discard it. Hold the project commit lock while rechecking job identity and calling `publish` with a fresh generation name. Both products become visible under that name in one rename. The caller owns workspace identity checks, catalog updates and generation cleanup. The server discovers published generations by directory name. A failed or cancelled preparation can leave reusable validated shared-cache tiles.
- **Overpass.** Construct `TiledRoadAccess` with the route CRS, cache directory, distance limit, and public or all-drivable classes. It reads `RF_PLANNER_OVERPASS_URL` and defaults to the public interpreter. Pass candidate points after corridor, terrain, and exclusion filtering to `allowed`. Reuse the same instance for batches and `overlay`. The overlay takes router locations per alternative and returns shared WGS84 polylines in Leaflet order, indices per alternative, and nearest distances in metres.
- **CoreScope.** Use `CoreScopeClient::new(DEFAULT_BASE_URL)` and call `fetch_repeaters` with `DEFAULT_MAXIMUM_PAGES` (20). The client uses `https://corescope.eth0.no` by default in the server and requests up to 500 records per paginated request. `filter_corridor` takes geographic endpoints, a half-width in metres, and a UTC clock value for deterministic freshness labels. It returns the router records and count for the dashboard.

`shared_cache_directory` resolves `RF_PLANNER_CACHE_DIR` or defaults to
`<data>/shared-cache`. The terrain downloader stores WCS responses and COG
metadata, byte chunks and missing-tile markers under
`sources/<provider-id>/<fetch-definition-hash>/`. Validated projected results
and their provider reports are stored under `composed/`. Overpass uses its
existing keys under `overpass/`. The legacy Kartverket adapter remains
available to callers that use the old WCS cache and request API.

Run network and raster operations on blocking workers. Cancellation is
cooperative between transport reads, processing chunks and tiles. A blocked
read returns control when it completes or the HTTP timeout expires. See the
[data source contract](../../../docs/design/DATA_SOURCES.md) for definitions,
HTTP payloads and cache keys.

## Verification

From `rust/`:

```sh
cargo test --locked -p repeaternet-providers
cargo clippy --locked -p repeaternet-providers --all-targets -- -D warnings
```

Tests use loopback HTTP servers, temporary caches, and tiny synthetic GeoTIFFs. They check Python-compatible requests and cache keys, tile geometry, raster rejection, retries, cancellation and publication, road proximity and overlay reuse, and CoreScope pagination and corridor records.


Run the opt-in live Rio and Oslo probes with
`cargo test --locked -p repeaternet-providers live_rio_and_oslo -- --ignored --nocapture`.
The deterministic test suite uses a synthetic EPSG:4326 Float32 fixture with
a linear elevation ramp, tiled Deflate and floating-point predictor 3, and
a reduced-resolution IFD.
