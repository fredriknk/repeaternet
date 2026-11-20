use std::{fs, path::Path};

use axum::{
    body::to_bytes,
    http::{HeaderMap, header},
};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::*;

const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const FIXTURE_ID: &str = "00000000000000000002";

fn open(root: &Path) -> (Router, ProjectStore) {
    let store = ProjectStore::new(root.join("projects.sqlite3"), root).unwrap();
    (router(store.clone(), "", true, None), store)
}

async fn send(
    app: &Router,
    key: &str,
    method: Method,
    path: &str,
    data: Option<Value>,
) -> (StatusCode, HeaderMap, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, "127.0.0.1:8000")
        .header(header::COOKIE, format!("planner_workspace={key}"));
    let body = data.map_or_else(Body::empty, |data| {
        Body::from(serde_json::to_vec(&data).unwrap())
    });
    request = request.header(header::CONTENT_TYPE, "application/json");
    let response = app
        .clone()
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap();
    let (parts, body) = response.into_parts();
    let body = to_bytes(body, 16 * 1024 * 1024).await.unwrap();
    let value = serde_json::from_slice(&body)
        .unwrap_or_else(|error| panic!("{path}: {error}: {}", String::from_utf8_lossy(&body)));
    (parts.status, parts.headers, value)
}

async fn expect(
    app: &Router,
    key: &str,
    method: Method,
    path: &str,
    data: Option<Value>,
    status: StatusCode,
) -> Value {
    let (actual, _, payload) = send(app, key, method, path, data).await;
    assert_eq!(actual, status, "{path}: {payload}");
    payload
}

async fn state(app: &Router, key: &str) -> Value {
    expect(app, key, Method::GET, "/api/state", None, StatusCode::OK).await
}

