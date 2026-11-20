use repeaternet_exports::{
    coverage::{
        build_route_overlay, write_coverage, CoverageAttachments, CoverageFormat, CoverageGrid,
        MetadataOptions,
    },
    route::{
        write_route_csv, write_route_geojson, write_route_html, CoverageOmission,
        RouteReportOptions, MAX_PROFILE_POINTS,
    },
    spool, ExportError, MAX_COVERAGE_EXPORT_BYTES,
};
use serde_json::{json, Value};
use std::io::{self, Read, Write};

fn identity(point: [f64; 2]) -> Result<[f64; 2], ExportError> {
    Ok(point)
}

fn assert_json(actual: &Value, expected: &Value) {
    match (actual, expected) {
        (Value::Number(a), Value::Number(b)) => {
            let (a, b) = (a.as_f64().unwrap(), b.as_f64().unwrap());
            assert!((a - b).abs() <= 1e-10 * b.abs().max(1.0), "{a} != {b}");
        }
        (Value::Array(a), Value::Array(b)) => {
            assert_eq!(a.len(), b.len());
            for (a, b) in a.iter().zip(b) {
                assert_json(a, b);
            }
        }
        (Value::Object(a), Value::Object(b)) => {
            assert_eq!(a.keys().collect::<Vec<_>>(), b.keys().collect::<Vec<_>>());
            for (key, value) in a {
                assert_json(value, &b[key]);
            }
        }
        _ => assert_eq!(actual, expected),
    }
}

#[test]
fn python_coverage_oracle_and_html() {
    let fixture: Value = serde_json::from_str(include_str!("fixtures/coverage.json")).unwrap();
    let job = &fixture["job"];
    // Capture the oracle's grid transform samples so serializer parity is
    // independent of which projection library supplies the callbacks.
    let reverse = |point| {
        let origin = [500_000.0, 6_650_000.0];
        let [x, y]: [f64; 2] = point;
        for cell in fixture["json"]["cells"].as_array().unwrap() {
            if cell["x"] == x && cell["y"] == y {
                return Ok([
                    cell["longitude"].as_f64().unwrap(),
                    cell["latitude"].as_f64().unwrap(),
                ]);
            }
        }
        let row = ((origin[1] + 100.0 - y) / 50.0).floor().clamp(0.0, 1.0) as usize;
        let col = ((x - origin[0]) / 50.0).floor().clamp(0.0, 1.0) as usize;
        let index = row * 2 + col;
        let ring = &fixture["geojson"]["features"][index]["geometry"]["coordinates"][0];
        let east = x > origin[0] + col as f64 * 50.0;
        let north = y > origin[1] + (1 - row) as f64 * 50.0;
        let corner = match (east, north) {
            (false, false) => 0,
            (true, false) => 1,
            (true, true) => 2,
            (false, true) => 3,
        };
        Ok(serde_json::from_value(ring[corner].clone()).unwrap())
    };
    let grid = CoverageGrid::new(job, &reverse, &identity).unwrap();
    let cells = || fixture["cells"].as_array().unwrap().iter().cloned().map(Ok);
    let targets = Some(&fixture["targets"]);
    let counts = grid.summarize(cells()).unwrap();
    let metadata = grid.metadata(
        &counts,
        MetadataOptions {
            stale: true,
            target_report: targets,
            exported_at: None,
        },
    );
    assert_eq!(counts.state_count("unknown_terrain"), Some(1));
    assert_eq!(counts.state_count("not_evaluated"), Some(2));
    let mut output = Vec::new();
    let mut attachments = CoverageAttachments {
        target_report: targets,
        ..Default::default()
    };
    for (format, key) in [
        (CoverageFormat::Json, "json"),
        (CoverageFormat::GeoJson, "geojson"),
    ] {
        output.clear();
        write_coverage(&mut output, format, &grid, cells(), &metadata, &attachments).unwrap();
        assert_json(&serde_json::from_slice(&output).unwrap(), &fixture[key]);
    }
    let comparison = json!({"type":"height", "baseline_job_id":"<baseline>", "scenario_job_id":"<trial>", "counts":{"<transition>":2}});
    let scenario = json!({"type":"failure", "reference_id":"<scenario>", "failed_ids":["<router>"], "lost_local_cells":3});
    attachments.scenario_comparison = Some(&comparison);
    attachments.failure_scenario = Some(&scenario);
    output.clear();
    write_coverage(
        &mut output,
        CoverageFormat::Html,
        &grid,
        cells(),
        &metadata,
        &attachments,
    )
    .unwrap();
    let html = String::from_utf8(output).unwrap();
    assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
    assert!(html.contains("Not evaluated"));
    assert!(html.contains("Unknown terrain"));
    assert!(html.contains("STALE"));
    assert!(html.contains("&lt;trial&gt;"));
    assert!(html.contains("&lt;transition&gt;"));
    assert!(html.contains("&lt;scenario&gt;"));
    assert!(!html.contains("<script"));
    assert!(build_route_overlay(&grid, cells(), &metadata, targets).is_err());
}

