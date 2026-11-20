use super::*;

fn near(actual: f64, expected: f64, tolerance: f64) {
    assert!(
        (actual - expected).abs() <= tolerance,
        "expected {expected} ± {tolerance}, received {actual}"
    );
}

// Reference points from tests/test_rf_math.py.
#[test]
fn rf_math_reference_values() {
    near(
        free_space_path_loss_db(100_000.0, 100.0).unwrap(),
        112.44,
        0.01,
    );
    near(
        first_fresnel_radius_m(5_000.0, 5_000.0, 1_000.0).unwrap(),
        27.38,
        0.055,
    );
    assert_eq!(first_fresnel_radius_m(0.0, 0.0, 869.5).unwrap(), 0.0);
    near(
        earth_bulge_m(50_000.0, 100_000.0, 1.0).unwrap(),
        196.20,
        0.197,
    );
    assert_eq!(earth_bulge_m(0.0, 100_000.0, 4.0 / 3.0).unwrap(), 0.0);
    assert_eq!(
        received_power_dbm(22.0, 2.0, 1.0, 120.0, 3.0, 1.0, 2.0),
        -97.0
    );
    near(elevation_angle_deg(100.0, 0.0, 100.0).unwrap(), 45.0, 1e-12);
    assert_eq!(knife_edge_loss_db(-1.0), 0.0);
    near(knife_edge_loss_db(0.0), 6.03, 0.05);
    assert!(knife_edge_v(5.0, 1_000.0, 1_000.0, 869.5).is_finite());
}

#[test]
fn antenna_csv_sorts_samples_and_interpolates() {
    let csv = "\u{feff}label,\"gain_dbi\",elevation_deg\r\n\
               \"upper, sample\",2,10\r\ncenter,6,0\r\nlower,-4,-10\r\n";
    let pattern = ElevationPattern::from_csv_reader(csv.as_bytes()).unwrap();
    near(pattern.gain_at(5.0), 4.0, 1e-12);
    assert_eq!(pattern.gain_at(-90.0), -4.0);
    assert_eq!(pattern.gain_at(90.0), 2.0);
    assert_eq!(AntennaPattern::Elevation(pattern).peak_gain_dbi(), 6.0);
    assert!(ElevationPattern::new(vec![0.0, 0.0], vec![1.0, 2.0]).is_err());
    assert!(ElevationPattern::from_csv_reader("angle,gain\n-90,0\n".as_bytes()).is_err());
}

// Geometry and test profiles from tests/test_diffraction.py.
#[test]
fn bullington_single_obstacle_matches_knife_edge_geometry() {
    let result =
        bullington_loss_db(&[0.0, 400.0, 1000.0], &[0.0, 30.0, 0.0], &[10.0; 3], 900.0).unwrap();
    let v = knife_edge_v(20.0, 400.0, 600.0, 900.0);
    let single = knife_edge_loss_db(v);
    near(
        result.loss_db,
        single + (1.0 - (-single / 6.0).exp()) * 10.02,
        1e-12,
    );
    assert_eq!(result.obstacles.len(), 1);
    near(result.obstacles[0].distance_m, 400.0, 1e-12);
    near(result.obstacles[0].elevation_m, 30.0, 1e-12);
    near(result.obstacles[0].v, v, 1e-12);
}

#[test]
fn smooth_los_profile_does_not_accumulate_artificial_edges() {
    let losses = [15, 71, 701].map(|count| {
        let distances: Vec<_> = (0..count)
            .map(|index| 700.0 * index as f64 / (count - 1) as f64)
            .collect();
        let result =
            bullington_loss_db(&distances, &vec![0.0; count], &vec![3.0; count], 869.5).unwrap();
        assert_eq!(result.obstacles.len(), 1);
        result.loss_db
    });
    near(losses[0], losses[1], 1e-10);
    near(losses[0], losses[2], 1e-10);
    assert!(losses[0] > 0.0 && losses[0] < 5.0);
}