#[tokio::test]
async fn project_lifecycle_preserves_assets_and_revisions() {
    let root = tempfile::tempdir().unwrap();
    let (app, store) = open(root.path());
    let initial = state(&app, A).await;
    let unnamed = initial["projects"]["active_id"].as_str().unwrap();
    assert_eq!(initial["job"]["input_revision"], 0);
    assert_eq!(initial["projects"]["active_name"], "Untitled plan");
    expect(
        &app,
        A,
        Method::POST,
        "/api/projects/recover-previous",
        None,
        StatusCode::NOT_FOUND,
    )
    .await;
    expect(
        &app,
        A,
        Method::GET,
        "/api/export/project",
        None,
        StatusCode::NOT_FOUND,
    )
    .await;

    let first = json!({"a": [59, 10], "rf": {}, "candidates": {}, "unknown": {"note": "Tromsø", "optional": null}});
    let changed = json!({"a": [60, 11], "coverage": {"cell_size_m": 250}, "future": true});
    let created = expect(
        &app,
        A,
        Method::POST,
        "/api/projects",
        Some(json!({"name": "  Survey  ", "plan": first})),
        StatusCode::OK,
    )
    .await;
    let id = created["id"].as_str().unwrap();
    assert_eq!(created["name"], "Survey");
    assert_eq!(created["plan"], first);
    let path = store
        .project_path(A, &store.get(A, id).unwrap().unwrap())
        .unwrap();
    fs::create_dir_all(path.join("dtm")).unwrap();
    fs::write(path.join("dtm/ground.tif"), b"terrain bytes").unwrap();
    fs::create_dir_all(path.join("coverage-terrain-generations/generation-1/dom")).unwrap();
    fs::write(
        path.join("coverage-terrain-generations/generation-1/dom/extra.tif"),
        b"supplemental",
    )
    .unwrap();
    let error = expect(
        &app,
        A,
        Method::POST,
        "/api/projects/autosave",
        Some(json!({"project_id": unnamed, "plan": changed})),
        StatusCode::CONFLICT,
    )
    .await;
    assert_eq!(
        error["detail"],
        "Autosave belongs to a project that is no longer active"
    );
    let saved = expect(
        &app,
        A,
        Method::POST,
        "/api/projects/autosave",
        Some(json!({"project_id": id, "plan": changed})),
        StatusCode::OK,
    )
    .await;
    assert_eq!(saved["project_id"], id);
    assert_eq!(saved["has_previous_revision"], true);
    let saved_state = state(&app, A).await;
    assert_eq!(saved_state["coverage_settings"]["cell_size_m"], 250);
    assert_eq!(saved_state["terrain"]["dtm"], json!(["ground.tif"]));
    assert_eq!(saved_state["coverage_terrain"]["dom"], json!(["extra.tif"]));
    assert!(
        saved_state["projects"]["storage_usage_bytes"]
            .as_u64()
            .unwrap()
            > 22
    );
    let (status, headers, exported) = send(&app, A, Method::GET, "/api/export/project", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers[header::CONTENT_DISPOSITION],
        "attachment; filename=\"route.webplan.json\""
    );
    assert_eq!(exported, changed);
    for plan in [&first, &changed] {
        let res = expect(
            &app,
            A,
            Method::POST,
            "/api/projects/recover-previous",
            None,
            StatusCode::OK,
        )
        .await;
        assert_eq!(&res["plan"], plan);
    }

    let duplicate = expect(
        &app,
        A,
        Method::POST,
        &format!("/api/projects/{id}/duplicate"),
        Some(json!({"name":"Copy"})),
        StatusCode::OK,
    )
    .await;
    let copy = duplicate["id"].as_str().unwrap();
    assert_ne!(copy, id);
    assert_eq!(duplicate["plan"], changed);
    let copy_path = store
        .project_path(A, &store.get(A, copy).unwrap().unwrap())
        .unwrap();
    assert_eq!(
        fs::read(copy_path.join("dtm/ground.tif")).unwrap(),
        b"terrain bytes"
    );
    assert_eq!(
        fs::read(copy_path.join("coverage-terrain-generations/generation-1/dom/extra.tif"))
            .unwrap(),
        b"supplemental"
    );
    assert!(store.get(A, copy).unwrap().unwrap().previous_plan.is_none());
    expect(
        &app,
        A,
        Method::POST,
        &format!("/api/projects/{copy}/archive"),
        None,
        StatusCode::CONFLICT,
    )
    .await;
    expect(
        &app,
        A,
        Method::DELETE,
        &format!("/api/projects/{copy}"),
        None,
        StatusCode::CONFLICT,
    )
    .await;
    let rename = expect(
        &app,
        A,
        Method::PATCH,
        &format!("/api/projects/{copy}"),
        Some(json!({"name":"  Saved copy  "})),
        StatusCode::OK,
    )
    .await;
    assert_eq!(rename, json!({"id":copy,"name":"Saved copy"}));

    expect(
        &app,
        A,
        Method::POST,
        &format!("/api/projects/{id}/activate"),
        None,
        StatusCode::OK,
    )
    .await;
    assert_eq!(
        expect(
            &app,
            A,
            Method::POST,
            &format!("/api/projects/{copy}/archive"),
            None,
            StatusCode::OK
        )
        .await,
        json!({"ok":true})
    );
    assert!(copy_path.is_dir());
    assert_eq!(
        state(&app, A).await["projects"]["archived_items"][0]["id"],
        copy
    );
    expect(
        &app,
        A,
        Method::POST,
        &format!("/api/projects/{copy}/activate"),
        None,
        StatusCode::NOT_FOUND,
    )
    .await;
    expect(
        &app,
        A,
        Method::POST,
        "/api/projects",
        Some(json!({"name":"Saved copy"})),
        StatusCode::CONFLICT,
    )
    .await;
    expect(
        &app,
        A,
        Method::POST,
        &format!("/api/projects/{copy}/restore"),
        Some(json!({"name":"Restored"})),
        StatusCode::OK,
    )
    .await;
    assert_eq!(state(&app, A).await["projects"]["active_id"], copy);
    expect(
        &app,
        A,
        Method::POST,
        &format!("/api/projects/{id}/activate"),
        None,
        StatusCode::OK,
    )
    .await;
    expect(
        &app,
        A,
        Method::DELETE,
        &format!("/api/projects/{copy}"),
        None,
        StatusCode::OK,
    )
    .await;
    assert!(!copy_path.exists());
    expect(
        &app,
        A,
        Method::DELETE,
        &format!("/api/projects/{copy}"),
        None,
        StatusCode::NOT_FOUND,
    )
    .await;

    store.set_run_state(A, id, "running").unwrap();
    drop(app);
    let (restarted, _) = open(root.path());
    let restored = state(&restarted, A).await;
    assert_eq!(restored["projects"]["active_id"], id);
    assert_eq!(restored["plan"], changed);
    assert_eq!(restored["job"]["state"], "interrupted");
    assert_eq!(restored["job"]["input_revision"], 1);
    assert_eq!(restored["job"]["snapshot_version"], 0);
    assert!(restored["job"]["job_id"].is_null());
    assert_eq!(restored["projects"]["has_previous_revision"], true);
    expect(
        &restarted,
        A,
        Method::GET,
        "/api/result",
        None,
        StatusCode::NOT_FOUND,
    )
    .await;
}

