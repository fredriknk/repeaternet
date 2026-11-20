use std::{
    fs::{self, File, FileTimes},
    path::{Path, PathBuf},
    sync::{Arc, Barrier},
    thread,
    time::{Duration, UNIX_EPOCH},
};

use repeaternet_project_store::{normalize_project_name, valid_workspace_key, Error, ProjectStore};
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use tempfile::{tempdir, TempDir};

const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

struct Store {
    temp: TempDir,
    store: ProjectStore,
}

impl Store {
    fn open(fixture: Option<&[u8]>) -> Self {
        let temp = tempdir().unwrap();
        let database = temp.path().join("projects.sqlite3");
        if let Some(bytes) = fixture {
            fs::write(&database, bytes).unwrap();
        }
        let store = ProjectStore::new(database, temp.path()).unwrap();
        Self { temp, store }
    }

    fn root(&self) -> &Path {
        self.temp.path()
    }

    fn workspace(&self) -> PathBuf {
        self.root().join(A)
    }

    fn db(&self) -> Connection {
        Connection::open(self.root().join("projects.sqlite3")).unwrap()
    }

    fn set_directory(&self, project_id: &str, directory: &str) {
        self.db()
            .execute(
                "UPDATE projects SET directory=? WHERE id=?",
                params![directory, project_id],
            )
            .unwrap();
    }
}

fn mirror(directory: &Path) -> Value {
    serde_json::from_slice(&fs::read(directory.join("plan.json")).unwrap()).unwrap()
}

