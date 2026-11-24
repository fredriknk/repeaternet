use std::{
    cell::Cell,
    collections::{BTreeMap, BTreeSet},
    io::Cursor,
};

use repeaternet_coverage::*;
use repeaternet_rf_core::{
    DiffractionModel, ObstacleSurface, Point, RfError, RfSettings, Site, SiteKind, TerrainLayer,
    TerrainSource, ValidationMode,
};
use repeaternet_terrain::{ArrayTerrain, LonLat, TerrainCrs, Transformer};
use serde_json::{json, Value};

const CRS: TerrainCrs = TerrainCrs::Etrs89Utm33;

fn router(id: &str, x: f64, y: f64) -> Site {
    let mut source = Site::new(id, x, y);
    source.kind = SiteKind::Router;
    source.antenna_height_m = 100.0;
    source
}

fn terrain(strip: f64, origin: Point) -> ArrayTerrain {
    let mut values = vec![0.0; 21 * 21];
    for row in values.chunks_mut(21) {
        row[10] = strip;
    }
    ArrayTerrain::new(21, 21, values, None, origin, 100.0, CRS).unwrap()
}

fn python_fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/coverage-python-v3.json")).unwrap()
}

#[test]
fn full_python_grid_fixture_matches_real_rf_and_wire_defaults() {
    let fixture = python_fixture();
    let settings = CoverageSettings {
        cell_size_m: 500.0,
        profile_step_m: 100.0,
        maximum_cells: 1,
        ..CoverageSettings::default()
    };
    let rust_defaults = serde_json::to_value(CoverageSettings::default()).unwrap();
    assert_eq!(rust_defaults, fixture["defaults"]);
    assert_eq!(
        serde_json::from_value::<CoverageSettings>(json!({})).unwrap(),
        CoverageSettings::default()
    );
    let transform = Transformer::new(CRS).unwrap();
    let source = router("router", 400.0, 1000.0);
    for (case, strip, strict, target_x) in [
        ("flat", 0.0, true, 1500.0),
        ("ridge", 200.0, true, 1500.0),
        ("valley", -100.0, true, 1500.0),
        ("path_nodata", f64::NAN, false, 1500.0),
        ("ground_nodata", f64::NAN, false, 1100.0),
        ("capped", 0.0, false, 1500.0),
        ("asymmetric", 0.0, false, 1500.0),
    ] {
        let mut settings = settings.clone();
        if case == "capped" {
            settings.maximum_profile_samples = 8;
        }
        if case == "asymmetric" {
            settings.client = ClientRadioProfile {
                height_agl_m: 1.2,
                tx_power_dbm: 14.0,
                gain_dbi: 0.0,
                feed_loss_db: 1.0,
                sensitivity_dbm: -75.0,
                miscellaneous_loss_db: 2.0,
                ..ClientRadioProfile::default()
            };
        }
        // The Python oracle used Bullington over the surface with no model correction.
        // This fixture samples at the native raster resolution, so no step allowance applies.
        let rf = RfSettings {
            diffraction_model: DiffractionModel::Bullington,
            obstacle_surface: ObstacleSurface::Surface,
            model_correction_db: 0.0,
            validation_mode: if strict {
                ValidationMode::StrictLos
            } else {
                ValidationMode::Propagation
            },
            ..RfSettings::default()
        };
        let terrain = terrain(strip, Point::default());
        let evaluator = CoverageEvaluator::new(&terrain, &rf, &transform);
        let area =
            CoverageArea::rectangle([target_x - 250.0, 750.0, target_x + 250.0, 1250.0]).unwrap();
        let mut grid = evaluator
            .calculate(
                std::slice::from_ref(&source),
                &settings,
                &area,
                RunOptions::default(),
            )
            .unwrap();
        let expected: CoverageGrid = serde_json::from_value(fixture[case].clone()).unwrap();
        if case == "asymmetric" {
            assert!(!grid.cells[0].sources[0].valid_downlink);
            assert!(grid.cells[0].sources[0].valid_uplink);
            assert_eq!(grid.cells[0].state, CoverageState::Uncovered);
            settings.mode = CoverageMode::Uplink;
            let directional = evaluator
                .calculate(
                    std::slice::from_ref(&source),
                    &settings,
                    &area,
                    RunOptions::default(),
                )
                .unwrap();
            assert_eq!(directional.cells[0].state, CoverageState::Covered);
        }
        let actual = &mut grid.cells[0];
        let oracle = &expected.cells[0];
        assert!(
            (actual.latitude - oracle.latitude).abs() < 1e-8,
            "{case} latitude"
        );
        assert!(
            (actual.longitude - oracle.longitude).abs() < 1e-8,
            "{case} longitude"
        );
        actual.latitude = oracle.latitude;
        actual.longitude = oracle.longitude;
        check_margin(&mut actual.best_margin_db, oracle.best_margin_db, case);
        for (row, expected_row) in actual.sources.iter_mut().zip(&oracle.sources) {
            check_margin(
                &mut row.downlink_margin_db,
                expected_row.downlink_margin_db,
                case,
            );
            check_margin(
                &mut row.uplink_margin_db,
                expected_row.uplink_margin_db,
                case,
            );
            check_margin(
                &mut row.two_way_margin_db,
                expected_row.two_way_margin_db,
                case,
            );
        }
        assert_eq!(grid, expected, "{case}");
    }
}