#[tokio::test]
async fn python_catalog_is_opened_in_place_with_workspace_scoping() {
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("projects.sqlite3"),
        include_bytes!("../../project-store/tests/fixtures/project_catalog_v1.sqlite3"),
    )
    .unwrap();
    let (app, store) = open(root.path());
    let mine = state(&app, A).await;
    let other = state(&app, B).await;
    assert_eq!(mine["projects"]["active_id"], FIXTURE_ID);
    assert_eq!(mine["projects"]["active_name"], "Nordlys");
    assert_eq!(mine["projects"]["archived_items"][0]["name"], "Old plan");
    assert_eq!(mine["plan"]["unknown"]["text"], "Tromsø 🛰");
    assert_eq!(
        mine["plan"]["unknown"]["large"].to_string(),
        "92233720368547758091234"
    );
    assert_eq!(mine["job"]["state"], "interrupted");
    assert_eq!(mine["projects"]["previous_result"]["snapshot_version"], 7);
    assert_eq!(mine["projects"]["previous_result"]["stale"], true);
    assert_eq!(other["plan"]["b"], json!([63, 13]));
    assert_ne!(
        other["projects"]["active_id"],
        mine["projects"]["active_id"]
    );
    assert_eq!(
        store.get(A, FIXTURE_ID).unwrap().unwrap().run_state,
        "interrupted"
    );
    for (name, body) in [
        ("activate", None),
        ("archive", None),
        ("duplicate", Some(json!({"name":"Foreign"}))),
        ("restore", Some(json!({"name":"Foreign"}))),
    ] {
        expect(
            &app,
            B,
            Method::POST,
            &format!("/api/projects/{FIXTURE_ID}/{name}"),
            body,
            StatusCode::NOT_FOUND,
        )
        .await;
    }
    for (method, body) in [
        (Method::PATCH, Some(json!({"name":"Foreign"}))),
        (Method::DELETE, None),
    ] {
        expect(
            &app,
            B,
            method,
            &format!("/api/projects/{FIXTURE_ID}"),
            body,
            StatusCode::NOT_FOUND,
        )
        .await;
    }
    expect(
        &app,
        B,
        Method::POST,
        "/api/projects/autosave",
        Some(json!({"project_id":FIXTURE_ID,"plan":{}})),
        StatusCode::CONFLICT,
    )
    .await;
    assert_eq!(state(&app, A).await["plan"], mine["plan"]);
    let restored = expect(
        &app,
        A,
        Method::POST,
        "/api/projects/recover-previous",
        None,
        StatusCode::OK,
    )
    .await;
    assert_eq!(restored["plan"]["a"], json!([60, 10]));
    assert!(state(&app, A).await["projects"]["previous_result"].is_null());
}

