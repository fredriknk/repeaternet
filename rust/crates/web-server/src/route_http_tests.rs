use std::{
    io::Write,
    path::Path,
    sync::{Arc, mpsc},
    time::Duration,
};

use axum::{body::to_bytes, extract::State};
use repeaternet_terrain::{LonLat, Point, TerrainCrs, Transformer};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::*;
use crate::route_http::{Scheduler, SnapshotGate};

const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const C: &str = "cccccccccccccccccccccccccccccccccccccccccccccccc";
const TILE: &[u8] = include_bytes!("../../terrain/tests/fixtures/fallback.tif");
const ROTATED: &[u8] = include_bytes!("../../terrain/tests/fixtures/rotated.tif");
const BOUNDARY: &str = "rr-terrain-test";

fn plan() -> Value {
    let coord = |x, y| {
        let transform = Transformer::new(TerrainCrs::from_epsg(25833).unwrap()).unwrap();
        let LonLat {
            latitude,
            longitude,
        } = transform.reverse(Point { x, y }).unwrap();
        json!([latitude, longitude])
    };
    let manual = coord(500170.0, 6650850.0);
    json!({
        "format": "repeaternet-web", "version": 1,
        "a": coord(500050.0, 6650850.0), "b": coord(500290.0, 6650850.0),
        "rf": {"lora": {"enabled": true, "manual_sensitivity_override": false}},
        "candidates": {"candidates_per_cell": 1, "refine_radius_m": 0, "maximum_new_routers": 2, "maximum_hops": 3},
        "manual_routers": [{"id": "M-alternative", "name": "Manual site", "latitude": manual[0], "longitude": manual[1]}],
        "search_effort": "quick", "unknown_future_field": {"keep": true},
    })
}

fn app(root: &Path, token: &str) -> (Router, AppState) {
    let store = ProjectStore::new(root.join("projects.sqlite3"), root).unwrap();
    let mut app = AppState::new(store, root);
    app.scheduler = Arc::new(Scheduler::new(1));
    (router_with_state(app.clone(), token, true), app)
}

async fn request(
    app: &Router,
    cookie: Option<&str>,
    method: Method,
    path: &str,
    content_type: &str,
    body: Body,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut req = Request::builder()
        .method(method)
        .uri(path)
        .header("host", "127.0.0.1:8000")
        .header("content-type", content_type);
    if let Some(cookie) = cookie {
        req = req.header("cookie", cookie);
    }
    let response = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let (parts, body) = response.into_parts();
    (
        parts.status,
        parts.headers,
        to_bytes(body, 16 * 1024 * 1024).await.unwrap().to_vec(),
    )
}

async fn api(
    app: &Router,
    cookie: &str,
    method: Method,
    path: &str,
    data: Value,
    status: StatusCode,
) -> Value {
    let (actual, _, body) = request(
        app,
        Some(cookie),
        method,
        path,
        "application/json",
        Body::from(data.to_string()),
    )
    .await;
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(actual, status, "{path}: {body}");
    body
}

fn multipart(tile: &[u8], filename: &str) -> Vec<u8> {
    let mut body = format!("--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\nContent-Type: image/tiff\r\n\r\n").into_bytes();
    body.extend(tile);
    write!(body, "\r\n--{BOUNDARY}--\r\n").unwrap();
    body
}

async fn upload(app: &Router, cookie: &str, kind: &str, tile: &[u8], status: StatusCode) -> Value {
    let (actual, _, body) = request(
        app,
        Some(cookie),
        Method::POST,
        &format!("/api/terrain/{kind}"),
        &format!("multipart/form-data; boundary={BOUNDARY}"),
        Body::from(multipart(tile, "../../user-supplied.tif")),
    )
    .await;
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(actual, status, "{body}");
    body
}

async fn state(app: &Router, cookie: &str) -> Value {
    api(
        app,
        cookie,
        Method::GET,
        "/api/state",
        Value::Null,
        StatusCode::OK,
    )
    .await
}