fn check_margin(actual: &mut Option<f64>, oracle: Option<f64>, case: &str) {
    match (*actual, oracle) {
        (Some(a), Some(b)) => assert!((a - b).abs() < 1e-7, "{case}: {a} != {b}"),
        _ => assert_eq!(*actual, oracle, "{case}"),
    }
    *actual = oracle;
}

struct CountingTerrain {
    samples: Cell<usize>,
    cancel_after: Option<usize>,
}

impl TerrainSource for CountingTerrain {
    fn resolution_m(&self) -> f64 {
        100.0
    }
    fn has_surface(&self) -> bool {
        false
    }
    fn sample(&self, points: &[Point], _: TerrainLayer) -> Result<Vec<f64>, RfError> {
        self.samples.set(self.samples.get() + 1);
        Ok(vec![0.0; points.len()])
    }
}

impl CountingTerrain {
    fn cancelled(&self) -> bool {
        self.cancel_after
            .is_some_and(|limit| self.samples.get() >= limit)
    }
}

#[test]
fn grid_mask_orientation_estimate_and_preview_limits() {
    let transform = Transformer::new(CRS).unwrap();
    let origin = transform
        .forward(LonLat {
            latitude: 60.0,
            longitude: 15.0,
        })
        .unwrap();
    let to_wgs = |x, y| {
        let p = transform
            .reverse(Point {
                x: origin.x + x,
                y: origin.y + y,
            })
            .unwrap();
        [p.longitude, p.latitude]
    };
    let settings = CoverageSettings {
        area_mode: AreaMode::Drawn,
        area_polygon_wgs84: Some(vec![
            to_wgs(0.0, 0.0),
            to_wgs(400.0, 0.0),
            to_wgs(0.0, 300.0),
            to_wgs(0.0, 0.0),
        ]),
        cell_size_m: 100.0,
        ..CoverageSettings::default()
    };
    let source = router("R", origin.x + 20.0, origin.y + 20.0);
    let sources = [source];
    let area = resolve_area(&settings, &transform, &sources, &[], 100.0, None).unwrap();
    let terrain = terrain(0.0, origin);
    let rf = RfSettings::default();
    let evaluator = CoverageEvaluator::new(&terrain, &rf, &transform).with_parallel_workers(4);
    let estimate = evaluator
        .estimate(&sources, &settings, &area, &|| false)
        .unwrap();
    let grid = evaluator
        .calculate(&sources, &settings, &area, RunOptions::default())
        .unwrap();
    assert!(estimate.outside_area_cells > 0 && estimate.requested_area_cells > 0);
    assert_eq!(estimate.requested_area_cells, grid.inside_area_cells);
    assert_eq!(estimate.unknown_cells, 0);
    assert_eq!(
        grid.cells
            .iter()
            .filter(|cell| cell.state == CoverageState::OutsideArea)
            .count(),
        grid.outside_area_cells
    );
    assert!(grid.cells[0].x < grid.cells[1].x);
    assert!(grid.cells[0].y > grid.cells[grid.columns].y);
    assert_eq!(
        cell_footprint(
            grid.cells
                .iter()
                .find(|cell| cell.state == CoverageState::OutsideArea)
                .unwrap(),
            grid.bounds,
            grid.effective_cell_size_m,
            &transform
        )
        .unwrap(),
        None
    );
    let many_sources: Vec<_> = (0..32)
        .map(|i| router(&format!("R{i}"), origin.x, origin.y))
        .collect();
    let preview = CoverageSettings {
        cell_size_m: 1.0,
        ..CoverageSettings::default()
    }
    .preview_settings();
    let estimate = evaluator
        .estimate(&many_sources, &preview, &area, &|| false)
        .unwrap();
    assert!(estimate.requested_cells <= 128 && estimate.planned_evaluations <= 4096);
    assert!(estimate.effective_cell_size_m > 1.0);
}