#[tokio::test]
async fn python_fingerprints_distinguish_coverage_edits_from_route_and_terrain_changes() {
    let root = tempfile::tempdir().unwrap();
    let (app, store) = open(root.path());
    let plan = json!({"a":[59.0,10],"text":"Tromsø 🛰","p":1e-7,"q":1e16,"negative_zero":-0.0});
    let mut plan = plan;
    plan["integer"] = serde_json::from_str("92233720368547758091234").unwrap();
    let project = store.create(A, "Summary", &plan).unwrap();
    let directory = store.project_path(A, &project).unwrap();
    fs::create_dir(directory.join("dtm")).unwrap();
    let ground = directory.join("dtm/ground.tif");
    fs::write(&ground, b"ground").unwrap();
    let timestamp = std::time::UNIX_EPOCH + std::time::Duration::new(1_700_000_000, 123_456_700);
    fs::File::options()
        .write(true)
        .open(&ground)
        .unwrap()
        .set_modified(timestamp)
        .unwrap();
    let summary = json!({"router_count":2,"plan_fingerprint":"26a56bcac0dfce51720a44f89442e21cdf011a9f562e42119d4be70231a6759d",
        "terrain_fingerprint":[["dtm/ground.tif", 6, 1_700_000_000_123_456_700_i64]]});
    store
        .save_result_summary(A, &project.id, Some(&summary))
        .unwrap();
    let read = state(&app, A).await;
    assert_eq!(read["projects"]["previous_result"]["stale"], false);
    assert!(read["job"]["job_id"].is_null());
    expect(
        &app,
        A,
        Method::GET,
        "/api/result",
        None,
        StatusCode::NOT_FOUND,
    )
    .await;

    plan["coverage"] = json!({"cell_size_m": 1234});
    let target = json!({"type":"FeatureCollection","features":[{"type":"Feature","geometry":{"type":"Point","coordinates":[10,59]},"properties":{"name":"  Cabin  "}}]});
    plan["coverage_targets"] = target;
    expect(
        &app,
        A,
        Method::POST,
        "/api/projects/autosave",
        Some(json!({"plan":plan})),
        StatusCode::OK,
    )
    .await;
    let read = state(&app, A).await;
    assert_eq!(read["projects"]["previous_result"]["stale"], false);
    assert_eq!(
        read["plan"]["coverage_targets"]["features"][0]["properties"]["name"],
        "Cabin"
    );
    assert!(
        read["plan"]["coverage_targets"]["features"][0]["id"]
            .as_str()
            .unwrap()
            .starts_with("T-")
    );
    fs::write(&ground, b"edited terrain").unwrap();
    assert_eq!(
        state(&app, A).await["projects"]["previous_result"]["stale"],
        true
    );
    plan["a"] = json!([62, 11]);
    expect(
        &app,
        A,
        Method::POST,
        "/api/projects/autosave",
        Some(json!({"plan":plan})),
        StatusCode::OK,
    )
    .await;
    assert!(state(&app, A).await["projects"]["previous_result"].is_null());
    assert!(
        store
            .get(A, &project.id)
            .unwrap()
            .unwrap()
            .result_summary
            .is_none()
    );
}

#[tokio::test]
async fn historical_coverage_is_project_scoped_and_never_live_after_restart() {
    let root = tempfile::tempdir().unwrap();
    let (app, store) = open(root.path());
    let project = store.create(A, "Coverage", &json!({})).unwrap();
    let directory = store.project_path(A, &project).unwrap();
    let job = json!({"project_id": project.id, "job_id": "44444444444444444444444444444444", "state":"running",
        "created_at": 1700000000, "total": 12, "terrain_fingerprint": []});
    let latest = directory.join("coverage-job.json");
    fs::write(&latest, serde_json::to_vec(&job).unwrap()).unwrap();
    let report_dir = directory.join("coverage-target-reports");
    fs::create_dir(&report_dir).unwrap();
    let report = json!({"project_id":project.id,"report_id":"55555555555555555555555555555555","coverage_job_id":"44444444444444444444444444444444",
        "targets":[{"state":"pass"},{"state":"fail"},{"state":"unknown"},{"state":"unresolved"}]});
    fs::write(
        report_dir.join("44444444444444444444444444444444-55555555555555555555555555555555.json"),
        serde_json::to_vec(&report).unwrap(),
    )
    .unwrap();

    let restored = state(&app, A).await;
    assert_eq!(restored["coverage_job"]["state"], "interrupted");
    let runs = expect(
        &app,
        A,
        Method::GET,
        "/api/coverage/jobs",
        None,
        StatusCode::OK,
    )
    .await;
    assert_eq!(
        runs["jobs"][0]["job_id"],
        "44444444444444444444444444444444"
    );
    assert_eq!(runs["jobs"][0]["stale"], true);
    assert_eq!(runs["jobs"][0]["route_current"], false);
    assert_eq!(runs["jobs"][0]["terrain_current"], true);
    assert_eq!(runs["jobs"][0]["requested_cells"], 12);
    let detail = expect(
        &app,
        A,
        Method::GET,
        "/api/coverage/jobs/44444444444444444444444444444444",
        None,
        StatusCode::OK,
    )
    .await;
    assert_eq!(detail["state"], "interrupted");
    let reports = expect(
        &app,
        A,
        Method::GET,
        "/api/coverage/target-reports",
        None,
        StatusCode::OK,
    )
    .await;
    assert_eq!(reports["reports"][0]["target_count"], 4);
    assert_eq!(reports["reports"][0]["unresolved_count"], 1);
    assert_eq!(
        expect(
            &app,
            B,
            Method::GET,
            "/api/coverage/jobs",
            None,
            StatusCode::OK
        )
        .await,
        json!({"jobs":[]})
    );
    expect(
        &app,
        B,
        Method::GET,
        "/api/coverage/jobs/44444444444444444444444444444444",
        None,
        StatusCode::NOT_FOUND,
    )
    .await;
    expect(
        &app,
        A,
        Method::POST,
        "/api/projects/autosave",
        Some(json!({"plan":{"a":[59,10]}})),
        StatusCode::OK,
    )
    .await;
    assert_eq!(
        serde_json::from_slice::<Value>(&fs::read(latest).unwrap()).unwrap()["state"],
        "interrupted"
    );
}

