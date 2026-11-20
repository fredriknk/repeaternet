# RepeaterNet terrain

This crate implements terrain sampling, offline coordinate transforms, and route corridors for the Rust planner. It uses `tiff-reader`, `proj4rs`, and `geo` with no runtime GDAL, PROJ database, GEOS, or Python dependency. [GDAL GeoTIFF behavior](https://gdal.org/en/stable/drivers/raster/gtiff.html) · [PROJ resource requirements](https://proj.org/en/stable/resource_files.html) · [TIFF decoder](https://crates.io/crates/tiff-reader/0.11.1)

## Integrating

- Add `repeaternet-terrain = { path = "../terrain" }` to a consumer beside this crate. The workspace owns the lockfile. The terrain crate depends on the sibling `repeaternet-rf-core` crate and uses its `Point`, `TerrainLayer`, and `TerrainSource` types directly.
- Use `GeoTiff::open(path)` to validate upload metadata and decode the first storage block. `RasterMetadata` provides the CRS, dimensions, resolution, affine transform, axis-aligned bounds, and a four-corner footprint. Subsequent reads report any decode errors in other blocks. An upload that requires complete integrity validation must still check every storage block before publication.
- Use `RasterTerrain::open(dtm_paths, dom_paths)` or `RasterTerrain::from_tiles(dtm, dom)` to build a mosaic. Put route tiles before supplemental tiles in each list. The first unmasked pixel wins even if its value is nonfinite. The first DTM determines resolution, and only DTM tiles contribute mosaic bounds. For readiness reason codes, test nonfinite samples against `terrain.bounds().contains(point)` as the Python web implementation does.
- `TerrainSource::sample` returns one elevation per point in order. Outside and masked pixels produce NaN. DOM requests use ground if the DOM list is empty. When DOM exists, its missing samples stay NaN until the shared RF `profile::sample_profile` fills them with DTM. Any nonfinite ground sample invalidates the RF profile. `ArrayTerrain` provides Python-style bilinear sampling on a row-major grid with increasing Y.
- Use `Transformer::new(terrain.crs())` for existing rasters and `Transformer::for_norway_longitude(longitude)` to select the Python planner's download zone. Geographic coordinates use longitude and latitude in decimal degrees. Projected coordinates use easting and northing in metres. The WGS84/ETRS89 transformation is static and two-dimensional.
- `profile::bounded_profile_step` returns the effective spacing and required sample count without allocating or coarsening. Check `fits()` before sampling. The caller formats HTTP diagnostics from the structured values. `profile::sample_profile` re-exports RF core's implementation, including both antenna height references, Earth curvature, and Fresnel clearance.
- `corridor` provides inclusive straight-line membership, square-ended and miter-joined route polygons, round fixed-site buffers, and projected bounds. It also provides the Python planner's local WGS84 approximation for MeshCore corridor membership and distance to the finite route segment. Polygon ring order and floating-point overlay precision may differ from Shapely.

## Coordinate reference systems

- WGS84 UTM zones 1–60 north (EPSG:32601–32660) and south (EPSG:32701–32760) and ETRS89 UTM zones 28–37 north (EPSG:25828–25837) are supported.
- The embedded catalog adds 320 named metre CRSs, for 450 registered codes total. Examples include Web and World Mercator (3857, 3395), ETRS89-LAEA Europe (3035), Lambert-93 (2154), NZTM2000 (2193), Swedish and Finnish national grids (3006, 3067), equidistant cylindrical (4087), and WGS84 polar stereographic (3031, 3413). The catalog uses static 2-D operations between WGS84 and the WGS84, ETRS89, RGF93 v1 and NZGD2000 bases. Coordinate epochs and grid-based or other datum shifts are outside this model. The source definitions are from IOGP's EPSG Geodetic Parameter Dataset v12.029 as distributed with PyProj 3.8.0 and PROJ 9.8.1. [Embedded catalog](src/epsg.txt) · [IOGP EPSG Dataset Terms of Use](https://iogp.georepository.com/terms-of-use.html)
- GeoTIFF 1.0 and 1.1 require an explicit projected model key. Registered codes may omit the metre unit key. Redundant geodetic keys must agree with the catalog. Conflicting values, extra projection parameters on registered codes, and explicit datum shifts are rejected. Vertical GeoKeys do not affect horizontal transforms or band values. [OGC GeoTIFF standard](https://docs.ogc.org/is/19-008r4/19-008r4.html)
- User-defined GeoTIFFs must declare metres by code or an explicit unit size of exactly one metre and identify WGS84 or ETRS89 by geographic CRS or datum. Numeric parameters use degrees and Greenwich longitude. The parser requires the explicit GeoDoubleParams encodings in the table. Supported parameter variants are listed by key ID in the parser. [GeoKey parser](src/raster/geokeys.rs)

| Method GeoKey | Accepted custom encodings |
| --- | --- |
| 1 | Transverse Mercator with natural origin and scale |
| 7 | Ellipsoidal Mercator with equatorial origin and either scale or standard parallel |
| 8, 9 | Lambert conformal conic with two standard parallels or one standard parallel and scale |
| 10 | Lambert azimuthal equal area, including exact north and south polar origins |
| 11 | Albers equal area with two standard parallels |
| 15, 16 | Polar stereographic with scale or standard parallel, and oblique stereographic with scale |
| 17, 28 | Ellipsoidal equidistant cylindrical and cylindrical equal area |

Files outside the catalog or custom subset are rejected, including OSGB36, NAD83, unsupported methods, EPSG codes above the GeoTIFF authority range, external CRS definitions, WKT-only or ESRI citation-based custom CRSs, alternate custom parameter encodings, non-metre units, and geographic or geocentric models. Full Rasterio CRS parity remains open.

Existing callers can use the named Norwegian CRS constants, `TerrainCrs::from_epsg`, `Transformer::new`, and `RasterTerrain::crs` as before. Use `epsg_code()` for authority-aware metadata. The legacy `epsg()` returns the GeoTIFF user-defined marker, 32767, for custom CRSs. Keep the full copyable `TerrainCrs` in memory or reopen the original GeoTIFF when reconstructing a custom transform. The display label is for humans. Mosaic tiles must share the same CRS identity. Mixed-CRS mosaics and semantic equivalence across differently encoded definitions are outside the mosaic loader.

The embedded catalog and projection engine compile into the executable. This avoids shipping a native PROJ build, its registry and its transformation grids. [proj4rs backend](https://github.com/3liz/proj4rs)

## TIFF decoder and georeferencing

Single-band grayscale TIFF and BigTIFF are supported. Elevation bands can be 8/16/32-bit signed or unsigned integer or 32/64-bit floating point. The reader supports strips and tiles, uncompressed, LZW, Deflate, PackBits, and Zstandard storage, and integer and floating-point predictors in the supported layouts. It reads scale-and-tiepoint and affine-matrix georeferencing, including rotated/sheared grids and the PixelIsPoint half-pixel shift.

Raster sampling floors the inverse affine coordinate and groups points into 256 × 256 pixel regions. Within each region the reader requests the smallest rectangle enclosing its sample pixels. The single-band read copies contiguous rows, and the sampler converts only requested values to `f64`. The decoder reads the tiles or strips intersecting each rectangle. Storage blocks can decode to at most 32 MiB. Each open file has a 1 MiB decoded block cache with up to 16 slots. GDAL nodata values, GDAL sparse blocks and full-resolution internal transparency masks are supported. A mask overrides nodata for its tile.

The Kartverket coverage profile traced most cold time to iterating over full typed windows and collecting them into `Vec<f64>`. The smaller reads and direct pixel conversion also lower temporary allocation. [Profiling breakdown and timings](../coverage/README.md#cache-benchmark)

The reader rejects undefined georeferencing, GCP-only georeferencing, multiple tiepoints, unsupported compression or band layouts, and `.msk` and `.aux.xml` sidecars. JPEG, WebP, LERC, packed elevation bands, 64-bit integer and complex elevation, alpha/multiband inputs, external georeferencing, overview selection, and raster reprojection are outside the current scope. The reader samples raw band values, with no vertical datum, scale, or unit conversion.

## Verification

Run from `rust` using `cargo test --locked -p repeaternet-terrain` and `cargo clippy --locked -p repeaternet-terrain --all-targets -- -D warnings`. The synthetic TIFF and CSV fixtures were generated with Rasterio 1.5.1, PyProj 3.8.0, and Shapely 2.1.2. The generators (`generate.py` and `generate_epsg.py`) were removed with the Python runtime and remain available at commit `18b7cee`; the fixtures and catalog are frozen. The suite checks four PyProj forward and inverse reference points per named CRS, with 1 mm forward and 1e-7 degree inverse tolerances. Rasterio fixtures cover GeoTIFF 1.1, BigTIFF, both UTM hemispheres, named non-UTM CRSs, and every supported custom method through raster open and elevation sampling. Mutations of those files exercise conflicting and malformed keys. Decoder fixtures cover endian variants, strips, compressed tiles, rotated and PixelIsPoint transforms, nodata, internal masks and sparse blocks. Routine Rust tests use the saved fixtures and require neither Python nor provider access.

The optional `kartverket_50m_matches_rasterio` test reads the documented local 50 m NHM cache fixture. Set `REPEATERNET_TERRAIN_KARTVERKET_FIXTURE` to its absolute path and run `cargo test --locked -p repeaternet-terrain kartverket_50m_matches_rasterio -- --ignored`. The source raster stays in the local cache. [Kartverket fixture identity and checksum](../../../docs/fixtures/kartverket-nhm-25832-50m.md)

All 14 tests, including the cached Kartverket test, and Clippy with warnings denied passed in an isolated terrain and RF workspace on Rust 1.100, on Windows x86-64 MSVC and Linux x86-64 (Docker). The Windows test executable used the workspace's static CRT setting. Its PE import table listed only Windows system DLLs. The assembled dashboard still needs clean-machine verification.

## Server integration handoff

- Route, preview, and inspection callers that construct `Transformer::new(terrain.crs())` already have the full CRS. For outlines of rotated rasters, use `RasterMetadata::footprint()` for real raster corners. `bounds` describes the axis-aligned envelope.
- `coverage_runs::transform` currently reconstructs a transform from an EPSG string in the saved manifest. For custom CRSs, compute and persist `footprint_latlng` at evaluation time with `cell_footprint(cell, grid.bounds, grid.effective_cell_size_m, &transform)`, and use those footprints in paging and scenario responses. Keep the existing EPSG-string path for historical named runs. If a historical custom run needs reconstruction, use its fingerprint-matched original raster. Treat a missing or mismatched raster as unavailable for reprojection.
- Keep Kartverket zone selection and service availability independent of the supported upload CRSs. A supplemental plan that requires tiles in an unsupported WCS CRS must report provider unavailability. Downloaded tiles can enter a mosaic only when their CRS matches the route terrain.
- The named constants, CRS constructor and transform APIs, and raster APIs remain compatible at current call sites. The resumed `TerrainError::MismatchedCrs` variant stores `actual` as `Box<TerrainCrs>`. Code constructing that error would wrap the value in `Box::new`, and code reading the CRS value can dereference it. No sibling crate currently constructs or destructures that variant.
- `proj4rs` provides native thread safety and the required projections without optional features. Carry the EPSG dataset acknowledgment and terms link into release notices. [IOGP EPSG Dataset Terms of Use](https://iogp.georepository.com/terms-of-use.html)