#[test]
fn profile_caps_avoid_sampling_and_interrupted_cells_never_stream() {
    let transform = Transformer::new(CRS).unwrap();
    let rf = RfSettings::default();
    let sources = [router("A", 100.0, 1000.0), router("B", 200.0, 1000.0)];
    let area = CoverageArea::rectangle([1000.0, 900.0, 1400.0, 1100.0]).unwrap();
    let terrain = CountingTerrain {
        samples: Cell::new(0),
        cancel_after: None,
    };
    let evaluator = CoverageEvaluator::new(&terrain, &rf, &transform);
    let mut settings = CoverageSettings {
        cell_size_m: 200.0,
        maximum_profile_samples: 8,
        profile_step_m: 100.0,
        ..CoverageSettings::default()
    };
    let capped = evaluator
        .calculate_serial(&sources, &settings, &area, RunOptions::default())
        .unwrap();
    assert_eq!(terrain.samples.get(), 1);
    assert_eq!(capped.unresolved_cells, 2);
    assert_eq!(capped.unresolved_source_evaluations, 4);
    settings.maximum_profile_samples = 32;
    settings.maximum_evaluations = 1;
    assert!(evaluator
        .calculate_serial(&sources, &settings, &area, RunOptions::default())
        .is_err());
    settings.maximum_evaluations = 10;
    let full = evaluator
        .calculate_serial(&sources, &settings, &area, RunOptions::default())
        .unwrap();

    let terrain = CountingTerrain {
        samples: Cell::new(0),
        cancel_after: Some(4),
    };
    let evaluator = CoverageEvaluator::new(&terrain, &rf, &transform);
    let mut stream = Vec::new();
    let cancel = || terrain.cancelled();
    let mut sink = |cells: &[CoverageCell]| write_cell_chunk(&mut stream, cells);
    let prefix = evaluator
        .calculate_serial(
            &sources,
            &settings,
            &area,
            RunOptions {
                cancelled: &cancel,
                on_chunk: Some(&mut sink),
                chunk_size: 128,
                ..RunOptions::default()
            },
        )
        .unwrap();
    assert_eq!(prefix.completed_cells, 1);
    assert!(prefix.cells.is_empty());
    let stored: Vec<_> = read_cell_stream(Cursor::new(stream), 64 * 1024)
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(stored, full.cells[..1]);
    assert!(read_cell_stream(Cursor::new(b"[123456]\n"), 4)
        .next()
        .unwrap()
        .is_err());
}

