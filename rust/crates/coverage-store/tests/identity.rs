//! Compatibility fixtures captured from the Python implementation at `18b7cee`.

use repeaternet_coverage_store::*;
use serde_json::{json, Value};
use std::fs;
use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};
use tempfile::tempdir;

const PROJECT: &str = "bbbbbbbbbbbbbbbbbbbb";
const JOB: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const OTHER: &str = "cccccccccccccccccccccccccccccccc";
const NATIVE_MODEL: &str = "coverage-rust-v1";

fn goldens() -> Value {
    serde_json::from_str(include_str!("fixtures/python-goldens.json")).unwrap()
}

fn python_job() -> Value {
    let fixture: Value = serde_json::from_str(include_str!("fixtures/python-job.json")).unwrap();
    fixture["job"].clone()
}

fn python_terrain() -> LegacyIdentity {
    LegacyIdentity::new(
        PROJECT,
        vec![("dtm/local.tif".into(), 9214, 1_734_567_890_123_456_789)],
    )
}

#[test]
fn canonical_json_matches_every_python_golden() {
    let goldens = goldens();
    let cases = goldens["canonical"].as_array().unwrap();
    assert!(cases.len() > 3000);
    for case in cases {
        let input = case["input"].as_str().unwrap();
        let value: Value = serde_json::from_str(input).unwrap();
        assert_eq!(
            python_canonical_json(&value).as_deref(),
            case["canonical"].as_str(),
            "{input}"
        );
        assert_eq!(
            python_fingerprint(&value).as_deref(),
            case["sha256"].as_str(),
            "{input}"
        );
    }
    for input in goldens["unsupported"].as_array().unwrap() {
        let value: Value = serde_json::from_str(input.as_str().unwrap()).unwrap();
        assert_eq!(python_fingerprint(&value), None, "{input}");
    }
}

#[test]
fn route_settings_and_reuse_fingerprints_match_python() {
    let goldens = goldens();
    let route = &goldens["route"];
    let plan: Value = serde_json::from_str(route["plan_text"].as_str().unwrap()).unwrap();
    assert_eq!(
        python_route_fingerprint(&plan).as_deref(),
        route["route_fingerprint"].as_str()
    );
    assert_eq!(
        python_fingerprint(&plan).as_deref(),
        route["plan_fingerprint"].as_str()
    );
    assert_ne!(
        route_fingerprint(&plan).as_deref(),
        route["route_fingerprint"].as_str()
    );

    let settings = &goldens["settings"];
    for (text, digest) in [
        ("settings_text", "settings_fingerprint"),
        ("radio_text", "radio_fingerprint"),
    ] {
        let value: Value = serde_json::from_str(settings[text].as_str().unwrap()).unwrap();
        assert_eq!(
            python_fingerprint(&value).as_deref(),
            settings[digest].as_str()
        );
    }

    let reuse = &goldens["reuse"];
    let job: Value = serde_json::from_str(reuse["job_text"].as_str().unwrap()).unwrap();
    assert_eq!(
        python_reuse_fingerprint(&job).as_deref(),
        reuse["reuse_fingerprint"].as_str()
    );
    assert_eq!(job, python_job());
    assert_eq!(job["settings_fingerprint"], reuse["settings_fingerprint"]);
    assert_eq!(job["radio_fingerprint"], reuse["radio_fingerprint"]);
    let mut partial = job.clone();
    partial.as_object_mut().unwrap().remove("rows");
    assert_eq!(python_reuse_fingerprint(&partial), None);
    // Python reuse identity never authorizes a native reuse.
    assert_eq!(reuse_fingerprint(&job), None);
}

fn create(path: &Path, size: u64, mtime_ns: i128) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, vec![b'x'; size as usize]).unwrap();
    let nanos = Duration::from_nanos(mtime_ns.unsigned_abs() as u64);
    let time = if mtime_ns < 0 {
        UNIX_EPOCH - nanos
    } else {
        UNIX_EPOCH + nanos
    };
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(time)
        .unwrap();
}

