use std::path::{Path, PathBuf};

use geo::Area;
use repeaternet_rf_core::{HeightReference, RfError, Site};
use repeaternet_terrain::{
    coordinates::{norway_utm_epsg, LonLat, Transformer},
    corridor::{
        corridor_bounds, corridor_geometry, distance_to_segment_m, inside_corridor, site_buffer,
        within_corridor_latlon,
    },
    profile::{bounded_profile_step, sample_profile},
    ArrayTerrain, GeoTiff, Point, RasterTerrain, TerrainCrs, TerrainError, TerrainLayer,
    TerrainSource,
};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn point(x: f64, y: f64) -> Point {
    Point { x, y }
}

fn row(line: &str) -> Vec<&str> {
    line.split(',').collect()
}

fn number(value: &str) -> f64 {
    value.parse().unwrap()
}

fn near(actual: f64, expected: f64, tolerance: f64) {
    assert!(
        actual == expected
            || (actual.is_nan() && expected.is_nan())
            || (actual - expected).abs() <= tolerance,
        "actual {actual:?}, expected {expected:?}, tolerance {tolerance}",
    );
}

#[test]
fn coordinates_match_pyproj_in_all_three_zones() {
    for line in include_str!("fixtures/coordinates.csv").lines().skip(1) {
        let fields = row(line);
        let crs = TerrainCrs::from_epsg(fields[0].parse().unwrap()).unwrap();
        let transform = Transformer::new(crs).unwrap();
        let geographic = LonLat {
            longitude: number(fields[1]),
            latitude: number(fields[2]),
        };
        let projected = point(number(fields[3]), number(fields[4]));
        let actual = transform.forward(geographic).unwrap();
        near(actual.x, projected.x, 1e-5);
        near(actual.y, projected.y, 1e-5);
        let actual = transform.reverse(projected).unwrap();
        near(actual.longitude, geographic.longitude, 1e-9);
        near(actual.latitude, geographic.latitude, 1e-9);
    }
    assert_eq!(norway_utm_epsg(11.999).unwrap(), 25832);
    assert_eq!(norway_utm_epsg(12.0).unwrap(), 25833);
    assert_eq!(norway_utm_epsg(23.999).unwrap(), 25833);
    assert_eq!(norway_utm_epsg(24.0).unwrap(), 25835);
    assert!(norway_utm_epsg(f64::NAN).is_err());
    assert!(TerrainCrs::from_epsg(4326).is_err());
}

#[test]
fn geotiff_samples_and_metadata_match_rasterio() {
    for line in include_str!("fixtures/raster_metadata.csv").lines().skip(1) {
        let fields = row(line);
        let tile = GeoTiff::open(fixture(fields[0])).unwrap_or_else(|error| panic!("{error}"));
        let meta = tile.metadata();
        let actual = [meta.resolution_m]
            .into_iter()
            .chain(meta.bounds.as_array())
            .chain(meta.geotransform);
        for (actual, expected) in actual.zip(&fields[1..]) {
            near(actual, number(expected), 1e-8);
        }
        assert_eq!(meta.has_internal_mask, fields[0] == "masked.tif");
        let (points, expected): (Vec<Point>, Vec<f64>) =
            include_str!("fixtures/raster_samples.csv")
                .lines()
                .skip(1)
                .map(row)
                .filter(|sample| sample[0] == fields[0])
                .map(|sample| {
                    (
                        point(number(sample[1]), number(sample[2])),
                        number(sample[3]),
                    )
                })
                .unzip();
        let actual = tile.sample(&points).unwrap();
        assert_eq!(actual.len(), expected.len());
        for (&actual, expected) in actual.iter().zip(expected) {
            near(actual, expected, 0.0);
        }
    }
}