#[test]
fn parallel_grid_stream_matches_serial_bytes_and_cancellation_prefix() {
    let terrain = terrain(f64::NAN, Point::default());
    let rf = RfSettings::default();
    let transform = Transformer::new(CRS).unwrap();
    let settings = CoverageSettings {
        cell_size_m: 100.0,
        profile_step_m: 100.0,
        maximum_profile_samples: 8,
        ..CoverageSettings::default()
    };
    let sources = [router("B", 1850.0, 1700.0), router("A", 200.0, 1700.0)];
    let area = CoverageArea::rectangle([0.0, 0.0, 1900.0, 1800.0]).unwrap();
    let run = |workers, cancel_at| {
        let mut bytes = Vec::new();
        let mut reports = Vec::new();
        let stop = Cell::new(false);
        let grid = CoverageEvaluator::new(&terrain, &rf, &transform)
            .with_parallel_workers(workers)
            .calculate(
                &sources,
                &settings,
                &area,
                RunOptions {
                    cancelled: &|| stop.get(),
                    progress: Some(&mut |done, total| {
                        reports.push((done, total));
                        if Some(done) == cancel_at {
                            stop.set(true);
                        }
                    }),
                    on_chunk: Some(&mut |cells| write_cell_chunk(&mut bytes, cells)),
                    chunk_size: 7,
                },
            )
            .unwrap();
        (grid, bytes, reports)
    };
    let serial = run(1, None);
    let parallel = run(4, None);
    assert_eq!(parallel, serial);
    assert_eq!(parallel.2.last(), Some(&(342, 342)));
    assert!(parallel.2.windows(2).all(|pair| pair[0].0 < pair[1].0));

    let (prefix, bytes, reports) = run(4, Some(20));
    assert_eq!(reports, [(20, 342)]);
    let serial_prefix = run(1, Some(20));
    assert_eq!(prefix, serial_prefix.0);
    assert_eq!(bytes, serial_prefix.1);
    assert_eq!(reports, serial_prefix.2);
    let chunks: Vec<Vec<CoverageCell>> = bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect();
    assert_eq!(chunks.iter().map(Vec::len).collect::<Vec<_>>(), [7, 7, 6]);
    let full_cells: Vec<_> = read_cell_stream(Cursor::new(parallel.1), 64 * 1024)
        .collect::<Result<_, _>>()
        .unwrap();
    let cells = chunks.concat();
    assert_eq!(cells, full_cells[..20]);
    let count = |state| cells.iter().filter(|cell| cell.state == state).count();
    assert_eq!(prefix.completed_cells, cells.len());
    assert_eq!(
        prefix.evaluated_cells,
        count(CoverageState::Covered) + count(CoverageState::Uncovered)
    );
    assert_eq!(prefix.unknown_cells, count(CoverageState::UnknownTerrain));
    assert_eq!(prefix.unresolved_cells, count(CoverageState::Unresolved));
    assert_eq!(
        prefix.unresolved_source_evaluations,
        cells
            .iter()
            .map(|cell| cell.unresolved_sources)
            .sum::<usize>()
    );
}

fn ids(ids: &[&str]) -> Vec<String> {
    ids.iter().map(|id| (*id).to_owned()).collect()
}

fn edge(a: &str, b: &str, valid: Option<bool>) -> NetworkLink {
    NetworkLink {
        source_id: a.into(),
        target_id: b.into(),
        valid,
        unresolved: valid.is_none(),
        rejection: None,
        rejection_detail: None,
    }
}

fn cell(index: usize, state: CoverageState, source: Option<(&str, f64)>) -> CoverageCell {
    CoverageCell {
        index,
        state,
        latitude: 60.0,
        longitude: 10.0 + index as f64 * 0.01,
        sources: source
            .into_iter()
            .map(|(id, margin)| CoverageSourceResult {
                source_id: id.into(),
                valid_two_way: true,
                two_way_margin_db: Some(margin),
                ..CoverageSourceResult::default()
            })
            .collect(),
        ..CoverageCell::default()
    }
}

