use repeaternet_rf_core::{
    delta_bullington_loss_db, AntennaSettings, ElevationPattern, EvaluationOptions, LinkEvaluator,
    SiteKind, TerrainLayer, TerrainSource,
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
            assert!((hull.raw_tangent(receiver) - tangent).abs() < 1e-12);
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
fn fine_two_way_coverage_finds_a_ridge_missed_by_the_coarse_sweep() {
    struct Ridge;
    impl ElevationLookup for Ridge {
        fn ground(&self, point: Point) -> f64 {
            if (point.y - 6_600_120.0).abs() < 20.0 {
                80.0
            } else {
                0.0
            }
        }
        fn surface(&self, _: Point) -> Option<f64> {
            None
        }
        fn has_surface(&self) -> bool {
            false
        }
    }
    let transform = Transformer::new(TerrainCrs::from_epsg(25833).unwrap()).unwrap();
    let site = Site::new("Router", 500_000.0, 6_600_000.0);
    let centre = Point {
        x: site.position.x,
        y: site.position.y + 800.0,
    };
    let rf = RfSettings {
        validation_mode: ValidationMode::StrictLos,
        ..RfSettings::default()
    };
    let radio = ClientRadioProfile::default().as_radio_budget();
    let predict = |step| {
        sweep(
            &Ridge,
            &site,
            &rf,
            &radio,
            LiveMode::TwoWay,
            SweepGrid::new(1_000.0, step).unwrap(),
            10.0,
        )
        .unwrap()
    };
    assert!(predict(200.0).margin_at_point(centre) >= 10.0);
    let fine = predict(30.0);
    assert!(fine.margin_at_point(centre) < 10.0);
    let location = transform.reverse(site.position).unwrap();
    let raster = resample(&fine, location, &transform, -20.0).unwrap();
    let centre = transform.reverse(centre).unwrap();
    assert!(
        raster.margin_at_point(
            Point {
                x: MERCATOR_RADIUS * centre.longitude.to_radians(),
                y: MERCATOR_RADIUS * centre.latitude.to_radians().tan().asinh(),
            },
            -20.0,
        ) < 10.0
    );
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
        propagation: LivePropagation {
            diffraction_model: DiffractionModel::Bullington,
            diffraction_model_configured: DiffractionModel::Bullington,
            model_correction_db: 0.0,
            step_allowance_db: 0.0,
            terminal_clutter_db: 0.0,
        },
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
        let mut site = Site {
            kind: SiteKind::Router,
            antenna_height_m: 18.0,
            ground_elevation_m: terrain.ground(Point::default()),
            surface_elevation_m: terrain.surface(Point::default()),
            ..Site::new("router", 0.0, 0.0)
        };
        rf.clutter_loss_enabled = clutter_enabled;
        rf.validation_mode = validation_mode;
        for model in [
            DiffractionModel::Bullington,
            DiffractionModel::DeltaBullington,
            DiffractionModel::Deygout3,
        ] {
            rf.diffraction_model = model;
            let mut batch_rf = rf.clone();
            if model == DiffractionModel::Deygout3 {
                batch_rf.diffraction_model = DiffractionModel::DeltaBullington;
            }
            for surface in [ObstacleSurface::Ground, ObstacleSurface::Surface] {
                rf.obstacle_surface = surface;
                batch_rf.obstacle_surface = surface;
                let evaluator = LinkEvaluator::new(&terrain, &batch_rf);
                for terminal in [TerminalClutter::None, TerminalClutter::P2108Dom] {
                    site.terminal_clutter = terminal;
                    site.clutter_height_m = dom.then_some(25.0);
                    for mode in [LiveMode::TwoWay, LiveMode::Downlink, LiveMode::Uplink] {
                        let polar =
                            sweep(&terrain, &site, &rf, &client, mode, grid, pixel).unwrap();
                        assert_eq!(polar.propagation().diffraction_model_configured, model);
                        assert_eq!(
                            polar.propagation().diffraction_model,
                            batch_rf.diffraction_model
                        );
                        let sweep = Sweep::new(&site, &rf, &client, mode, grid, pixel).unwrap();
                        compare_with_evaluator(&terrain, &evaluator, &sweep, &polar, &mut observed);
                    }
                }
            }
        }
    }
    assert!(observed.iter().all(|&count| count > 100), "{observed:?}");
}