#[test]
fn bullington_clear_profile_reversal_and_offset() {
    let d = [0.0, 100.0, 350.0, 700.0, 1000.0];
    let terrain = [0.0, 20.0, 50.0, 10.0, 0.0];
    let los = [10.0, 11.25, 12.5, 13.75, 15.0];
    let forward = bullington_loss_db(&d, &terrain, &los, 869.5).unwrap();
    let backwards = bullington_loss_db(
        &[0.0, 300.0, 650.0, 900.0, 1000.0],
        &[0.0, 10.0, 50.0, 20.0, 0.0],
        &[15.0, 13.75, 12.5, 11.25, 10.0],
        869.5,
    )
    .unwrap();
    near(forward.loss_db, backwards.loss_db, 1e-12);
    let offset = d.map(|distance| distance + 7000.0);
    assert_eq!(
        bullington_loss_db(&offset, &terrain, &los, 869.5).unwrap(),
        forward
    );
    let clear = bullington_loss_db(&d, &[0.0; 5], &[100.0; 5], 869.5).unwrap();
    assert_eq!(clear, DiffractionResult::default());
    assert_eq!(
        bullington_loss_db(&[0.0, 100.0], &[10.0; 2], &[3.0; 2], 869.5).unwrap(),
        DiffractionResult::default()
    );
}

#[test]
fn bullington_rejects_invalid_profiles() {
    let valid = &[0.0, 1.0, 2.0];
    for (distances, terrain, frequency) in [
        (&[0.0, 0.0, 1.0][..], &[0.0; 3][..], 900.0),
        (valid, &[0.0, f64::NAN, 0.0], 900.0),
        (valid, &[0.0; 2], 900.0),
        (valid, &[0.0; 3], 0.0),
    ] {
        assert!(bullington_loss_db(distances, terrain, &[1.0; 3], frequency).is_err());
    }
}

struct TestTerrain {
    resolution_m: f64,
    ground: fn(f64) -> f64,
    surface: Option<fn(f64) -> f64>,
}

impl TerrainSource for TestTerrain {
    fn resolution_m(&self) -> f64 {
        self.resolution_m
    }

    fn has_surface(&self) -> bool {
        self.surface.is_some()
    }

    fn sample(&self, points: &[Point], layer: TerrainLayer) -> Result<Vec<f64>, RfError> {
        let elevation = match layer {
            TerrainLayer::GroundDtm => self.ground,
            TerrainLayer::SurfaceDom => self.surface.expect("surface sampling was not requested"),
        };
        Ok(points.iter().map(|point| elevation(point.x)).collect())
    }
}

fn flat(resolution_m: f64) -> TestTerrain {
    TestTerrain {
        resolution_m,
        ground: |_| 0.0,
        surface: None,
    }
}

fn site(id: &str, x: f64, y: f64, kind: SiteKind, antenna_height_m: f64) -> Site {
    Site {
        kind,
        antenna_height_m,
        ..Site::new(id, x, y)
    }
}

#[test]
fn optimistic_screen_uses_peak_pattern_gain() {
    let antenna = AntennaSettings {
        pattern: AntennaPattern::Elevation(
            ElevationPattern::from_csv_reader(
                "elevation_deg,gain_dbi\n-90,0\n0,20\n90,0\n".as_bytes(),
            )
            .unwrap(),
        ),
        ..AntennaSettings::default()
    };
    let settings = RfSettings {
        receiver_sensitivity_dbm: -65.0,
        fade_margin_db: 0.0,
        endpoint_a: antenna.clone(),
        endpoint_b: antenna,
        ..RfSettings::default()
    };
    let terrain = flat(10.0);
    let a = site("A", 0.0, 10.0, SiteKind::EndpointA, 20.0);
    let b = site("B", 1000.0, 10.0, SiteKind::EndpointB, 20.0);
    let evaluator = LinkEvaluator::new(&terrain, &settings);
    let link = evaluator
        .evaluate(&a, &b, EvaluationOptions::default())
        .unwrap();
    assert!(link.valid);
    assert!(evaluator.optimistic_margin_db(&a, &b).unwrap() >= link.worst_margin_db());
    near(link.forward.tx_gain_dbi, 20.0, 1e-12);
}

