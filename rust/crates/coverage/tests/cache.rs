use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed},
        mpsc, Condvar, Mutex,
    },
    thread,
    time::Duration,
};

use repeaternet_coverage::*;
use repeaternet_rf_core::{
    calibration::{step_allowance_db, COARSE_STEP_ALLOWANCE_DB},
    HeightReference, Point, RfError, RfSettings, Site, SiteKind, TerminalClutter, TerrainLayer,
    TerrainSource,
};
use repeaternet_terrain::{LonLat, TerrainCrs, Transformer};

const REVISION: [u8; 32] = [1; 32];
const CRS: TerrainCrs = TerrainCrs::Etrs89Utm33;

#[derive(Default)]
struct Terrain {
    profiles: AtomicUsize,
    missing: AtomicBool,
    fail_next: AtomicBool,
    entered: Mutex<usize>,
    entered_changed: Condvar,
    blocked: Mutex<bool>,
    released: Condvar,
    ridge: f64,
}

impl TerrainSource for Terrain {
    fn resolution_m(&self) -> f64 {
        20.0
    }

    fn has_surface(&self) -> bool {
        false
    }

    fn sample(&self, points: &[Point], _: TerrainLayer) -> Result<Vec<f64>, RfError> {
        if points.len() > 1 {
            self.profiles.fetch_add(1, Relaxed);
            {
                let mut entered = self.entered.lock().unwrap();
                *entered += 1;
                self.entered_changed.notify_all();
            }
            drop(
                self.released
                    .wait_while(self.blocked.lock().unwrap(), |blocked| *blocked)
                    .unwrap(),
            );
            if self.fail_next.swap(false, Relaxed) {
                return Err(RfError::InvalidInput("Injected profile read error"));
            }
            if self.missing.load(Relaxed) {
                return Ok(vec![f64::NAN; points.len()]);
            }
        }
        Ok(points
            .iter()
            .map(|point| {
                if (500_400.0..500_600.0).contains(&point.x) {
                    self.ridge
                } else {
                    0.0
                }
            })
            .collect())
    }
}

fn source() -> Site {
    let mut source = Site::new("router", 500_000.0, 6_500_000.0);
    source.kind = SiteKind::Router;
    source.antenna_height_m = 30.0;
    source
}

fn settings() -> CoverageSettings {
    CoverageSettings {
        cell_size_m: 100.0,
        profile_step_m: 20.0,
        maximum_cells: 1,
        ..CoverageSettings::default()
    }
}

fn area(x: f64) -> CoverageArea {
    CoverageArea::rectangle([x, 6_499_950.0, x + 100.0, 6_500_050.0]).unwrap()
}

fn grid(
    eval: &CoverageEvaluator<'_, Terrain>,
    source: &Site,
    settings: &CoverageSettings,
    area: &CoverageArea,
) -> CoverageGrid {
    eval.calculate(
        std::slice::from_ref(source),
        settings,
        area,
        RunOptions::default(),
    )
    .unwrap()
}

