
# Real terrain benchmark fixtures

The repository checks in synthetic rasters for routine tests. Real Kartverket
rasters used for local measurements stay outside Git.

- The [50 m NHM fixture manifest](fixtures/kartverket-nhm-25832-50m.md)
  records the checksum, reconstructed request and provenance of the
  2,000 × 400 raster used for historical coverage measurements and optional
  Rust regression tests.
- The native coverage benchmark also documents measurements with a separate
  400 × 400 Kartverket DTM. It accepts an optional local raster path as its
  first argument and uses synthetic terrain when called without one.
  [Cache benchmark and raster checksum](../rust/crates/coverage/README.md#cache-benchmark)

## Run the optional NHM regressions

Verify the full-width fixture against the NHM manifest. Set
`REPEATERNET_TERRAIN_KARTVERKET_FIXTURE` to its absolute local path. From
`rust/`, run:

```sh
cargo test --locked -p repeaternet-terrain kartverket_50m_matches_rasterio -- --ignored
cargo test --locked -p repeaternet-optimizer --test kartverket_routes strict_route_across_95_km_fits_the_router_budget -- --ignored
```

The terrain test checks samples against frozen Rasterio values. The
optimizer test checks a 95 km route against a fixed router budget.
Both read the local file. Routine CI skips them.
[Terrain test](../rust/crates/terrain/tests/parity.rs) · [Route test](../rust/crates/optimizer/tests/kartverket_routes.rs)

## Record a new fixture

Keep terrain outside Git unless its terms permit redistribution. Record the
exact files and the experiment with the benchmark output, outside the timed
section. Keep the file checksum, route coordinates, RF settings, source
positions and any random seed fixed when comparing runs.

```yaml
fixture_id: # stable name such as region-resolution-v1
provider: # source organization and product
source_url: # dataset page or exact request without credentials
license: # name and terms URL
retrieved_at: # UTC date or unknown
files:
  - path: # local path
    kind: dtm # dtm or dom
    sha256:
    crs:
    bounds: [left, bottom, right, top]
    resolution_m:
area_description: # terrain characteristics and sampled area
endpoints: # fixed [latitude, longitude] pairs for route tests
router_heights_m:
rf_profile: # frequency, model, validation and margins
reference_route: # expected result, when testing route search
known_limitations: # nodata, resampling and provenance
```
