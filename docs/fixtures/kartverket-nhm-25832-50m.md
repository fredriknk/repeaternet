# Local Kartverket NHM coverage benchmark fixture

Verified: 2026-09-27. Elevation data: © [Kartverket](https://www.kartverket.no/).
The provider's [open-data terms](https://www.kartverket.no/api-og-data/vilkar-for-bruk)
identify CC BY 4.0 for its free products. The
[terrain-data page](https://kartverket.no/en/api-and-data/terrengdata) describes
height data available through downloads and APIs. The raster remains outside Git;
the benchmark makes no provider requests.

## Identity and provenance

```yaml
fixture_id: kartverket-nhm-25832-50m-local-20260829
provider: Kartverket, National Elevation Model DTM WCS
source_url: https://wcs.geonorge.no/skwms1/wcs.hoyde-dtm-nhm-25832
license: CC BY 4.0
license_url: https://www.kartverket.no/api-og-data/vilkar-for-bruk
retrieved_at: unknown # Original response log was not retained.
original_cache_modified_at_utc: 2026-08-29T18:40:06Z
verified_at: 2026-09-27
files:
  - path: cache/wcs_large_smoke/kartverket_dtm_ee927a9a60cf36b4ef38.tif
    kind: dtm
    sha256: a597652dbd0959083616eda264beb081ed5c00c2f82bf995c4d28a19dd2d6e04
    bytes: 4195950
    crs: EPSG:25832
    bounds: [550000, 6620000, 650000, 6640000]
    resolution_m: 50
    dimensions: [2000, 400]
    bands: 1
    dom: absent
    nodata_metadata: absent
wcs_request:
  service: WCS
  version: 1.0.0
  request: GetCoverage
  coverage: nhm_dtm_topo_25832
  format: GeoTIFF
  crs: EPSG:25832
  bbox: 550000,6620000,650000,6640000
  width: 2000
  height: 400
```

Provenance evidence: the provider's `cache_path` function, using the repository's
configured service and the request above, reproduces the exact filename. Raster
CRS, bounds, dimensions and resolution match that request. The cache timestamp is
filesystem evidence, not proof of the original retrieval date. The checksum fixes
the bytes used for these measurements; a later provider download can differ.

## Fixed experiment

The benchmark uses a square inside the full raster, inset by one pixel:
`[590050, 6620050, 609950, 6639950]` in EPSG:25832, 396.01 km². This samples
varied land and near-sea-level terrain around the Oslofjord. It does not benchmark
the full 100 km raster length. All source locations are generated reference
positions, not existing MeshCore routers or proposed installation sites.

- Deterministic sources `R00` through `R(N-1)`: `x = 590050 + 19900*(i+1)/(N+1)`,
  `y = 6630000`, height 50 m AGL. No random candidate seed is used.
- Two-source anchors `[latitude, longitude]`: `[59.79645799031811, 10.722926038677]`
  and `[59.79485720530941, 10.841065230990004]`.
- Client: 1.5 m AGL, 20 dBm TX, 2.15 dBi gain, 0 dB feed/miscellaneous loss,
  −130 dBm sensitivity. Frequency 869.5 MHz; propagation validation; 0 dB fade
  margin; remaining radio settings use the recorded code revision's defaults.
- Profile step 50 m; cap 4,096 samples. Preview grid: 16×16 cells at 1,243.75 m.
  Standard grid: 64×64 cells at 310.9375 m. Each cold run has a fresh 50,000-entry
  scalar cache; the same child process then repeats with that cache warm.
- Reference assertion: every serialized cell and source result agrees between
  cold and warm runs. For the two-source preview, 203 cells are covered, 53 are
  uncovered and none are unknown with the settings above.
- `reference_route`: not applicable; this measures the area-coverage engine,
  without a route search or an asserted radio-connected backbone.

Reproduce from the repository root (the exact local raster must be present):

```powershell
python tools/benchmark_mesh_coverage.py --dtm cache/wcs_large_smoke/kartverket_dtm_ee927a9a60cf36b4ef38.tif --sources 2 --cells 256 --repeats 3 --output docs/benchmarks/coverage-raster-preview.json
python tools/benchmark_mesh_coverage.py --dtm cache/wcs_large_smoke/kartverket_dtm_ee927a9a60cf36b4ef38.tif --sources 8 --cells 4096 --repeats 3 --output docs/benchmarks/coverage-raster-standard.json
python tools/benchmark_mesh_coverage.py --dtm cache/wcs_large_smoke/kartverket_dtm_ee927a9a60cf36b4ef38.tif --sources 32 --cells 7744 --repeats 3 --output docs/benchmarks/coverage-raster-large-32x7744-c12.json
```

The commands hash and open the file before timing. They spawn one fresh child per
repetition and report median/range, child peak working set, sampling calls/returned
array bytes, RF/cache counts, result size, first chunk, cancellation latency and
full-grid equivalence. Cold application caches do not imply cold OS/storage
caches. Sampling-call counts and returned array bytes are not physical disk I/O.

## Limits of the evidence

This is real-raster performance and numerical-repeatability evidence. It does
not establish field RF accuracy, packet delivery, DOM/clutter fidelity, survey
accuracy, long-distance route quality, concurrent web-worker memory, live-browser
latency or container restart behavior. The terrain is already resampled to 50 m;
coverage cells and terrain profiles have separate resolution controls. A missing
nodata tag does not certify that every numeric elevation is a valid survey return.
The exact original provider response time was not retained and is recorded as
unknown above rather than inferred from the file timestamp.