// Surface-obstruction and DTM fallback fixtures from tests/test_terrain.py.
#[test]
fn profile_uses_surface_obstructions_and_ground_fallback() {
    let terrain = TestTerrain {
        surface: Some(|x| {
            if (x - 500.0).abs() < 1e-9 {
                20.0
            } else {
                f64::NAN
            }
        }),
        ..flat(10.0)
    };
    let a = site("A", 0.0, 50.0, SiteKind::EndpointA, 10.0);
    let b = site("B", 1000.0, 50.0, SiteKind::EndpointB, 10.0);
    let profile = sample_profile(&terrain, &a, &b, 869.5, 4.0 / 3.0, Some(5.0)).unwrap();
    assert!(profile.surface_available);
    assert_eq!(profile.points.len(), 101);
    assert_eq!(profile.surface_elevation_m[50], 20.0);
    assert_eq!(profile.dtm_elevation_m[50], 0.0);
    assert_eq!(profile.surface_elevation_m[49], profile.dtm_elevation_m[49]);
    assert!(profile.clearance_m[50] < 0.0);
    assert_eq!(profile.fresnel_clearance_ratio[0], f64::INFINITY);
    let ground_only = sample_profile(&flat(10.0), &a, &b, 869.5, 4.0 / 3.0, None).unwrap();
    assert!(!ground_only.surface_available);
    assert_eq!(ground_only.surface_elevation_m, ground_only.dtm_elevation_m);

    let settings = RfSettings {
        clutter_loss_enabled: true,
        clutter_loss_db_per_m: 0.4,
        ..RfSettings::default()
    };
    let link = LinkEvaluator::new(&terrain, &settings)
        .evaluate(
            &a,
            &b,
            EvaluationOptions {
                include_profile: false,
                ..EvaluationOptions::default()
            },
        )
        .unwrap();
    assert_eq!(link.forward.clutter_loss_db, 8.0);
    assert_eq!(link.reverse.clutter_loss_db, 8.0);
    assert!(link.profile.is_none());
}

// Reference profiles and client radio fixture from tests/test_coverage.py.
#[test]
fn strict_los_flat_ridge_and_valley_match_python() {
    let settings = RfSettings {
        validation_mode: ValidationMode::StrictLos,
        ..RfSettings::default()
    };
    let a = site("router", 200.0, 1000.0, SiteKind::Router, 100.0);
    let b = site("client", 1800.0, 1000.0, SiteKind::Client, 1.5);
    let ridge = |x: f64| {
        if (x - 1000.0).abs() < 1e-9 {
            200.0
        } else {
            0.0
        }
    };
    let valley = |x: f64| {
        if (x - 1000.0).abs() < 1e-9 {
            -100.0
        } else {
            0.0
        }
    };
    for (ground, blocked) in [(flat(100.0).ground, false), (ridge, true), (valley, false)] {
        let terrain = TestTerrain {
            ground,
            ..flat(100.0)
        };
        let result = LinkEvaluator::new(&terrain, &settings)
            .evaluate(&a, &b, EvaluationOptions::default())
            .unwrap();
        assert_eq!(
            (result.valid, result.los_clear, result.fresnel_clear),
            (!blocked, !blocked, !blocked)
        );
        near(result.maximum_fresnel_radius_m, 11.743716374962917, 1e-11);
        if blocked {
            near(result.minimum_clearance_m, -149.30022759378434, 1e-10);
            near(
                result.minimum_fresnel_clearance_ratio,
                -12.71320107083698,
                1e-10,
            );
            near(result.diffraction_loss_db, 47.98844327794896, 1e-10);
            near(result.worst_margin_db(), 1.0037653421579193, 1e-10);
            assert_eq!(result.minimum_clearance_distance_m, 800.0);
            let propagation = RfSettings::default();
            let rf_only = LinkEvaluator::new(&terrain, &propagation)
                .evaluate(&a, &b, EvaluationOptions::default())
                .unwrap();
            assert!(rf_only.valid && !rf_only.los_clear);
        } else {
            near(result.minimum_clearance_m, 7.644477907706796, 1e-10);
            near(
                result.minimum_fresnel_clearance_ratio,
                1.344579957788927,
                1e-10,
            );
            near(result.worst_margin_db(), 48.99220862010688, 1e-10);
            assert_eq!(result.diffraction_loss_db, 0.0);
            assert_eq!(result.minimum_clearance_distance_m, 1500.0);
        }
    }
}