#[test]
fn bridge_failure_loses_reference_coverage_without_losing_local_service() {
    let network = CoverageNetwork {
        router_ids: ids(&["A", "B", "C", "D"]),
        source_ids: ids(&["A", "B", "C"]),
        network_links: vec![
            edge("A", "B", Some(true)),
            edge("B", "C", Some(true)),
            edge("A", "D", None),
        ],
    };
    let cells = [
        cell(0, CoverageState::Covered, Some(("C", 7.0))),
        cell(1, CoverageState::Covered, Some(("B", 4.0))),
        cell(2, CoverageState::UnknownTerrain, None),
        cell(3, CoverageState::Unresolved, None),
        cell(4, CoverageState::OutsideArea, None),
    ];
    let report = analyze_node_failures(&cells, &network, &ids(&["B"]), "A").unwrap();
    assert_eq!(
        report.components,
        vec![ids(&["A"]), ids(&["C"]), ids(&["D"])]
    );
    assert_eq!(report.disconnected_source_ids, ids(&["C"]));
    assert_eq!(report.unresolved_links, 1);
    assert_eq!(report.lost_reference_connected_cells, 2);
    assert_eq!(report.lost_local_cells, 1);
    assert!(report.cells[0].locally_covered);
    assert!(!report.cells[0].reference_connected);
    assert_eq!(
        (
            report.unknown_cells,
            report.unresolved_cells,
            report.outside_area_cells
        ),
        (1, 1, 1)
    );
    assert!(
        !analyze_node_failures(&cells, &network, &ids(&["A"]), "A")
            .unwrap()
            .reference_available
    );
    assert!(analyze_node_failures(&cells, &network, &ids(&["D"]), "A").is_err());
}

#[test]
fn source_order_endpoint_selection_and_height_scenario_are_immutable() {
    let a = router("A", 0.0, 1000.0);
    let b = router("B", 1800.0, 1000.0);
    let endpoint = Site {
        kind: SiteKind::EndpointA,
        ..Site::new("endpoint", 500.0, 1050.0)
    };
    let active = [b.clone(), a.clone(), endpoint.clone()];
    let settings = CoverageSettings {
        source_ids: ids(&["A"]),
        include_endpoints: true,
        maximum_profile_samples: 8,
        profile_step_m: 100.0,
        ..CoverageSettings::default()
    };
    let sources = select_sources(&active, std::slice::from_ref(&endpoint), &settings).unwrap();
    assert_eq!(
        sources
            .iter()
            .map(|site| site.id.as_str())
            .collect::<Vec<_>>(),
        ["A", "endpoint"]
    );
    let overrides = BTreeMap::from([("A".into(), 5.0)]);
    let adjusted = apply_height_overrides(&sources, &overrides).unwrap();
    assert_eq!(sources[0].antenna_height_m, 100.0);
    assert_eq!(adjusted[0].antenna_height_m, 5.0);
    assert!(apply_height_overrides(&sources, &BTreeMap::from([("endpoint".into(), 5.0)])).is_err());
    let network = CoverageNetwork {
        router_ids: ids(&["A", "B"]),
        source_ids: ids(&["A", "endpoint"]),
        network_links: vec![
            edge("A", "B", Some(true)),
            edge("A", "endpoint", Some(true)),
        ],
    };
    let terrain = terrain(0.0, Point::default());
    let rf = RfSettings::default();
    let transform = Transformer::new(CRS).unwrap();
    let evaluator = CoverageEvaluator::new(&terrain, &rf, &transform);
    let report = evaluator
        .revalidate_backbone(&network, &active, &adjusted, &overrides, &settings, &|| {
            false
        })
        .unwrap();
    assert_eq!(report.backbone_revalidated_edges, 1);
    assert_eq!(report.backbone_unresolved_edges, 1);
    assert_eq!(report.network_links[0].valid, None);
    assert_eq!(report.network_links[1], network.network_links[1]);
    assert_eq!(network.network_links[0].valid, Some(true));
}

#[test]
fn source_selection_includes_mesh_clients_alongside_selected_routers() {
    let client = Site {
        kind: SiteKind::Client,
        ..Site::new("CL-school", 500.0, 1050.0)
    };
    let excluded = router("R1", 0.0, 1000.0);
    let selected = router("R2", 1800.0, 1000.0);
    let settings = CoverageSettings {
        source_ids: ids(&["R2"]),
        ..CoverageSettings::default()
    };
    let active = [client, excluded, selected];
    let sources = select_sources(&active, &active, &settings).unwrap();
    assert_eq!(
        sources
            .iter()
            .map(|site| site.id.as_str())
            .collect::<Vec<_>>(),
        ["CL-school", "R2"]
    );
}

