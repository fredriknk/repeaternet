use repeaternet_rf_core::{
    AntennaSettings, ElevationPattern, EvaluationOptions, LinkEvaluator, SiteKind, TerrainLayer,
    TerrainSource,
};
use repeaternet_terrain::{ArrayTerrain, TerrainCrs};

use super::*;
use crate::ClientRadioProfile;

#[test]
fn hull_queries_match_brute_force_for_every_prefix() {
    let mut hull = UpperHull::default();
    let points: Vec<_> = (1..=96)
        .map(|i| {
            let x = f64::from(i);
            Point {
                x,
                y: (x / 3.0).sin() * 30.0 + x.sqrt() - 0.025 * x * x,
            }
        })
        .collect();
    for (i, &point) in points.iter().enumerate() {
        hull.push(point);
        for f in -40..=40 {
            let slope = f64::from(f) * 0.25;
            let receiver = Point {
                x: point.x + 0.5,
                y: f64::from(f) * 12.5,
            };
            let support = points[..=i]
                .iter()
                .map(|p| p.y - slope * p.x)
                .reduce(f64::max)
                .unwrap();
            let tangent = points[..=i]
                .iter()
                .map(|p| (p.y - receiver.y) / (receiver.x - p.x))
                .reduce(f64::max)
                .unwrap();
            assert!((hull.support(slope) - support).abs() < 1e-12);
            assert!((hull.tangent(receiver) - tangent).abs() < 1e-12);
        }
    }
}

#[test]
fn quantisation_reserves_state_codes_and_rounds_half_db() {
    for (margin, code) in [
        (f64::NAN, 1),
        (f64::NEG_INFINITY, 2),
        (-20.01, 2),
        (-20.0, 3),
        (-19.76, 3),
        (-19.75, 4),
        (0.0, 43),
        (60.0, 163),
        (106.0, 255),
        (200.0, 255),
    ] {
        assert_eq!(quantise(margin, -20.0), code);
    }
}

#[test]
fn polar_interpolation_wraps_and_preserves_nearest_unknown_state() {
    let mut polar = PolarMargins {
        site: Point::default(),
        grid: SweepGrid {
            radius_m: 20.0,
            step_m: 10.0,
            rays: 4,
            samples_per_ray: 2,
        },
        margins: vec![
            0.0, 0.0, 10.0, 10.0, 10.0, 20.0, 20.0, 20.0, 30.0, 30.0, 30.0, 40.0,
        ],
    };
    assert!((polar.margin_at(-1.0, 1.0, 15.0) - 20.0).abs() < 1e-12);
    assert_eq!(polar.margin_at(0.0, 3.0, 3.0), 0.0);
    polar.margins[5] = f32::NAN;
    assert_eq!(polar.margin_at(0.0, 1.0, 14.0), 0.0);
    assert!(polar.margin_at(1.0, 0.0, 16.0).is_nan());
    polar.margins[5] = f32::NEG_INFINITY;
    assert_eq!(polar.margin_at(1.0, 0.0, 16.0), f64::NEG_INFINITY);
    assert!((polar.margin_at(0.0, 1.0, 16.0) - 6.0).abs() < 1e-12);
}

struct Nearest {
    array: ArrayTerrain,
    pixel: f64,
    origin: Point,
}

impl TerrainSource for Nearest {
    fn resolution_m(&self) -> f64 {
        self.array.resolution_m()
    }

    fn has_surface(&self) -> bool {
        self.array.has_surface()
    }

    fn sample(&self, points: &[Point], layer: TerrainLayer) -> Result<Vec<f64>, RfError> {
        self.array.sample(
            &points
                .iter()
                .map(|p| {
                    let snap = |v: f64, o: f64| o + ((v - o) / self.pixel).round() * self.pixel;
                    Point {
                        x: snap(p.x, self.origin.x),
                        y: snap(p.y, self.origin.y),
                    }
                })
                .collect::<Vec<_>>(),
            layer,
        )
    }
}

impl ElevationLookup for Nearest {
    fn ground(&self, p: Point) -> f64 {
        self.sample(&[p], TerrainLayer::GroundDtm).unwrap()[0]
    }

    fn surface(&self, p: Point) -> Option<f64> {
        self.array
            .has_surface()
            .then(|| self.sample(&[p], TerrainLayer::SurfaceDom).unwrap()[0])
    }

    fn has_surface(&self) -> bool {
        self.array.has_surface()
    }
}