#[test]
fn warm_grid_matches_uncached_and_rf_input_changes_recompute() {
    let cache = CoverageMetricsCache::new(64);
    let terrain = Terrain::default();
    let rf = RfSettings::default();
    let transform = Transformer::new(CRS).unwrap();
    let area = area(500_950.0);
    let site = source();
    let config = settings();
    let evaluate = |terrain: &Terrain,
                    rf: &RfSettings,
                    revision,
                    source: &Site,
                    settings: &CoverageSettings| {
        let uncached = CoverageEvaluator::new(terrain, rf, &transform);
        let cached = CoverageEvaluator::new(terrain, rf, &transform).with_cache(&cache, revision);
        let expected = grid(&uncached, source, settings, &area);
        let before = terrain.profiles.load(Relaxed);
        assert_eq!(grid(&cached, source, settings, &area), expected);
        assert_eq!(terrain.profiles.load(Relaxed), before + 1);
        assert_eq!(grid(&cached, source, settings, &area), expected);
        assert_eq!(terrain.profiles.load(Relaxed), before + 1);
        expected
    };
    let baseline = evaluate(&terrain, &rf, REVISION, &site, &config);
    let ridge = Terrain {
        ridge: 180.0,
        ..Terrain::default()
    };
    let revised = evaluate(&ridge, &rf, [2; 32], &site, &config);
    assert_ne!(baseline, revised);
    // Model the same terrain in an independent project.
    assert_eq!(baseline, evaluate(&terrain, &rf, [3; 32], &site, &config));
    let mut changed_rf = rf.clone();
    changed_rf.fade_margin_db += 6.0;
    assert_ne!(
        baseline,
        evaluate(&terrain, &changed_rf, REVISION, &site, &config)
    );
    let mut changed_client = config.clone();
    changed_client.client.tx_power_dbm -= 10.0;
    assert_ne!(
        baseline,
        evaluate(&terrain, &rf, REVISION, &site, &changed_client)
    );
    let mut sampled = config.clone();
    sampled.profile_step_m *= 2.0;
    evaluate(&terrain, &rf, REVISION, &site, &sampled);

    let adjusted = apply_height_overrides(
        std::slice::from_ref(&site),
        &BTreeMap::from([("router".into(), 5.0)]),
    )
    .unwrap();
    assert_ne!(
        baseline,
        evaluate(&terrain, &rf, REVISION, &adjusted[0], &config)
    );
    for variant in [
        Site {
            position: Point {
                x: site.position.x + 1.0,
                ..site.position
            },
            ..site.clone()
        },
        Site {
            ground_elevation_m: 2.0,
            ..site.clone()
        },
        Site {
            surface_elevation_m: Some(2.0),
            ..site.clone()
        },
        Site {
            height_reference: HeightReference::SurfaceDom,
            ..site.clone()
        },
        Site {
            kind: SiteKind::EndpointA,
            ..site.clone()
        },
        Site {
            terminal_clutter: TerminalClutter::P2108Dom,
            ..site.clone()
        },
        Site {
            terminal_clutter: TerminalClutter::P2108Dom,
            clutter_height_m: Some(40.0),
            ..site.clone()
        },
    ] {
        evaluate(&terrain, &rf, REVISION, &variant, &config);
    }

    let eval = CoverageEvaluator::new(&terrain, &rf, &transform).with_cache(&cache, REVISION);
    let before = cache.stats();
    let mut stricter = config.clone();
    stricter.maximum_profile_samples = 3;
    assert_eq!(
        grid(&eval, &site, &stricter, &area).cells[0].sources[0].rejection,
        Some(Rejection::ProfileSampleLimit)
    );
    assert_eq!(cache.stats(), before);
    // Display mode and labels do not affect RF. An older scalar must be
    // rendered with the identity of the current source.
    let mut relabelled = site;
    relabelled.id = "new label".into();
    let mut downlink = config;
    downlink.mode = CoverageMode::Downlink;
    let cell = &grid(&eval, &relabelled, &downlink, &area).cells[0];
    assert_eq!(cell.sources[0].source_id, "new label");
    assert_eq!(cache.stats().hits, before.hits + 1);
}

#[test]
fn saved_grid_and_point_budget_apply_the_calibrated_step_allowance() {
    let terrain = Terrain::default();
    let rf = RfSettings::default();
    let transform = Transformer::new(CRS).unwrap();
    let evaluator = CoverageEvaluator::new(&terrain, &rf, &transform);
    let source = source();
    let sources = std::slice::from_ref(&source);
    let step = COARSE_STEP_ALLOWANCE_DB[2].0;
    let allowance = step_allowance_db(step, terrain.resolution_m());
    let area = area(500_950.0);
    let fine = settings();
    let coarse = CoverageSettings {
        profile_step_m: step,
        ..fine.clone()
    };
    let fine_grid = grid(&evaluator, &source, &fine, &area);
    let coarse_grid = grid(&evaluator, &source, &coarse, &area);
    let coarse_cell = &coarse_grid.cells[0];
    let margin = fine_grid.cells[0].best_margin_db.unwrap() - coarse_cell.best_margin_db.unwrap();
    assert!((margin - allowance).abs() < 0.01);

    let link = evaluator
        .evaluate_point(
            LonLat {
                longitude: coarse_cell.longitude,
                latitude: coarse_cell.latitude,
            },
            sources,
            &coarse,
            ExactPointOptions {
                final_sample_step_m: step,
                include_serving_profile: true,
                cancelled: &|| false,
            },
        )
        .unwrap()
        .profile_link
        .unwrap();
    assert_eq!(link.forward.step_allowance_db, allowance);
    assert_eq!(link.reverse.step_allowance_db, allowance);
}