#[test]
fn comparison_checks_legacy_manifest_and_rejects_stale_or_incomplete_runs() {
    let manifest = json!({
        "job_id": "b", "project_id": "p", "state": "complete", "terrain_fingerprint": "t",
        "radio_fingerprint": "r", "model_version": PYTHON_MODEL_VERSION, "settings": {},
        "rows": 1, "columns": 2, "effective_cell_size_m": 100.0,
        "area_bounds_wgs84": [60.0, 10.0, 60.01, 10.01],
        "requested_cells": 2, "total": 2, "router_ids": ["A"], "source_ids": ["A"], "network_links": []
    });
    let baseline: ComparisonRun = serde_json::from_value(manifest).unwrap();
    let mut scenario = baseline.clone();
    scenario.job_id = "s".into();
    let before = [
        cell(0, CoverageState::Covered, Some(("A", 7.0))),
        cell(1, CoverageState::Unresolved, None),
    ];
    let mut after = [
        cell(0, CoverageState::Covered, Some(("A", 9.0))),
        cell(1, CoverageState::Covered, Some(("A", 7.0))),
    ];
    let compare =
        |left: &[CoverageCell], right: &[CoverageCell], scenario: &ComparisonRun, terrain: &str| {
            compare_runs(
                &baseline,
                scenario,
                terrain,
                "A",
                left.iter().map(Ok),
                right.iter().map(Ok),
            )
        };
    let report = compare(&before, &after, &scenario, "t").unwrap();
    assert_eq!(report.grid.counts.unresolved, 1);
    assert_eq!(report.grid.counts.retained_reference_connected, 1);
    assert_eq!(report.grid.cells[0].margin_delta_db, Some(2.0));
    assert_eq!(report.approximate_sampled_area_km2, 0.01);
    assert!(compare(&before, &after, &scenario, "changed").is_err());
    assert!(compare(&before, &after[..1], &scenario, "t").is_err());
    scenario.settings.maximum_profile_samples = 1000;
    assert!(compare(&before, &after, &scenario, "t").is_err());
    scenario.settings = baseline.settings.clone();
    scenario.settings.client.tx_power_dbm -= 1.0;
    assert!(compare(&before, &after, &scenario, "t").is_err());
    scenario.settings = baseline.settings.clone();
    after[1].longitude += 1.0;
    assert!(compare(&before, &after, &scenario, "t").is_err());
    after[1] = cell(1, CoverageState::NotEvaluated, None);
    assert!(compare(&before, &after, &scenario, "t").is_err());
    let compare_layer = compare_coverage_cells(
        &before[..1],
        &[cell(0, CoverageState::Uncovered, None)],
        &BTreeSet::from(["A"]),
        &BTreeSet::new(),
    )
    .unwrap();
    assert_eq!(compare_layer.cells[0].local_change, CoverageChange::Lost);
    assert_eq!(compare_layer.counts.baseline_coverage_percent, Some(100.0));
    assert_eq!(compare_layer.counts.scenario_coverage_percent, Some(0.0));
}

#[test]
fn mesh_area_includes_endpoints_and_view_uses_southwest_northeast() {
    let transform = Transformer::new(CRS).unwrap();
    let origin = transform
        .forward(LonLat {
            latitude: 60.0,
            longitude: 15.0,
        })
        .unwrap();
    let source = router("router", origin.x + 1000.0, origin.y);
    let mut a = Site::new("A", origin.x, origin.y);
    let mut b = Site::new("B", origin.x + 2000.0, origin.y);
    a.kind = SiteKind::EndpointA;
    b.kind = SiteKind::EndpointB;
    let endpoints = [a, b];
    let sources = [source];
    let mut settings = CoverageSettings {
        area_buffer_m: 150.0,
        ..CoverageSettings::default()
    };
    let mesh = resolve_area(&settings, &transform, &sources, &endpoints, 50.0, None).unwrap();
    assert!(endpoints.iter().all(|site| mesh.includes(site.position)));
    assert!(mesh.includes(Point {
        x: origin.x + 300.0,
        y: origin.y
    }));
    settings.area_includes_clients = false;
    let repeaters = resolve_area(&settings, &transform, &sources, &endpoints, 50.0, None).unwrap();
    assert!(!repeaters.includes(endpoints[0].position));
    settings.area_mode = AreaMode::View;
    let view = resolve_area(
        &settings,
        &transform,
        &sources,
        &endpoints,
        50.0,
        Some([59.99, 14.99, 60.01, 15.01]),
    )
    .unwrap();
    assert!(view.includes(origin));
    assert!(resolve_area(
        &settings,
        &transform,
        &sources,
        &endpoints,
        50.0,
        Some([60.01, 14.99, 59.99, 15.01])
    )
    .is_err());
}