fn compare_with_evaluator(
    terrain: &Nearest,
    evaluator: &LinkEvaluator<'_, Nearest>,
    sweep: &Sweep<'_>,
    polar: &PolarMargins,
    observed: &mut [usize; 4],
) {
    let site = sweep.site;
    let client = sweep.client;
    let step = sweep.step;
    let exact = sweep.propagation.diffraction_model == DiffractionModel::Bullington;
    // Only live delta-Bullington interpolates the smooth-earth correction.
    let tolerance = if exact { 1e-6 } else { 0.05 };
    for ray in [0, 17, 47, 87, 122, 164, 220, 309, 387, 510, 690] {
        let azimuth = ray as f64 * TAU / polar.ray_count() as f64;
        let (sin, cos) = azimuth.sin_cos();
        // Verify the production sweep before and after its f32 storage rounding.
        sweep.ray(terrain, azimuth, |k, margin| {
            assert_eq!(polar.ray(ray)[k], margin as f32);
            if k < 2 {
                return; // RF profiles use at least three points.
            }
            let distance = k as f64 * step;
            let position = Point {
                x: distance * sin,
                y: distance * cos,
            };
            if (position.distance_to(site.position) / step).ceil() > k as f64 {
                return; // Coordinate rounding can put the endpoint one ulp past the exact step.
            }
            let target = Site {
                position,
                ground_elevation_m: terrain.ground(position),
                antenna_height_m: client.antenna.height_agl_m,
                kind: SiteKind::Client,
                terminal_clutter: TerminalClutter::None,
                ..Site::new("client", 0.0, 0.0)
            };
            let link = evaluator
                .evaluate(
                    site,
                    &target,
                    EvaluationOptions {
                        sample_step_m: Some(step),
                        include_profile: false,
                        step_allowance: true,
                        target_radio: Some(client),
                        ..EvaluationOptions::default()
                    },
                )
                .unwrap();
            let expected =
                if sweep.rf.validation_mode == ValidationMode::StrictLos && !link.los_clear {
                    observed[0] += 1;
                    f64::NEG_INFINITY
                } else {
                    observed[1] += usize::from(link.los_clear && link.diffraction_loss_db == 0.0);
                    observed[2] += usize::from(link.los_clear && link.diffraction_loss_db > 0.0);
                    observed[3] += usize::from(!link.los_clear && link.diffraction_loss_db > 0.0);
                    let down = link.forward.usable_margin_db;
                    let up = link.reverse.usable_margin_db;
                    match sweep.mode {
                        LiveMode::TwoWay => down.min(up),
                        LiveMode::Downlink => down,
                        LiveMode::Uplink => up,
                    }
                };
            assert!(
                margin == expected || (margin - expected).abs() <= tolerance,
                "ray {ray}, sample {k}, {:?}, {:?}, {:?}, {:?}: {margin} vs {expected}",
                sweep.mode,
                sweep.rf.diffraction_model,
                sweep.rf.obstacle_surface,
                site.terminal_clutter,
            );
            if exact {
                assert_eq!(polar.ray(ray)[k], expected as f32);
            }
        });
        assert_eq!(polar.ray(ray)[0], polar.ray(ray)[1]);
    }
}