#[test]
fn mosaic_precedence_mask_override_and_product_separation() {
    let masked = fixture("masked.tif");
    let fallback = fixture("fallback.tif");
    let positions = [
        point(500005.0, 6650995.0),
        point(500015.0, 6650995.0),
        point(500355.0, 6650645.0),
        point(f64::INFINITY, 6650995.0),
        point(499999.0, 6650995.0),
    ];
    let terrain = RasterTerrain::open(&[&masked, &fallback], &[&masked]).unwrap();
    assert_eq!(terrain.resolution_m(), 10.0);
    assert_eq!(
        terrain.bounds().as_array(),
        [500000.0, 6650640.0, 500360.0, 6651000.0]
    );
    assert!(terrain.has_surface());
    let ground = terrain.sample(&positions, TerrainLayer::GroundDtm).unwrap();
    assert_eq!(&ground[..3], &[-9999.0, 500.0, 500.0]);
    assert!(ground[3..].iter().all(|value| value.is_nan()));
    let surface = terrain
        .sample(&positions, TerrainLayer::SurfaceDom)
        .unwrap();
    assert_eq!(surface[0], -9999.0);
    assert!(surface[1..].iter().all(|value| value.is_nan()));
    assert!(terrain
        .sample(&[], TerrainLayer::GroundDtm)
        .unwrap()
        .is_empty());
    let reversed = RasterTerrain::open(&[&fallback, &masked], &[]).unwrap();
    assert!(!reversed.has_surface());
    assert_eq!(reversed.resolution_m(), 20.0);
    assert_eq!(
        reversed
            .sample(&positions[..3], TerrainLayer::SurfaceDom)
            .unwrap(),
        [500.0; 3]
    );
}

#[test]
fn unmasked_nonfinite_values_keep_tile_priority() {
    let terrain =
        RasterTerrain::open(&[fixture("nonfinite.tif"), fixture("fallback.tif")], &[]).unwrap();
    let samples = terrain
        .sample(
            &[
                point(500005.0, 6650995.0),
                point(500015.0, 6650995.0),
                point(500005.0, 6650985.0),
            ],
            TerrainLayer::GroundDtm,
        )
        .unwrap();
    assert!(samples[0].is_nan());
    assert_eq!(&samples[1..], &[f64::INFINITY, f64::NEG_INFINITY]);
}

#[test]
fn malformed_and_unsupported_inputs_return_errors() {
    for (file, message) in [
        ("unprojected.tif", "must declare a projected CRS"),
        ("unsupported_crs.tif", "Unsupported terrain CRS EPSG:27700"),
        ("feet.tif", "coordinates must use metres"),
        (
            "custom_datum_unsupported.tif",
            "Unsupported GeoTIFF geographic datum",
        ),
        ("no_crs.tif", "Missing GeoTIFF key directory"),
    ] {
        match GeoTiff::open(fixture(file)) {
            Err(error) => assert!(error.to_string().contains(message), "{error}"),
            Ok(_) => panic!("{file} unexpectedly opened"),
        }
    }
    assert!(matches!(
        RasterTerrain::open(&[fixture("masked.tif")], &[fixture("point.tif")]),
        Err(TerrainError::MismatchedCrs { .. }),
    ));
    assert!(matches!(
        RasterTerrain::open(&[fixture("custom_tm.tif"), fixture("custom_lcc.tif")], &[]),
        Err(TerrainError::MismatchedCrs { .. }),
    ));
    assert!(RasterTerrain::open::<PathBuf>(&[], &[]).is_err());
    assert!(GeoTiff::open(fixture("profile.csv")).is_err());
}

#[test]
fn conflicting_or_malformed_geokeys_fail_at_open() {
    for (name, expected) in [
        (
            "conflicting_projection.tif",
            "Unsupported or conflicting GeoTIFF key 3075",
        ),
        ("conflicting_angular_units.tif", "GeoTIFF angular units"),
        (
            "unsupported_datum_shift.tif",
            "Unsupported or conflicting GeoTIFF key 2062",
        ),
        ("duplicate_geokey.tif", "Duplicate GeoTIFF key 1025"),
        (
            "missing_projection_parameter.tif",
            "Missing custom GeoTIFF parameter 3092",
        ),
        (
            "invalid_double_offset.tif",
            "Missing GeoTIFF DOUBLE at key 3092",
        ),
        (
            "conflicting_base.tif",
            "Conflicting GeoTIFF geographic CRS or datum",
        ),
    ] {
        let Err(error) = GeoTiff::open(fixture(name)) else {
            panic!("{name} unexpectedly opened");
        };
        assert!(error.to_string().contains(expected), "{error}");
    }
}

