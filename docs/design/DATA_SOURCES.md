# Data sources: worldwide terrain and map providers

Status: design contract for the `feat/data-sources` branch, 2026-11-21.
Implementation agents build against this document. If you must deviate, change
this file in the same commit and say why.

## Goal

RepeaterNet is locked to Norway because route and coverage terrain can only be
downloaded from Kartverket's WCS, and the basemaps are hard-coded. This change
adds a **provider registry** for terrain (DTM/DOM) and map layers, with:

- Built-in worldwide defaults (Kartverket for Norway, USGS 3DEP for the USA,
  Copernicus GLO-30 and GLO-90 globally; OpenStreetMap, OpenTopoMap,
  Kartverket and Sentinel-2 basemaps).
- Custom providers that a user adds from the dashboard or by editing
  `<data>/providers.json`.
- Source kinds: **WCS** (1.0.0 and 2.0.1), **single COG** and **tiled COG**
  (one COG per grid cell, URL template), plus map layers as **XYZ/WMTS tile
  templates** and **WMS**.
- Automatic geographic provider selection, priority, regional high-resolution
  preference and global fallback, per download tile, with gap filling.
- Caching, validation, CRS conversion (warp to the project's projected CRS) and
  on-demand downloads (HTTP range reads of only the COG blocks needed).

Non-goals: raster reprojection of uploaded files, vertical datum conversion,
authenticated OAuth flows, offline basemap caching.

## Key decision: warp on download

The terrain engine, RF profiles, coverage grids and saved-run identities all
work in one **projected metre CRS** per project (`TerrainCrs`, mosaic tiles
must share it). We keep that invariant. Every downloaded tile is produced in
the project's **target CRS** on the existing planner grid:

```
plan tile (target CRS, bounds, width×height)
  └─ for each ranked provider intersecting the tile:
       fetch source pixels (WCS GetCoverage or COG range reads)
       warp/resample into the target grid (bilinear, nodata-aware)
       fill only pixels that are still empty
  └─ write a validated single-band Float32 GeoTIFF in the target CRS
```

So nothing downstream changes: `RasterTerrain`, coverage, optimizer and
saved-run fingerprints read local GeoTIFFs as today.

### Target CRS selection

`select_target_crs(area_lonlat, ranked_providers)`:

1. If the project already has route terrain, use its CRS (supplemental
   coverage downloads always do this).
2. If the best provider for the area centroid declares projected `native_crs`
   codes (e.g. Kartverket: 25832, 25833, 25835), choose among them. For
   Kartverket keep `TerrainCrs::for_norway_longitude` behaviour exactly
   (declare `"crs_rule": "norway_utm"` in its definition). Generic rule:
   choose the declared UTM code whose zone central meridian is nearest the
   centroid longitude.
3. Otherwise WGS84 UTM for the centroid: EPSG:326zz (north) or 327zz (south).
   Beyond ±84° latitude use EPSG:3413 (north) or 3031 (south).

Mixed-CRS mosaics stay unsupported; a provider whose native CRS differs from
the target is warped.

### Geographic source rasters

Global COGs are in geographic CRSs (Copernicus: EPSG:4326; USGS 3DEP:
EPSG:4269 NAD83). The terrain crate's local `GeoTiff` stays projected-only.
Add a separate **source raster** reader in the terrain crate that opens a
`tiff_reader::TiffSource` (local or remote), accepts geographic models
(GTModelType 2 with 4326, 4258, 4269, 4283, 4619, 4167 treated as WGS84 to
within ~1–2 m; document the approximation) and projected models via the
existing geokey parser, chooses an overview IFD by resolution, and returns
windows as `f32` plus validity. Longitudes are degrees east; latitudes degrees
north.

## Provider definition file

Location: `<data>/providers.json` (server-wide, all workspaces). Optional
override path: env `RF_PLANNER_PROVIDERS_FILE`. The embedded built-ins live in
`rust/crates/providers/src/builtin_providers.json` with the same schema.
Written atomically (temp + rename). Unknown fields are rejected.

```json
{
  "version": 1,
  "disabled_builtins": ["copernicus-glo90"],
  "terrain": [ /* TerrainProvider */ ],
  "basemaps": [ /* MapLayer */ ]
}
```

### TerrainProvider

| Field | Type | Rules |
| --- | --- | --- |
| `id` | string | `^[a-z0-9][a-z0-9-]{0,47}$`, unique across built-in and custom |
| `name` | string | 1–80 chars, shown to people |
| `kind` | `"wcs"` \| `"cog"` \| `"cog_tiles"` | |
| `products` | object | keys `dtm` and/or `dom`; at least one |
| `coverage` | Coverage | where the provider has data |
| `resolution_m` | number | nominal native ground resolution, 0.1–10,000 |
| `priority` | integer | −1000…1000, default 0; higher wins when resolution ties |
| `surface_model` | bool | default false. True when the "dtm" product is really a surface model (Copernicus, SRTM). Shown as a caveat. |
| `missing_tile` | `"nodata"` \| `"sea_level"` | cog_tiles only; HTTP 404/403 for a cell means no file. `sea_level` fills 0 m (Copernicus ocean). Default `nodata`. |
| `crs_rule` | `"nearest_utm"` \| `"norway_utm"` | optional, WCS only |
| `max_concurrency` | integer | 1–8, default 2 |
| `headers` | object string→string | optional HTTP headers; values may contain `${ENV_NAME}` |
| `attribution` | string | required, ≤ 300 chars |
| `license` | string | optional |
| `homepage` | string | optional https URL |
| `enabled` | bool | default true |

`products.<dtm|dom>` by kind:

- **wcs**: `{ "endpoint": url, "coverage": string, "version": "1.0.0"|"2.0.1",
  "native_crs": ["EPSG:25833", ...], "format": "GeoTIFF"|"image/tiff",
  "params": {string: string}, "axis_labels": ["E","N"] }`. Services that use a
  different endpoint or coverage name per CRS (Kartverket) give
  `"per_crs": {"EPSG:25832": {"endpoint": url, "coverage": string}, ...}`
  instead of `endpoint`/`coverage`/`native_crs`; `native_crs` is then the
  key list in order. `native_crs` lists CRSs the server can
  return; when the target CRS is listed request it directly (no warp),
  otherwise request in `native_crs[0]` over the transformed envelope (+2
  source pixels margin) and warp. WCS 2.0.1 uses `subset=E(..)`/`subset=N(..)`
  (or `Lat/Long`), `subsettingCrs`, `outputCrs` and `scaleSize`; the axis labels
  come from `axis_labels` (default `["E","N"]` for projected, `["Long","Lat"]`
  for geographic native CRSs).
- **cog**: `{ "url": url }`. CRS and geotransform come from the file.
- **cog_tiles**: `{ "url_template": string, "tile_degrees": 1.0 }`. Template
  placeholders (cell = floor of south-west corner):
  `{NS}` `N`/`S`, `{ns}` lowercase, `{EW}` `E`/`W`, `{ew}` lowercase,
  `{lat}` abs latitude, `{lat2}` zero-padded to 2, `{lon}` abs longitude,
  `{lon3}` zero-padded to 3, `{lat_n}` / `{lat_n2}` (northern edge, used by
  USGS), `{lat_signed}`, `{lon_signed}`. Unknown placeholders are a
  validation error.

URLs must be `https://` (or `http://` only for loopback/`localhost`, so tests
and local GeoServer work). Every placeholder used must be known. `${ENV}`
substitution happens at request time; missing env vars fail the request with
a message naming the variable, never the value.

### Coverage

```json
{ "type": "global" }
{ "type": "bbox", "bbox": [west, south, east, north] }          // degrees
{ "type": "polygon", "rings": [[[lon, lat], ...]] }              // outer ring(s), WGS84
```

Validate: finite, west < east (no antimeridian crossing; use two boxes via
`"type":"multi_bbox","bboxes":[[...],[...]]`), south < north, |lat| ≤ 90,
polygon rings closed or auto-closed, ≥ 4 points, valid.

### MapLayer (basemaps and overlays)

| Field | Type | Rules |
| --- | --- | --- |
| `id`, `name`, `attribution`, `enabled`, `coverage` | as above | coverage optional (default global) |
| `kind` | `"xyz"` \| `"wms"` | WMTS in Web Mercator is an `xyz` template |
| `url` | string | xyz: must contain `{z}`, `{x}`, `{y}`; optional `{s}`, `{r}`. wms: base URL |
| `subdomains` | string | optional, e.g. `"abc"` |
| `min_zoom`, `max_zoom` | integer | 0–24 |
| `overlay` | bool | default false; overlays are transparent layers on top |
| `opacity` | number | 0–1, default 1 |
| `wms` | object | wms only: `{ "layers": string, "styles": "", "format": "image/png", "transparent": true, "version": "1.3.0" }` |
| `default` | bool | at most one base layer may be default |

Map layers load directly in the browser (no server proxy). `headers` are not
supported for map layers.

## Built-in providers

Verified live 2026-11-21 (HTTP 200, `Accept-Ranges: bytes`).

| id | kind | coverage | res | priority | notes |
| --- | --- | --- | --- | --- | --- |
| `kartverket-nhm` | wcs 1.0.0 | Norway mainland bbox `[4.0, 57.9, 31.3, 71.3]` | 1 m | 100 | DTM+DOM, `crs_rule: norway_utm`, one endpoint per CRS via `per_crs` (values from `kartverket_wcs.json`, which this replaces). Returns nodata outside Norway → gap fill. |
| `usgs-3dep-13` | cog_tiles | CONUS + AK + HI multi-bbox | 10 m | 50 | DTM. `https://prd-tnm.s3.amazonaws.com/StagedProducts/Elevation/13/TIFF/current/{ns}{lat_n2}{ew}{lon3}/USGS_13_{ns}{lat_n2}{ew}{lon3}.tif`, NAD83 geographic, ~400 MB files → range reads only. Missing tile → nodata. |
| `copernicus-glo30` | cog_tiles | global | 30 m | 10 | `https://copernicus-dem-30m.s3.amazonaws.com/Copernicus_DSM_COG_10_{NS}{lat2}_00_{EW}{lon3}_00_DEM/Copernicus_DSM_COG_10_{NS}{lat2}_00_{EW}{lon3}_00_DEM.tif`. `surface_model: true`, `missing_tile: sea_level`. Attribution "© DLR e.V. 2010-2014 and © Airbus Defence and Space GmbH 2014-2018 provided under COPERNICUS by the European Union and ESA". |
| `copernicus-glo90` | cog_tiles | global | 90 m | 5 | `https://copernicus-dem-90m.s3.amazonaws.com/Copernicus_DSM_COG_30_{NS}{lat2}_00_{EW}{lon3}_00_DEM/Copernicus_DSM_COG_30_{NS}{lat2}_00_{EW}{lon3}_00_DEM.tif`. Fallback for the few GLO-30 gaps. |

Map layers: `osm` (default, OpenStreetMap, global, max 19),
`opentopomap` (`https://{s}.tile.opentopomap.org/{z}/{x}/{y}.png`, abc, max 17,
"Map data © OpenStreetMap contributors, SRTM · style © OpenTopoMap (CC-BY-SA)"),
`kartverket-topo` and `kartverket-grey` (Norway bbox, max 18, existing URL and
`topograatone`), `s2cloudless-2016`
(`https://tiles.maps.eox.at/wmts/1.0.0/s2cloudless_3857/default/g/{z}/{y}/{x}.jpg`,
"Sentinel-2 cloudless – https://s2maps.eu by EOX IT Services GmbH (contains
modified Copernicus Sentinel data 2016)", max 15).

## Selection and ranking

For a download tile (target CRS bounds → WGS84 footprint polygon, densified):

1. Candidates: enabled providers that offer the product and whose coverage
   intersects the footprint. If the project pins a provider
   (`terrain.source = "<id>"`), only that provider is used (no fallback),
   unless `terrain.fallback = true` (default true) — then pinned first,
   automatic order after.
2. Rank: providers whose coverage **contains** the footprint centroid first;
   then finer `resolution_m`; then higher `priority`; then id.
3. Fill: process ranked providers in order, filling only empty pixels, until
   no pixel is empty or the list is exhausted. A provider that returns zero
   valid pixels for the tile is recorded but not an error. A transport error
   from a provider is an error for the tile only if no later provider fills
   every pixel the failed provider could have filled; otherwise it is a
   warning in the result (`warnings[]`).
4. Remaining empty pixels are written as nodata (existing readiness logic
   reports gaps).

**Plan resolution.** `effective = max(requested, finest resolution_m among
providers ranked first for any tile)`, then the existing auto-coarsening by
pixel budget. So a 10 m request in Norway stays 10 m, and in Brazil becomes
30 m without wasting pixels.

**DOM.** Only providers offering `dom`. If none covers the area, the DOM plan
is omitted and the estimate says so (`dom_unavailable_reason`). Never fill DOM
from a DTM provider.

## Fetch, cache and validation

Cache root: `shared-cache/` as today.

- `shared-cache/kartverket/` keeps the existing names so current caches still
  hit when the generic WCS request equals the legacy one (nice-to-have).
- `shared-cache/sources/<provider-id>/<sha256(definition-fetch-fields)[..12]>/`:
  - `cog/<sha(url)[..20]>/header.bin` + `blocks/<offset>-<len>.bin`: raw
    byte ranges fetched from a COG (chunk-aligned, 256 KiB default chunk,
    coalesce adjacent ranges, max 16 MiB per request). Remote `TiffSource`
    reads hit this cache first. Also a `missing` marker file for 404 cells.
  - `wcs/<sha(request)[..20]>.tif` for WCS responses.
- `shared-cache/composed/<sha(tile request)[..24]>-<product>.tif`: final warped
  composite tiles. Key = target CRS identity, bounds, width, height, product,
  ordered list of (provider id, definition fetch hash). Estimates count these
  for cache hits.

Changing a provider's fetch-relevant fields changes its hash, so stale caches
are never reused. Cache directories are created lazily.

Validation:

- Definitions: schema above, on load and on save; the API returns all errors
  with JSON paths (`terrain[2].products.dtm.url_template: unknown placeholder
  {foo}`).
- Source responses: HTTP status, `Content-Type` not HTML/XML (WCS exceptions
  come back as XML: surface their `ExceptionText`), body limit 512 MiB per
  WCS response, TIFF parses, single band, supported sample type.
- COG: requires a tiled layout (or strips) and a geotransform; rejects
  rotated grids for geographic sources; checks `Content-Range` and object
  length consistency; if the server ignores ranges (HTTP 200 for a range
  request), only accept bodies ≤ 64 MiB, otherwise error "server does not
  support range requests".
- Values: drop non-finite and outside −12,000…9,000 m as nodata; a provider
  whose valid pixels in a tile are > 50 % outside −500…9,000 m is rejected for
  that tile with a warning (catches wrong units/scale).
- Output: the composed tile is re-opened with `GeoTiff::open` and fully read
  before publication, as today.

Downloads are cooperative-cancellable between HTTP chunks and tiles and report
per-tile progress with the provider names used.

## GeoTIFF writer

Terrain crate (or providers crate) gains a minimal writer: little-endian
classic TIFF (BigTIFF when > 4 GiB), single band Float32, tiled 256×256,
Deflate with floating-point predictor (3) or uncompressed, `GDAL_NODATA`
`-32767`, ModelPixelScale + ModelTiepoint, GeoKeys for a registered EPSG
projected CRS (GTModelType 1, GTRasterType PixelIsArea, ProjectedCSType).
Custom CRSs are not written (targets are always registered codes). Round-trip
tests through `GeoTiff::open`.

## Rust API (providers crate)

Module `repeaternet_providers::sources` (new). Names are guidance; keep the
shape.

```rust
pub struct ProviderRegistry { /* built-ins + custom, validated */ }
impl ProviderRegistry {
    pub fn builtin() -> Self;
    pub fn load(data_dir: &Path) -> Result<Self>;               // reads providers.json if present
    pub fn validate_custom(json: &serde_json::Value) -> Result<CustomFile, Vec<ValidationIssue>>;
    pub fn save_custom(data_dir: &Path, file: &CustomFile) -> Result<()>;
    pub fn terrain(&self) -> &[TerrainProvider];               // effective, enabled + disabled flagged
    pub fn basemaps(&self) -> &[MapLayer];
    pub fn rank(&self, product: Product, footprint_wgs84: &MultiPolygon<f64>, pin: SourceChoice) -> Vec<&TerrainProvider>;
    pub fn describe_area(&self, area_wgs84: &MultiPolygon<f64>, include_dom: bool, pin: SourceChoice) -> AreaSources; // for UI preview
    pub fn select_target_crs(&self, area_wgs84: &MultiPolygon<f64>) -> Result<TerrainCrs>;
}
pub struct TerrainDownloader { /* registry snapshot, cache root, client, limits, timing */ }
impl TerrainDownloader {
    pub fn plan_download(&self, crs, bounds, requested_resolution_m, area: DownloadArea, options: PlanOptions, include_dom, pin) -> Result<SourcePlan>;
    pub fn plan_coverage_extension(&self, crs, missing_dtm, missing_dom: Option<_>, resolution_m, pin) -> Result<SourceCoveragePlan>;
    pub fn estimate(&self, crs, plan: &SourcePlan, existing_directory: &Path) -> Result<SourceEstimate>;
    pub fn fetch_plan(&self, product, crs, plan, cancelled, progress: impl FnMut(TileProgress)) -> Result<FetchedTiles>; // paths + warnings + providers used
    pub fn probe(&self, provider_id, at: LonLat) -> Result<ProbeResult>; // "Test" button: fetch a ~1 km window at a point, report elevation stats, CRS, timing
}
```

Reuse the existing tile grid planner (`build_tiles`, `nice_resolution`,
limits) by moving it out of `kartverket.rs` into a shared module.
`StagedGeneration` stays as is. `KartverketProvider` can remain as a thin
compatibility layer or be removed once the server no longer uses it.

`SourceEstimate` extends today's `TerrainEstimate` JSON with:

```json
{
  "crs": "EPSG:32633",
  "sources": [
    {"id": "kartverket-nhm", "name": "Kartverket NHM", "product": "dtm", "tiles": 3, "resolution_m": 1, "surface_model": false, "attribution": "© Kartverket"},
    {"id": "copernicus-glo30", "name": "Copernicus GLO-30", "product": "dtm", "tiles": 1, "resolution_m": 30, "surface_model": true, "attribution": "..."}
  ],
  "dom_unavailable_reason": null,
  "uncovered_area_km2": 0.0,
  "warnings": []
}
```

`tiles` counts tiles where the provider is ranked first. Existing keys
(`summary`, `tile_count`, `cache_hits`, …) keep their meaning.

## HTTP API (web-server)

| Method | Path | Body / result |
| --- | --- | --- |
| GET | `/api/data-sources` | `{ "terrain": [ProviderView], "basemaps": [MapLayer], "custom": CustomFile, "file": "<path>", "errors": [ValidationIssue] }`. `ProviderView` = definition + `builtin: bool`, `enabled: bool`, `products: ["dtm","dom"]`, `coverage_bbox: [w,s,e,n]`. Header values are redacted to `"***"` unless they are `${ENV}` references. |
| PUT | `/api/data-sources` | full `CustomFile`. 400 with `{"error": "...", "issues": [{"path": "...", "message": "..."}]}` on validation failure. Saves atomically; returns the GET payload. Redacted `"***"` header values keep their stored value. |
| POST | `/api/data-sources/validate` | `CustomFile` → `{ "ok": bool, "issues": [...] }` without saving. |
| POST | `/api/data-sources/{id}/probe` | `{ "latitude": .., "longitude": .. }` → `ProbeResult` `{ ok, crs, resolution_m, valid_fraction, min_m, max_m, mean_m, elapsed_ms, bytes_fetched, message }`. Runs on a worker, bounded to 30 s. |
| POST | `/api/data-sources/preview` | `{ "a": [lat,lon], "b": [lat,lon], "known_routers"?, "manual_routers"?, "terrain": {...} }` → `{ "crs": "EPSG:…", "sources": [...as in estimate, without tile counts but with coverage share], "dom_available": bool, "summary": "Kartverket NHM · 1 m DTM and surface; gaps from Copernicus GLO-30 (30 m surface model)" }`. Cheap, no network. |

Existing `/api/terrain/estimate`, `/api/terrain/prepare`,
`/api/coverage/terrain/estimate` and `/api/coverage/terrain/prepare` keep
their routes and job semantics; they use the registry instead of Kartverket.
`terrain` options gain `"source": "auto" | "<provider-id>"` (default `"auto"`)
and `"fallback": bool` (default true). Unknown/disabled source ids → 400
"Terrain source X is not available". Saved plans without `source` mean auto.

Concurrency: definition changes take effect for new estimates. A running
download keeps the registry snapshot it started with. Estimates and prepares
record the registry fingerprint; prepare after a definition change → 409
"Data sources changed; estimate again".

## Dashboard

Follow `docs/UX_DESIGN_RULES.md`.

**Route tab → 2 Terrain step** (primary location):

- Copy: "Download elevation for your route from the best available source, or
  upload your own files." No "In Norway…" text.
- New first control: `Terrain source` select: `Automatic · best available`
  (default) + each enabled terrain provider by name with resolution
  (`Kartverket NHM · 1 m · Norway`). Below it, a status line from
  `/api/data-sources/preview` (debounced on endpoint/router changes, only
  when the Terrain step is open or the estimate is requested):
  "For this route: Kartverket NHM · 1 m ground and surface. Gaps: Copernicus
  GLO-30 (30 m surface model)." Surface-model sources carry a visible caveat
  text, not just colour.
- `Fill gaps from other sources` checkbox (only when a specific source is
  pinned).
- Button "Estimate Kartverket terrain" → **Estimate terrain download**. The
  inline estimate and the combined estimate dialog list sources with tile
  counts and attributions.
- If no DOM source covers the route, the "Also download surface terrain"
  checkbox shows the reason inline and the estimate says DTM only.
- Job stage text names the provider: "DTM tile 3/7 · Copernicus GLO-30".

**⋯ project menu → Data sources…** (rare operation, opens a dialog; add the
row to the IA table in `UX_DESIGN_RULES.md`):

- Two sections, **Terrain** and **Map layers**. Each row: name, kind,
  coverage (bbox text or Global), resolution, priority, built-in/custom tag,
  enabled toggle (`aria-pressed` or checkbox), and for custom rows Edit and
  Remove (Remove goes through `askActionDialog` naming the source).
- **Add source** opens an editor form with a kind select (WCS, COG, COG tiles,
  XYZ map tiles, WMS map) and the relevant fields; an "Edit as JSON"
  disclosure shows the definition JSON. Field errors from the validate
  endpoint appear next to fields (`aria-invalid`, `.field-error`) and focus
  moves to the first one.
- **Test** (terrain): probes at the current map centre and shows the result
  (valid fraction, min/max, elapsed). **Preview** (map layer): adds the layer
  to the map temporarily.
- Save writes via PUT and refreshes the map layer control and Terrain source
  select without a page reload. Focus returns to the opener.

**Map**: build the Leaflet layer control from `/api/data-sources.basemaps`.
Remember the chosen base layer in `localStorage`. Layers whose coverage does
not intersect the current view are listed but marked "(outside view)" and not
auto-selected; when the default base layer is regional and the view leaves its
coverage, do not switch automatically (no surprise changes). The map
attribution shows each active layer's attribution. Terrain-source attribution
appears in the estimate and in exports' assumptions.

## Tests

- Providers crate: schema validation (good/bad files, JSON paths), template
  expansion for N/S/E/W edges (lat −0.5 → `S01`; lon −0.5 → `W001`; USGS
  northern-edge), ranking (contains > intersects, resolution, priority, pin,
  fallback), target CRS selection (Oslo → 25832 like today, Tromsø → 25833,
  Kirkenes → 25835, Stockholm → 32633, Rio → 32723, Svalbard 78°N 16°E is
  outside the Kartverket bbox → 32633), COG range source against a loopback
  server serving a generated tiled Deflate COG in EPSG:4326 with an overview
  (check only needed ranges fetched, cache reuse with zero requests the second
  time, 404 → missing marker, server ignoring Range), WCS 1.0.0 and 2.0.1
  request shapes and XML exception surfacing, warp accuracy (geographic
  source with a linear elevation ramp → projected tile, max error < 0.5 m),
  gap fill across two providers, value-range rejection, cancellation, writer
  round trip.
- Web-server: data-sources GET/PUT/validate round trip and redaction;
  estimate/prepare through a loopback COG provider outside Norway (e.g. Rio)
  ending with a route search on the downloaded terrain; pinned-source errors;
  registry-changed 409.
- Node: layer control built from definitions, terrain source select and
  preview status, data sources dialog validation/focus.
- Optional ignored live test: fetch a 2×2 km Copernicus window near Rio and a
  Kartverket window near Oslo.