#[test]
fn asymmetric_client_radio_changes_transmit_and_receive_budgets() {
    let radio = RadioBudget {
        antenna: AntennaSettings {
            pattern: AntennaPattern::ConstantGain(0.0),
            height_agl_m: 1.2,
            ..AntennaSettings::default()
        },
        tx_power_dbm: 14.0,
        sensitivity_dbm: -122.0,
        miscellaneous_loss_db: 2.0,
    };
    let a = site("router", 0.0, 50.0, SiteKind::Router, 10.0);
    let b = site("client", 1000.0, 50.0, SiteKind::Client, 1.2);
    let link = LinkEvaluator::new(&flat(10.0), &RfSettings::default())
        .evaluate(
            &a,
            &b,
            EvaluationOptions {
                target_radio: Some(&radio),
                ..EvaluationOptions::default()
            },
        )
        .unwrap();
    assert!(link.valid);
    assert_eq!(link.forward.source_id, "router");
    assert_eq!(link.reverse.source_id, "client");
    assert_eq!(link.forward.sensitivity_dbm, -122.0);
    assert_eq!(link.reverse.sensitivity_dbm, -130.0);
    assert_eq!(link.forward.tx_power_dbm, 22.0);
    assert_eq!(link.reverse.tx_power_dbm, 14.0);
    assert_eq!(link.forward.miscellaneous_loss_db, 0.0);
    assert_eq!(link.reverse.miscellaneous_loss_db, 2.0);
    near(link.forward.received_power_dbm, -73.40021559174679, 1e-10);
    near(link.reverse.received_power_dbm, -83.40021559174677, 1e-10);
    near(link.forward.usable_margin_db, 38.599784408253214, 1e-10);
    near(link.reverse.usable_margin_db, 36.59978440825323, 1e-10);
    near(link.forward.departure_angle_deg, -0.5041898451633419, 1e-12);
    near(
        link.reverse.departure_angle_deg,
        link.forward.arrival_angle_deg,
        1e-12,
    );
}

#[test]
fn missing_ground_and_coincident_sites_fail_profile_evaluation() {
    let terrain = TestTerrain {
        ground: |x| {
            if (x - 1000.0).abs() < 1e-9 {
                f64::NAN
            } else {
                0.0
            }
        },
        ..flat(100.0)
    };
    let a = Site::new("A", 200.0, 1000.0);
    let b = Site::new("B", 1800.0, 1000.0);
    assert!(matches!(
        sample_profile(&terrain, &a, &b, 869.5, 1.0, None),
        Err(RfError::MissingTerrain)
    ));
    assert!(matches!(
        sample_profile(&terrain, &a, &a, 869.5, 1.0, None),
        Err(RfError::InvalidInput(_))
    ));
}

#[test]
fn lora_sensitivity_and_surface_height_reference() {
    let mut settings = RfSettings::default();
    assert_eq!(settings.effective_sensitivity_dbm().unwrap(), -130.0);
    settings.lora.enabled = true;
    settings.lora.manual_sensitivity_override = false;
    near(
        settings.effective_sensitivity_dbm().unwrap(),
        -137.03089986991944,
        1e-12,
    );
    settings.lora.spreading_factor = 6;
    assert!(settings.effective_sensitivity_dbm().is_err());
    let mut a = Site {
        ground_elevation_m: 20.0,
        surface_elevation_m: Some(30.0),
        ..Site::new("A", 0.0, 0.0)
    };
    assert_eq!(a.antenna_absolute_elevation_m(), 23.0);
    a.height_reference = HeightReference::SurfaceDom;
    assert_eq!(a.antenna_absolute_elevation_m(), 33.0);
    a.surface_elevation_m = None;
    assert_eq!(a.antenna_absolute_elevation_m(), 23.0);
}