async fn terminal(app: &Router, cookie: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let state = state(app, cookie).await;
            if !matches!(
                state["job"]["state"].as_str(),
                Some("queued" | "running" | "preparing")
            ) {
                return state;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap()
}

fn cookie(key: &str) -> String {
    format!("planner_workspace={key}")
}

fn set_cookie(headers: &HeaderMap) -> &str {
    headers[axum::http::header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
}

fn link_request() -> Value {
    let plan = plan();
    let site = |id, kind, point: &Value| {
        json!({
            "id": id, "kind": kind, "latitude": point[0], "longitude": point[1],
            "antenna_height_m": 20,
        })
    };
    json!({
        "source": site("A", "endpoint_a", &plan["a"]),
        "target": site("B", "endpoint_b", &plan["b"]),
    })
}

#[tokio::test]
async fn state_serves_calibrated_rf_defaults_and_metadata() {
    use repeaternet_rf_core::calibration as c;
    let root = tempfile::tempdir().unwrap();
    let (app, _) = app(root.path(), "");
    let defaults = state(&app, &cookie(A)).await;
    assert_eq!(
        defaults["rf"]["diffraction_model"],
        json!(c::DEFAULT_DIFFRACTION_MODEL)
    );
    assert_eq!(
        defaults["rf"]["obstacle_surface"],
        json!(c::DEFAULT_OBSTACLE_SURFACE)
    );
    assert_eq!(
        defaults["rf"]["model_correction_db"],
        json!(c::MODEL_CORRECTION_DB)
    );
    assert_eq!(defaults["rf"]["k_factor"], json!(c::K_FACTOR));
    for role in ["endpoint_a", "endpoint_b", "router"] {
        assert_eq!(
            defaults["rf"][role]["terminal_clutter"],
            json!(c::DEFAULT_TERMINAL_CLUTTER)
        );
    }
    assert_eq!(
        defaults["candidates"]["final_sample_step_m"],
        json!(c::CERTIFICATION_MAX_STEP_M)
    );
    assert_eq!(
        defaults["calibration"],
        json!({
            "model_correction_db": c::MODEL_CORRECTION_DB,
            "new_site_sigma_db": c::NEW_SITE_SIGMA_DB,
            "time_variability_sigma_db": c::TIME_VARIABILITY_SIGMA_DB,
            "certification_max_step_m": c::CERTIFICATION_MAX_STEP_M,
            "coarse_step_allowance_db": c::COARSE_STEP_ALLOWANCE_DB,
            "terminal_clutter_radius_m": c::TERMINAL_CLUTTER_RADIUS_M,
            "source": "docs/RF_CALIBRATION.md, 2026-11-22",
        })
    );
}

#[test]
fn calibrated_plan_fields_change_route_and_saved_radio_identities() {
    use crate::{coverage_runs, route_http, route_inputs};
    use repeaternet_rf_core::calibration;

    let root = tempfile::tempdir().unwrap();
    let (_, app) = app(root.path(), "");
    let mut plan = plan();
    let mut known = plan["manual_routers"][0].clone();
    known["id"] = json!("K-site");
    plan["known_routers"] = json!([known]);
    // A current-format plan; legacy plans without it drop a stored Bullington.
    plan["rf"]["obstacle_surface"] = json!("ground");
    let identities = |plan: &Value| {
        let inputs = route_inputs::parse(plan.clone(), &app.defaults).unwrap();
        let route = json!({"rf_settings": inputs.rf_payload});
        let radio = coverage_runs::radio(&app, plan, &route).unwrap();
        (
            route_http::cache_version(&json!([]), &route["rf_settings"], plan).unwrap(),
            repeaternet_coverage_store::fingerprint(&radio),
        )
    };
    let baseline = identities(&plan);
    for (path, value) in [
        ("/rf/diffraction_model", json!("bullington")),
        ("/rf/diffraction_model", json!("deygout3")),
        ("/rf/obstacle_surface", json!("surface")),
        (
            "/rf/model_correction_db",
            json!(calibration::MODEL_CORRECTION_DB + 1.0),
        ),
        ("/rf/endpoint_a/terminal_clutter", json!("p2108_dom")),
        ("/rf/endpoint_b/terminal_clutter", json!("p2108_dom")),
        ("/rf/router/terminal_clutter", json!("p2108_dom")),
        ("/manual_routers/0/terminal_clutter", json!("p2108_dom")),
        ("/known_routers/0/terminal_clutter", json!("none")),
    ] {
        let mut changed = plan.clone();
        match path.split('/').collect::<Vec<_>>().as_slice() {
            ["", "rf", key] => changed["rf"][key] = value,
            ["", "rf", role, key] => changed["rf"][role][key] = value,
            ["", list, "0", key] => changed[list][0][key] = value,
            _ => unreachable!(),
        }
        let (cache, radio) = identities(&changed);
        assert_ne!(cache, baseline.0, "{path}");
        assert_ne!(radio, baseline.1, "{path}");
    }

    plan["rf"]["router"]["terminal_clutter"] = json!("p2108_dom");
    plan["known_routers"][0]["terminal_clutter"] = json!("none");
    let inputs = route_inputs::parse(plan.clone(), &app.defaults).unwrap();
    let modes: std::collections::BTreeMap<_, _> = inputs
        .locations
        .iter()
        .map(|site| (site.id.as_str(), json!(site.terminal_clutter)))
        .collect();
    assert_eq!(modes["M-alternative"], "p2108_dom");
    assert_eq!(modes["K-site"], "none");

    for (key, invalid) in [
        ("diffraction_model", json!("free_space")),
        ("obstacle_surface", json!("water")),
        ("model_correction_db", json!(-1)),
        ("model_correction_db", json!(21)),
        ("model_correction_db", json!("3")),
        ("endpoint_a", json!({"terminal_clutter": "unknown"})),
    ] {
        let mut changed = plan.clone();
        changed["rf"][key] = invalid;
        assert!(
            route_inputs::parse(changed, &app.defaults).is_err(),
            "{key}"
        );
    }
    for list in ["manual_routers", "known_routers"] {
        let mut changed = plan.clone();
        changed[list][0]["terminal_clutter"] = Value::Null;
        changed[list][0]["policy"] = json!("excluded");
        assert!(
            route_inputs::parse(changed, &app.defaults).is_err(),
            "{list}"
        );
    }
    plan["candidates"]["final_sample_step_m"] = json!(calibration::CERTIFICATION_MAX_STEP_M + 1.0);
    let error = route_inputs::parse(plan, &app.defaults).err().unwrap();
    assert!(error.detail().contains("final_sample_step_m"));
    assert!(
        error
            .detail()
            .contains(&calibration::CERTIFICATION_MAX_STEP_M.to_string())
    );
}

#[tokio::test]
async fn link_evaluation_uses_route_terrain_and_current_radio_settings() {
    let root = tempfile::tempdir().unwrap();
    let (app, _) = app(root.path(), "");
    let cookie = cookie(A);
    upload(&app, &cookie, "dtm", TILE, StatusCode::OK).await;
    let path = "/api/link/evaluate";
    let baseline = api(
        &app,
        &cookie,
        Method::POST,
        path,
        link_request(),
        StatusCode::OK,
    )
    .await;
    assert_eq!(baseline["source_id"], "A");
    assert_eq!(baseline["target_id"], "B");
    assert!((baseline["distance_m"].as_f64().unwrap() - 240.0).abs() < 0.01);
    assert!(baseline["profile"].is_null());
    assert!(baseline["los_clear"].is_boolean());
    assert!(baseline["minimum_fresnel_clearance_ratio"].is_number());
    assert_eq!(
        baseline["diffraction_model"],
        json!(repeaternet_rf_core::calibration::DEFAULT_DIFFRACTION_MODEL)
    );
    for direction in ["forward", "reverse"] {
        let budget = &baseline[direction];
        assert_eq!(
            budget["model_correction_db"],
            json!(repeaternet_rf_core::calibration::MODEL_CORRECTION_DB)
        );
        assert_eq!(budget["terminal_clutter_db"], 0.0);
        assert_eq!(budget["step_allowance_db"], 0.0);
    }

    let mut plan = plan();
    let power = baseline["forward"]["tx_power_dbm"].as_f64().unwrap();
    plan["rf"] = json!({"tx_power_dbm": power + 3.0, "required_fresnel_clearance": 0.7});
    api(
        &app,
        &cookie,
        Method::POST,
        "/api/projects/autosave",
        json!({"plan": plan}),
        StatusCode::OK,
    )
    .await;
    let mut body = link_request();
    body["include_profile"] = json!(true);
    let link = api(&app, &cookie, Method::POST, path, body, StatusCode::OK).await;
    assert_eq!(link["required_fresnel_clearance"], 0.7);
    assert_eq!(link["forward"]["tx_power_dbm"], power + 3.0);
    let margin_change =
        link["worst_margin_db"].as_f64().unwrap() - baseline["worst_margin_db"].as_f64().unwrap();
    assert!((margin_change - 3.0).abs() < 0.001);
    let distances = link["profile"]["distances_m"].as_array().unwrap();
    assert!(distances.len() >= 3);
    assert_eq!(distances.last(), Some(&link["distance_m"]));
    assert_eq!(
        link["profile"]["fresnel_radius_m"]
            .as_array()
            .unwrap()
            .len(),
        distances.len()
    );
}

#[tokio::test]
async fn link_profile_uses_the_selected_surface_and_router_clutter_override() {
    let root = tempfile::tempdir().unwrap();
    let (app, _) = app(root.path(), "");
    let cookie = cookie(A);
    let ground = live_fixture(100, 8.0, |_, _| 0.0);
    let surface = live_fixture(100, 8.0, |_, _| 40.0);
    upload(&app, &cookie, "dtm", &ground, StatusCode::OK).await;
    upload(&app, &cookie, "dom", &surface, StatusCode::OK).await;

    let mut plan = plan();
    let a = geographic(499_900.0, 6_650_000.0);
    let b = geographic(500_100.0, 6_650_000.0);
    plan["manual_routers"][0]["terminal_clutter"] = json!("p2108_dom");
    let request = json!({
        "source": {"id": "M-alternative", "kind": "router", "latitude": a.latitude,
                   "longitude": a.longitude, "antenna_height_m": 3.0},
        "target": {"id": "B", "kind": "endpoint_b", "latitude": b.latitude,
                   "longitude": b.longitude, "antenna_height_m": 3.0},
        "include_profile": true,
    });
    for (obstacle_surface, override_mode) in [("ground", "p2108_dom"), ("surface", "none")] {
        plan["rf"]["obstacle_surface"] = json!(obstacle_surface);
        plan["rf"]["diffraction_model"] = json!("bullington");
        plan["manual_routers"][0]["terminal_clutter"] = json!(override_mode);
        api(
            &app,
            &cookie,
            Method::POST,
            "/api/projects/autosave",
            json!({"plan": plan}),
            StatusCode::OK,
        )
        .await;
        let link = api(
            &app,
            &cookie,
            Method::POST,
            "/api/link/evaluate",
            request.clone(),
            StatusCode::OK,
        )
        .await;
        assert_eq!(link["diffraction_model"], "bullington");
        assert_eq!(link["los_clear"], obstacle_surface == "ground");
        assert_eq!(link["profile"]["surface_elevation_m"][1], 40.0);
        assert_eq!(
            link["forward"]["terminal_clutter_db"],
            link["reverse"]["terminal_clutter_db"]
        );
        let clutter = link["forward"]["terminal_clutter_db"].as_f64().unwrap();
        assert_eq!(clutter > 0.0, override_mode == "p2108_dom");
    }
}

#[tokio::test]
async fn link_evaluation_reports_invalid_sites_and_missing_ground() {
    let root = tempfile::tempdir().unwrap();
    let (app, _) = app(root.path(), "");
    let cookie = cookie(A);
    let path = "/api/link/evaluate";
    let invalid = StatusCode::UNPROCESSABLE_ENTITY;
    let missing = api(&app, &cookie, Method::POST, path, link_request(), invalid).await;
    assert_eq!(missing["detail"], "Prepare or upload ground terrain first");
    upload(&app, &cookie, "dtm", TILE, StatusCode::OK).await;
    let mut body = link_request();
    body["source"]["latitude"] = json!(61.0);
    let outside = api(&app, &cookie, Method::POST, path, body.clone(), invalid).await;
    assert_eq!(
        outside["detail"],
        "Ground terrain is missing at a link site"
    );
    body["source"]["latitude"] = json!("");
    let bad = api(&app, &cookie, Method::POST, path, body, invalid).await;
    assert_eq!(bad["detail"], "Invalid source coordinates");
    let bad = api(&app, &cookie, Method::POST, path, json!({}), invalid).await;
    assert_eq!(bad["detail"], "Invalid source coordinates");
}

async fn coverage_terminal(app: &Router, cookie: &str, path: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let job = api(app, cookie, Method::GET, path, Value::Null, StatusCode::OK).await;
            if !matches!(job["state"].as_str(), Some("queued" | "running")) {
                return job;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn saved_coverage_queue_scenarios_targets_and_staleness() {
    let root = tempfile::tempdir().unwrap();
    let (app, state_handle) = app(root.path(), "");
    let cookie = cookie(A);
    upload(&app, &cookie, "dtm", TILE, StatusCode::OK).await;
    let mut plan = plan();
    plan["manual_routers"][0]["policy"] = json!("required");
    let point = geographic(500135.0, 6650840.0);
    let settings = json!({"area_buffer_m": 60.0, "area_includes_clients": false, "cell_size_m": 35.0, "maximum_cells": 80});
    plan["coverage"] = settings.clone();
    api(
        &app,
        &cookie,
        Method::POST,
        "/api/optimize",
        plan.clone(),
        StatusCode::OK,
    )
    .await;
    assert_eq!(terminal(&app, &cookie).await["job"]["state"], "complete");
    let route = api(
        &app,
        &cookie,
        Method::GET,
        "/api/result",
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(route["router_count"], 1, "{route}");
    let request = json!({"snapshot_version": route["snapshot_version"], "alternative_id": route["active_alternative_id"],
        "settings": settings});
    let send = |method, path: &str, data, status| {
        let path = path.to_owned();
        let app = app.clone();
        let cookie = cookie.clone();
        async move { api(&app, &cookie, method, &path, data, status).await }
    };
    let targets = json!({"type": "FeatureCollection", "features": [
        {"type": "Feature", "id": "T-test", "geometry": {"type": "Point", "coordinates": [point.longitude, point.latitude]},
         "properties": {"name": "Calibration point", "reference_id": "M-alternative"}}
    ]});
    send(
        Method::PUT,
        "/api/coverage/targets",
        targets,
        StatusCode::OK,
    )
    .await;

    let route_cache = state(&app, &cookie).await["rf_cache"].clone();
    let cold = send(
        Method::POST,
        "/api/coverage/preview",
        request.clone(),
        StatusCode::OK,
    )
    .await;
    let cold_cache = state_handle.coverage_cache.stats();
    assert!(cold_cache.entries > 0, "{cold}");
    let warm = send(
        Method::POST,
        "/api/coverage/preview",
        request.clone(),
        StatusCode::OK,
    )
    .await;
    let warm_cache = state_handle.coverage_cache.stats();
    assert_eq!(cold["cells"], warm["cells"]);
    assert!(warm_cache.hits > cold_cache.hits);
    assert_eq!(warm_cache.misses, cold_cache.misses);
    assert_eq!(state(&app, &cookie).await["rf_cache"], route_cache);

    let worker = state_handle.scheduler.reserve_worker().unwrap();
    let queued = send(
        Method::POST,
        "/api/coverage/jobs",
        request.clone(),
        StatusCode::OK,
    )
    .await;
    let queued_path = format!("/api/coverage/jobs/{}", queued["job_id"].as_str().unwrap());
    assert_eq!(queued["state"], "queued");
    send(
        Method::POST,
        &format!("{queued_path}/cancel"),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(
        coverage_terminal(&app, &cookie, &queued_path).await["state"],
        "cancelled"
    );
    send(
        Method::GET,
        &format!("{queued_path}/export.json"),
        Value::Null,
        StatusCode::CONFLICT,
    )
    .await;
    let partial = send(
        Method::GET,
        &format!("{queued_path}/export.json?include_partial=true"),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(partial["metadata"]["partial"], true);
    drop(worker);

    let baseline = send(
        Method::POST,
        "/api/coverage/jobs",
        request.clone(),
        StatusCode::OK,
    )
    .await;
    let baseline_id = baseline["job_id"].as_str().unwrap();
    let baseline_path = format!("/api/coverage/jobs/{baseline_id}");
    let complete = coverage_terminal(&app, &cookie, &baseline_path).await;
    assert_eq!(complete["state"], "complete", "{complete}");
    assert_eq!(complete["source_ids"], json!(["M-alternative"]));
    assert!(state_handle.coverage_cache.stats().hits > warm_cache.hits);
    let scenario = send(
        Method::POST,
        &format!("{baseline_path}/scenario"),
        json!({"reference_id":"M-alternative","failed_ids":["M-alternative"]}),
        StatusCode::OK,
    )
    .await;
    assert_eq!(scenario["reference_available"], false);
    assert_eq!(scenario["baseline_reference_available"], true);
    assert_eq!(
        scenario["cells"].as_array().unwrap().len() as u64,
        complete["requested_cells"].as_u64().unwrap()
    );
    let old_report = send(
        Method::POST,
        &format!("{baseline_path}/targets"),
        json!({}),
        StatusCode::OK,
    )
    .await;
    assert_eq!(old_report["targets"][0]["id"], "T-test", "{old_report}");

    let mut changed = request.clone();
    changed["source_height_overrides"] = json!({"M-alternative": 19.0});
    let adjusted = send(Method::POST, "/api/coverage/jobs", changed, StatusCode::OK).await;
    let adjusted_id = adjusted["job_id"].as_str().unwrap();
    assert_ne!(baseline_id, adjusted_id);
    let adjusted_path = format!("/api/coverage/jobs/{adjusted_id}");
    assert_eq!(
        coverage_terminal(&app, &cookie, &adjusted_path).await["state"],
        "complete"
    );
    let comparison = send(Method::POST, "/api/coverage/compare",
        json!({"baseline_job_id":baseline_id,"scenario_job_id":adjusted_id,"reference_id":"M-alternative"}),
        StatusCode::OK).await;
    assert_eq!(comparison["baseline_job_id"], baseline_id);
    assert!(
        comparison["cells"]
            .as_array()
            .unwrap()
            .iter()
            .any(|cell| cell["footprint_latlng"].is_array())
    );
    let new_report = send(
        Method::POST,
        &format!("{adjusted_path}/targets"),
        json!({}),
        StatusCode::OK,
    )
    .await;
    let reports = send(
        Method::GET,
        "/api/coverage/target-reports",
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(reports["reports"].as_array().unwrap().len(), 2);
    let diff = send(Method::POST, "/api/coverage/target-reports/compare",
        json!({"baseline_report_id":old_report["report_id"],"scenario_report_id":new_report["report_id"]}),
        StatusCode::OK).await;
    assert_eq!(diff["targets"][0]["id"], "T-test");
    let download_query = format!(
        "scenario_job_id={adjusted_id}&reference_id=M-alternative&target_report_id={}",
        old_report["report_id"].as_str().unwrap()
    );
    let download = send(
        Method::GET,
        &format!("{baseline_path}/export.json?{download_query}"),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(
        download["scenario_comparison"]["scenario_job_id"],
        adjusted_id
    );
    assert_eq!(
        download["metadata"]["target_report_id"],
        old_report["report_id"]
    );
    let geojson = send(
        Method::GET,
        &format!("{adjusted_path}/export.geojson"),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(geojson["type"], "FeatureCollection");
    let (status, headers, bytes) = self::request(
        &app,
        Some(&cookie),
        Method::GET,
        &format!("{baseline_path}/report.html?{download_query}"),
        "text/plain",
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "text/html");
    assert!(
        String::from_utf8(bytes)
            .unwrap()
            .contains("Calibration point")
    );

    let worker = state_handle.scheduler.reserve_worker().unwrap();
    let click = json!({"latitude":point.latitude,"longitude":point.longitude,"coverage_job_id":adjusted_id});
    let earlier = send(
        Method::POST,
        "/api/coverage/inspect",
        click.clone(),
        StatusCode::OK,
    )
    .await;
    let recent = send(Method::POST, "/api/coverage/inspect", click, StatusCode::OK).await;
    let inspect =
        |job: &Value| format!("/api/coverage/inspect/{}", job["job_id"].as_str().unwrap());
    send(
        Method::GET,
        &inspect(&earlier),
        Value::Null,
        StatusCode::NOT_FOUND,
    )
    .await;
    drop(worker);
    let inspection = coverage_terminal(&app, &cookie, &inspect(&recent)).await;
    assert_eq!(inspection["state"], "complete", "{inspection}");
    assert_eq!(
        inspection["result"]["selected_source"]["antenna_height_m"],
        19.0
    );

    let reused = send(Method::POST, "/api/coverage/jobs", request, StatusCode::OK).await;
    assert_eq!(reused["job_id"], baseline_id);
    assert_eq!(reused["reused"], true);
    let (reopened, _) = self::app(root.path(), "");
    let restored = state(&reopened, &cookie).await;
    assert_eq!(restored["coverage_job"]["job_id"], baseline_id);
    assert_eq!(restored["coverage_job"]["stale"], false);
    assert_eq!(restored["coverage_job"]["route_current"], true);

    let mut plan = state(&app, &cookie).await["plan"].clone();
    plan["coverage"]["cell_size_m"] = json!(70.0);
    send(
        Method::POST,
        "/api/projects/autosave",
        json!({"plan":plan}),
        StatusCode::OK,
    )
    .await;
    let stale = send(Method::GET, &adjusted_path, Value::Null, StatusCode::OK).await;
    assert_eq!(stale["stale"], true);
    assert_eq!(stale["route_current"], true);
    send(
        Method::GET,
        &format!("{adjusted_path}/export.geojson"),
        Value::Null,
        StatusCode::CONFLICT,
    )
    .await;
    let stale_export = send(
        Method::GET,
        &format!("{adjusted_path}/export.json?include_stale=1"),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(stale_export["metadata"]["stale"], true);
    api(
        &app,
        &self::cookie(B),
        Method::GET,
        &adjusted_path,
        Value::Null,
        StatusCode::NOT_FOUND,
    )
    .await;
}

#[tokio::test]
async fn terrain_upload_admission_releases_after_disconnect_and_keeps_state_responsive() {
    use tokio::io::AsyncWriteExt;
    use tokio_util::io::ReaderStream;

    let root = tempfile::tempdir().unwrap();
    let (router, app) = app(root.path(), "");
    let hold_upload = |key: &str| {
        let (producer, consumer) = tokio::io::duplex(4096);
        let router = router.clone();
        let cookie = cookie(key);
        let response = tokio::spawn(async move {
            request(
                &router,
                Some(&cookie),
                Method::POST,
                "/api/terrain/dtm",
                &format!("multipart/form-data; boundary={BOUNDARY}"),
                Body::from_stream(ReaderStream::new(consumer)),
            )
            .await
        });
        (producer, response)
    };
    let wait_for_stage = |key: &'static str| {
        let app = app.clone();
        async move {
            let project = app.store.active(key).unwrap();
            let directory = app.store.project_path(key, &project).unwrap();
            tokio::time::timeout(Duration::from_secs(5), async {
                while !std::fs::read_dir(&directory)
                    .unwrap()
                    .flatten()
                    .any(|entry| {
                        entry
                            .file_name()
                            .to_string_lossy()
                            .starts_with(".terrain-upload-")
                    })
                {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
        }
    };
    let (mut first, aborted) = hold_upload(A);
    let truncated = format!(
        "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"terrain.tif\"\r\n\r\n"
    );
    first.write_all(truncated.as_bytes()).await.unwrap();
    wait_for_stage(A).await;
    let (mut second, other) = hold_upload(B);
    second.write_all(truncated.as_bytes()).await.unwrap();
    wait_for_stage(B).await;
    assert_eq!(state(&router, &cookie(A)).await["job"]["state"], "idle");
    upload(
        &router,
        &cookie(A),
        "dtm",
        TILE,
        StatusCode::TOO_MANY_REQUESTS,
    )
    .await;
    upload(
        &router,
        &cookie(C),
        "dtm",
        TILE,
        StatusCode::TOO_MANY_REQUESTS,
    )
    .await;
    drop(first);
    assert_eq!(aborted.await.unwrap().0, StatusCode::BAD_REQUEST);
    upload(&router, &cookie(A), "dtm", TILE, StatusCode::OK).await;
    second.write_all(TILE).await.unwrap();
    second
        .write_all(format!("\r\n--{BOUNDARY}--\r\n").as_bytes())
        .await
        .unwrap();
    drop(second);
    assert_eq!(other.await.unwrap().0, StatusCode::OK);
    upload(&router, &cookie(C), "dtm", TILE, StatusCode::OK).await;
}

#[tokio::test]
async fn custom_crs_coverage_keeps_saved_footprints_and_limits_kartverket() {
    let root = tempfile::tempdir().unwrap();
    let (app, _) = app(root.path(), "");
    let cookie = cookie(A);
    let tile = include_bytes!("../../terrain/tests/fixtures/custom_tm.tif");
    upload(&app, &cookie, "dtm", tile, StatusCode::OK).await;
    let transform = Transformer::new(
        repeaternet_terrain::GeoTiff::open(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../terrain/tests/fixtures/custom_tm.tif"),
        )
        .unwrap()
        .metadata()
        .crs,
    )
    .unwrap();
    let center = transform
        .forward(LonLat {
            latitude: 60.1,
            longitude: 14.9,
        })
        .unwrap();
    let endpoint = |offset| {
        let p = transform
            .reverse(Point {
                x: center.x + offset,
                y: center.y,
            })
            .unwrap();
        [p.latitude, p.longitude]
    };
    let settings = json!({"include_endpoints":true,"area_includes_clients":false,"area_buffer_m":1.0,"cell_size_m":2.0});
    let plan = json!({"a":endpoint(-2.0),"b":endpoint(2.0),"coverage":settings,
        "candidates":{"infrastructure_policy":"existing_only", "refine_radius_m":0}});
    api(
        &app,
        &cookie,
        Method::POST,
        "/api/optimize",
        plan,
        StatusCode::OK,
    )
    .await;
    let state = terminal(&app, &cookie).await;
    assert_eq!(state["job"]["state"], "complete", "{state}");
    let route = api(
        &app,
        &cookie,
        Method::GET,
        "/api/result",
        Value::Null,
        StatusCode::OK,
    )
    .await;
    let mut coverage = json!({"snapshot_version":route["snapshot_version"],
        "alternative_id":route["active_alternative_id"],"settings":settings});
    let ready = api(
        &app,
        &cookie,
        Method::POST,
        "/api/coverage/terrain/prepare",
        coverage.clone(),
        StatusCode::OK,
    )
    .await;
    assert_eq!(ready["state"], "complete");
    assert_eq!(ready["terrain_estimate"]["tile_count"], 0);
    let job = api(
        &app,
        &cookie,
        Method::POST,
        "/api/coverage/jobs",
        coverage.clone(),
        StatusCode::OK,
    )
    .await;
    let path = format!("/api/coverage/jobs/{}", job["job_id"].as_str().unwrap());
    let complete = coverage_terminal(&app, &cookie, &path).await;
    assert_eq!(complete["state"], "complete", "{complete}");
    coverage["settings"]["area_buffer_m"] = json!(100.0);
    api(
        &app,
        &cookie,
        Method::POST,
        "/api/coverage/terrain/estimate",
        coverage,
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    let page = api(
        &app,
        &cookie,
        Method::GET,
        &format!("{path}/cells"),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert!(page["cells"][0]["footprint_latlng"].is_array());
    api(
        &app,
        &cookie,
        Method::DELETE,
        "/api/terrain/dtm",
        Value::Null,
        StatusCode::OK,
    )
    .await;
    let after = api(
        &app,
        &cookie,
        Method::GET,
        &format!("{path}/cells"),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(after["stale"], true);
    assert_eq!(after["cells"], page["cells"]);
    let error = api(
        &app,
        &cookie,
        Method::GET,
        &format!("{path}/export.json?include_stale=true"),
        Value::Null,
        StatusCode::CONFLICT,
    )
    .await;
    assert!(
        error["detail"]
            .as_str()
            .unwrap()
            .contains("CRS is unavailable")
    );
}

#[tokio::test]
async fn verified_python_runs_compare_without_a_live_route_until_terrain_changes() {
    let root = tempfile::tempdir().unwrap();
    let (app, handle) = app(root.path(), "");
    let cookie = cookie(A);
    upload(&app, &cookie, "dtm", TILE, StatusCode::OK).await;
    let project = handle.store.active(A).unwrap();
    let directory = handle.store.project_path(A, &project).unwrap();
    let files = crate::catalog::TerrainFiles::read(&directory, false);
    let fingerprint = files.fingerprint(&directory, None).unwrap();
    let golden: Value = serde_json::from_str(include_str!(
        "../../coverage-store/tests/fixtures/python-job.json"
    ))
    .unwrap();
    let mut previous = golden["job"].clone();
    previous["project_id"] = json!(project.id);
    previous["terrain_fingerprint"] = fingerprint;
    previous["router_ids"] = previous["source_ids"].clone();
    previous["area_bounds_wgs84"] = json!([60.0, 10.0, 60.1, 10.1]);
    let mut later = previous.clone();
    later["job_id"] = json!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
    later["result_file"] = json!("coverage-results/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb.jsonl");
    std::fs::create_dir(directory.join("coverage-jobs")).unwrap();
    std::fs::create_dir(directory.join("coverage-results")).unwrap();
    for job in [&previous, &later] {
        let id = job["job_id"].as_str().unwrap();
        std::fs::write(
            directory.join("coverage-jobs").join(format!("{id}.json")),
            job.to_string(),
        )
        .unwrap();
        std::fs::write(
            directory.join(job["result_file"].as_str().unwrap()),
            include_bytes!("../../coverage-store/tests/fixtures/python-cells.jsonl"),
        )
        .unwrap();
    }
    let path = format!(
        "/api/coverage/jobs/{}",
        previous["job_id"].as_str().unwrap()
    );
    let status = api(
        &app,
        &cookie,
        Method::GET,
        &path,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(status["stale_reason"], "legacy_python_route_not_current");
    let request = json!({"baseline_job_id":previous["job_id"],"scenario_job_id":later["job_id"],"reference_id":"R-coverage"});
    let report = api(
        &app,
        &cookie,
        Method::POST,
        "/api/coverage/compare",
        request.clone(),
        StatusCode::OK,
    )
    .await;
    assert_eq!(report["counts"]["outside_area_cells"], 1);
    upload(&app, &cookie, "dom", TILE, StatusCode::OK).await;
    let changed = api(
        &app,
        &cookie,
        Method::GET,
        &path,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(changed["stale_reason"], "legacy_terrain_changed");
    api(
        &app,
        &cookie,
        Method::POST,
        "/api/coverage/compare",
        request,
        StatusCode::CONFLICT,
    )
    .await;
}

#[tokio::test]
async fn authenticated_upload_to_profiles_alternatives_exports_and_restart() {
    let root = tempfile::tempdir().unwrap();
    let (app, _) = app(root.path(), "test-pass");
    let (status, _, _) = request(
        &app,
        Some(&cookie(A)),
        Method::POST,
        "/api/optimize",
        "application/json",
        Body::from(plan().to_string()),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, cookies, _) = request(
        &app,
        None,
        Method::POST,
        "/login",
        "application/x-www-form-urlencoded",
        Body::from("token=test-pass"),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let access = set_cookie(&cookies);
    let (status, _, _) = request(
        &app,
        Some(access),
        Method::POST,
        "/api/optimize",
        "application/json",
        Body::from(plan().to_string()),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, cookies, _) = request(
        &app,
        Some(access),
        Method::GET,
        "/",
        "text/html",
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let authenticated = format!("{access}; {}", set_cookie(&cookies));
    let initial = state(&app, &authenticated).await;
    let project_id = initial["projects"]["active_id"].as_str().unwrap();
    let mutate_plan = |path: &'static str, data: Value, expected| {
        let app = app.clone();
        let authenticated = authenticated.clone();
        async move {
            let current = state(&app, &authenticated).await;
            let revision = current["projects"]["plan_revision"].as_str().unwrap();
            let request = Request::builder()
                .method(Method::POST)
                .uri(path)
                .header("host", "127.0.0.1:8000")
                .header("cookie", &authenticated)
                .header("content-type", "application/json")
                .header("x-repeaternet-project", project_id)
                .header("x-repeaternet-plan-revision", revision)
                .body(Body::from(data.to_string()))
                .unwrap();
            let response = app.oneshot(request).await.unwrap();
            let (parts, body) = response.into_parts();
            let body: Value =
                serde_json::from_slice(&to_bytes(body, 1024 * 1024).await.unwrap()).unwrap();
            assert_eq!(parts.status, expected, "{body}");
            body
        }
    };
    let optimize = |plan, status| mutate_plan("/api/optimize", plan, status);
    let missing = optimize(plan(), StatusCode::UNPROCESSABLE_ENTITY).await;
    assert_eq!(missing["detail"], "Prepare or upload ground terrain first");
    let mut wrong_rf = plan();
    wrong_rf["rf"]["router"] = json!({"pattern_csv": "../unsafe.csv"});
    optimize(wrong_rf, StatusCode::UNPROCESSABLE_ENTITY).await;
    let uploaded = upload(&app, &authenticated, "dtm", TILE, StatusCode::OK).await;
    let name = uploaded["name"].as_str().unwrap();
    assert_eq!(name.len(), 20);
    assert!(name.ends_with(".tif"));
    assert!(
        uploaded["bounds"][0][0].as_f64().unwrap() < uploaded["bounds"][1][0].as_f64().unwrap()
    );
    upload(&app, &authenticated, "dom", TILE, StatusCode::OK).await;
    let readiness = api(
        &app,
        &authenticated,
        Method::POST,
        "/api/terrain/coverage?route_only=true",
        plan(),
        StatusCode::OK,
    )
    .await;
    assert_eq!(readiness["covered"], true);
    assert_eq!(readiness["surface_available"], true);
    assert_eq!(readiness["dtm_outlines"].as_array().unwrap().len(), 1);
    assert_eq!(readiness["line_gaps"], json!([]));
    let mut outside = plan();
    outside["b"] = json!([59, 15]);
    let readiness = api(
        &app,
        &authenticated,
        Method::POST,
        "/api/terrain/coverage",
        outside.clone(),
        StatusCode::OK,
    )
    .await;
    assert_eq!(
        readiness["uncovered_sites"][0]["reason"],
        "outside_coverage"
    );
    optimize(outside, StatusCode::UNPROCESSABLE_ENTITY).await;

    let job = optimize(plan(), StatusCode::OK).await;
    let completed = terminal(&app, &authenticated).await;
    assert_eq!(completed["job"]["state"], "complete", "{completed}");
    assert_eq!(completed["projects"]["previous_result"]["found"], true);
    let result = api(
        &app,
        &authenticated,
        Method::GET,
        "/api/result",
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(result["job_id"], job["job_id"]);
    assert_eq!(result["input_revision"], job["input_revision"]);
    assert_eq!(
        result["snapshot_version"],
        completed["job"]["snapshot_version"]
    );
    assert_eq!(result["router_count"], 0);
    assert_eq!(result["rf_settings"]["frequency_mhz"], 869.5);
    assert!(
        result["links"][0]["profile"]["distances_m"]
            .as_array()
            .unwrap()
            .len()
            > 2
    );
    assert_eq!(result["links"][0]["profile"]["surface_available"], true);
    assert!(
        result["links"][0]["forward"]["usable_margin_db"]
            .as_f64()
            .unwrap()
            > 0.0
    );
    let alternatives = result["alternatives"].as_array().unwrap();
    let alternative = alternatives
        .iter()
        .find(|item| item["selected"] == false)
        .expect("second certified topology");
    let id = alternative["id"].as_str().unwrap();
    let selected = api(
        &app,
        &authenticated,
        Method::POST,
        &format!("/api/alternatives/{id}/select"),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(selected["active_alternative_id"], id);
    assert_ne!(selected["route"], result["route"]);
    assert_eq!(
        selected["snapshot_version"].as_u64().unwrap(),
        result["snapshot_version"].as_u64().unwrap() + 1
    );
    assert_eq!(
        state(&app, &authenticated).await["rf_cache"]["misses"],
        completed["rf_cache"]["misses"]
    );
    for (kind, needle) in [
        ("csv", "hop,from,to"),
        ("geojson", "FeatureCollection"),
        ("html", "Terrain resolution"),
        ("project", "unknown_future_field"),
    ] {
        let (status, headers, body) = request(
            &app,
            Some(&authenticated),
            Method::GET,
            &format!("/api/export/{kind}"),
            "text/plain",
            Body::empty(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            headers["content-disposition"]
                .to_str()
                .unwrap()
                .starts_with("attachment;")
        );
        assert!(String::from_utf8(body).unwrap().contains(needle));
    }

    let other = api(
        &app,
        &authenticated,
        Method::POST,
        "/api/projects",
        json!({"name": "Another project"}),
        StatusCode::OK,
    )
    .await;
    assert_ne!(other["id"], project_id);
    api(
        &app,
        &authenticated,
        Method::GET,
        "/api/result",
        Value::Null,
        StatusCode::NOT_FOUND,
    )
    .await;
    api(
        &app,
        &authenticated,
        Method::POST,
        &format!("/api/projects/{project_id}/activate"),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    let reopened_job = state(&app, &authenticated).await["job"].clone();
    let mut expected = selected.clone();
    expected["input_revision"] = reopened_job["input_revision"].clone();
    assert_eq!(reopened_job["input_revision"], selected["input_revision"]);
    assert_eq!(
        api(
            &app,
            &authenticated,
            Method::GET,
            "/api/result",
            Value::Null,
            StatusCode::OK
        )
        .await,
        expected
    );

    let (restarted, _) = self::app(root.path(), "test-pass");
    let revived = state(&restarted, &authenticated).await;
    assert_eq!(revived["projects"]["active_id"], project_id);
    assert_eq!(
        revived["projects"]["previous_result"]["active_alternative_id"],
        id
    );
    assert_eq!(revived["projects"]["previous_result"]["stale"], false);
    expected["input_revision"] = revived["job"]["input_revision"].clone();
    let restored = api(
        &restarted,
        &authenticated,
        Method::GET,
        "/api/result",
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(restored, expected);
    let (status, _, report) = request(
        &restarted,
        Some(&authenticated),
        Method::GET,
        "/api/export/html",
        "text/plain",
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        String::from_utf8(report)
            .unwrap()
            .contains("Terrain resolution")
    );
    let first = result["active_alternative_id"].as_str().unwrap();
    let reselected = api(
        &restarted,
        &authenticated,
        Method::POST,
        &format!("/api/alternatives/{first}/select"),
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(reselected["links"], result["links"]);

    let mut coverage_only = completed["plan"].clone();
    coverage_only["coverage"] = json!({"cell_size_m": 250});
    mutate_plan(
        "/api/projects/autosave",
        json!({"project_id": project_id, "plan": coverage_only}),
        StatusCode::OK,
    )
    .await;
    api(
        &app,
        &authenticated,
        Method::GET,
        "/api/result",
        Value::Null,
        StatusCode::OK,
    )
    .await;
    let mut changed = coverage_only.clone();
    changed["rf"]["frequency_mhz"] = json!(868.1);
    let edited = mutate_plan(
        "/api/projects/autosave",
        json!({"project_id": project_id, "plan": changed}),
        StatusCode::OK,
    )
    .await;
    assert_eq!(edited["route_result_available"], false);
    api(
        &app,
        &authenticated,
        Method::GET,
        "/api/result",
        Value::Null,
        StatusCode::NOT_FOUND,
    )
    .await;
    let undone = mutate_plan(
        "/api/projects/autosave",
        json!({"project_id": project_id, "plan": coverage_only}),
        StatusCode::OK,
    )
    .await;
    assert_eq!(undone["route_result_available"], true);
    let recovered = api(
        &app,
        &authenticated,
        Method::GET,
        "/api/result",
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(recovered["job_id"], reselected["job_id"]);
    assert_eq!(
        recovered["snapshot_version"],
        reselected["snapshot_version"]
    );
    assert_eq!(recovered["links"], reselected["links"]);
    let mut stale = plan();
    stale["input_revision"] = job["input_revision"].clone();
    optimize(stale, StatusCode::CONFLICT).await;
    api(
        &app,
        &authenticated,
        Method::DELETE,
        "/api/terrain/dtm",
        Value::Null,
        StatusCode::OK,
    )
    .await;
    optimize(plan(), StatusCode::UNPROCESSABLE_ENTITY).await;
    api(
        &app,
        &authenticated,
        Method::DELETE,
        "/api/cache",
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(state(&app, &authenticated).await["rf_cache"]["entries"], 0);
}

fn gate(app: &AppState) -> (tokio::sync::oneshot::Receiver<()>, mpsc::Sender<()>) {
    let (published, ready) = tokio::sync::oneshot::channel();
    let (resume, receiver) = mpsc::channel();
    *app.scheduler.snapshot_gate.lock().unwrap() = Some(SnapshotGate {
        published,
        resume: receiver,
    });
    (ready, resume)
}

#[tokio::test]
async fn cancellation_keep_and_capacity_share_workspace_guards() {
    let root = tempfile::tempdir().unwrap();
    let (app, state_app) = app(root.path(), "");
    let [a, b, c] = [cookie(A), cookie(B), cookie(C)];
    for cookie in [&a, &b, &c] {
        upload(&app, cookie, "dtm", TILE, StatusCode::OK).await;
    }

    let (ready, resume) = gate(&state_app);
    let job = api(
        &app,
        &a,
        Method::POST,
        "/api/optimize",
        plan(),
        StatusCode::OK,
    )
    .await;
    tokio::time::timeout(Duration::from_secs(5), ready)
        .await
        .unwrap()
        .unwrap();
    let before = api(
        &app,
        &a,
        Method::GET,
        "/api/result",
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(before["found"], true);
    assert_eq!(before["search_complete"], false);
    let queued = api(
        &app,
        &b,
        Method::POST,
        "/api/optimize",
        plan(),
        StatusCode::OK,
    )
    .await;
    assert_eq!(queued["state"], "queued");
    api(
        &app,
        &c,
        Method::POST,
        "/api/optimize",
        plan(),
        StatusCode::TOO_MANY_REQUESTS,
    )
    .await;
    api(
        &app,
        &a,
        Method::POST,
        "/api/projects",
        json!({"name": "Busy"}),
        StatusCode::CONFLICT,
    )
    .await;
    api(
        &app,
        &a,
        Method::POST,
        "/api/projects/autosave",
        json!({"plan": {}}),
        StatusCode::CONFLICT,
    )
    .await;
    api(
        &app,
        &a,
        Method::DELETE,
        "/api/terrain/dtm",
        Value::Null,
        StatusCode::CONFLICT,
    )
    .await;
    api(
        &app,
        &a,
        Method::POST,
        "/api/cancel",
        json!({"job_id": queued["job_id"]}),
        StatusCode::CONFLICT,
    )
    .await;
    api(
        &app,
        &b,
        Method::POST,
        "/api/stop-and-keep",
        json!({"job_id": queued["job_id"]}),
        StatusCode::CONFLICT,
    )
    .await;
    api(
        &app,
        &b,
        Method::POST,
        "/api/cancel",
        json!({"job_id": queued["job_id"]}),
        StatusCode::OK,
    )
    .await;
    assert_eq!(terminal(&app, &b).await["job"]["state"], "cancelled");
    api(
        &app,
        &a,
        Method::POST,
        "/api/stop-and-keep",
        json!({"job_id": job["job_id"]}),
        StatusCode::OK,
    )
    .await;
    resume.send(()).unwrap();
    let stopped = terminal(&app, &a).await;
    assert_eq!(stopped["job"]["state"], "stopped");
    assert_eq!(
        stopped["job"]["snapshot_version"],
        before["snapshot_version"]
    );
    assert_eq!(
        stopped["projects"]["previous_result"]["search_complete"],
        false
    );
    assert_eq!(
        api(
            &app,
            &a,
            Method::GET,
            "/api/result",
            Value::Null,
            StatusCode::OK
        )
        .await,
        before
    );
    assert_eq!(state_app.store.active(A).unwrap().run_state, "stopped");
    let (restarted, _) = self::app(root.path(), "");
    assert_eq!(state(&restarted, &a).await["job"]["state"], "stopped");
    let restored = api(
        &restarted,
        &a,
        Method::GET,
        "/api/result",
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(restored["links"], before["links"]);
    assert_eq!(restored["snapshot_version"], before["snapshot_version"]);

    let (ready, resume) = gate(&state_app);
    let job = api(
        &app,
        &a,
        Method::POST,
        "/api/optimize",
        plan(),
        StatusCode::OK,
    )
    .await;
    tokio::time::timeout(Duration::from_secs(5), ready)
        .await
        .unwrap()
        .unwrap();
    api(
        &app,
        &a,
        Method::POST,
        "/api/cancel",
        json!({"job_id": job["job_id"]}),
        StatusCode::OK,
    )
    .await;
    resume.send(()).unwrap();
    assert_eq!(terminal(&app, &a).await["job"]["state"], "cancelled");
    api(
        &app,
        &a,
        Method::GET,
        "/api/result",
        Value::Null,
        StatusCode::NOT_FOUND,
    )
    .await;
    assert!(state_app.store.active(A).unwrap().result_summary.is_none());

    let (ready, resume) = gate(&state_app);
    api(
        &app,
        &a,
        Method::POST,
        "/api/optimize",
        plan(),
        StatusCode::OK,
    )
    .await;
    tokio::time::timeout(Duration::from_secs(5), ready)
        .await
        .unwrap()
        .unwrap();
    let project = state_app.store.active(A).unwrap();
    let directory = state_app.store.project_path(A, &project).unwrap();
    std::fs::write(directory.join("dtm/new-tile.tif"), TILE).unwrap();
    api(
        &app,
        &a,
        Method::GET,
        "/api/result",
        Value::Null,
        StatusCode::CONFLICT,
    )
    .await;
    resume.send(()).unwrap();
    assert_eq!(terminal(&app, &a).await["job"]["state"], "failed");
    assert!(state_app.store.active(A).unwrap().result_summary.is_none());
}

#[tokio::test]
async fn terrain_validation_rejects_late_corruption_and_bounds_multipart_streams() {
    let root = tempfile::tempdir().unwrap();
    let (app, state_app) = app(root.path(), "");
    let cookie = cookie(A);
    let masked = include_bytes!("../../terrain/tests/fixtures/masked.tif");
    upload(&app, &cookie, "dtm", masked, StatusCode::OK).await;
    let mut corrupt = ROTATED.to_vec();
    let tif = tiff_reader::TiffFile::from_bytes(ROTATED.to_vec()).unwrap();
    let image = tif.ifd(0).unwrap();
    let offset = *image.tile_offsets().unwrap().last().unwrap() as usize;
    let size = *image.tile_byte_counts().unwrap().last().unwrap() as usize;
    corrupt[offset..offset + size].fill(0);
    upload(&app, &cookie, "dtm", &corrupt, StatusCode::BAD_REQUEST).await;
    upload(
        &app,
        &cookie,
        "dtm",
        include_bytes!("../../terrain/tests/fixtures/unprojected.tif"),
        StatusCode::BAD_REQUEST,
    )
    .await;
    let padded = {
        let mut padded = TILE.to_vec();
        padded.resize(17 * 1024 * 1024, 0);
        padded
    };
    upload(&app, &cookie, "dom", &padded, StatusCode::OK).await;

    async fn small_upload(
        State(app): State<AppState>,
        workspace: api::Workspace,
        multipart: Result<axum::extract::Multipart, axum::extract::multipart::MultipartRejection>,
    ) -> api::ApiResult<axum::Json<Value>> {
        crate::terrain_http::upload_bounded(app, workspace, "dtm", multipart, 1024).await
    }
    let limited = Router::new()
        .route("/api/terrain/dtm", post(small_upload))
        .with_state(state_app.clone());
    upload(
        &limited,
        &cookie,
        "dtm",
        &vec![b'0'; 1025],
        StatusCode::BAD_REQUEST,
    )
    .await;
    let after = state(&app, &cookie).await;
    assert_eq!(after["terrain"]["dtm"].as_array().unwrap().len(), 1);
    assert_eq!(after["terrain"]["dom"].as_array().unwrap().len(), 1);
    let project = state_app.store.active(A).unwrap();
    let directory = state_app.store.project_path(A, &project).unwrap();
    assert!(std::fs::read_dir(directory).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".terrain-")
    }));
}

async fn coverage_terrain_terminal(app: &Router, cookie: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let current = state(app, cookie).await;
            if !matches!(
                current["coverage_terrain_job"]["state"].as_str(),
                Some("queued" | "preparing")
            ) {
                break current;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap()
}

async fn mock_upstream(router: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    (
        endpoint,
        tokio::spawn(async { axum::serve(listener, router).await.unwrap() }),
    )
}

#[tokio::test]
async fn data_sources_edit_validate_and_redact() {
    let root = tempfile::tempdir().unwrap();
    let (app, _) = app(root.path(), "");
    let cookie = cookie(A);
    let call = |method, url, body, status| api(&app, &cookie, method, url, body, status);
    let initial = call(
        Method::GET,
        "/api/data-sources",
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert!(initial["errors"].as_array().unwrap().is_empty());
    assert_eq!(initial["terrain"][0]["products"], json!(["dtm", "dom"]));
    assert_eq!(initial["terrain"][0]["builtin"], true);
    assert_eq!(initial["terrain"][0]["attribution"], "© Kartverket");
    assert_eq!(initial["basemaps"][0]["id"], "osm");
    let custom = json!({
        "version": 1, "disabled_builtins": ["copernicus-glo90"],
        "terrain":[{
            "id":"local", "name":"Local elevation", "kind":"cog", "coverage":{"type":"global"},
            "products":{"dtm":{"url":"http://localhost:8080/local.tif"}},
            "attribution":"Local survey", "resolution_m":20,
            "headers":{"authorization":"test-sensitive","x-key":"${SOURCE_KEY}"}
        }]
    });
    let valid = call(
        Method::POST,
        "/api/data-sources/validate",
        custom.clone(),
        StatusCode::OK,
    )
    .await;
    assert_eq!(valid, json!({"ok":true, "issues":[]}));
    assert!(!root.path().join("providers.json").exists());
    let saved = call(
        Method::PUT,
        "/api/data-sources",
        custom.clone(),
        StatusCode::OK,
    )
    .await;
    assert_eq!(
        saved["custom"]["terrain"][0]["headers"],
        json!({"authorization":"***","x-key":"${SOURCE_KEY}"})
    );
    assert!(!saved.to_string().contains("test-sensitive"));
    let disabled = saved["terrain"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == "copernicus-glo90")
        .unwrap();
    assert_eq!(disabled["enabled"], false);
    let mut edit = saved["custom"].clone();
    edit["terrain"][0]["name"] = json!("Updated local elevation");
    call(
        Method::PUT,
        "/api/data-sources",
        edit.clone(),
        StatusCode::OK,
    )
    .await;
    let file = std::fs::read_to_string(root.path().join("providers.json")).unwrap();
    assert!(file.contains("test-sensitive"));
    edit["terrain"][0]["products"]["dtm"]["url"] = json!("http://example.com/plain.tif");
    let invalid = call(
        Method::PUT,
        "/api/data-sources",
        edit,
        StatusCode::BAD_REQUEST,
    )
    .await;
    assert_eq!(invalid["issues"][0]["path"], "terrain[0].products.dtm.url");
    assert_eq!(
        std::fs::read_to_string(root.path().join("providers.json")).unwrap(),
        file
    );
    let bad =
        json!({"a":[-22.95,-43.21],"b":[-22.94,-43.20],"terrain":{"source":"copernicus-glo90"}});
    call(
        Method::POST,
        "/api/data-sources/preview",
        bad,
        StatusCode::BAD_REQUEST,
    )
    .await;
}

#[tokio::test]
async fn rio_cog_estimate_prepare_and_search_with_registry_change_guard() {
    use axum::{response::IntoResponse, routing::get};
    const COG: &[u8] = include_bytes!("../../providers/tests/fixtures/geographic-cog.tif");
    let (upstream, task) = mock_upstream(Router::new().route(
        "/rio.tif",
        get(|headers: HeaderMap| async move {
            assert_eq!(headers["range"], "bytes=0-262143");
            let range = format!("bytes 0-{}/{}", COG.len() - 1, COG.len());
            (
                StatusCode::PARTIAL_CONTENT,
                [("content-type", "image/tiff"), ("content-range", &range)],
                COG,
            )
                .into_response()
        }),
    ))
    .await;
    let root = tempfile::tempdir().unwrap();
    let mut definitions = json!({
        "version":1,
        "terrain":[{
            "id":"rio-local", "name":"Rio local elevation", "kind":"cog",
            "products":{"dtm":{"url":format!("{upstream}/rio.tif")}},
            "coverage":{"type":"bbox","bbox":[-43.4,-23.21,-42.9,-22.7]},
            "resolution_m":20,"attribution":"Synthetic Rio fixture"
        }]
    });
    std::fs::write(root.path().join("providers.json"), definitions.to_string()).unwrap();
    let (_, mut runtime) = app(root.path(), "");
    runtime.terrain_limits.maximum_tile_area_km2 = 1.0;
    let app = router_with_state(runtime, "", true);
    let cookie = cookie(A);
    let call = |method, url, body, status| api(&app, &cookie, method, url, body, status);
    let mut plan = json!({
        "a": [-22.95,-43.21], "b": [-22.948,-43.208],
        "terrain":{"source":"rio-local","fallback":false},
        "candidates":{"corridor_width_m":100,"candidates_per_cell":1,"refine_radius_m":0},
        "search_effort":"quick"
    });
    let preview = call(
        Method::POST,
        "/api/data-sources/preview",
        plan.clone(),
        StatusCode::OK,
    )
    .await;
    assert_eq!(preview["crs"], "EPSG:32723");
    assert_eq!(preview["dom_available"], false);
    assert_eq!(preview["sources"][0]["coverage_share"], 1.0);
    let estimate_url = "/api/terrain/estimate";
    let prepare_url = "/api/terrain/prepare";
    let estimate = call(Method::POST, estimate_url, plan.clone(), StatusCode::OK).await;
    assert_eq!(estimate["effective_resolution_m"], 20.0);
    assert_eq!(estimate["sources"][0]["id"], "rio-local");
    assert!(estimate["dom_unavailable_reason"].is_string());
    definitions["terrain"][0]["name"] = json!("Rio updated elevation");
    call(
        Method::PUT,
        "/api/data-sources",
        definitions,
        StatusCode::OK,
    )
    .await;
    call(
        Method::POST,
        prepare_url,
        plan.clone(),
        StatusCode::CONFLICT,
    )
    .await;
    plan["registry_fingerprint"] = estimate["registry_fingerprint"].clone();
    call(
        Method::POST,
        prepare_url,
        plan.clone(),
        StatusCode::CONFLICT,
    )
    .await;
    let estimate = call(Method::POST, estimate_url, plan.clone(), StatusCode::OK).await;
    plan["registry_fingerprint"] = estimate["registry_fingerprint"].clone();
    call(Method::POST, prepare_url, plan.clone(), StatusCode::OK).await;
    let prepared = terminal(&app, &cookie).await;
    assert_eq!(prepared["job"]["state"], "terrain_ready", "{prepared}");
    assert_eq!(prepared["job"]["providers_used"], json!(["rio-local"]));
    assert_eq!(
        prepared["terrain_sources"][0]["attribution"],
        "Synthetic Rio fixture"
    );
    let readiness = call(
        Method::POST,
        "/api/terrain/coverage?route_only=true",
        plan.clone(),
        StatusCode::OK,
    )
    .await;
    assert_eq!(readiness["covered"], true, "{readiness}");
    plan.as_object_mut().unwrap().remove("registry_fingerprint");
    call(Method::POST, "/api/optimize", plan.clone(), StatusCode::OK).await;
    assert_eq!(terminal(&app, &cookie).await["job"]["state"], "complete");
    plan["terrain"]["source"] = json!("missing");
    call(Method::POST, estimate_url, plan, StatusCode::BAD_REQUEST).await;
    task.abort();
}

fn geographic(x: f64, y: f64) -> LonLat {
    Transformer::new(TerrainCrs::Etrs89Utm33)
        .unwrap()
        .reverse(Point { x, y })
        .unwrap()
}

fn live_fixture(width: u32, step: f64, mut elevation: impl FnMut(f64, f64) -> f32) -> Vec<u8> {
    use repeaternet_terrain::{Bounds, write_elevation_tiff};
    let half = f64::from(width) * step / 2.0;
    let mut values = Vec::with_capacity((width * width) as usize);
    for row in 0..width {
        for column in 0..width {
            values.push(elevation(
                (f64::from(column) + 0.5) * step - half,
                half - (f64::from(row) + 0.5) * step,
            ));
        }
    }
    let mut tile = Vec::new();
    write_elevation_tiff(
        &mut tile,
        TerrainCrs::Etrs89Utm33,
        Bounds {
            min_x: 500000.0 - half,
            max_x: 500000.0 + half,
            min_y: 6650000.0 - half,
            max_y: 6650000.0 + half,
        },
        width,
        width,
        &values,
    )
    .unwrap();
    tile
}

fn live_request(radius: f64, resolution: f64) -> Value {
    let site = geographic(500000.0, 6650000.0);
    json!({
        "site": {"id":"A", "kind":"endpoint_a", "latitude":site.latitude,
            "longitude":site.longitude, "antenna_height_m":55.0},
        "radius_m":radius, "resolution_m":resolution
    })
}

async fn live_response(app: &Router, cookie: &str, payload: Value) -> (Value, Vec<u8>) {
    let (status, headers, bytes) = request(
        app,
        Some(cookie),
        Method::POST,
        "/api/live-coverage",
        "application/json",
        Body::from(payload.to_string()),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&bytes[..bytes.len().min(1024)])
    );
    assert_eq!(headers["content-type"], "application/octet-stream");
    assert_eq!(headers["cache-control"], "no-store");
    assert_eq!(
        headers["content-length"]
            .to_str()
            .unwrap()
            .parse::<usize>()
            .unwrap(),
        bytes.len()
    );
    assert_eq!(&bytes[..4], b"RNLC");
    let header_end = 8 + u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
    let header: Value = serde_json::from_slice(&bytes[8..header_end]).unwrap();
    let pixels = bytes[header_end..].to_vec();
    assert_eq!(
        pixels.len(),
        (header["width"].as_u64().unwrap() * header["height"].as_u64().unwrap()) as usize
    );
    (header, pixels)
}

#[tokio::test]
async fn live_coverage_validates_and_returns_a_mercator_raster_without_a_route() {
    let root = tempfile::tempdir().unwrap();
    let (app, _) = app(root.path(), "");
    let cookie = cookie(A);
    let path = "/api/live-coverage";
    let payload = live_request(1_000.0, 50.0);
    let post = |payload, status| api(&app, &cookie, Method::POST, path, payload, status);
    let response = post(payload.clone(), StatusCode::CONFLICT).await;
    assert_eq!(
        response["detail"],
        "No terrain yet. Download terrain to see coverage."
    );
    for (field, value) in [
        ("mode", json!("overlap")),
        ("radius_m", json!("1000")),
        ("resolution_m", json!(4.99)),
        ("floor_db", json!(0.01)),
        ("client", json!({"height_agl_m": 0})),
        ("project_id", Value::Null),
    ] {
        let mut bad = payload.clone();
        bad[field] = value;
        post(bad, StatusCode::BAD_REQUEST).await;
    }
    let mut bad = payload.clone();
    bad["project_id"] = json!("a-different-project");
    assert_eq!(
        post(bad, StatusCode::CONFLICT).await["detail"],
        "The active project changed"
    );

    upload(
        &app,
        &cookie,
        "dtm",
        &live_fixture(45, 10.0, |_, _| 93.0),
        StatusCode::OK,
    )
    .await;
    let (header, pixels) = live_response(&app, &cookie, payload.clone()).await;
    assert_eq!(header["site_id"], "A");
    assert_eq!(header["width"], 41);
    assert_eq!(header["height"], 41);
    assert_eq!(header["rays"], 360);
    assert_eq!(header["samples_per_ray"], 20);
    assert_eq!(header["resolution_m"], 50.0);
    assert_eq!(header["radius_m"], 1000.0);
    assert_eq!(header["floor_db"], -20.0);
    assert_eq!(header["step_db"], 0.5);
    assert_eq!(header["site_ground_m"], 93.0);
    assert_eq!(header["antenna_elevation_m"], 148.0);
    assert!(header["pixel_size"].as_f64().unwrap() > 50.0);
    assert!(header["origin_x"].as_f64().unwrap() > 0.0);
    assert!(header["origin_y"].as_f64().unwrap() > 0.0);
    assert!(header["elapsed_ms"].as_f64().unwrap() > 0.0);
    assert!(header["covered_fraction"].as_f64().unwrap() > 0.0);
    assert!(header["unknown_fraction"].as_f64().unwrap() > 0.0);
    let bounds = header["bounds_wgs84"].as_array().unwrap();
    assert!(bounds[0].as_f64().unwrap() < payload["site"]["latitude"].as_f64().unwrap());
    assert!(bounds[1].as_f64().unwrap() < payload["site"]["longitude"].as_f64().unwrap());
    assert!(bounds[2].as_f64().unwrap() > payload["site"]["latitude"].as_f64().unwrap());
    assert!(bounds[3].as_f64().unwrap() > payload["site"]["longitude"].as_f64().unwrap());
    assert_eq!(pixels[0], 0);
    assert!(pixels.contains(&1));
    assert!(pixels[20 * 41 + 20] >= 43);

    let mut outside = payload.clone();
    outside["site"]["longitude"] = json!(14.8);
    assert_eq!(
        post(outside, StatusCode::CONFLICT).await["detail"],
        "No ground terrain at this site"
    );

    // Replacing route terrain must invalidate previously cached tiles.
    api(
        &app,
        &cookie,
        Method::DELETE,
        "/api/terrain/dtm",
        Value::Null,
        StatusCode::OK,
    )
    .await;
    upload(
        &app,
        &cookie,
        "dtm",
        &live_fixture(45, 10.0, |_, _| 123.0),
        StatusCode::OK,
    )
    .await;
    let (header, _) = live_response(&app, &cookie, payload).await;
    assert_eq!(header["site_ground_m"], 123.0);
}

#[tokio::test]
async fn live_coverage_reports_model_and_applies_terminal_clutter_override() {
    use repeaternet_rf_core::{
        DiffractionModel, calibration::step_allowance_db, p2108_terminal_correction_db,
    };

    let root = tempfile::tempdir().unwrap();
    let (app, state) = app(root.path(), "");
    let rf = crate::route_inputs::rf(&state.defaults["rf"]).unwrap();
    let cookie = cookie(A);
    let mut payload = live_request(1_000.0, 50.0);
    payload["site"]["kind"] = json!("router");
    payload["site"]["antenna_height_m"] = json!(3.0);
    payload["site"]["terminal_clutter"] = json!("p2108_dom");
    upload(
        &app,
        &cookie,
        "dtm",
        &live_fixture(250, 10.0, |_, _| 93.0),
        StatusCode::OK,
    )
    .await;
    let (header, _) = live_response(&app, &cookie, payload.clone()).await;
    let model = match rf.diffraction_model {
        DiffractionModel::Deygout3 => DiffractionModel::DeltaBullington,
        model => model,
    };
    assert_eq!(header["diffraction_model"], json!(model));
    assert_eq!(
        header["diffraction_model_configured"],
        json!(rf.diffraction_model)
    );
    assert_eq!(header["model_correction_db"], rf.model_correction_db);
    assert_eq!(header["step_allowance_db"], step_allowance_db(50.0, 10.0));
    assert_eq!(header["terminal_clutter_db"], 0.0);
    assert!(header["clutter_height_m"].is_null());

    upload(
        &app,
        &cookie,
        "dom",
        &live_fixture(250, 10.0, |_, _| 119.0),
        StatusCode::OK,
    )
    .await;
    let (header, with_clutter) = live_response(&app, &cookie, payload.clone()).await;
    assert_eq!(header["clutter_height_m"], 26.0);
    let expected = p2108_terminal_correction_db(rf.frequency_mhz, 3.0, 26.0);
    assert!(expected > 0.0);
    assert!((header["terminal_clutter_db"].as_f64().unwrap() - expected).abs() < 1e-9);

    payload["site"]["terminal_clutter"] = json!("none");
    let (header, without_clutter) = live_response(&app, &cookie, payload.clone()).await;
    assert!(header["clutter_height_m"].is_null());
    assert_eq!(header["terminal_clutter_db"], 0.0);
    assert!(with_clutter[20 * 41 + 20] < without_clutter[20 * 41 + 20]);

    payload["site"]["terminal_clutter"] = json!("p833");
    api(
        &app,
        &cookie,
        Method::POST,
        "/api/live-coverage",
        payload,
        StatusCode::BAD_REQUEST,
    )
    .await;
}

#[tokio::test]
async fn live_site_circles_estimate_and_prepare_without_a_route_snapshot() {
    use axum::extract::Query;
    use repeaternet_terrain::{Bounds, write_elevation_tiff};
    use std::collections::HashMap;

    let (upstream, server) = mock_upstream(Router::new().route(
        "/wcs",
        get(|Query(query): Query<HashMap<String, String>>| async move {
            let bounds: Vec<f64> = query["bbox"]
                .split(',')
                .map(|v| v.parse().unwrap())
                .collect();
            let width: u32 = query["width"].parse().unwrap();
            let height: u32 = query["height"].parse().unwrap();
            let mut tile = Vec::new();
            write_elevation_tiff(
                &mut tile,
                TerrainCrs::Etrs89Utm33,
                Bounds {
                    min_x: bounds[0],
                    min_y: bounds[1],
                    max_x: bounds[2],
                    max_y: bounds[3],
                },
                width,
                height,
                &vec![58.0; (width * height) as usize],
            )
            .unwrap();
            ([("content-type", "image/tiff")], tile)
        }),
    ))
    .await;
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("providers.json"),
        json!({
            "version": 1,
            "terrain": [{
                "id": "live-loopback", "name": "Test terrain", "kind": "wcs",
                "coverage": {"type":"global"}, "resolution_m": 25.0, "attribution": "Test fixture",
                "products": {"dtm": {
                    "endpoint": format!("{upstream}/wcs"), "coverage":"ground", "version":"1.0.0",
                    "format":"GeoTIFF", "native_crs":["EPSG:25833"]
                }}
            }]
        })
        .to_string(),
    )
    .unwrap();
    let (_, mut state) = app(root.path(), "");
    state.coverage_terrain_resolution = Some(25.0);
    state.terrain_limits.maximum_tile_area_km2 = 4.0;
    let app = router_with_state(state, "", true);
    let cookie = cookie(A);
    let site = |x| {
        let position = geographic(x, 6650000.0);
        json!({"latitude":position.latitude, "longitude":position.longitude})
    };
    let mut payload = json!({
        "live":{"sites":[site(499900.0),site(500250.0)],"radius_m":1000},
        "terrain":{"source":"live-loopback","fallback":false}
    });
    let estimate = "/api/coverage/terrain/estimate";
    let prepare = "/api/coverage/terrain/prepare";
    let post = |url, data, status| api(&app, &cookie, Method::POST, url, data, status);
    assert_eq!(
        post(estimate, payload.clone(), StatusCode::CONFLICT).await["detail"],
        "No terrain yet. Download terrain to see coverage."
    );
    upload(
        &app,
        &cookie,
        "dtm",
        &live_fixture(100, 30.0, |_, _| 42.0),
        StatusCode::OK,
    )
    .await;
    let covered = post(estimate, payload.clone(), StatusCode::OK).await;
    assert_eq!(covered["tile_count"], 0, "{covered}");
    assert_eq!(
        post(prepare, payload.clone(), StatusCode::OK).await["state"],
        "complete"
    );
    payload["live"]["sites"][1] = site(502000.0);
    let missing = post(estimate, payload.clone(), StatusCode::OK).await;
    assert!(missing["tile_count"].as_u64().unwrap() > 0, "{missing}");
    payload["registry_fingerprint"] = json!("outdated");
    post(prepare, payload.clone(), StatusCode::CONFLICT).await;
    payload["registry_fingerprint"] = missing["registry_fingerprint"].clone();
    let mut live = live_request(1_000.0, 50.0);
    let supplemental_site = site(502000.0);
    live["site"]["latitude"] = supplemental_site["latitude"].clone();
    live["site"]["longitude"] = supplemental_site["longitude"].clone();
    let unavailable = post("/api/live-coverage", live.clone(), StatusCode::CONFLICT).await;
    assert_eq!(unavailable["detail"], "No ground terrain at this site");
    let job = post(prepare, payload.clone(), StatusCode::OK).await;
    assert!(job["job_id"].is_string(), "{job}");
    let finished = coverage_terrain_terminal(&app, &cookie).await;
    assert_eq!(
        finished["coverage_terrain_job"]["state"], "complete",
        "{finished}"
    );
    assert_eq!(
        post(estimate, payload, StatusCode::OK).await["tile_count"],
        0
    );
    let (header, _) = live_response(&app, &cookie, live).await;
    assert_eq!(header["site_ground_m"], 58.0);
    server.abort();
}

#[tokio::test]
#[ignore = "timing benchmark with a 44 km synthetic elevation raster"]
async fn benchmark_live_coverage() {
    let tile = live_fixture(2200, 20.0, |x, y| {
        (300.0
            + 180.0 * (x / 2100.0).sin() * (y / 2900.0).cos()
            + 80.0 * ((x + y) / 950.0).sin()
            + 13.0 * ((x - 2.0 * y) / 250.0).cos()) as f32
    });
    for resolution in [30.0, 120.0] {
        let root = tempfile::tempdir().unwrap();
        let (app, _) = app(root.path(), "");
        let cookie = cookie(A);
        upload(&app, &cookie, "dtm", &tile, StatusCode::OK).await;
        let payload = live_request(20_000.0, resolution);
        let (cold, _) = live_response(&app, &cookie, payload.clone()).await;
        let mut warm = Vec::new();
        for _ in 0..5 {
            let (header, _) = live_response(&app, &cookie, payload.clone()).await;
            warm.push(header["elapsed_ms"].as_f64().unwrap());
        }
        warm.sort_by(f64::total_cmp);
        println!(
            "20 km @ {resolution} m: cold {:.1} ms, warm {:?} ms",
            cold["elapsed_ms"].as_f64().unwrap(),
            warm
        );
    }
}

#[tokio::test]
async fn kartverket_http_prepares_reuses_cache_and_keeps_terrain_after_cancel_and_failure() {
    use axum::{response::IntoResponse, routing::get};
    use std::sync::atomic::{AtomicUsize, Ordering};

    const WCS_TILE: &[u8] = include_bytes!("../../providers/tests/fixtures/tile-west.tif");
    const EAST_TILE: &[u8] = include_bytes!("../../providers/tests/fixtures/tile-east.tif");
    let calls = Arc::new(AtomicUsize::new(0));
    let requested = Arc::new(tokio::sync::Notify::new());
    let resume = Arc::new(tokio::sync::Notify::new());
    let (upstream, task) = mock_upstream(Router::new().route(
        "/wcs",
        get({
            let (calls, requested, resume) = (calls.clone(), requested.clone(), resume.clone());
            move |axum::extract::Query(query): axum::extract::Query<
                std::collections::HashMap<String, String>,
            >| {
                let (calls, requested, resume) = (calls.clone(), requested.clone(), resume.clone());
                async move {
                    if query.values().any(|value| value.starts_with("500100.0,")) {
                        return ([("content-type", "image/tiff")], EAST_TILE).into_response();
                    }
                    match calls.fetch_add(1, Ordering::SeqCst) {
                        0 => ([("content-type", "image/tiff")], WCS_TILE).into_response(),
                        1 => {
                            requested.notify_one();
                            resume.notified().await;
                            ([("content-type", "image/tiff")], WCS_TILE).into_response()
                        }
                        _ => StatusCode::FORBIDDEN.into_response(),
                    }
                }
            }
        }),
    ))
    .await;
    let root = tempfile::tempdir().unwrap();
    let (_, mut runtime) = app(root.path(), "");
    runtime.terrain_limits.maximum_tile_area_km2 = 0.01;
    runtime.coverage_terrain_resolution = Some(10.0);
    let wcs = |coverage| {
        json!({
            "endpoint": format!("{upstream}/wcs"),
            "coverage": coverage,
            "version": "1.0.0",
            "format": "GeoTIFF",
            "native_crs": ["EPSG:25833"]
        })
    };
    std::fs::write(
        root.path().join("providers.json"),
        json!({
            "version": 1,
            "terrain": [{
                "id": "kartverket-loopback", "name": "Kartverket loopback", "kind": "wcs",
                "coverage": {"type":"global"}, "resolution_m": 1.0, "attribution": "Kartverket",
                "products": {"dtm": wcs("ground"), "dom": wcs("surface")}
            }]
        })
        .to_string(),
    )
    .unwrap();
    let app = router_with_state(runtime.clone(), "", true);
    let cookie = cookie(A);
    let a = geographic(500025.0, 6650050.0);
    let b = geographic(500075.0, 6650050.0);
    let mut body = json!({
        "a": [a.latitude,a.longitude], "b": [b.latitude,b.longitude],
        "candidates": {"corridor_width_m": 10},
        "terrain": {"requested_resolution_m": 10, "include_dom": false, "source": "kartverket-loopback", "fallback": false},
    });
    let post = |url, data| api(&app, &cookie, Method::POST, url, data, StatusCode::OK);
    let estimate = post("/api/terrain/estimate", body.clone()).await;
    assert_eq!(estimate["tile_count"], 1);
    assert_eq!(estimate["cache_hits"], 0);
    assert!(estimate["estimated_uncached_raw_mib"].as_f64().unwrap() > 0.0);
    let job = post("/api/terrain/prepare", body.clone()).await;
    let finished = terminal(&app, &cookie).await;
    assert_eq!(finished["job"]["state"], "terrain_ready", "{finished}");
    assert_eq!(finished["job"]["job_id"], job["job_id"]);
    assert_eq!(finished["terrain"]["dtm"].as_array().unwrap().len(), 1);
    assert_eq!(
        post("/api/terrain/coverage?route_only=true", body.clone()).await["covered"],
        true
    );

    assert!(root.path().join("shared-cache/composed").is_dir());
    post("/api/projects", json!({"name": "Same corridor"})).await;
    let estimate = post("/api/terrain/estimate", body.clone()).await;
    assert_eq!(estimate["cache_hits"], estimate["cache_total"]);
    assert_eq!(estimate["cache_hits"], 1);
    assert_eq!(estimate["estimated_uncached_raw_mib"], 0.0);
    post("/api/terrain/prepare", body.clone()).await;
    let finished = terminal(&app, &cookie).await;
    assert_eq!(finished["job"]["state"], "terrain_ready");
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    post("/api/optimize", body.clone()).await;
    assert_eq!(terminal(&app, &cookie).await["job"]["state"], "complete");
    let result = api(
        &app,
        &cookie,
        Method::GET,
        "/api/result",
        Value::Null,
        StatusCode::OK,
    )
    .await;
    let sw = geographic(500120.0, 6650020.0);
    let ne = geographic(500180.0, 6650080.0);
    let extension = json!({
        "snapshot_version": result["snapshot_version"],
        "alternative_id": result["active_alternative_id"],
        "map_bounds": [sw.latitude, sw.longitude, ne.latitude, ne.longitude],
        "settings": {"area_mode":"view", "include_endpoints":true},
    });
    let estimate = post("/api/coverage/terrain/estimate", extension.clone()).await;
    assert_eq!(estimate["tile_count"], 1, "{estimate}");
    let worker = runtime.scheduler.reserve_worker().unwrap();
    let queued = post("/api/coverage/terrain/prepare", extension.clone()).await;
    assert_eq!(queued["state"], "queued");
    let cancel = format!(
        "/api/coverage/terrain/{}/cancel",
        queued["job_id"].as_str().unwrap()
    );
    post(&cancel, Value::Null).await;
    assert_eq!(
        coverage_terrain_terminal(&app, &cookie).await["coverage_terrain_job"]["state"],
        "cancelled"
    );
    drop(worker);

    post("/api/coverage/terrain/prepare", extension.clone()).await;
    let extended = coverage_terrain_terminal(&app, &cookie).await;
    assert_eq!(
        extended["coverage_terrain_job"]["state"], "complete",
        "{extended}"
    );
    assert_eq!(
        extended["coverage_terrain"]["dtm"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        post("/api/coverage/terrain/estimate", extension).await["tile_count"],
        0
    );
    assert_eq!(
        api(
            &app,
            &cookie,
            Method::GET,
            "/api/result",
            Value::Null,
            StatusCode::OK
        )
        .await,
        result
    );

    let worker = runtime.scheduler.reserve_worker().unwrap();
    let queued = post("/api/terrain/prepare", body.clone()).await;
    assert_eq!(queued["state"], "queued");
    post("/api/cancel", json!({"job_id": queued["job_id"]})).await;
    assert_eq!(terminal(&app, &cookie).await["job"]["state"], "cancelled");
    drop(worker);

    body["terrain"]["include_dom"] = json!(true);
    let job = post("/api/terrain/prepare", body.clone()).await;
    tokio::time::timeout(Duration::from_secs(5), requested.notified())
        .await
        .unwrap();
    let during_download = api(
        &app,
        &cookie,
        Method::GET,
        "/api/state",
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert_eq!(
        during_download["job"]["stage"],
        "DOM tile 1/1 · Kartverket loopback"
    );
    post("/api/cancel", json!({"job_id": job["job_id"]})).await;
    resume.notify_one();
    let cancelled = terminal(&app, &cookie).await;
    assert_eq!(cancelled["job"]["state"], "cancelled", "{cancelled}");
    assert_eq!(cancelled["terrain"], finished["terrain"]);

    post("/api/terrain/prepare", body.clone()).await;
    let failed = terminal(&app, &cookie).await;
    assert_eq!(failed["job"]["state"], "failed", "{failed}");
    assert!(
        failed["job"]["stage"]
            .as_str()
            .unwrap()
            .contains("Kartverket")
    );
    assert_eq!(failed["terrain"], finished["terrain"]);
    assert_eq!(failed["coverage_terrain"], extended["coverage_terrain"]);
    assert_eq!(post("/api/terrain/estimate", body).await["cache_hits"], 1);
    task.abort();
}

#[tokio::test]
async fn meshcore_records_can_be_routed_with_overpass_overlay() {
    use axum::{
        Json,
        response::IntoResponse,
        routing::{get, post},
    };
    let location = geographic(500170.0, 6650850.0);
    let left = geographic(500020.0, 6650850.0);
    let right = geographic(500310.0, 6650850.0);
    let roads = json!({"elements":[{"type":"way","id":1,"geometry":[
        {"lat":left.latitude,"lon":left.longitude},{"lat":right.latitude,"lon":right.longitude}
    ]}]});
    let (endpoint, task) = mock_upstream(
        Router::new()
            .route(
                "/api/nodes",
                get(move || async move {
                    Json(json!({"total":1,"nodes":[{
                        "public_key":"abcdef123456", "role":"repeater", "name":"Hill test",
                        "lat": location.latitude, "lon":location.longitude
                    }]}))
                }),
            )
            .route(
                "/api/interpreter",
                post(move |body: String| {
                    let roads = roads.clone();
                    async move {
                        assert!(body.contains("highway"));
                        Json(roads)
                    }
                }),
            )
            .route(
                "/failed/api/nodes",
                get(|| async { StatusCode::BAD_REQUEST.into_response() }),
            ),
    )
    .await;
    let root = tempfile::tempdir().unwrap();
    let (_, mut runtime) = app(root.path(), "");
    runtime.corescope_endpoint = endpoint.clone();
    runtime.overpass_endpoint = Some(format!("{endpoint}/api/interpreter"));
    let router = router_with_state(runtime.clone(), "", true);
    let cookie = cookie(A);
    upload(&router, &cookie, "dtm", TILE, StatusCode::OK).await;
    let mut inputs = plan();
    inputs["manual_routers"] = json!([]);
    let imported = api(
        &router,
        &cookie,
        Method::POST,
        "/api/meshcore",
        inputs.clone(),
        StatusCode::OK,
    )
    .await;
    assert_eq!(imported["count"], 1);
    assert_eq!(imported["routers"][0]["name"], "Hill test");
    assert_eq!(imported["routers"][0]["provenance"], "CoreScope");
    let mut repeater = imported["routers"][0].clone();
    repeater["policy"] = json!("required");
    inputs["known_routers"] = json!([repeater]);
    inputs["candidates"]["road_access_enabled"] = json!(true);
    inputs["candidates"]["maximum_road_distance_m"] = json!(25);
    api(
        &router,
        &cookie,
        Method::POST,
        "/api/optimize",
        inputs.clone(),
        StatusCode::OK,
    )
    .await;
    let finished = terminal(&router, &cookie).await;
    assert_eq!(finished["job"]["state"], "complete", "{finished}");
    let result = api(
        &router,
        &cookie,
        Method::GET,
        "/api/result",
        Value::Null,
        StatusCode::OK,
    )
    .await;
    assert!(
        result["road_polylines"]
            .as_array()
            .is_some_and(|lines| !lines.is_empty()),
        "{result}"
    );
    let active = result["alternatives"]
        .as_array()
        .unwrap()
        .iter()
        .find(|alt| alt["selected"] == true)
        .unwrap();
    assert_eq!(active["existing_router_count"], 1);
    assert!(
        active["nearest_road_distance_m"]
            .as_object()
            .unwrap()
            .values()
            .any(|distance| distance.as_f64().is_some_and(|d| d < 1.0))
    );
    assert_eq!(
        finished["projects"]["previous_result"]["road_polylines"],
        result["road_polylines"]
    );

    runtime.corescope_endpoint = format!("{endpoint}/failed");
    let bad = router_with_state(runtime, "", true);
    let problem = api(
        &bad,
        &cookie,
        Method::POST,
        "/api/meshcore",
        inputs,
        StatusCode::BAD_GATEWAY,
    )
    .await;
    assert!(problem["detail"].as_str().unwrap().contains("CoreScope"));
    task.abort();
}

#[test]
fn legacy_plans_without_obstacle_surface_use_the_calibrated_model() {
    let defaults = crate::catalog::dashboard_defaults();
    let legacy = json!({"rf": {"diffraction_model": "bullington"}});
    let rf = crate::route_inputs::merged(&defaults, &legacy, "rf").unwrap();
    assert_eq!(rf["diffraction_model"], defaults["rf"]["diffraction_model"]);
    let chosen = json!({"rf": {"diffraction_model": "bullington", "obstacle_surface": "ground"}});
    let rf = crate::route_inputs::merged(&defaults, &chosen, "rf").unwrap();
    assert_eq!(rf["diffraction_model"], "bullington");
}