#[test]
fn terrain_sources_are_shareable_and_concurrent_reads_keep_tile_values() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<RasterTerrain>();
    assert_send_sync::<ArrayTerrain>();
    let tile = GeoTiff::open(fixture("rotated.tif")).unwrap();
    let [x, a, b, y, d, e] = tile.metadata().geotransform;
    std::thread::scope(|scope| {
        for row in [0, 63, 127, 192, 259] {
            let tile = &tile;
            scope.spawn(move || {
                for col in [0, 127, 269] {
                    let (row, col) = (f64::from(row), f64::from(col));
                    let p = point(
                        x + a * (col + 0.5) + b * (row + 0.5),
                        y + d * (col + 0.5) + e * (row + 0.5),
                    );
                    near(
                        tile.sample(&[p]).unwrap()[0],
                        100.0 + col * 0.25 + row * 0.5,
                        0.0,
                    );
                }
            });
        }
    });
}

#[test]
fn concurrent_mosaic_batches_preserve_mask_nodata_and_point_order() {
    let masked = fixture("masked.tif");
    let sparse = fixture("sparse.tif");
    let fallback = fixture("fallback.tif");
    let terrain = RasterTerrain::open(&[&masked, &sparse, &fallback], &[&masked]).unwrap();
    let cases = [
        (point(500175.0, 6650825.0), 500.0, f64::NAN), // Mask hole and sparse block.
        (point(500005.0, 6650995.0), -9999.0, -9999.0), // Mask overrides nodata.
        (point(500015.0, 6650995.0), 17.0, f64::NAN),  // Fill from the second tile.
        (point(500025.0, 6650995.0), 102.0, 102.0),    // First tile wins.
        (point(500245.0, 6650965.0), 500.0, f64::NAN), // Fill from the third tile.
        (point(499995.0, 6650995.0), f64::NAN, f64::NAN),
        (point(500015.0, 6650995.0), 17.0, f64::NAN),
    ];
    let barrier = std::sync::Barrier::new(cases.len());
    std::thread::scope(|scope| {
        for shift in 0..cases.len() {
            let (terrain, barrier) = (&terrain, &barrier);
            scope.spawn(move || {
                let mut cases = cases;
                cases.rotate_left(shift);
                let positions = cases.map(|(point, _, _)| point);
                barrier.wait();
                for _ in 0..16 {
                    let ground = terrain.sample(&positions, TerrainLayer::GroundDtm).unwrap();
                    let surface = terrain
                        .sample(&positions, TerrainLayer::SurfaceDom)
                        .unwrap();
                    for ((_, ground_z, surface_z), (dtm, dom)) in
                        cases.into_iter().zip(ground.into_iter().zip(surface))
                    {
                        near(dtm, ground_z, 0.0);
                        near(dom, surface_z, 0.0);
                    }
                }
            });
        }
    });
}

#[test]
fn profile_matches_python_oracle_and_preserves_missing_terrain() {
    let crs = TerrainCrs::Etrs89Utm33;
    let terrain = ArrayTerrain::new(
        7,
        1,
        vec![100.0; 7],
        Some(vec![100.0, 125.0, 130.0, f64::NAN, 135.0, 110.0, 100.0]),
        point(0.0, 0.0),
        100.0,
        crs,
    )
    .unwrap();
    let mut source = Site::new("A", 0.0, 0.0);
    source.ground_elevation_m = 100.0;
    source.antenna_height_m = 15.0;
    let mut target = Site::new("B", 600.0, 0.0);
    target.ground_elevation_m = 100.0;
    target.antenna_height_m = 25.0;
    let profile =
        sample_profile(&terrain, &source, &target, 869.5, 4.0 / 3.0, Some(100.0)).unwrap();
    assert!(profile.surface_available);
    assert_eq!(profile.points.len(), 7);
    for (i, line) in include_str!("fixtures/profile.csv")
        .lines()
        .skip(1)
        .enumerate()
    {
        let actual = [
            profile.distances_m[i],
            profile.points[i].x,
            profile.points[i].y,
            profile.dtm_elevation_m[i],
            profile.surface_elevation_m[i],
            profile.earth_bulge_m[i],
            profile.effective_obstruction_m[i],
            profile.los_elevation_m[i],
            profile.fresnel_radius_m[i],
            profile.clearance_m[i],
            profile.fresnel_clearance_ratio[i],
        ];
        for (actual, expected) in actual.into_iter().zip(row(line)) {
            near(actual, number(expected), 1e-10);
        }
    }
    source.height_reference = HeightReference::SurfaceDom;
    source.surface_elevation_m = Some(124.0);
    let profile = sample_profile(&terrain, &source, &target, 869.5, 4.0 / 3.0, Some(50.0)).unwrap();
    assert_eq!(profile.los_elevation_m[0], 139.0);
    target.position.x = 1000.0;
    assert!(matches!(
        sample_profile(&terrain, &source, &target, 869.5, 4.0 / 3.0, None),
        Err(RfError::MissingTerrain),
    ));
}