#[test]
fn python_route_oracle() {
    let fixture: Value = serde_json::from_str(include_str!("fixtures/route.json")).unwrap();
    let route = &fixture["route"];
    let mut output = Vec::new();
    write_route_csv(&mut output, route).unwrap();
    assert_eq!(output, fixture["csv"].as_str().unwrap().as_bytes());
    output.clear();
    write_route_geojson(&mut output, route, None).unwrap();
    assert_json(
        &serde_json::from_slice(&output).unwrap(),
        &fixture["geojson"],
    );
    output.clear();
    write_route_html(
        &mut output,
        route,
        RouteReportOptions {
            generated_at: fixture["stamp"].as_str().unwrap(),
            candidate_settings: Some(&fixture["candidates"]),
            coverage_omission: Some(CoverageOmission::Stale),
            ..Default::default()
        },
    )
    .unwrap();
    let html = String::from_utf8(output).unwrap();
    let data = html
        .split_once("id=\"route-map-data\" type=\"application/json\">")
        .unwrap()
        .1
        .split_once("</script>")
        .unwrap()
        .0;
    assert_json(&serde_json::from_str(data).unwrap(), &fixture["map"]);
    assert!(data.contains("\\u003c/script>"));
    assert!(html.contains("One independent path passed validation"));
    assert!(html.contains("Primary route first (3 hops)"));
    assert!(html.contains("1. A → R-1"));
    assert!(html.contains("different route, terrain or settings"));
    assert!(!html.contains("<img src=x"));
}

fn small_job() -> Value {
    json!({"state":"complete","job_id":"grid","terrain_crs":"EPSG:4326",
        "rows":1,"columns":2,"total":2,"grid_bounds_projected":[10,60,12,61],
        "area_bounds_wgs84":[60,10,61,12],"effective_cell_size_m":1,"settings":{"mode":"two_way"}})
}

#[test]
fn empty_partial_and_invalid_grids() {
    let empty = json!({});
    let mut output = Vec::new();
    write_route_geojson(&mut output, &empty, None).unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&output).unwrap()["features"],
        json!([])
    );
    output.clear();
    write_route_html(&mut output, &empty, Default::default()).unwrap();
    assert!(String::from_utf8(output)
        .unwrap()
        .contains("No site coordinates available"));
    let mut job = small_job();
    let grid = CoverageGrid::new(&job, &identity, &identity).unwrap();
    let counts = grid.summarize([]).unwrap();
    assert_eq!(counts.state_count("not_evaluated"), Some(2));
    assert!(grid.metadata(&counts, Default::default())["partial"] == true);
    let duplicate = || Ok(json!({"index":0,"state":"covered"}));
    assert!(grid.summarize([duplicate(), duplicate()]).is_err());
    job["total"] = json!(0);
    assert!(CoverageGrid::new(&job, &identity, &identity).is_err());
}