#[test]
fn unknown_errors_and_profiles_do_not_populate_the_scalar_cache() {
    let cache = CoverageMetricsCache::new(8);
    let terrain = Terrain::default();
    let rf = RfSettings::default();
    let transform = Transformer::new(CRS).unwrap();
    let cached = CoverageEvaluator::new(&terrain, &rf, &transform).with_cache(&cache, REVISION);
    let site = source();
    let settings = settings();
    let area = area(500_950.0);
    terrain.fail_next.store(true, Relaxed);
    assert!(cached
        .calculate(
            std::slice::from_ref(&site),
            &settings,
            &area,
            RunOptions::default()
        )
        .is_err());
    terrain.missing.store(true, Relaxed);
    for _ in 0..2 {
        assert_eq!(
            grid(&cached, &site, &settings, &area).cells[0].state,
            CoverageState::UnknownTerrain
        );
    }
    assert_eq!(cache.stats().entries, 0);
    assert_eq!(terrain.profiles.load(Relaxed), 3);
    terrain.missing.store(false, Relaxed);
    let location: LonLat = transform
        .reverse(Point {
            x: 501_000.0,
            y: 6_500_000.0,
        })
        .unwrap();
    for expected_profiles in [5, 6] {
        let result = cached
            .evaluate_point(
                location,
                std::slice::from_ref(&site),
                &settings,
                ExactPointOptions {
                    final_sample_step_m: 20.0,
                    include_serving_profile: true,
                    cancelled: &|| false,
                },
            )
            .unwrap();
        assert!(result.profile_link.unwrap().profile.is_some());
        assert_eq!(terrain.profiles.load(Relaxed), expected_profiles);
        assert_eq!(cache.stats().entries, 1);
    }
    assert_eq!(cache.stats().hits, 1);
}

#[test]
fn lru_retains_recent_links_under_capacity_pressure() {
    let cache = CoverageMetricsCache::new(2);
    let terrain = Terrain::default();
    let rf = RfSettings::default();
    let transform = Transformer::new(CRS).unwrap();
    let eval = CoverageEvaluator::new(&terrain, &rf, &transform).with_cache(&cache, REVISION);
    for offset in [0.0, 100.0, 0.0, 200.0, 0.0, 100.0] {
        grid(&eval, &source(), &settings(), &area(500_950.0 + offset));
        assert!(cache.stats().entries <= 2);
    }
    assert_eq!(
        cache.stats(),
        CoverageCacheStats {
            hits: 2,
            misses: 4,
            evictions: 2,
            entries: 2,
            capacity: 2
        }
    );
}

#[test]
fn cancelled_owner_leaves_no_entry_and_can_be_retried() {
    let cache = CoverageMetricsCache::new(1);
    let terrain = Terrain::default();
    let rf = RfSettings::default();
    let transform = Transformer::new(CRS).unwrap();
    let eval = CoverageEvaluator::new(&terrain, &rf, &transform).with_cache(&cache, REVISION);
    let config = settings();
    let area = area(500_950.0);
    let source = source();
    let partial = eval
        .calculate(
            std::slice::from_ref(&source),
            &config,
            &area,
            RunOptions {
                cancelled: &|| terrain.profiles.load(Relaxed) == 1,
                ..RunOptions::default()
            },
        )
        .unwrap();
    assert_eq!(partial.completed_cells, 0);
    assert_eq!(cache.stats().entries, 0);
    let retry = grid(&eval, &source, &config, &area);
    assert_eq!(retry.completed_cells, 1);
    assert_eq!(cache.stats().entries, 1);
    assert_eq!(terrain.profiles.load(Relaxed), 2);
}