#[tokio::test]
async fn runtime_guards_share_the_workspace_lock_and_autosave_cancels_inspection() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    let root = tempfile::tempdir().unwrap();
    let (_, store) = open(root.path());
    let app_state = AppState::new(store);
    let app = api::router(app_state.clone());
    let initial = state(&app, A).await;
    let active = initial["projects"]["active_id"].as_str().unwrap();
    let session = app_state.session(A);
    session.lock().await.as_mut().unwrap().route_job["state"] = json!("running");
    expect(
        &app,
        A,
        Method::POST,
        "/api/projects",
        Some(json!({"name":"Blocked"})),
        StatusCode::CONFLICT,
    )
    .await;
    expect(
        &app,
        A,
        Method::POST,
        "/api/projects/autosave",
        Some(json!({"plan": {"a":[59,10]}})),
        StatusCode::CONFLICT,
    )
    .await;
    let cancel = Arc::new(AtomicBool::new(false));
    {
        let mut guard = session.lock().await;
        let session = guard.as_mut().unwrap();
        session.route_job["state"] = json!("idle");
        session.inspection_job = Some(json!({"state":"running"}));
        session.inspection_cancel = Some(cancel.clone());
    }
    expect(
        &app,
        A,
        Method::POST,
        &format!("/api/projects/{active}/activate"),
        None,
        StatusCode::CONFLICT,
    )
    .await;
    expect(
        &app,
        A,
        Method::POST,
        "/api/projects/autosave",
        Some(json!({"plan": {"a":[59,10]}})),
        StatusCode::OK,
    )
    .await;
    assert!(cancel.load(Ordering::Relaxed));
}

#[tokio::test]
async fn target_import_validates_and_save_creates_project_revision() {
    let root = tempfile::tempdir().unwrap();
    let (app, _) = open(root.path());
    let collection = json!({"type":"FeatureCollection","features":[
        {"type":"Feature","geometry":{"type":"Polygon","coordinates":[[[10,59],[11,59],[10,60],[10.0,59.0]]]}, "properties": {"name":"  Field  "}}
    ]});
    let request = Request::builder().method(Method::POST).uri("/api/coverage/targets/import")
        .header(header::HOST, "127.0.0.1:8000")
        .header(header::COOKIE, format!("planner_workspace={A}"))
        .header(header::CONTENT_TYPE, "multipart/form-data; boundary=example")
        .body(Body::from(format!("--example\r\nContent-Disposition: form-data; name=\"file\"; filename=\"targets.geojson\"\r\n\r\n{collection}\r\n--example--\r\n"))).unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let normalized: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap()).unwrap();
    assert_eq!(normalized["features"][0]["properties"]["name"], "Field");
    assert!(state(&app, A).await["plan"].is_null());
    let saved = expect(
        &app,
        A,
        Method::PUT,
        "/api/coverage/targets",
        Some(normalized.clone()),
        StatusCode::OK,
    )
    .await;
    assert_eq!(saved["targets"], normalized);
    assert_eq!(saved["count"], 1);
    let mut invalid = collection;
    invalid["features"][0]["geometry"]["coordinates"] =
        json!([[[0, 0], [1, 1], [0, 1], [1, 0], [0, 0]]]);
    expect(
        &app,
        A,
        Method::PUT,
        "/api/coverage/targets",
        Some(invalid),
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    assert_eq!(state(&app, A).await["plan"]["coverage_targets"], normalized);
    let recovered = expect(
        &app,
        A,
        Method::POST,
        "/api/projects/recover-previous",
        None,
        StatusCode::OK,
    )
    .await;
    assert_eq!(recovered["plan"], json!({}));
}