#[test]
fn sweeps_match_link_evaluator_in_all_modes() {
    let step = 30.0;
    let grid = SweepGrid::new(3600.0, step).unwrap();
    let origin = Point {
        x: -3750.197,
        y: -3750.371,
    };
    let pixel = 10.0;
    let width = 751;
    let mut ground = Vec::with_capacity(width * width);
    let mut surface = Vec::with_capacity(width * width);
    for y in 0..width {
        for x in 0..width {
            let dx = (x as f64 - 375.0) / 100.0;
            let dy = (y as f64 - 375.0) / 100.0;
            let elevation = 110.0 + 40.0 * dx.sin() * dy.cos() + 20.0 * (dx + dy).sin();
            ground.push(elevation);
            surface.push(elevation + 8.0 * (0.8 * dx + dy).cos().max(0.0));
        }
    }
    let pattern = AntennaPattern::Elevation(
        ElevationPattern::new(
            vec![-90.0, -3.0, 0.0, 3.0, 90.0],
            vec![-8.0, 1.0, 6.0, 2.0, -3.0],
        )
        .unwrap(),
    );
    let mut rf = RfSettings {
        k_factor: 1.33,
        router: AntennaSettings {
            pattern,
            feed_loss_db: 1.7,
            ..AntennaSettings::default()
        },
        miscellaneous_loss_db: 2.7,
        clutter_loss_db_per_m: 0.15,
        required_fresnel_clearance: 1.0,
        ..RfSettings::default()
    };
    let mut client = ClientRadioProfile::default().as_radio_budget();
    client.miscellaneous_loss_db = 0.7;
    client.antenna.feed_loss_db = 3.0;
    let mut observed = [0; 4];
    for (dom, clutter_enabled, validation_mode) in [
        (false, true, ValidationMode::Propagation),
        (true, false, ValidationMode::Propagation),
        (true, true, ValidationMode::Propagation),
        (true, true, ValidationMode::StrictLos),
    ] {
        let terrain = Nearest {
            array: ArrayTerrain::new(
                width,
                width,
                ground.clone(),
                dom.then(|| surface.clone()),
                origin,
                pixel,
                TerrainCrs::Etrs89Utm33,
            )
            .unwrap(),
            pixel,
            origin,
        };
        let site = Site {
            kind: SiteKind::Router,
            antenna_height_m: 18.0,
            ground_elevation_m: terrain.ground(Point::default()),
            surface_elevation_m: terrain.surface(Point::default()),
            ..Site::new("router", 0.0, 0.0)
        };
        rf.clutter_loss_enabled = clutter_enabled;
        rf.validation_mode = validation_mode;
        let evaluator = LinkEvaluator::new(&terrain, &rf);
        for mode in [LiveMode::TwoWay, LiveMode::Downlink, LiveMode::Uplink] {
            let polar = sweep(&terrain, &site, &rf, &client, mode, grid).unwrap();
            for ray in [0, 17, 47, 87, 122, 164, 220, 309, 387, 510, 690] {
                let azimuth = ray as f64 * TAU / grid.rays as f64;
                let (sin, cos) = azimuth.sin_cos();
                // Verify the production sweep's f64 arithmetic before its f32 storage rounding.
                Sweep::new(&site, &rf, &client, mode, grid).unwrap().ray(&terrain, azimuth, |k, margin| {
                    assert_eq!(polar.ray(ray)[k], margin as f32);
                    if k < 2 {
                        return; // RF profiles use at least three points.
                    }
                    let distance = k as f64 * step;
                    let position = Point { x: distance * sin, y: distance * cos };
                    if (position.distance_to(site.position) / step).ceil() > k as f64 {
                        return; // Coordinate rounding can put the endpoint one ulp past the exact step.
                    }
                    let target = Site {
                        position,
                        ground_elevation_m: terrain.ground(position),
                        antenna_height_m: client.antenna.height_agl_m,
                        kind: SiteKind::Client,
                        ..Site::new("client", 0.0, 0.0)
                    };
                    let link = evaluator.evaluate(&site, &target, EvaluationOptions {
                        sample_step_m: Some(step), include_profile: false,
                        target_radio: Some(&client), ..EvaluationOptions::default()
                    }).unwrap();
                    let expected = if validation_mode == ValidationMode::StrictLos && !link.los_clear {
                        observed[0] += 1;
                        f64::NEG_INFINITY
                    } else {
                        observed[1] += usize::from(link.los_clear && link.diffraction_loss_db == 0.0);
                        observed[2] += usize::from(link.los_clear && link.diffraction_loss_db > 0.0);
                        observed[3] += usize::from(!link.los_clear && link.diffraction_loss_db > 0.0);
                        let down = link.forward.usable_margin_db;
                        let up = link.reverse.usable_margin_db;
                        match mode {
                            LiveMode::TwoWay => down.min(up),
                            LiveMode::Downlink => down,
                            LiveMode::Uplink => up,
                        }
                    };
                    assert!(margin == expected || (margin - expected).abs() < 1e-6,
                        "ray {ray}, k {k}, DOM {dom}, clutter {clutter_enabled}, {mode:?}: {margin} vs {expected}");
                    assert_eq!(polar.ray(ray)[k], expected as f32);
                });
                assert_eq!(polar.ray(ray)[0], polar.ray(ray)[1]);
            }
        }
    }
    assert!(observed.iter().all(|&count| count > 100), "{observed:?}");
}

#[test]
fn first_step_has_no_interior_and_ground_gaps_stop_the_ray() {
    struct Gap;
    impl ElevationLookup for Gap {
        fn ground(&self, p: Point) -> f64 {
            if (p.y - 200.0).abs() < 1.0 {
                f64::NAN
            } else {
                0.0
            }
        }
        fn surface(&self, p: Point) -> Option<f64> {
            Some(if (p.y - 50.0).abs() < 1.0 {
                1000.0
            } else {
                f64::NAN
            })
        }
        fn has_surface(&self) -> bool {
            true
        }
    }
    let grid = SweepGrid::new(300.0, 100.0).unwrap();
    let site = Site::new("A", 0.0, 0.0);
    let rf = RfSettings {
        validation_mode: ValidationMode::StrictLos,
        ..RfSettings::default()
    };
    let client = ClientRadioProfile::default().as_radio_budget();
    let polar = sweep(&Gap, &site, &rf, &client, LiveMode::Downlink, grid).unwrap();
    let ray = polar.ray(0);
    let expected = received_power_dbm(
        rf.tx_power_dbm,
        2.15,
        1.0,
        free_space_path_loss_db(100.0, rf.frequency_mhz).unwrap(),
        2.15,
        0.0,
        rf.miscellaneous_loss_db,
    ) - client.sensitivity_dbm
        - rf.fade_margin_db;
    assert_eq!(ray[..2], [expected as f32; 2]);
    assert!(ray[2..].iter().all(|margin| margin.is_nan()));
}
