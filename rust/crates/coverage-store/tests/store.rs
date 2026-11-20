use repeaternet_coverage_store::*;
use serde_json::{json, Value};
use std::{fs, io::Write, path::Path};
use tempfile::tempdir;

const PROJECT: &str = "bbbbbbbbbbbbbbbbbbbb";
const JOB: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const OTHER: &str = "cccccccccccccccccccccccccccccccc";
const NATIVE_MODEL: &str = "coverage-rust-v1";

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/python-job.json")).unwrap()
}

fn cell(index: u64) -> Value {
    json!({"index":index,"state":"uncovered","x":0.5,"y":0.5,
        "latitude":60.0,"longitude":10.0,"sources":[]})
}

fn identity() -> (Value, CurrentIdentity) {
    let fixture = fixture();
    let mut job = fixture["job"].clone();
    job["route_fingerprint"] = json!(route_fingerprint(&fixture["plan"]).unwrap());
    job["settings_fingerprint"] = json!(fingerprint(&job["settings"]));
    job["radio_fingerprint"] = json!(fingerprint(&job["radio_settings"]));
    job["coverage_store_format"] = json!(FORMAT);
    job["model_version"] = json!(NATIVE_MODEL);
    job["state"] = json!("queued");
    let current = CurrentIdentity::new(job.clone()).unwrap();
    (job, current)
}

fn result(root: &Path) -> std::path::PathBuf {
    root.join(format!("coverage-results/{JOB}.jsonl"))
}

#[test]
fn python_golden_is_readable_and_stale_even_with_matching_values() {
    let root = tempdir().unwrap();
    let fixture = fixture();
    let mut job = fixture["job"].clone();
    fs::create_dir(root.path().join("coverage-results")).unwrap();
    fs::write(
        result(root.path()),
        include_bytes!("fixtures/python-cells.jsonl"),
    )
    .unwrap();
    let path = root.path().join("coverage-job.json");
    fs::write(&path, serde_json::to_vec(&job).unwrap()).unwrap();
    let store = Store::open(root.path(), PROJECT).unwrap();
    // A native identity built from the Python values must still not adopt the run.
    let mut fields = job.clone();
    fields.as_object_mut().unwrap().remove("model_version");
    let python_fields = CurrentIdentity::new(fields).unwrap();
    assert!(CurrentIdentity::new(job.clone()).is_none());
    assert_eq!(
        python_fields.stale_reason(&job),
        Some(reason::LEGACY_ROUTE_NOT_CURRENT)
    );
    assert_ne!(fingerprint(&fixture["plan"]), job["route_fingerprint"]);
    assert!(reuse_fingerprint(&job).is_none());
    assert!(store.find_reusable(&job, &python_fields).unwrap().is_none());
    let first = store.page(JOB, 0, 1, Some(&python_fields)).unwrap();
    assert!(first.stale);
    assert_eq!(
        first.stale_reason,
        Some(reason::LEGACY_IDENTITY_UNAVAILABLE)
    );
    assert_eq!(
        store.load(JOB).unwrap()["stale_reason"],
        reason::LEGACY_IDENTITY_UNAVAILABLE
    );
    assert_eq!(first.next_cursor, Some(1));
    let second = store.page(JOB, 1, 1, None).unwrap();
    assert_eq!(second.cells[0]["state"], "outside_area");
    assert_eq!(second.stored_cells, 2);
    job["state"] = json!("running");
    fs::write(path, serde_json::to_vec(&job).unwrap()).unwrap();
    assert_eq!(
        store.recover_interrupted().unwrap().unwrap()["state"],
        "interrupted"
    );
    assert_eq!(store.page(JOB, 0, 2, None).unwrap().stored_cells, 2);
}

#[test]
fn restart_retains_only_committed_chunks_and_archives_previous_run() {
    let root = tempdir().unwrap();
    let store = Store::open(root.path(), PROJECT).unwrap();
    let (job, current) = identity();
    store.start(job.clone()).unwrap();
    store
        .progress(JOB, json!({"state":"running","done":1}), &[cell(0)])
        .unwrap();
    let length = fs::metadata(result(root.path())).unwrap().len();
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(result(root.path()))
        .unwrap();
    file.write_all(b"torn chunk").unwrap();
    drop(file);
    assert_eq!(
        store.page(JOB, 0, 5, Some(&current)).unwrap().stored_cells,
        1
    );
    let restored = Store::open(root.path(), PROJECT).unwrap();
    assert_eq!(
        restored.recover_interrupted().unwrap().unwrap()["state"],
        "interrupted"
    );
    assert_eq!(fs::metadata(result(root.path())).unwrap().len(), length);
    assert!(restored.find_reusable(&job, &current).unwrap().is_none());
    let mut successor = job;
    successor["job_id"] = json!(OTHER);
    restored.start(successor).unwrap();
    assert_eq!(restored.load(JOB).unwrap()["state"], "interrupted");
    assert_eq!(restored.history().unwrap().len(), 2);
}