#[test]
fn exact_point_inspection_and_all_target_geometries_share_rf_and_limits() {
    let transform = Transformer::new(CRS).unwrap();
    let origin = transform
        .forward(LonLat {
            latitude: 60.0,
            longitude: 15.0,
        })
        .unwrap();
    let to_wgs = |x, y| {
        let p = transform
            .reverse(Point {
                x: origin.x + x,
                y: origin.y + y,
            })
            .unwrap();
        [p.longitude, p.latitude]
    };
    let terrain = terrain(0.0, origin);
    let rf = RfSettings::default();
    let evaluator = CoverageEvaluator::new(&terrain, &rf, &transform);
    let sources = [router("A", origin.x + 100.0, origin.y + 100.0)];
    let network = CoverageNetwork {
        router_ids: ids(&["A"]),
        source_ids: ids(&["A"]),
        network_links: vec![],
    };
    let bounds = [
        origin.x + 100.0,
        origin.y + 100.0,
        origin.x + 900.0,
        origin.y + 900.0,
    ];
    let area = CoverageArea::rectangle(bounds).unwrap();
    let mut settings = CoverageSettings {
        cell_size_m: 200.0,
        ..CoverageSettings::default()
    };
    let grid = evaluator
        .calculate(&sources, &settings, &area, RunOptions::default())
        .unwrap();
    let point = to_wgs(400.0, 500.0);
    let features = json!({"type": "FeatureCollection", "features": [
        {"type": "Feature", "id": "house", "geometry": {"type": "Point", "coordinates": point},
         "properties": {"name": " House ", "reference_id": "A"}},
        {"type": "Feature", "id": "road", "geometry": {"type": "LineString",
         "coordinates": [to_wgs(300.0, 500.0), to_wgs(770.0, 500.0)]}},
        {"type": "Feature", "id": "zone", "geometry": {"type": "Polygon",
         "coordinates": [[to_wgs(310.0, 310.0), to_wgs(680.0, 310.0), to_wgs(680.0, 680.0),
                          to_wgs(310.0, 680.0), to_wgs(310.0, 310.0)]]}},
        {"type": "Feature", "id": "outside", "geometry": {"type": "Point", "coordinates": to_wgs(1200.0, 500.0)}}
    ]});
    let targets = normalize_targets(&serde_json::to_vec(&features).unwrap()).unwrap();
    assert_eq!(targets.features[0].properties.name, "House");
    assert_eq!(targets.features[1].properties.name, "Target 2");
    let exact = evaluator
        .evaluate_point(
            LonLat {
                longitude: point[0],
                latitude: point[1],
            },
            &sources,
            &settings,
            ExactPointOptions {
                final_sample_step_m: 100.0,
                include_serving_profile: true,
                cancelled: &|| false,
            },
        )
        .unwrap();
    assert_eq!(exact.coverage.state, PointState::Evaluated);
    assert_eq!(exact.coverage.selected_source_id.as_deref(), Some("A"));
    assert!(exact.profile_link.unwrap().profile.is_some());

    let assess = |settings: &CoverageSettings, grid: &CoverageGrid, workers| {
        CoverageEvaluator::new(&terrain, &rf, &transform)
            .with_parallel_workers(workers)
            .assess_targets(TargetAssessmentInput {
                targets: &targets,
                grid,
                cells: &grid.cells,
                network: &network,
                sources: &sources,
                settings,
                final_sample_step_m: 100.0,
                road_spacing_m: 100.0,
                cancelled: &|| false,
            })
    };
    let first = assess(&settings, &grid, 4).unwrap();
    assert_eq!(
        serde_json::to_vec(&first).unwrap(),
        serde_json::to_vec(&assess(&settings, &grid, 1).unwrap()).unwrap()
    );
    assert_eq!(
        first
            .targets
            .iter()
            .map(|target| target.state)
            .collect::<Vec<_>>(),
        [
            TargetState::Pass,
            TargetState::Pass,
            TargetState::Pass,
            TargetState::OutsideAnalysedArea
        ]
    );
    assert_eq!(
        first.assumptions.reference_connected_source_ids["A"],
        ids(&["A"])
    );
    let TargetStats::Road(road) = &first.targets[1].stats else {
        panic!("expected road stats")
    };
    assert!((road.covered_length_m - 470.0).abs() < 1e-5);
    assert_eq!(road.sample_count, 6);
    let TargetStats::Polygon(zone) = &first.targets[2].stats else {
        panic!("expected polygon stats")
    };
    assert!((zone.covered_area_m2 - 370.0 * 370.0).abs() < 0.01);
    assert_eq!(zone.sampled_cells, 4);
    let first_json = serde_json::to_value(&first).unwrap();
    assert_eq!(first_json["targets"][0]["passing_sources"], json!(["A"]));
    assert!(first_json["targets"][3].get("passing_sources").is_none());
    let round_trip: TargetAssessment = serde_json::from_value(first_json.clone()).unwrap();
    assert_eq!(serde_json::to_value(&round_trip).unwrap(), first_json);

    settings.profile_step_m = 100.0;
    settings.maximum_profile_samples = 3;
    let capped_grid = evaluator
        .calculate(&sources, &settings, &area, RunOptions::default())
        .unwrap();
    let second = assess(&settings, &capped_grid, 4).unwrap();
    for target in &second.targets[..3] {
        assert_eq!(target.state, TargetState::Unresolved);
    }
    let baseline = SavedTargetReport {
        report_id: "baseline".into(),
        coverage_job_id: "first".into(),
        project_id: "project".into(),
        target_fingerprint: "targets".into(),
        terrain_fingerprint: "terrain".into(),
        radio_fingerprint: "rf".into(),
        model_version: PYTHON_MODEL_VERSION.into(),
        grid_compatibility: TargetGridCompatibility {
            rows: grid.rows,
            columns: grid.columns,
            effective_cell_size_m: grid.effective_cell_size_m,
            area_bounds_wgs84: area_bounds_wgs84(bounds, &transform).unwrap(),
        },
        client_profile: settings.client.clone(),
        profile_step_m: 100.0,
        maximum_profile_samples: None,
        assessment: first,
    };
    let mut scenario = SavedTargetReport {
        report_id: "scenario".into(),
        assessment: second,
        ..baseline.clone()
    };
    let comparison = compare_target_reports(&baseline, &scenario).unwrap();
    assert_eq!(comparison.counts.uncertain, 4);
    scenario.assessment.assumptions.road_sample_spacing_m = 200.0;
    assert!(compare_target_reports(&baseline, &scenario).is_err());
}

#[test]
fn targets_reject_open_rings_and_unbounded_work() {
    let invalid = br#"{"type":"FeatureCollection","features":[{"type":"Feature","geometry":
        {"type":"Polygon","coordinates":[[[0,0],[1,0],[1,1],[0,1]]]}}]}"#;
    assert!(normalize_targets(invalid).is_err());
    assert!(line_sample_positions(2_000_000.0, 25.0).is_err());
    let samples = line_sample_positions(250.0, 100.0).unwrap();
    assert_eq!(
        samples,
        [(0.0, 50.0), (100.0, 100.0), (200.0, 75.0), (250.0, 25.0)]
    );
}