#[test]
fn scenario_backbone_reuses_scalars_with_its_own_radio_and_sampling_cap() {
    let cache = CoverageMetricsCache::new(4);
    let terrain = Terrain {
        ridge: 50.0,
        ..Terrain::default()
    };
    let rf = RfSettings::default();
    let transform = Transformer::new(CRS).unwrap();
    let eval = CoverageEvaluator::new(&terrain, &rf, &transform).with_cache(&cache, REVISION);
    let left = source();
    let right = Site {
        id: "right".into(),
        position: Point {
            x: 501_000.0,
            ..left.position
        },
        ..left.clone()
    };
    let network = CoverageNetwork {
        router_ids: vec![left.id.clone(), right.id.clone()],
        source_ids: vec![left.id.clone(), right.id.clone()],
        network_links: vec![NetworkLink {
            source_id: left.id.clone(),
            target_id: right.id.clone(),
            valid: Some(true),
            unresolved: false,
            rejection: None,
            rejection_detail: None,
        }],
    };
    let sites = [left, right];
    let revalidate = |height, cap| {
        let overrides = BTreeMap::from([("router".into(), height)]);
        let adjusted = apply_height_overrides(&sites, &overrides).unwrap();
        eval.revalidate_backbone(
            &network,
            &sites,
            &adjusted,
            &overrides,
            &CoverageSettings {
                maximum_profile_samples: cap,
                ..settings()
            },
            &|| false,
        )
        .unwrap()
    };
    let cold = revalidate(10.0, 128);
    assert_eq!(cold, revalidate(10.0, 128));
    assert_eq!(terrain.profiles.load(Relaxed), 1);
    assert_eq!(revalidate(10.0, 3).backbone_unresolved_edges, 1);
    revalidate(20.0, 128);
    assert_eq!(terrain.profiles.load(Relaxed), 2);
    assert_eq!(cache.stats().hits, 1);
}

#[test]
fn concurrent_waiters_reuse_a_link_and_cancel_independently() {
    let cache = CoverageMetricsCache::new(2);
    let terrain = Terrain {
        blocked: Mutex::new(true),
        ..Terrain::default()
    };
    let rf = RfSettings::default();
    let transform = Transformer::new(CRS).unwrap();
    let eval = CoverageEvaluator::new(&terrain, &rf, &transform).with_cache(&cache, REVISION);
    let source = [source()];
    let settings = settings();
    let area = area(500_950.0);
    let calculate = |cancelled: &dyn Fn() -> bool| {
        eval.calculate(
            &source,
            &settings,
            &area,
            RunOptions {
                cancelled,
                ..RunOptions::default()
            },
        )
        .unwrap()
    };
    let stop = AtomicBool::new(false);
    let (waiting, waiter_started) = mpsc::channel();
    let (finished, waiter_finished) = mpsc::channel();
    thread::scope(|scope| {
        let owner = scope.spawn(|| calculate(&|| false));
        let entered = terrain
            .entered_changed
            .wait_timeout_while(
                terrain.entered.lock().unwrap(),
                Duration::from_secs(5),
                |count| *count == 0,
            )
            .unwrap();
        let owner_entered = *entered.0 > 0;
        drop(entered.0);
        let warm = scope.spawn(|| calculate(&|| false));
        scope.spawn(|| {
            let polls = AtomicUsize::new(0);
            let grid = calculate(&|| {
                // Several polls occur in grid and ground preparation. Keep
                // going until a cache wait has had time to cycle.
                if polls.fetch_add(1, Relaxed) == 12 {
                    waiting.send(()).unwrap();
                }
                stop.load(Relaxed)
            });
            finished.send(grid.completed_cells).unwrap();
        });
        let waiter_polled = waiter_started.recv_timeout(Duration::from_secs(2)).is_ok();
        stop.store(true, Relaxed);
        let stopped = waiter_finished.recv_timeout(Duration::from_secs(2));
        *terrain.blocked.lock().unwrap() = false;
        terrain.released.notify_all();
        assert!(owner_entered && waiter_polled);
        assert_eq!(stopped.unwrap(), 0);
        assert_eq!(owner.join().unwrap(), warm.join().unwrap());
    });
    assert_eq!(terrain.profiles.load(Relaxed), 1);
    assert_eq!(cache.stats().hits, 1);
    assert_eq!(cache.stats().misses, 1);
}