#[test]
fn array_interpolates_and_sample_cap_never_coarsens() {
    let ground = ArrayTerrain::new(
        2,
        2,
        vec![0.0, 10.0, 20.0, 30.0],
        None,
        point(500.0, 600.0),
        10.0,
        TerrainCrs::Etrs89Utm33,
    )
    .unwrap();
    let samples = ground
        .sample(
            &[
                point(505.0, 605.0),
                point(510.0, 610.0),
                point(510.1, 610.0),
            ],
            TerrainLayer::SurfaceDom,
        )
        .unwrap();
    assert_eq!(&samples[..2], &[15.0, 30.0]);
    assert!(samples[2].is_nan());
    let fit = bounded_profile_step(200.0, 50.0, 100.0, 3).unwrap();
    let overflow = bounded_profile_step(200.00001, 100.0, 50.0, 3).unwrap();
    assert_eq!(
        (fit.step_m, fit.required_samples, fit.fits()),
        (100.0, 3, true)
    );
    assert_eq!(
        (overflow.step_m, overflow.required_samples, overflow.fits()),
        (100.0, 4, false)
    );
    for params in [
        (-1.0, 10.0, 10.0, 8),
        (10.0, 0.0, 10.0, 8),
        (10.0, 10.0, f64::INFINITY, 8),
        (10.0, 10.0, 10.0, 2),
    ] {
        assert!(bounded_profile_step(params.0, params.1, params.2, params.3).is_err());
    }
}

#[test]
fn square_corridor_membership_includes_extended_ends() {
    let start = point(0.0, 0.0);
    let end = point(10000.0, 0.0);
    for inside in [
        point(-2000.0, 2000.0),
        point(12000.0, -2000.0),
        point(5000.0, 1999.0),
    ] {
        assert!(inside_corridor(inside, start, end, 2000.0));
    }
    for outside in [
        point(-2000.1, 0.0),
        point(5000.0, 2000.1),
        point(f64::NAN, 0.0),
    ] {
        assert!(!inside_corridor(outside, start, end, 2000.0));
    }
    assert!(inside_corridor(point(2.0, -2.0), start, start, 2.0));
    assert!(!inside_corridor(point(2.1, -2.0), start, start, 2.0));
    assert_eq!(
        corridor_bounds(&[start, start], 2.0, &[])
            .unwrap()
            .as_array(),
        [-2.0, -2.0, 2.0, 2.0]
    );
    assert!(corridor_geometry(&[], 2.0, &[]).is_err());
    assert!(site_buffer(start, 0.0).is_err());
    let start = LonLat {
        longitude: 10.0,
        latitude: 60.0,
    };
    let end = LonLat {
        longitude: 10.2,
        latitude: 60.0,
    };
    let behind = LonLat {
        longitude: 10.0 - 1900.0 / 55660.0,
        latitude: 60.0 + 1900.0 / 111320.0,
    };
    assert!(within_corridor_latlon(behind, start, end, 2000.0));
    near(
        distance_to_segment_m(behind, start, end),
        1900.0_f64.hypot(1900.0),
        1e-8,
    );
}

#[test]
fn corridor_geometry_matches_shapely_bounds_and_area() {
    for line in include_str!("fixtures/corridors.csv").lines().skip(1) {
        let fields = row(line);
        let p0 = point(0.0, 0.0);
        let p1 = point(7000.0, 0.0);
        let (path, sites): (Vec<_>, Vec<_>) = match fields[0] {
            "square" => (vec![p0, p1], vec![]),
            "diagonal" => (vec![p0, point(7000.0, 7000.0)], vec![]),
            "bent" => (
                vec![p0, p1, point(7000.0, 1700.0)],
                vec![point(7050.0, 1500.0), point(10000.0, 2000.0)],
            ),
            "acute" => (vec![p0, p1, point(500.0, 550.0)], vec![]),
            "loop" => (
                vec![
                    p0,
                    point(5000.0, 0.0),
                    point(5000.0, 5000.0),
                    point(0.0, 5000.0),
                    p0,
                    point(5000.0, 0.0),
                ],
                vec![],
            ),
            name => panic!("Unknown fixture {name}"),
        };
        let geometry = corridor_geometry(&path, 200.0, &sites).unwrap();
        let bounds = corridor_bounds(&path, 200.0, &sites).unwrap();
        for (actual, expected) in bounds.as_array().into_iter().zip(&fields[1..5]) {
            near(actual, number(expected), 0.0001);
        }
        near(geometry.unsigned_area(), number(fields[5]), 0.2);
        if fields[0] == "bent" {
            assert_eq!(geometry.0.len(), 2);
        }
        if fields[0] == "loop" {
            assert_eq!(geometry.0[0].interiors().len(), 1);
        }
    }
}