#[test]
fn overlay_and_nonfinite_json() {
    let job = small_job();
    let grid = CoverageGrid::new(&job, &identity, &identity).unwrap();
    let cells = || {
        (0..2).map(|index| {
            Ok(json!({
                "index":index,"state":"unknown_terrain","x":10.5 + f64::from(index),"y":60.5,
                "best_margin_db":f64::NAN,
            }))
        })
    };
    let metadata = grid.metadata(
        &grid.summarize(cells()).unwrap(),
        MetadataOptions {
            exported_at: Some(f64::INFINITY),
            ..Default::default()
        },
    );
    let overlay = build_route_overlay(&grid, cells(), &metadata, None).unwrap();
    assert!(overlay
        .image()
        .starts_with("data:image/png;base64,iVBORw0KGgo"));
    assert!((overlay.bounds()[0][0] - 60.0).abs() < 1e-10);
    let route = json!({"route":[{"id":"A","latitude":60,"longitude":10}]});
    let mut output = Vec::new();
    write_route_html(
        &mut output,
        &route,
        RouteReportOptions {
            coverage: Some(&overlay),
            ..Default::default()
        },
    )
    .unwrap();
    let html = String::from_utf8(output).unwrap();
    assert!(html.contains("Predicted coverage"));
    assert!(html.contains(overlay.image()));
    let mut file = spool(|file| {
        write_coverage(
            file,
            CoverageFormat::Json,
            &grid,
            cells(),
            &metadata,
            &Default::default(),
        )
    })
    .unwrap();
    let mut output = String::new();
    file.read_to_string(&mut output).unwrap();
    let document: Value = serde_json::from_str(&output).unwrap();
    assert!(document["cells"][0]["best_margin_db"].is_null());
    assert!(document["metadata"]["exported_at"].is_null());
}

#[test]
fn bounded_profile_and_streaming_output() {
    let distances: Vec<_> = (0..2001).collect();
    let heights = vec![10; distances.len()];
    let mut terrain = heights.clone();
    terrain[1234] = 95;
    let route = json!({"route":[{"id":"A"},{"id":"B"}],"links":[{
        "source_id":"B","target_id":"A","distance_m":2000,"minimum_clearance_distance_m":766,
        "profile":{"distances_m":distances,"dtm_elevation_m":terrain,"earth_bulge_m":heights,
        "effective_obstruction_m":terrain,"los_elevation_m":heights,"fresnel_radius_m":heights,
        "clearance_m":heights,"surface_available":true}
    }]});
    let mut output = Vec::new();
    write_route_html(&mut output, &route, Default::default()).unwrap();
    let html = String::from_utf8(output).unwrap();
    let samples: usize = html
        .split_once("data-profile-points=\"")
        .unwrap()
        .1
        .split_once('"')
        .unwrap()
        .0
        .parse()
        .unwrap();
    assert!((990..=MAX_PROFILE_POINTS).contains(&samples));
    assert!(html.contains("Minimum Fresnel clearance at 1.234 km"));
    let job = json!({"job_id":"large","terrain_crs":"EPSG:4326","rows":128,"columns":128,
        "requested_cells":16_384,"grid_bounds_projected":[0,0,128,128],"effective_cell_size_m":1});
    let grid = CoverageGrid::new(&job, &identity, &identity).unwrap();
    let padding = "x".repeat(256 * 1024);
    let cells =
        (0..16_384).map(|index| Ok(json!({"index":index,"state":"unresolved","padding":padding})));
    let mut bytes = 0;
    struct Count<'a>(&'a mut u64);
    impl Write for Count<'_> {
        fn write(&mut self, data: &[u8]) -> io::Result<usize> {
            *self.0 += data.len() as u64;
            Ok(data.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let error = write_coverage(
        Count(&mut bytes),
        CoverageFormat::Json,
        &grid,
        cells,
        &Value::Null,
        &Default::default(),
    )
    .unwrap_err();
    assert!(matches!(error, ExportError::LimitExceeded));
    assert!((MAX_COVERAGE_EXPORT_BYTES - 256 * 1024..=MAX_COVERAGE_EXPORT_BYTES).contains(&bytes));
}

#[test]
fn drawn_grid_reconstructs_outside_area_and_includes_boundary() {
    let mut job = small_job();
    job["settings"]["area_mode"] = json!("drawn");
    job["settings"]["area_polygon_wgs84"] = json!([
        [10.0, 60.0],
        [10.5, 60.0],
        [10.5, 61.0],
        [10.0, 61.0],
        [10.0, 60.0]
    ]);
    let grid = CoverageGrid::new(&job, &identity, &identity).unwrap();
    let counts = grid.summarize([]).unwrap();
    assert_eq!(counts.state_count("not_evaluated"), Some(1));
    assert_eq!(counts.state_count("outside_area"), Some(1));

    let cells = || {
        [Ok(json!({
            "index": 0, "state": "uncovered", "longitude": 10.5, "latitude": 60.5,
            "x": 10.5, "y": 60.5,
        }))]
    };
    let metadata = grid.metadata(&grid.summarize(cells()).unwrap(), Default::default());
    assert_eq!(metadata["complete"], true);
    assert!(build_route_overlay(&grid, cells(), &metadata, None).is_ok());
}