#[test]
fn terrain_metadata_matches_python_nanoseconds_and_ordering() {
    let goldens = goldens();
    let terrain = &goldens["terrain"];
    let root = tempdir().unwrap();
    let mut coverage = Vec::new();
    let mut route = Vec::new();
    // Reverse the creation order: Python sorts the tuples itself.
    for file in terrain["files"].as_array().unwrap().iter().rev() {
        let relative = file[0].as_str().unwrap();
        let path = root.path().join(relative);
        create(
            &path,
            file[1].as_u64().unwrap(),
            file[2].as_i64().unwrap().into(),
        );
        let entry = python_terrain_entry(root.path(), &path).unwrap();
        if !relative.starts_with("coverage-terrain-generations/") {
            route.push(entry.clone());
        }
        coverage.push(entry);
    }
    let expected: Value =
        serde_json::from_str(terrain["coverage_terrain_text"].as_str().unwrap()).unwrap();
    assert_eq!(python_terrain_fingerprint(coverage.clone()), expected);
    assert_eq!(expected, terrain["coverage_terrain_fingerprint"]);
    assert_eq!(
        python_terrain_fingerprint(route),
        terrain["route_terrain_fingerprint"]
    );
    let text = python_canonical_json(&python_terrain_fingerprint(coverage)).unwrap();
    assert!(text.contains("[\"dom/b\\u00e5.tif\",17,1700000000000000000]"));
    assert!(text.contains(r#"["dtm/z.tif",0,-1234567800]"#));

    assert_eq!(
        python_mtime_ns(UNIX_EPOCH - Duration::from_nanos(100)),
        -100
    );
    for bad in [root.path().to_path_buf(), root.path().join("../x.tif")] {
        assert!(python_terrain_entry(root.path(), &bad).is_err());
    }
    assert!(python_terrain_entry(root.path(), Path::new("dtm/a.tif")).is_err());
}

fn write_legacy(root: &Path, id: &str, job: &Value, cells: &[u8], current: bool) {
    fs::create_dir_all(root.join("coverage-results")).unwrap();
    fs::create_dir_all(root.join("coverage-jobs")).unwrap();
    fs::write(root.join(format!("coverage-results/{id}.jsonl")), cells).unwrap();
    let mut job = job.clone();
    job["job_id"] = json!(id);
    job["result_file"] = json!(format!("coverage-results/{id}.jsonl"));
    // Python writes manifests with json.dumps(indent=2) and ASCII escapes.
    let text = serde_json::to_string_pretty(&job).unwrap();
    let path = if current {
        root.join("coverage-job.json")
    } else {
        root.join(format!("coverage-jobs/{id}.json"))
    };
    fs::write(path, text).unwrap();
}

const CELLS: &[u8] = include_bytes!("fixtures/python-cells.jsonl");

#[test]
fn legacy_runs_report_precise_reasons_and_are_never_current() {
    let root = tempdir().unwrap();
    let job = python_job();
    write_legacy(root.path(), JOB, &job, CELLS, true);
    let store = Store::open(root.path(), PROJECT).unwrap();
    let live = python_terrain();
    assert!(live.terrain_matches(&job));
    assert_eq!(live.stale_reason(&job), reason::LEGACY_ROUTE_NOT_CURRENT);
    let loaded = store.load_with_legacy(JOB, &live).unwrap();
    assert_eq!(loaded["stale"], true);
    assert_eq!(loaded["stale_reason"], reason::LEGACY_ROUTE_NOT_CURRENT);
    assert_eq!(
        store.current_with_legacy(&live).unwrap().unwrap()["stale_reason"],
        reason::LEGACY_ROUTE_NOT_CURRENT
    );
    let page = store
        .page_with_identities(JOB, 0, 2, None, Some(&live))
        .unwrap();
    assert_eq!(page.stored_cells, 2);
    assert_eq!(page.stale_reason, Some(reason::LEGACY_ROUTE_NOT_CURRENT));

    let changed = LegacyIdentity::new(PROJECT, vec![("dtm/local.tif".into(), 9214, 1)]);
    assert_eq!(changed.stale_reason(&job), reason::LEGACY_TERRAIN);
    let elsewhere = LegacyIdentity::from_fingerprint("x", live.terrain().clone());
    assert_eq!(elsewhere.stale_reason(&job), reason::LEGACY_PROJECT);
    let mut tampered = job.clone();
    tampered["settings"]["cell_size_m"] = json!(2.0);
    assert_eq!(live.stale_reason(&tampered), reason::LEGACY_EMBEDDED);
    let mut float_text = job.clone();
    // Same number, different JSON type: Python hashes 1 and 1.0 differently.
    float_text["radio_settings"]["frequency_mhz"] = json!(868);
    assert_eq!(live.stale_reason(&float_text), reason::LEGACY_EMBEDDED);
    let mut stale = job.clone();
    stale["stale"] = json!(true);
    assert_eq!(live.stale_reason(&stale), reason::PERSISTED_STALE);
    let mut older = job.clone();
    older["model_version"] = json!("coverage-v2");
    assert_eq!(live.stale_reason(&older), reason::UNSUPPORTED_FORMAT);
    assert_eq!(
        job_stale_reason(&older, None, Some(&live)),
        Some(reason::UNSUPPORTED_FORMAT)
    );

    // No native identity, even one built from the run's own values, adopts it.
    let mut native = job.clone();
    native["coverage_store_format"] = json!(FORMAT);
    assert!(CurrentIdentity::new(native.clone()).is_none());
    native.as_object_mut().unwrap().remove("model_version");
    let current = CurrentIdentity::new(native).unwrap();
    assert!(store.find_reusable(&job, &current).unwrap().is_none());
    assert_eq!(
        job_stale_reason(&job, Some(&current), Some(&live)),
        Some(reason::LEGACY_ROUTE_NOT_CURRENT)
    );
    let report = json!({"project_id":PROJECT,"coverage_job_id":JOB,
        "report_id":format!("{:032x}",1),"terrain_fingerprint":job["terrain_fingerprint"],
        "radio_fingerprint":job["radio_fingerprint"],"model_version":job["model_version"],
        "targets":[]});
    assert!(matches!(
        store.save_report(JOB, &report, &current),
        Err(Error::Conflict)
    ));
}

#[test]
fn legacy_comparison_requires_live_terrain_radio_and_complete_grids() {
    let root = tempdir().unwrap();
    let job = python_job();
    write_legacy(root.path(), JOB, &job, CELLS, false);
    write_legacy(root.path(), OTHER, &job, CELLS, true);
    let store = Store::open(root.path(), PROJECT).unwrap();
    let live = python_terrain();
    let (baseline, scenario) = store.legacy_comparison(JOB, OTHER, &live).unwrap();
    assert_eq!(baseline["stale_reason"], reason::LEGACY_ROUTE_NOT_CURRENT);
    assert_eq!(scenario["job_id"], OTHER);
    assert!(store.comparison(JOB, OTHER, live.terrain()).is_ok());

    let invalid = |result: Result<(Value, Value)>| match result {
        Err(Error::Invalid(reason)) => reason,
        other => panic!("{other:?}"),
    };
    let changed = LegacyIdentity::new(PROJECT, vec![]);
    assert_eq!(
        invalid(store.legacy_comparison(JOB, OTHER, &changed)),
        reason::COMPARISON_TERRAIN
    );
    let elsewhere = LegacyIdentity::from_fingerprint("x", live.terrain().clone());
    assert_eq!(
        invalid(store.legacy_comparison(JOB, OTHER, &elsewhere)),
        reason::COMPARISON_PROJECT
    );

    let mut radio = job.clone();
    radio["radio_settings"]["frequency_mhz"] = json!(869.0);
    radio["radio_fingerprint"] = json!(python_fingerprint(&radio["radio_settings"]).unwrap());
    write_legacy(root.path(), OTHER, &radio, CELLS, true);
    assert_eq!(
        invalid(store.legacy_comparison(JOB, OTHER, &live)),
        reason::COMPARISON_RADIO
    );

    let mut forged = job.clone();
    forged["settings"]["cell_size_m"] = json!(5.0);
    write_legacy(root.path(), OTHER, &forged, CELLS, true);
    assert_eq!(
        invalid(store.legacy_comparison(JOB, OTHER, &live)),
        reason::COMPARISON_EMBEDDED
    );

    let mut running = job.clone();
    running["state"] = json!("running");
    write_legacy(root.path(), OTHER, &running, CELLS, true);
    assert_eq!(
        invalid(store.legacy_comparison(JOB, OTHER, &live)),
        reason::INCOMPLETE
    );

    let half = CELLS.split_inclusive(|byte| *byte == b'\n').next().unwrap();
    write_legacy(root.path(), OTHER, &job, half, true);
    assert_eq!(
        invalid(store.legacy_comparison(JOB, OTHER, &live)),
        reason::COMPARISON_GRID
    );

    let mut corrupt = CELLS.to_vec();
    corrupt[1] = b'[';
    write_legacy(root.path(), OTHER, &job, &corrupt, true);
    assert!(matches!(
        store.legacy_comparison(JOB, OTHER, &live),
        Err(Error::Json(_))
    ));
}

fn native(job_id: &str) -> (Value, CurrentIdentity) {
    let fixture: Value = serde_json::from_str(include_str!("fixtures/python-job.json")).unwrap();
    let mut job = fixture["job"].clone();
    job["job_id"] = json!(job_id);
    job["route_fingerprint"] = json!(route_fingerprint(&fixture["plan"]).unwrap());
    job["settings_fingerprint"] = json!(fingerprint(&job["settings"]));
    job["radio_fingerprint"] = json!(fingerprint(&job["radio_settings"]));
    job["coverage_store_format"] = json!(FORMAT);
    job["model_version"] = json!(NATIVE_MODEL);
    job["state"] = json!("queued");
    let current = CurrentIdentity::new(job.clone()).unwrap();
    (job, current)
}

fn cell(index: u64) -> Value {
    json!({"index":index,"state":"covered","x":0.5,"y":0.5,
        "latitude":60.0,"longitude":10.0,"sources":[{"source_id":"R-coverage"}]})
}

#[test]
fn native_identity_reports_each_changed_field() {
    let (job, current) = native(JOB);
    assert_eq!(current.stale_reason(&job), None);
    for (key, value, why) in [
        ("project_id", json!("other"), reason::PROJECT),
        ("route_fingerprint", json!("0".repeat(64)), reason::ROUTE),
        ("alternative_id", json!("alt-2"), reason::ALTERNATIVE),
        ("snapshot_version", json!(5), reason::SNAPSHOT),
        ("terrain_fingerprint", json!([]), reason::TERRAIN),
        (
            "settings_fingerprint",
            json!("1".repeat(64)),
            reason::SETTINGS,
        ),
        ("radio_fingerprint", json!("2".repeat(64)), reason::RADIO),
        (
            "model_version",
            json!(PYTHON_MODEL_VERSION),
            reason::MODEL_VERSION,
        ),
        ("stale", json!(true), reason::PERSISTED_STALE),
        (
            "coverage_store_format",
            json!("other"),
            reason::UNSUPPORTED_FORMAT,
        ),
    ] {
        let mut changed = job.clone();
        changed[key] = value;
        assert_eq!(current.stale_reason(&changed), Some(why), "{key}");
    }
    let mut embedded = job.clone();
    embedded["settings"]["cell_size_m"] = json!(3.0);
    assert_eq!(current.stale_reason(&embedded), Some(reason::EMBEDDED));
    assert_eq!(
        current.route_stale_reason(&embedded),
        None,
        "route currency ignores settings"
    );
    assert_eq!(
        job_stale_reason(&job, None, None),
        Some(reason::CURRENT_UNAVAILABLE)
    );

    let mut pinned = job.clone();
    pinned["model_version"] = json!("coverage-rust-v2");
    let pinned = CurrentIdentity::new(pinned).unwrap();
    assert_eq!(pinned.stale_reason(&job), Some(reason::MODEL_VERSION));
    let mut python = job.clone();
    python["model_version"] = json!(PYTHON_MODEL_VERSION);
    assert!(CurrentIdentity::new(python.clone()).is_none());
    assert_eq!(reuse_fingerprint(&python), None);

    let root = tempdir().unwrap();
    let store = Store::open(root.path(), PROJECT).unwrap();
    assert!(matches!(store.start(python), Err(Error::Invalid(_))));
    assert!(!root.path().join("coverage-job.json").exists());
}

#[test]
fn native_comparison_and_reuse_stay_in_the_native_domain() {
    let root = tempdir().unwrap();
    let store = Store::open(root.path(), PROJECT).unwrap();
    let (baseline, current) = native(JOB);
    store.start(baseline.clone()).unwrap();
    store
        .finish(JOB, json!({"state":"complete"}), &[cell(0), cell(1)])
        .unwrap();
    let (scenario, _) = native(OTHER);
    store.start(scenario).unwrap();
    store
        .finish(OTHER, json!({"state":"complete"}), &[cell(0), cell(1)])
        .unwrap();
    let terrain = baseline["terrain_fingerprint"].clone();
    let (a, b) = store.comparison(JOB, OTHER, &terrain).unwrap();
    assert_eq!(
        (a["job_id"].as_str(), b["job_id"].as_str()),
        (Some(JOB), Some(OTHER))
    );
    assert!(matches!(
        store.comparison(JOB, OTHER, &json!([])),
        Err(Error::Invalid(reason::COMPARISON_TERRAIN))
    ));
    assert!(matches!(
        store.legacy_comparison(JOB, OTHER, &python_terrain()),
        Err(Error::Invalid(reason::DOMAIN))
    ));
    let live = LegacyIdentity::from_fingerprint(PROJECT, terrain.clone());
    assert!(matches!(
        store.legacy_comparison(JOB, OTHER, &live),
        Err(Error::Invalid(reason::DOMAIN))
    ));
    assert_eq!(
        store.find_reusable(&baseline, &current).unwrap().unwrap()["state"],
        "complete"
    );

    // A Python run with matching values next to a native one is not comparable.
    let legacy = python_job();
    write_legacy(root.path(), &"d".repeat(32), &legacy, CELLS, false);
    let mixed = store.comparison(JOB, &"d".repeat(32), &terrain);
    assert!(
        matches!(mixed, Err(Error::Invalid(reason::DOMAIN))),
        "{mixed:?}"
    );
    assert_eq!(
        comparison_mismatch(PROJECT, &legacy, &legacy, &legacy["terrain_fingerprint"]),
        None
    );
    let reused = store.find_reusable(&baseline, &current).unwrap().unwrap();
    assert_eq!(reused["model_version"], NATIVE_MODEL);
}

#[test]
fn canonical_json_ignores_whitespace_and_field_order() {
    let a: Value = serde_json::from_str(r#"{"b": [1.50, {"d": 1e2, "c": "é"}], "a": -0}"#).unwrap();
    let b: Value =
        serde_json::from_str("{\"a\":0,\"b\":[1.5,{\"c\":\"\\u00e9\",\"d\":100.0}]}").unwrap();
    assert_eq!(python_fingerprint(&a), python_fingerprint(&b));
    assert_eq!(
        python_canonical_json(&a).unwrap(),
        "{\"a\":0,\"b\":[1.5,{\"c\":\"\\u00e9\",\"d\":100.0}]}"
    );
}

#[test]
fn python_nonfinite_manifests_stay_readable_but_native_ones_stay_strict() {
    let goldens = goldens();
    let text = goldens["reuse"]["nonfinite_manifest_text"]
        .as_str()
        .unwrap();
    assert!(serde_json::from_str::<Value>(text).is_err());
    let root = tempdir().unwrap();
    fs::create_dir_all(root.path().join("coverage-results")).unwrap();
    fs::write(
        root.path().join(format!("coverage-results/{JOB}.jsonl")),
        CELLS,
    )
    .unwrap();
    fs::write(root.path().join("coverage-job.json"), text).unwrap();
    let store = Store::open(root.path(), PROJECT).unwrap();
    let job = store.load_with_legacy(JOB, &python_terrain()).unwrap();
    let link = &job["report_links"][0];
    assert_eq!(link["worst_margin_db"], Value::Null);
    assert_eq!(link["distance_m"], Value::Null);
    assert_eq!(link["note"], "NaN -Infinity \"Infinity\"");
    assert_eq!(job["stale_reason"], reason::LEGACY_ROUTE_NOT_CURRENT);
    assert_eq!(store.page(JOB, 0, 2, None).unwrap().stored_cells, 2);

    let (mut native, _) = native(JOB);
    native["result_file"] = json!(format!("coverage-results/{JOB}.jsonl"));
    let native = serde_json::to_string(&native)
        .unwrap()
        .replace(r#""done":0"#, r#""done":NaN"#)
        .replace(r#""rows":1"#, r#""rows":1,"probe":NaN"#);
    assert!(native.contains("NaN"));
    fs::write(root.path().join("coverage-job.json"), native).unwrap();
    assert!(matches!(store.load(JOB), Err(Error::Json(_))));
    assert!(matches!(store.current(), Err(Error::Json(_))));
}