#[test]
fn incremental_delta_matches_batch_on_fractal_and_smooth_rays() {
    struct Profile<'a> {
        elevations: &'a [f64],
        step: f64,
    }

    impl ElevationLookup for Profile<'_> {
        fn ground(&self, p: Point) -> f64 {
            self.elevations[(p.y / self.step).round() as usize]
        }
        fn surface(&self, p: Point) -> Option<f64> {
            panic!("Unexpected DOM lookup at {p:?}");
        }
        fn has_surface(&self) -> bool {
            false
        }
    }

    let mut seed = 0x8352_04bc_u32;
    let mut next = || {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        f64::from(seed) / f64::from(u32::MAX)
    };
    let mut max_error: f64 = 0.0;
    let mut near_grazing = 0;
    let mut flat_transhorizon = 0;
    let mut rough_delta = 0;
    for case in 0..24 {
        let (step, count) = match case % 4 {
            0 => (120.0, 500),
            1 => (10.0, 900),
            2 => (30.0, 1000),
            _ => (500.0, 120),
        };
        let smooth = case < 8;
        let mut elevations = vec![100.0; count + 1];
        if !smooth {
            for stride in [128, 32, 8, 2] {
                let anchors: Vec<_> = (0..=count / stride + 1).map(|_| next()).collect();
                let amplitude = (stride as f64).sqrt() * (1.0 + 10.0 * next());
                for (i, z) in elevations.iter_mut().enumerate() {
                    let left = i / stride;
                    let fraction = (i % stride) as f64 / stride as f64;
                    let noise = mix_margin(anchors[left], anchors[left + 1], fraction);
                    *z += amplitude * (1.0 - (2.0 * noise - 1.0).abs());
                }
            }
        }
        let terrain = Profile {
            elevations: &elevations,
            step,
        };
        let rf = RfSettings {
            diffraction_model: DiffractionModel::DeltaBullington,
            frequency_mhz: [150.0, 433.0, 869.5, 2400.0][case % 4],
            k_factor: [1.0, 1.33, 0.8][case % 3],
            ..RfSettings::default()
        };
        let site = Site {
            antenna_height_m: if smooth {
                2.0 + case as f64 * 4.0
            } else {
                2.0 + 20.0 * next()
            },
            ground_elevation_m: elevations[0],
            ..Site::new("tx", 0.0, 0.0)
        };
        let mut client = ClientRadioProfile::default().as_radio_budget();
        client.antenna.height_agl_m = if smooth {
            2.0 + case as f64 * 3.0
        } else {
            1.0 + 10.0 * next()
        };
        let grid = SweepGrid::new(step * count as f64, step).unwrap();
        let sweep = Sweep::new(&site, &rf, &client, LiveMode::Downlink, grid, 10.0).unwrap();
        let distances: Vec<_> = (0..=count).map(|i| i as f64 * step).collect();
        sweep.ray(&terrain, 0.0, |j, margin| {
            if j < 2 {
                return; // The live first step has no interior; link profiles have at least one.
            }
            let d = distances[j];
            let hr = elevations[j] + client.antenna.height_agl_m;
            let los: Vec<_> = distances[..=j]
                .iter()
                .map(|x| sweep.ht + (hr - sweep.ht) * x / d)
                .collect();
            let bulges: Vec<_> = distances[..=j]
                .iter()
                .map(|x| x * (d - x) / sweep.ed)
                .collect();
            let batch = delta_bullington_loss_db(
                &distances[..=j],
                &elevations[..=j],
                &bulges,
                &los,
                rf.frequency_mhz,
                rf.k_factor,
            )
            .unwrap();
            let expected = sweep.margin(d, hr, sweep.samples[j - 1].loss_db + batch.loss_db);
            let error = (margin - expected).abs();
            assert!(error <= 0.05, "case {case}, {d} m: {margin} vs {expected}");
            max_error = max_error.max(error);
            let horizon =
                sweep.ed.sqrt() * batch.smooth_agl_m.iter().map(|h| h.sqrt()).sum::<f64>();
            if smooth && batch.loss_db > batch.bullington_db {
                near_grazing += usize::from(d < horizon && batch.smooth_bullington_db > 0.0);
                flat_transhorizon +=
                    usize::from(d >= horizon && (30_000.0..=60_000.0).contains(&d));
            }
            rough_delta += usize::from(!smooth && batch.loss_db > batch.bullington_db);
        });
    }
    assert!(near_grazing > 100 && flat_transhorizon > 100 && rough_delta > 100,
        "{near_grazing} near grazing, {flat_transhorizon} flat transhorizon, {rough_delta} rough with delta");
    println!("Maximum error vs batch: {max_error:.3e} dB");
}