#[test]
fn reuse_requires_full_ordered_grid_matching_every_identity_field() {
    let root = tempdir().unwrap();
    let store = Store::open(root.path(), PROJECT).unwrap();
    let (job, current) = identity();
    store.start(job.clone()).unwrap();
    assert!(store
        .finish(JOB, json!({"state":"complete"}), &[cell(1)])
        .is_err());
    assert_eq!(store.page(JOB, 0, 2, None).unwrap().stored_cells, 0);
    let mut unfinished = cell(1);
    unfinished["state"] = json!("not_evaluated");
    assert!(store
        .finish(JOB, json!({"state":"complete"}), &[cell(0), unfinished])
        .is_err());
    store
        .finish(JOB, json!({"state":"complete"}), &[cell(0), cell(1)])
        .unwrap();
    assert_eq!(
        store.find_reusable(&job, &current).unwrap().unwrap()["job_id"],
        JOB
    );
    let fingerprint = reuse_fingerprint(&job).unwrap();
    for field in REUSE_FIELDS {
        let mut changed = job.clone();
        changed.as_object_mut().unwrap().remove(field);
        assert!(reuse_fingerprint(&changed).is_none(), "{field}");
    }
    let mut changed = job.clone();
    changed["source_height_overrides"] = json!({"R-coverage":1.0});
    assert_ne!(reuse_fingerprint(&changed).unwrap(), fingerprint);
    assert!(store.find_reusable(&changed, &current).unwrap().is_none());
    changed["snapshot_version"] = json!(5);
    assert!(!CurrentIdentity::new(changed).unwrap().matches(&job));
    let mut successor = job.clone();
    successor["job_id"] = json!(OTHER);
    store.start(successor).unwrap();
    assert_eq!(
        store.find_reusable(&job, &current).unwrap().unwrap()["job_id"],
        JOB
    );

    let path = result(root.path());
    let original = fs::read(&path).unwrap();
    fs::write(&path, &original[..original.len() - 1]).unwrap();
    assert!(store.page(JOB, 0, 2, None).is_err());
    assert!(store.find_reusable(&job, &current).unwrap().is_none());
    let mut corrupt = original;
    corrupt[0] = b'{';
    fs::write(path, corrupt).unwrap();
    assert!(store.find_reusable(&job, &current).unwrap().is_none());
}

#[test]
fn rejects_traversal_wrong_project_and_mislabeled_results() {
    let root = tempdir().unwrap();
    let store = Store::open(root.path(), PROJECT).unwrap();
    assert!(matches!(
        store.load("../coverage-job"),
        Err(Error::UnsafePath)
    ));
    let mut job = fixture()["job"].clone();
    let path = root.path().join("coverage-job.json");
    job["result_file"] = json!("../other.jsonl");
    fs::write(&path, serde_json::to_vec(&job).unwrap()).unwrap();
    assert!(matches!(
        store.page(JOB, 0, 1, None),
        Err(Error::UnsafePath)
    ));
    job["result_file"] = json!(format!("coverage-results/{OTHER}.jsonl"));
    fs::write(&path, serde_json::to_vec(&job).unwrap()).unwrap();
    assert!(matches!(
        store.page(JOB, 0, 1, None),
        Err(Error::UnsafePath)
    ));
    assert!(matches!(
        Store::open(root.path(), "another-project")
            .unwrap()
            .load(JOB),
        Err(Error::NotFound)
    ));
}

