# Real terrain benchmark fixture manifest

No redistribution-approved DTM is currently checked into this repository.
An existing local Kartverket cache is now documented in the
[50 m NHM fixture manifest](fixtures/kartverket-nhm-25832-50m.md), with its verified
checksum, reconstructed request and provenance limitations. That file is used for
optional coverage benchmarks; CI does not require it.

Keep terrain data outside Git unless its provider explicitly permits it. Before
using a real raster as a release benchmark, copy this manifest and record:

```yaml
fixture_id: # stable name, e.g. region-resolution-v1
provider: # source organization and product name
source_url: # exact dataset landing page or download URL
license: # license name and terms URL
retrieved_at: # UTC date
sha256: # checksum of each exact file
files:
  - path: # local path, never a credential-bearing URL
    kind: dtm # dtm or dom
    sha256:
    crs: # raster CRS
    bounds: [left, bottom, right, top]
    resolution_m:
area_description: # terrain characteristics and route coverage
endpoints: # fixed [latitude, longitude] pairs inside the fixture
router_heights_m: # heights used for the benchmark
rf_profile: # frequency, model, validation and margins
reference_route: # expected route IDs/coordinates or no-route assertion
known_limitations: # nodata, interpolation, licensing/access notes
```

Record the completed manifest with the benchmark output outside the engine's
timed section. Use the same checksum, endpoint coordinates, RF settings and
candidate seed to compare cold and warm terrain access. A live provider must not
be required by automated tests or routine benchmark reproduction.