#[test]
#[ignore = "requires the documented Kartverket NHM cache fixture"]
fn kartverket_50m_matches_rasterio() {
    let path = std::env::var_os("REPEATERNET_TERRAIN_KARTVERKET_FIXTURE")
        .expect("set REPEATERNET_TERRAIN_KARTVERKET_FIXTURE to the documented raster path");
    let terrain = RasterTerrain::open(&[PathBuf::from(path)], &[]).unwrap();
    assert_eq!(terrain.crs(), TerrainCrs::Etrs89Utm32);
    assert_eq!(terrain.resolution_m(), 50.0);
    assert_eq!(
        terrain.bounds().as_array(),
        [550000.0, 6620000.0, 650000.0, 6640000.0]
    );
    let samples = terrain
        .sample(
            &[
                point(550025.0, 6639975.0),
                point(599975.0, 6630025.0),
                point(612825.0, 6626775.0),
                point(649975.0, 6620025.0),
                point(590025.0, 6620025.0),
                point(650000.0, 6630000.0),
            ],
            TerrainLayer::GroundDtm,
        )
        .unwrap();
    assert_eq!(
        &samples[..5],
        &[
            78.61672973632812,
            40.26431655883789,
            199.55859375,
            231.41580200195312,
            25.227943420410156,
        ]
    );
    assert!(samples[5].is_nan());
}

#[test]
fn projected_rasters_match_rasterio_and_pyproj() {
    for line in include_str!("fixtures/projected_rasters.csv")
        .lines()
        .skip(1)
    {
        let fields = row(line);
        let raster = GeoTiff::open(fixture(fields[0])).unwrap_or_else(|err| panic!("{err}"));
        let crs = raster.metadata().crs;
        assert_eq!(crs.epsg_code(), fields[1].parse().ok(), "{line}");
        assert_eq!(crs.epsg(), crs.epsg_code().unwrap_or(32767));
        let transform = Transformer::new(crs).unwrap();
        let geographic = LonLat {
            longitude: number(fields[2]),
            latitude: number(fields[3]),
        };
        let projected = transform.forward(geographic).unwrap();
        let error = (projected.x - number(fields[4])).hypot(projected.y - number(fields[5]));
        assert!(error < 1e-5, "{line}: {error} m");
        let back = transform
            .reverse(point(number(fields[4]), number(fields[5])))
            .unwrap();
        near(back.longitude, geographic.longitude, 1e-8);
        near(back.latitude, geographic.latitude, 1e-8);
        assert_eq!(raster.sample(&[projected]).unwrap(), [number(fields[6])]);
    }
}

#[test]
fn embedded_epsg_transforms_match_pyproj() {
    let mut failures = Vec::new();
    for line in include_str!("fixtures/epsg_transforms.csv").lines().skip(1) {
        let fields = row(line);
        let crs = TerrainCrs::from_epsg(fields[0].parse().unwrap()).unwrap();
        let geographic = LonLat {
            longitude: number(fields[1]),
            latitude: number(fields[2]),
        };
        let expected = point(number(fields[3]), number(fields[4]));
        let result = Transformer::new(crs).and_then(|transform| {
            Ok((transform.forward(geographic)?, transform.reverse(expected)?))
        });
        match result {
            Ok((projected, back)) => {
                let distance = (projected.x - expected.x).hypot(projected.y - expected.y);
                let lon_error = ((back.longitude - geographic.longitude + 180.0).rem_euclid(360.0)
                    - 180.0)
                    .abs();
                let lat_error = (back.latitude - geographic.latitude).abs();
                if distance >= 0.001 || lon_error >= 1e-7 || lat_error >= 1e-7 {
                    failures.push(format!(
                        "{line}: {distance} m, inverse error {lon_error}, {lat_error} deg"
                    ));
                }
            }
            Err(err) => failures.push(format!("{line}: {err}")),
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