#[test]
fn opens_python_v1_catalog_and_preserves_rows_and_unknown_plan_data() {
    let fixture = Store::open(Some(include_bytes!("fixtures/project_catalog_v1.sqlite3")));
    let store = &fixture.store;
    let before = store.get(A, "00000000000000000002").unwrap().unwrap();
    assert_eq!(before.name, "Nordlys");
    assert_eq!(before.directory, "projects/00000000000000000001");
    assert_eq!(before.workspace_key, A);
    assert_eq!(before.created_at, 1_700_000_000.25);
    assert_eq!(before.updated_at, 1_700_000_003.25);
    assert_eq!(before.run_state, "preparing");
    assert_eq!(before.plan["unknown"]["text"], "Tromsø 🛰");
    assert!(before.plan["unknown"]["optional"].is_null());
    assert_eq!(
        before.plan["unknown"]["large"].to_string(),
        "92233720368547758091234"
    );
    assert_eq!(before.previous_plan.unwrap()["a"], json!([60, 10]));
    assert_eq!(before.result_summary.unwrap()["snapshot_version"], 7);
    assert_eq!(store.list_active(A).unwrap().len(), 1);
    assert_eq!(store.list_archived(A).unwrap()[0].name, "Old plan");
    assert_eq!(store.active(B).unwrap().plan["b"], json!([63, 13]));

    let recovered = store.active(A).unwrap();
    assert_eq!(recovered.run_state, "interrupted");
    assert_eq!(store.get(A, &recovered.id).unwrap().unwrap(), recovered);
    assert_eq!(store.active(A).unwrap(), recovered);
    let swapped = store.recover_previous(A, &recovered.id).unwrap().unwrap();
    assert_eq!(swapped.plan["a"], json!([60, 10]));
    assert_eq!(swapped.previous_plan, Some(recovered.plan));
    assert_eq!(swapped.result_summary, None);
    assert_eq!(
        mirror(&store.project_path(A, &swapped).unwrap()),
        swapped.plan
    );

    let db = fixture.db();
    assert_eq!(
        db.pragma_query_value(None, "user_version", |r| r.get::<_, i32>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        db.pragma_query_value(None, "journal_mode", |r| r.get::<_, String>(0))
            .unwrap(),
        "wal"
    );
    let index: String = db.query_row(
        "SELECT tbl_name FROM sqlite_master WHERE type='index' AND name='projects_workspace_updated'",
        [],
        |r| r.get(0),
    ).unwrap();
    assert_eq!(index, "projects");
}

#[test]
fn active_initialization_is_stable_and_does_not_adopt_loose_files() {
    let fixture = Store::open(None);
    let store = &fixture.store;
    fs::create_dir(fixture.workspace()).unwrap();
    let loose = fixture.workspace().join("plan.json");
    fs::write(&loose, br#"{"a":[60,10]}"#).unwrap();
    let project = store.active(A).unwrap();
    assert_eq!(project.name, "Untitled plan");
    assert_eq!(project.plan, json!({}));
    assert_eq!(project.id.len(), 20);
    assert_ne!(
        project.id,
        project.directory.strip_prefix("projects/").unwrap()
    );
    assert_eq!(
        store
            .project_path(A, &project)
            .unwrap()
            .parent()
            .unwrap()
            .file_name()
            .unwrap(),
        "projects"
    );
    assert_eq!(fs::read(loose).unwrap(), br#"{"a":[60,10]}"#);
    assert_eq!(store.active(A).unwrap().id, project.id);
}

#[test]
fn catalog_and_paths_require_workspace_ownership() {
    let fixture = Store::open(None);
    let store = &fixture.store;
    let original = store.create(A, "Original", &json!({"a":[1,2]})).unwrap();
    assert!(store.list_active(B).unwrap().is_empty());
    assert!(store.get(B, &original.id).unwrap().is_none());
    assert!(store.activate(B, &original.id).unwrap().is_none());
    assert!(!store
        .autosave(B, &original.id, &json!({"a":[3,4]}))
        .unwrap());
    assert!(store.recover_previous(B, &original.id).unwrap().is_none());
    assert!(!store.rename(B, &original.id, "Hijacked").unwrap());
    assert!(store.duplicate(B, &original.id, "Copy").unwrap().is_none());
    assert!(!store.archive(B, &original.id).unwrap());
    assert!(store
        .restore(B, &original.id, "Restored")
        .unwrap()
        .is_none());
    assert!(store.delete(B, &original.id).unwrap().is_none());
    assert!(!store.sync_plan_mirror(B, &original.id).unwrap());
    store.set_run_state(B, &original.id, "running").unwrap();
    store
        .save_result_summary(B, &original.id, Some(&json!({"found":true})))
        .unwrap();
    assert_eq!(store.get(A, &original.id).unwrap(), Some(original.clone()));
    assert!(matches!(
        store.project_path(B, &original),
        Err(Error::ProjectNotFound)
    ));
    let mut forged = original.clone();
    forged.workspace_key = B.into();
    assert!(matches!(
        store.project_path(B, &forged),
        Err(Error::ProjectNotFound)
    ));
    assert!(!fixture.root().join(B).exists());
}

#[test]
fn archive_and_delete_protect_active_project_and_preserve_or_remove_files() {
    let fixture = Store::open(None);
    let store = &fixture.store;
    let original = store.create(A, "Original", &json!({"a":[1,2]})).unwrap();
    let original_path = store.project_path(A, &original).unwrap();
    let file = original_path.join("keep.bin");
    fs::write(&file, b"terrain").unwrap();
    assert!(matches!(
        store.archive(A, &original.id),
        Err(Error::ArchiveActive)
    ));
    assert!(matches!(
        store.delete(A, &original.id),
        Err(Error::DeleteActive)
    ));
    assert!(file.exists());
    let other = store.create(A, "Other", &json!({})).unwrap();
    assert!(store.archive(A, &original.id).unwrap());
    assert_eq!(fs::read(&file).unwrap(), b"terrain");
    assert_eq!(store.list_archived(A).unwrap()[0].id, original.id);
    assert!(store.activate(A, &original.id).unwrap().is_none());
    assert!(store.duplicate(A, &original.id, "Copy").unwrap().is_none());
    assert!(!store.autosave(A, &original.id, &json!({})).unwrap());
    assert!(!store.rename(A, &original.id, "New name").unwrap());
    assert!(matches!(
        store.restore(A, &original.id, "Other"),
        Err(Error::NameInUse)
    ));
    let restored = store
        .restore(A, &original.id, "  Restored  ")
        .unwrap()
        .unwrap();
    assert_eq!(restored.name, "Restored");
    assert_eq!(restored.plan, original.plan);
    assert_eq!(store.active(A).unwrap().id, restored.id);
    assert!(store.list_archived(A).unwrap().is_empty());
    assert!(file.exists());
    assert!(store.activate(A, &other.id).unwrap().is_some());
    assert!(store.archive(A, &original.id).unwrap());
    assert_eq!(
        store.delete(A, &original.id).unwrap().unwrap().id,
        original.id
    );
    assert!(store.get(A, &original.id).unwrap().is_none());
    assert!(!original_path.exists());
    assert!(store.project_path(A, &other).unwrap().exists());
}

#[test]
fn traversal_and_windows_path_aliases_are_rejected_before_mutation() {
    let fixture = Store::open(None);
    let store = &fixture.store;
    let old = store.create(A, "Original", &json!({})).unwrap();
    let active = store.create(A, "Other", &json!({})).unwrap();
    let active_path = store.project_path(A, &active).unwrap();
    for directory in [
        "../escape",
        "projects/../escape",
        "projects\\..\\escape",
        "/escape",
        r"C:\escape",
        r"C:escape",
        r"\\server\share",
        r"\\?\C:\escape",
        "projects/AUX",
        "projects/COM¹.txt",
        "projects/demo. ",
        "projects/tile:stream",
        "projects//escape",
    ] {
        fixture.set_directory(&old.id, directory);
        let error = store.project_path(A, &old).unwrap_err();
        assert!(
            matches!(error, Error::UnsafeStoragePath),
            "{directory}: {error}"
        );
        assert!(
            matches!(store.delete(A, &old.id), Err(Error::UnsafeStoragePath)),
            "{directory}"
        );
        assert!(store.get(A, &old.id).unwrap().is_some());
    }
    fixture.set_directory(&old.id, ".");
    assert!(matches!(
        store.delete(A, &old.id),
        Err(Error::DeleteWorkspaceRoot)
    ));
    assert!(store.project_path(A, &old).unwrap().is_dir());
    fixture.set_directory(&old.id, "projects");
    assert!(matches!(
        store.delete(A, &old.id),
        Err(Error::UnsafeStoragePath)
    ));
    fixture.set_directory(&old.id, &active.directory);
    assert!(matches!(
        store.delete(A, &old.id),
        Err(Error::UnsafeStoragePath)
    ));
    #[cfg(windows)]
    {
        fixture.set_directory(&old.id, &active.directory.to_ascii_uppercase());
        assert!(matches!(
            store.delete(A, &old.id),
            Err(Error::UnsafeStoragePath)
        ));
    }
    assert!(active_path.exists());
    assert!(!fixture.root().join("escape").exists());
    assert!(!valid_workspace_key(&A.to_ascii_uppercase()));
    assert!(matches!(
        store.active("../escape"),
        Err(Error::InvalidWorkspace)
    ));
    assert!(matches!(
        store.storage_usage("../escape"),
        Err(Error::InvalidWorkspace)
    ));
}

#[test]
fn revision_swap_updates_mirror_and_unchanged_autosave_preserves_history() {
    let fixture = Store::open(None);
    let store = &fixture.store;
    let first = json!({"a":[1,2], "unknown":null});
    let second = json!({"a":[3,4], "manual_routers":[{"id":"M-one"}]});
    let original = store.create(A, "Original", &first).unwrap();
    let path = store.project_path(A, &original).unwrap();
    assert!(store.recover_previous(A, &original.id).unwrap().is_none());
    assert!(store.autosave(A, &original.id, &first).unwrap());
    assert_eq!(store.get(A, &original.id).unwrap(), Some(original.clone()));
    assert!(store.autosave(A, &original.id, &second).unwrap());
    assert_eq!(mirror(&path), second);
    let saved = store.get(A, &original.id).unwrap().unwrap();
    assert_eq!(saved.previous_plan.as_ref(), Some(&first));
    assert!(store.autosave(A, &original.id, &second).unwrap());
    assert_eq!(store.get(A, &original.id).unwrap(), Some(saved));
    store
        .save_result_summary(A, &original.id, Some(&json!({"found":true})))
        .unwrap();
    let recovered = store.recover_previous(A, &original.id).unwrap().unwrap();
    assert_eq!(recovered.plan, first);
    assert_eq!(recovered.previous_plan, Some(second.clone()));
    assert_eq!(recovered.result_summary, None);
    assert_eq!(mirror(&path), recovered.plan);
    let swapped_back = store.recover_previous(A, &original.id).unwrap().unwrap();
    assert_eq!(swapped_back.plan, second);
    assert_eq!(swapped_back.previous_plan, Some(first));
    assert_eq!(mirror(&path), swapped_back.plan);
}

#[test]
fn duplicate_copies_only_saved_plan_and_terrain_with_file_mtimes() {
    let fixture = Store::open(None);
    let store = &fixture.store;
    let original = store.create(A, "Original", &json!({"revision":1})).unwrap();
    let path = store.project_path(A, &original).unwrap();
    let current = json!({"revision":2, "unknown":["ø",null]});
    store.autosave(A, &original.id, &current).unwrap();
    store.set_run_state(A, &original.id, "stopped").unwrap();
    store
        .save_result_summary(A, &original.id, Some(&json!({"found":true})))
        .unwrap();
    fs::write(path.join("plan.json"), br#"{"stale":true}"#).unwrap();
    let terrain = [
        "dtm/ground.tif",
        "dom/surface.tif",
        "terrain-generations/generation-1/dtm/tile.tif",
        "coverage-terrain-generations/generation-2/dom/tile.tif",
    ];
    let modified = UNIX_EPOCH + Duration::from_millis(1_700_000_000_123);
    for relative in terrain {
        let file = path.join(relative);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, relative).unwrap();
        File::options()
            .write(true)
            .open(file)
            .unwrap()
            .set_times(FileTimes::new().set_modified(modified))
            .unwrap();
    }
    let outputs = [
        "coverage-job.json",
        "coverage-jobs/run.json",
        "coverage-results/run.jsonl",
        "coverage-target-reports/run-report.json",
        "route-report.html",
    ];
    for relative in outputs {
        let file = path.join(relative);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, b"computed").unwrap();
    }
    let copy = store.duplicate(A, &original.id, "Copy").unwrap().unwrap();
    let copy_path = store.project_path(A, &copy).unwrap();
    assert_eq!(copy.plan, current);
    assert_eq!(mirror(&copy_path), current);
    assert_eq!(copy.previous_plan, None);
    assert_eq!(copy.result_summary, None);
    assert_eq!(copy.run_state, "idle");
    assert_eq!(store.active(A).unwrap().id, copy.id);
    for relative in terrain {
        let source = path.join(relative);
        let target = copy_path.join(relative);
        assert_eq!(fs::read(&target).unwrap(), relative.as_bytes());
        assert_eq!(
            fs::metadata(&target).unwrap().modified().unwrap(),
            fs::metadata(&source).unwrap().modified().unwrap()
        );
    }
    for relative in outputs {
        assert!(!copy_path.join(relative).exists(), "{relative}");
    }
    fs::write(copy_path.join(terrain[0]), b"edited").unwrap();
    assert_eq!(
        fs::read(path.join(terrain[0])).unwrap(),
        terrain[0].as_bytes()
    );
}

#[test]
fn duplicate_and_autosave_roll_back_failed_file_operations() {
    let fixture = Store::open(None);
    let store = &fixture.store;
    let source = store.create(A, "Original", &json!({"revision":1})).unwrap();
    let path = store.project_path(A, &source).unwrap();
    fs::create_dir(path.join("dtm")).unwrap();
    fs::write(path.join("dtm/tile.tif"), b"partial copy").unwrap();
    fs::write(path.join("dom"), b"not a tree").unwrap();
    assert!(matches!(
        store.duplicate(A, &source.id, "Copy"),
        Err(Error::UnsafeStoragePath)
    ));
    assert_eq!(store.list_active(A).unwrap().len(), 1);
    assert_eq!(store.active(A).unwrap().id, source.id);
    assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
    fs::remove_file(path.join("plan.json")).unwrap();
    fs::create_dir(path.join("plan.json")).unwrap();
    assert!(store
        .autosave(A, &source.id, &json!({"revision":2}))
        .is_err());
    assert_eq!(store.get(A, &source.id).unwrap().unwrap(), source);
}

#[test]
fn failed_delete_commit_restores_the_project_directory() {
    let fixture = Store::open(None);
    let store = &fixture.store;
    let original = store.create(A, "Original", &json!({})).unwrap();
    let original_path = store.project_path(A, &original).unwrap();
    fs::write(original_path.join("saved.bin"), b"project data").unwrap();
    let other = store.create(A, "Other", &json!({})).unwrap();
    fixture
        .db()
        .execute_batch(
            "CREATE TABLE valid_delete (id TEXT PRIMARY KEY);
         CREATE TABLE delete_audit (
             id TEXT REFERENCES valid_delete(id) DEFERRABLE INITIALLY DEFERRED
         );
         CREATE TRIGGER audit_delete AFTER DELETE ON projects
         BEGIN
             INSERT INTO delete_audit VALUES (old.id);
         END;",
        )
        .unwrap();
    assert!(matches!(
        store.delete(A, &original.id),
        Err(Error::Sqlite(_))
    ));
    assert_eq!(store.get(A, &original.id).unwrap(), Some(original));
    assert_eq!(
        fs::read(original_path.join("saved.bin")).unwrap(),
        b"project data"
    );
    assert_eq!(
        fs::read_dir(original_path.parent().unwrap())
            .unwrap()
            .count(),
        2
    );
    assert_eq!(store.active(A).unwrap().id, other.id);
}

#[test]
fn names_are_trimmed_and_unique_per_workspace_including_archived_rows() {
    let fixture = Store::open(None);
    let store = &fixture.store;
    assert_eq!(
        normalize_project_name(" \u{1f} Tromsø \t").unwrap(),
        "Tromsø"
    );
    assert!(normalize_project_name(&"🛰".repeat(80)).is_ok());
    assert!(matches!(
        normalize_project_name(&"🛰".repeat(81)),
        Err(Error::InvalidName)
    ));
    assert!(matches!(
        normalize_project_name("in\tvalid"),
        Err(Error::InvalidName)
    ));
    assert!(matches!(
        normalize_project_name("  "),
        Err(Error::InvalidName)
    ));
    let original = store.create(A, " Shared ", &json!({})).unwrap();
    let other = store.create(A, "Other", &json!({})).unwrap();
    assert_eq!(original.name, "Shared");
    assert!(store.archive(A, &original.id).unwrap());
    assert!(matches!(
        store.create(A, "Shared", &json!({})),
        Err(Error::NameInUse)
    ));
    assert!(matches!(
        store.rename(A, &other.id, " Shared "),
        Err(Error::NameInUse)
    ));
    assert!(matches!(
        store.duplicate(A, &other.id, "Shared"),
        Err(Error::NameInUse)
    ));
    assert_eq!(
        store.create(B, "Shared", &json!({})).unwrap().name,
        "Shared"
    );
    assert_eq!(
        store.create(A, "shared", &json!({})).unwrap().name,
        "shared"
    );
    assert!(matches!(
        store.create(A, "Array", &json!([])),
        Err(Error::InvalidPlan)
    ));
}

#[test]
fn restart_marks_pending_active_jobs_interrupted_and_recovers_stale_workspace_state() {
    let fixture = Store::open(None);
    let store = &fixture.store;
    let plan = store.create(A, "Original", &json!({})).unwrap();
    for state in ["queued", "running", "preparing", "stopped", "complete"] {
        store.set_run_state(A, &plan.id, state).unwrap();
        let expected = match state {
            "queued" | "running" | "preparing" => "interrupted",
            _ => state,
        };
        assert_eq!(store.active(A).unwrap().run_state, expected);
        assert_eq!(store.get(A, &plan.id).unwrap().unwrap().run_state, expected);
    }
    let latest = store.create(A, "Latest", &json!({})).unwrap();
    let foreign = store.create(B, "Foreign", &json!({})).unwrap();
    fixture
        .db()
        .execute("UPDATE projects SET updated_at=10 WHERE id=?", [&latest.id])
        .unwrap();
    fixture
        .db()
        .execute("UPDATE projects SET updated_at=1 WHERE id=?", [&plan.id])
        .unwrap();
    fixture
        .db()
        .execute(
            "UPDATE workspace_state SET active_project_id=? WHERE workspace_key=?",
            params![foreign.id, A],
        )
        .unwrap();
    assert_eq!(store.active(A).unwrap().id, latest.id);
    assert_eq!(store.active(B).unwrap().id, foreign.id);
}

#[test]
fn concurrent_autosaves_keep_both_revisions_and_the_current_mirror() {
    let fixture = Store::open(None);
    let store = &fixture.store;
    let original = store
        .create(A, "Concurrent", &json!({"version":0}))
        .unwrap();
    let barrier = Arc::new(Barrier::new(2));
    thread::scope(|scope| {
        for number in [1, 2] {
            let barrier = Arc::clone(&barrier);
            let id = &original.id;
            scope.spawn(move || {
                barrier.wait();
                assert!(store.autosave(A, id, &json!({"version":number})).unwrap());
            });
        }
    });
    let current = store.get(A, &original.id).unwrap().unwrap();
    let previous = current.previous_plan.as_ref().unwrap();
    let mut versions = [
        current.plan["version"].as_i64(),
        previous["version"].as_i64(),
    ];
    versions.sort();
    assert_eq!(versions, [Some(1), Some(2)]);
    assert_eq!(
        mirror(&store.project_path(A, &current).unwrap()),
        current.plan
    );
}

#[cfg(any(unix, windows))]
#[test]
fn workspace_and_project_links_cannot_escape_or_be_copied() {
    let fixture = Store::open(None);
    let store = &fixture.store;
    let outside = tempdir().unwrap();
    fs::write(outside.path().join("protected.tif"), b"outside workspace").unwrap();
    link_directory(outside.path(), &fixture.root().join(B));
    assert!(matches!(
        store.create(B, "Unsafe", &json!({})),
        Err(Error::UnsafeStoragePath)
    ));
    assert!(matches!(
        store.storage_usage(B),
        Err(Error::UnsafeStoragePath)
    ));
    assert!(!outside.path().join("projects").exists());

    let original = store.create(A, "Original", &json!({})).unwrap();
    let path = store.project_path(A, &original).unwrap();
    let usage = store.storage_usage(A).unwrap();
    link_directory(outside.path(), &path.join("dtm"));
    assert_eq!(store.storage_usage(A).unwrap(), usage);
    assert!(matches!(
        store.duplicate(A, &original.id, "Copy"),
        Err(Error::UnsafeStoragePath)
    ));
    store.create(A, "Other", &json!({})).unwrap();
    assert!(matches!(
        store.delete(A, &original.id),
        Err(Error::UnsafeStoragePath)
    ));
    let escaped = path.parent().unwrap().join("escaped");
    link_directory(outside.path(), &escaped);
    fixture.set_directory(&original.id, "projects/escaped");
    assert!(matches!(
        store.project_path(A, &original),
        Err(Error::UnsafeStoragePath)
    ));
    assert!(matches!(
        store.delete(A, &original.id),
        Err(Error::UnsafeStoragePath)
    ));
    assert_eq!(
        fs::read(outside.path().join("protected.tif")).unwrap(),
        b"outside workspace"
    );
}

#[cfg(unix)]
fn link_directory(target: &Path, link: &Path) {
    std::os::unix::fs::symlink(target, link).unwrap();
}

#[cfg(windows)]
fn link_directory(target: &Path, link: &Path) {
    // Junctions exercise reparse point handling without requiring elevation.
    let output = std::process::Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "New-Item -ItemType Junction -Path $env:TEST_JUNCTION_LINK -Target $env:TEST_JUNCTION_TARGET -ErrorAction Stop",
        ])
        .env("TEST_JUNCTION_LINK", link.to_str().unwrap().trim_start_matches(r"\\?\"))
        .env("TEST_JUNCTION_TARGET", target)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