#[test]
fn live_smooth_earth_interpolation_is_bounded() {
    let site = Site::new("tx", 0.0, 0.0);
    let client = ClientRadioProfile::default().as_radio_budget();
    let mut seed = 0xd597_51a9_u32;
    let mut next = || {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        f64::from(seed) / f64::from(u32::MAX)
    };
    let mut max_spherical_error = 0.0_f64;
    let mut max_delta_error = 0.0_f64;
    for k in 0..=6 {
        let k_factor = 0.5 + (4.0 / 3.0 - 0.5) * (f64::from(k) / 6.0);
        for frequency in 0..=13 {
            let rf = RfSettings {
                diffraction_model: DiffractionModel::DeltaBullington,
                frequency_mhz: 400.0 + 200.0 * f64::from(frequency),
                k_factor,
                ..RfSettings::default()
            };
            let step = [10.0, 30.0, 50.0][((k + frequency) % 3) as usize];
            let grid = SweepGrid::new(60_000.0, step).unwrap();
            let sweep = Sweep::new(&site, &rf, &client, LiveMode::Downlink, grid, 10.0).unwrap();
            let spherical = sweep.spherical.as_ref().unwrap();
            for case in 0..4096 {
                // Sample both short and long distances densely.
                let short = (100.0_f64 / step).ceil() as usize;
                let j = short + (next().powi(2) * (sweep.samples.len() - short) as f64) as usize;
                let sample = &sweep.samples[j - 1];
                let d = sample.distance;
                let grazing = d.powi(2) / (4.0 * sweep.ed);
                let clear = grazing + 0.276 * (d * sweep.wavelength).sqrt();
                let [ht, hr] = match case % 8 {
                    0 => [500.0 * next(), 500.0 * next()],
                    1 => [500.0 * next().powi(4), 500.0 * next().powi(4)],
                    2 => [grazing * (0.9 + 0.2 * next()), grazing],
                    3 => [clear * (0.96 + 0.08 * next()), clear],
                    4 => [0.0, 500.0 * next()],
                    5 => [500.0 * next(), 0.000_001],
                    6 => [0.01 + 3.0 * next(), 500.0 * next().powi(4)],
                    _ => [500.0 * next().powi(4), 0.01 + 3.0 * next()],
                }
                .map(|h| h.min(500.0));
                let exact = spherical_earth_loss_db(d, [ht, hr], sweep.ed / 2.0, sweep.wavelength);
                let actual = spherical.loss(j - 1, d, [ht, hr]).0;
                let spherical_error = (actual - exact).abs();
                let exact_delta = (exact - sweep.smooth_bullington(sample, j, ht, hr)).max(0.0);
                let delta = sweep.smooth_earth_correction(sample, j, ht, hr);
                let delta_error = (delta - exact_delta).abs();
                // With a zero endpoint and a collapsed clearance point the
                // exact implementation can return NaN. Preserve that case.
                assert!(
                    (actual.is_nan() && exact.is_nan()) || spherical_error <= 0.05,
                    "{d} m, [{ht}, {hr}] m, k={k_factor}, {} MHz: spherical error {spherical_error}",
                    rf.frequency_mhz,
                );
                assert!(
                    delta_error <= 0.05,
                    "{d} m, [{ht}, {hr}] m, k={k_factor}, {} MHz: delta error {delta_error}",
                    rf.frequency_mhz,
                );
                max_spherical_error = max_spherical_error.max(spherical_error);
                max_delta_error = max_delta_error.max(delta_error);
            }
        }
    }
    println!(
        "Maximum error over 401,408 paths: spherical {max_spherical_error:.6} dB, delta {max_delta_error:.6} dB"
    );
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
            panic!("Ground-only coverage should not sample DOM at {p:?}");
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
    let polar = sweep(&Gap, &site, &rf, &client, LiveMode::Downlink, grid, 10.0).unwrap();
    let ray = polar.ray(0);
    let expected = received_power_dbm(
        rf.tx_power_dbm,
        2.15,
        1.0,
        free_space_path_loss_db(100.0, rf.frequency_mhz).unwrap()
            + rf.model_correction_db
            + step_allowance_db(100.0, 10.0),
        2.15,
        0.0,
        rf.miscellaneous_loss_db,
    ) - client.sensitivity_dbm
        - rf.fade_margin_db;
    assert_eq!(ray[..2], [expected as f32; 2]);
    assert!(ray[2..].iter().all(|margin| margin.is_nan()));
}