#[test]
fn directory_redirect_is_rejected() {
    let root = tempdir().unwrap();
    let outside = tempdir().unwrap();
    let link = root.path().join("coverage-results");
    #[cfg(windows)]
    {
        // Create a real NTFS junction without developer mode or elevation.
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(outside.path())
            .status()
            .unwrap();
        assert!(status.success());
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(outside.path(), &link).unwrap();
    let store = Store::open(root.path(), PROJECT).unwrap();
    assert!(matches!(store.start(identity().0), Err(Error::UnsafePath)));
    assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
}

#[test]
fn limits_results_exports_and_reports() {
    let root = tempdir().unwrap();
    let store = Store::open(root.path(), PROJECT).unwrap();
    let (job, current) = identity();
    store.start(job.clone()).unwrap();
    store
        .finish(JOB, json!({"state":"complete"}), &[cell(0), cell(1)])
        .unwrap();
    let mut report = json!({"project_id":PROJECT, "coverage_job_id":JOB,
        "report_id":format!("{:032x}",0), "terrain_fingerprint":job["terrain_fingerprint"],
        "radio_fingerprint":job["radio_fingerprint"], "model_version":job["model_version"],
        "targets":[{"state":"pass"}]});
    report["large"] = json!("x".repeat(MAX_REPORT_BYTES as usize));
    assert!(matches!(
        store.save_report(JOB, &report, &current),
        Err(Error::ByteLimit)
    ));
    report.as_object_mut().unwrap().remove("large");
    for i in 0..MAX_REPORTS_PER_RUN {
        report["report_id"] = json!(format!("{i:032x}"));
        store.save_report(JOB, &report, &current).unwrap();
    }
    assert_eq!(store.reports().unwrap()[0]["pass_count"], 1);
    assert_eq!(
        store
            .find_report(report["report_id"].as_str().unwrap())
            .unwrap(),
        report
    );
    report["report_id"] = json!(format!("{:032x}", MAX_REPORTS_PER_RUN));
    assert!(matches!(
        store.save_report(JOB, &report, &current),
        Err(Error::ReportLimit)
    ));

    let output = root.path().join("export.json");
    write_export(&output, |writer| writer.write_all(b"previous")).unwrap();
    let block = vec![0; 1024 * 1024];
    assert!(matches!(
        write_export(&output, |writer| {
            for _ in 0..MAX_EXPORT_BYTES / block.len() as u64 {
                writer.write_all(&block)?;
            }
            writer.write_all(b"x")
        }),
        Err(Error::ByteLimit)
    ));
    assert_eq!(fs::read(output).unwrap(), b"previous");
    fs::OpenOptions::new()
        .write(true)
        .open(result(root.path()))
        .unwrap()
        .set_len(MAX_RESULT_BYTES + 1)
        .unwrap();
    assert!(matches!(store.page(JOB, 0, 1, None), Err(Error::ByteLimit)));
}

#[test]
fn concurrent_terminal_publishers_cannot_overwrite_the_winner() {
    let root = tempdir().unwrap();
    let store = Store::open(root.path(), PROJECT).unwrap();
    store.start(identity().0).unwrap();
    std::thread::scope(|scope| {
        let first =
            scope.spawn(|| store.finish(JOB, json!({"state":"complete"}), &[cell(0), cell(1)]));
        let second = scope.spawn(|| store.finish(JOB, json!({"state":"cancelled"}), &[]));
        let (a, b) = (first.join().unwrap(), second.join().unwrap());
        assert_ne!(a.is_ok(), b.is_ok());
        let page = store.page(JOB, 0, 2, None).unwrap();
        assert_eq!(
            page.stored_cells,
            if page.state == "complete" { 2 } else { 0 }
        );
    });
}

#[test]
fn result_append_over_quota_keeps_the_previous_manifest() {
    let root = tempdir().unwrap();
    let store = Store::open(root.path(), PROJECT).unwrap();
    let mut job = store.start(identity().0).unwrap();
    let path = result(root.path());
    fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(MAX_RESULT_BYTES - 1)
        .unwrap();
    job["coverage_store_committed_bytes"] = json!(MAX_RESULT_BYTES - 1);
    let manifest = root.path().join("coverage-job.json");
    let previous = serde_json::to_vec(&job).unwrap();
    fs::write(&manifest, &previous).unwrap();
    assert!(matches!(
        store.progress(JOB, json!({"done":1}), &[cell(0)]),
        Err(Error::ByteLimit)
    ));
    assert_eq!(fs::read(manifest).unwrap(), previous);
    assert_eq!(fs::metadata(path).unwrap().len(), MAX_RESULT_BYTES - 1);
}

#[test]
fn truncated_committed_data_does_not_prevent_restart() {
    let root = tempdir().unwrap();
    let store = Store::open(root.path(), PROJECT).unwrap();
    let (mut job, _) = identity();
    store.start(job.clone()).unwrap();
    store
        .progress(JOB, json!({"state":"running"}), &[cell(0)])
        .unwrap();
    fs::write(result(root.path()), b"").unwrap();
    assert_eq!(
        store.recover_interrupted().unwrap().unwrap()["state"],
        "interrupted"
    );
    assert!(store.page(JOB, 0, 1, None).is_err());
    job["job_id"] = json!(OTHER);
    store.start(job).unwrap();
}
