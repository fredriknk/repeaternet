use std::{
    fs,
    path::{Path, PathBuf},
};

use rusqlite::{Connection, OpenFlags, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};

use crate::{
    files, get_project, token, validate_workspace, write_transaction, Error, Project, ProjectStore,
    Result,
};

#[derive(Clone, Copy, Deserialize, Serialize)]
pub(super) enum Operation {
    Create,
    Delete,
    Plan,
}

#[derive(Deserialize, Serialize)]
struct Intent {
    operation: Operation,
    root: PathBuf,
    key: String,
    id: String,
    directory: String,
    token: String,
}

impl Intent {
    fn stage(&self, destination: &Path) -> PathBuf {
        destination.with_file_name(format!(".repeaternet-stage-{}", self.token))
    }

    fn marker(&self, directory: &Path) -> PathBuf {
        directory.join(format!(".repeaternet-owner-{}", self.token))
    }
}

impl ProjectStore {
    fn journal_path(&self) -> PathBuf {
        let mut path = self.database.as_os_str().to_owned();
        path.push(".repeaternet-intent");
        path.into()
    }

    pub(super) fn journal_exists(&self) -> Result<bool> {
        match files::metadata(&self.journal_path())? {
            None => Ok(false),
            Some(metadata) if metadata.is_file() && !files::is_link(&metadata) => Ok(true),
            Some(_) => Err(Error::UnsafeStoragePath),
        }
    }

    fn open_journal(&self, create: bool) -> Result<Connection> {
        self.journal_exists()?;
        let mut flags = OpenFlags::SQLITE_OPEN_READ_WRITE;
        if create {
            flags |= OpenFlags::SQLITE_OPEN_CREATE;
        }
        let journal = Connection::open_with_flags(self.journal_path(), flags)?;
        // The catalog's IMMEDIATE transaction is also the interprocess journal lock.
        journal.pragma_update(None, "synchronous", "FULL")?;
        journal.execute_batch(
            "CREATE TABLE IF NOT EXISTS pending (
                slot INTEGER PRIMARY KEY CHECK (slot=1),
                intent TEXT NOT NULL,
                ready INTEGER NOT NULL DEFAULT 0
            )",
        )?;
        Ok(journal)
    }

    pub(super) fn transaction<'a>(
        &self,
        connection: &'a mut Connection,
    ) -> Result<Transaction<'a>> {
        let transaction = write_transaction(connection)?;
        self.reconcile(&transaction)?;
        Ok(transaction)
    }

    pub(super) fn publish(
        &self,
        transaction: Transaction<'_>,
        project: &Project,
        operation: Operation,
        destination: &Path,
        apply: impl FnOnce(&Path, &dyn Fn() -> Result<()>) -> Result<()>,
    ) -> Result<()> {
        let intent = Intent {
            operation,
            root: self.data_root.clone(),
            key: project.workspace_key.clone(),
            id: project.id.clone(),
            directory: project.directory.clone(),
            token: token()?,
        };
        let stage = intent.stage(destination);
        if files::metadata(&stage)?.is_some() {
            return Err(Error::UnsafeStoragePath);
        }
        let journal = self.open_journal(true)?;
        journal.execute(
            "INSERT INTO pending(slot, intent) VALUES (1, ?)",
            [serde_json::to_string(&intent)?],
        )?;
        crash("intent");
        let result = apply(&stage, &|| {
            if matches!(intent.operation, Operation::Create) {
                fs::File::create_new(intent.marker(&stage))?;
            }
            journal.execute("UPDATE pending SET ready=1", [])?;
            crash("prepared");
            Ok(())
        });
        let result = result.and_then(|()| {
            crash("published");
            transaction.commit()?;
            crash("committed");
            Ok(())
        });
        drop(journal);

        // A different writer may acquire the catalog lock first and complete
        // this intent. Only finish our own intent after reacquiring the lock.
        let mut connection = self.connect()?;
        let transaction = write_transaction(&mut connection)?;
        self.reconcile_pending(&transaction, Some(&intent.token))?;
        transaction.commit()?;
        result
    }

    pub(super) fn reconcile(&self, transaction: &Transaction<'_>) -> Result<()> {
        self.reconcile_pending(transaction, None)
    }

    fn reconcile_pending(
        &self,
        transaction: &Transaction<'_>,
        expected: Option<&str>,
    ) -> Result<()> {
        if !self.journal_exists()? {
            return Ok(());
        }
        let journal = self.open_journal(false)?;
        let Some((encoded, ready)) = journal
            .query_row(
                "SELECT intent, ready FROM pending WHERE slot=1",
                [],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?)),
            )
            .optional()?
        else {
            return Ok(());
        };
        let intent: Intent = serde_json::from_str(&encoded)?;
        if expected.is_some_and(|expected| expected != intent.token) {
            return Ok(());
        }
        validate_workspace(&intent.key)?;
        if intent.root != self.data_root
            || intent.token.len() != 20
            || !intent
                .token
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        {
            return Err(Error::UnsafeStoragePath);
        }
        let current = get_project(transaction, &intent.key, &intent.id)?;
        if current
            .as_ref()
            .is_some_and(|row| row.directory != intent.directory)
        {
            return Err(Error::UnsafeStoragePath);
        }
        let parts = files::directory_parts(&intent.directory)?;
        if parts.is_empty() && !matches!(intent.operation, Operation::Plan) {
            return Err(Error::UnsafeStoragePath);
        }
        let workspace = self.workspace_directory(&intent.key, false)?;
        let destination = files::checked_directory(&workspace, parts, false)?;
        let destination = match intent.operation {
            Operation::Plan => destination.join("plan.json"),
            _ => destination,
        };
        let stage = intent.stage(&destination);
        let staged = files::metadata(&stage)?;
        match intent.operation {
            Operation::Create => {
                if ready && staged.is_none() {
                    if let Some(metadata) = files::metadata(&destination)? {
                        files::require_directory(&metadata)?;
                        let marker = intent.marker(&destination);
                        if current.is_some() {
                            remove_regular_file(&marker)?;
                        } else {
                            if files::metadata(&marker)?.is_none_or(|metadata| {
                                !metadata.is_file() || files::is_link(&metadata)
                            }) {
                                return Err(Error::UnsafeStoragePath);
                            }
                            self.reject_shared_directory(transaction, &intent.key, &destination)?;
                            files::check_delete_tree(&destination)?;
                            fs::rename(&destination, &stage)?;
                        }
                    }
                }
                if let Some(metadata) = files::metadata(&stage)? {
                    files::require_directory(&metadata)?;
                    self.reject_shared_directory(transaction, &intent.key, &stage)?;
                    files::check_delete_tree(&stage)?;
                    // Once cleanup starts, the staging tree may lose its marker
                    // or disappear entirely before the journal is cleared.
                    journal.execute("UPDATE pending SET ready=0", [])?;
                    fs::remove_dir_all(&stage)?;
                }
            }
            Operation::Delete => {
                if let Some(metadata) = staged {
                    files::require_directory(&metadata)?;
                    self.reject_shared_directory(transaction, &intent.key, &stage)?;
                    files::check_delete_tree(&stage)?;
                    if current.is_some() {
                        if files::metadata(&destination)?.is_some() {
                            return Err(Error::UnsafeStoragePath);
                        }
                        fs::rename(&stage, &destination)?;
                    } else {
                        fs::remove_dir_all(&stage).map_err(|source| Error::DeleteCleanup {
                            path: stage,
                            source,
                        })?;
                    }
                }
            }
            Operation::Plan => {
                if staged.is_some() {
                    journal.execute("UPDATE pending SET ready=0", [])?;
                    remove_regular_file(&stage)?;
                } else if ready {
                    let current = current.ok_or(Error::ProjectNotFound)?;
                    let repair = stage.with_extension("repair");
                    remove_regular_file(&repair)?;
                    let matches = match files::metadata(&destination)? {
                        Some(metadata) if metadata.is_file() && !files::is_link(&metadata) => {
                            serde_json::from_slice::<serde_json::Value>(&fs::read(&destination)?)
                                .is_ok_and(|plan| plan == current.plan)
                        }
                        Some(_) => return Err(Error::UnsafeStoragePath),
                        None => false,
                    };
                    if !matches {
                        files::write_new_plan(&repair, &current.plan)?;
                        fs::rename(repair, destination)?;
                    }
                }
            }
        }
        crash("reconciled");
        journal.execute("DELETE FROM pending WHERE slot=1", [])?;
        Ok(())
    }
}

fn remove_regular_file(path: &Path) -> Result<()> {
    if let Some(metadata) = files::metadata(path)? {
        if !metadata.is_file() || files::is_link(&metadata) {
            return Err(Error::UnsafeStoragePath);
        }
        fs::remove_file(path)?;
    }
    Ok(())
}

fn crash(_point: &str) {
    #[cfg(test)]
    if std::env::var("REPEATERNET_STORE_TEST_CRASH").as_deref() == Ok(_point) {
        std::process::exit(87);
    }
}

#[cfg(test)]
mod tests {
    use std::{env, process::Command};

    use serde_json::json;
    use tempfile::tempdir;

    use super::*;

    const KEY: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn open(base: &Path) -> Result<ProjectStore> {
        ProjectStore::new(base.join("projects.sqlite3"), base.join("data"))
    }

    #[test]
    fn crash_child() {
        let Some(base) = env::var_os("REPEATERNET_STORE_TEST_ROOT") else {
            return;
        };
        let store = open(Path::new(&base)).unwrap();
        let mut projects = store.list_active(KEY).unwrap();
        projects.extend(store.list_archived(KEY).unwrap());
        let original = projects
            .into_iter()
            .find(|project| project.name == "Original")
            .unwrap();
        match env::var("REPEATERNET_STORE_TEST_OPERATION")
            .unwrap()
            .as_str()
        {
            "create" => {
                store.create(KEY, "New", &json!({"revision": 3})).unwrap();
            }
            "duplicate" => {
                store.duplicate(KEY, &original.id, "New").unwrap();
            }
            "delete" => {
                store.delete(KEY, &original.id).unwrap();
            }
            "autosave" => {
                store
                    .autosave(KEY, &original.id, &json!({"revision": 3}))
                    .unwrap();
            }
            "recover" => {
                store.recover_previous(KEY, &original.id).unwrap();
            }
            "restore" => {
                store.restore(KEY, &original.id, "Original").unwrap();
            }
            _ => unreachable!(),
        }
        panic!("failpoint was not reached");
    }

    fn run_child(base: &Path, operation: &str, point: &str) {
        let output = Command::new(env::current_exe().unwrap())
            .args(["--exact", "recovery::tests::crash_child"])
            .env("REPEATERNET_STORE_TEST_ROOT", base)
            .env("REPEATERNET_STORE_TEST_OPERATION", operation)
            .env("REPEATERNET_STORE_TEST_CRASH", point)
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(87),
            "{operation}/{point}: {:?}",
            String::from_utf8_lossy(&output.stdout)
        );
    }

    #[test]
    fn recovery_preserves_an_occupied_delete_destination() {
        let base = tempdir().unwrap();
        let store = open(base.path()).unwrap();
        let original = store.create(KEY, "Original", &json!({})).unwrap();
        let path = store.project_path(KEY, &original).unwrap();
        fs::write(path.join("terrain.bin"), b"original").unwrap();
        store.create(KEY, "Other", &json!({})).unwrap();
        run_child(base.path(), "delete", "published");

        fs::create_dir(&path).unwrap();
        fs::write(path.join("user.bin"), b"user").unwrap();
        assert!(matches!(open(base.path()), Err(Error::UnsafeStoragePath)));
        assert_eq!(fs::read(path.join("user.bin")).unwrap(), b"user");

        fs::rename(&path, path.with_file_name("retained-user-data")).unwrap();
        open(base.path()).unwrap();
        assert_eq!(fs::read(path.join("terrain.bin")).unwrap(), b"original");
    }

    #[cfg(windows)]
    #[test]
    fn windows_open_handles_leave_recovery_retryable() {
        use std::os::windows::fs::OpenOptionsExt;

        let base = tempdir().unwrap();
        let store = open(base.path()).unwrap();
        let original = store
            .create(KEY, "Original", &json!({"revision": 1}))
            .unwrap();
        let path = store.project_path(KEY, &original).unwrap();
        store.create(KEY, "Other", &json!({})).unwrap();

        // Omit FILE_SHARE_DELETE, which Windows needs to replace the mirror.
        let handle = fs::OpenOptions::new()
            .read(true)
            .share_mode(3)
            .open(path.join("plan.json"))
            .unwrap();
        assert!(store
            .autosave(KEY, &original.id, &json!({"revision": 2}))
            .is_err());
        assert_eq!(
            store.get(KEY, &original.id).unwrap().unwrap().plan,
            original.plan
        );
        drop(handle);

        run_child(base.path(), "delete", "published");
        let stage = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(".repeaternet-stage-")
            })
            .unwrap();
        // FILE_FLAG_BACKUP_SEMANTICS opens a directory handle.
        let handle = fs::OpenOptions::new()
            .read(true)
            .share_mode(3)
            .custom_flags(0x02000000)
            .open(stage)
            .unwrap();
        assert!(matches!(open(base.path()), Err(Error::Io(_))));
        assert!(!path.exists());
        drop(handle);
        assert_eq!(
            open(base.path())
                .unwrap()
                .project_path(KEY, &original)
                .unwrap(),
            path
        );
    }

    #[test]
    #[ignore = "requires Python and the original dashboard source"]
    fn python_reopens_reconciled_catalog() {
        let base = tempdir().unwrap();
        let store = open(base.path()).unwrap();
        let original = store
            .create(KEY, "Original", &json!({"revision": 1}))
            .unwrap();
        run_child(base.path(), "autosave", "published");
        open(base.path()).unwrap();

        let module = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../src/rf_router_planner/project_store.py");
        let output = Command::new("python")
            .args([
                "-B",
                "-c",
                r#"
import json, runpy, sqlite3, sys
from pathlib import Path

store_class = runpy.run_path(sys.argv[1])["ProjectStore"]
root = Path(sys.argv[2])
db = root / "projects.sqlite3"
with sqlite3.connect(db) as connection:
    assert connection.execute("PRAGMA user_version").fetchone() == (1,)
    assert connection.execute(
        "SELECT name FROM sqlite_master WHERE type='table' ORDER BY name"
    ).fetchall() == [("projects",), ("workspace_state",)]
store = store_class(db, root / "data")
key, project_id = sys.argv[3:]
project = store.get(key, project_id)
assert project["plan"] == {"revision": 1}
assert json.loads((store.project_path(key, project) / "plan.json").read_text()) == project["plan"]
assert store.autosave(key, project_id, {"revision": 4})
"#,
            ])
            .arg(module)
            .arg(base.path())
            .args([KEY, &original.id])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            store.get(KEY, &original.id).unwrap().unwrap().plan,
            json!({"revision": 4})
        );
    }

    #[test]
    fn process_exit_at_publication_boundaries() {
        for operation in [
            "create",
            "duplicate",
            "delete",
            "autosave",
            "recover",
            "restore",
        ] {
            for point in ["intent", "prepared", "published", "committed", "reconciled"] {
                if operation == "delete" && point == "prepared" {
                    continue;
                }
                let base = tempdir().unwrap();
                let store = open(base.path()).unwrap();
                let original = store
                    .create(KEY, "Original", &json!({"revision": 1}))
                    .unwrap();
                store
                    .autosave(KEY, &original.id, &json!({"revision": 2}))
                    .unwrap();
                let original_path = store.project_path(KEY, &original).unwrap();
                fs::create_dir(original_path.join("dtm")).unwrap();
                fs::write(original_path.join("dtm/tile.tif"), b"terrain").unwrap();
                let loose = original_path
                    .parent()
                    .unwrap()
                    .join(".repeaternet-stage-unowned");
                fs::create_dir(&loose).unwrap();
                fs::write(loose.join("user.bin"), b"untouched").unwrap();
                let other = store.create(KEY, "Other", &json!({})).unwrap();
                if operation == "restore" {
                    store.archive(KEY, &original.id).unwrap();
                    // Exercise repair even when the initial mirror was stale.
                    fs::write(original_path.join("plan.json"), b"{}").unwrap();
                }

                run_child(base.path(), operation, point);
                if point == "published" {
                    run_child(base.path(), "reopen", "reconciled");
                }
                // Exercise startup recovery and a second reopen, including a crash
                // after recovery side effects and before clearing the journal.
                for _ in 0..2 {
                    let store = open(base.path()).unwrap();
                    let committed = matches!(point, "committed" | "reconciled");
                    let current = store.get(KEY, &original.id).unwrap();
                    if operation == "delete" && committed {
                        assert!(current.is_none());
                        assert!(!original_path.exists());
                    } else {
                        let current = current.unwrap();
                        assert_eq!(store.project_path(KEY, &current).unwrap(), original_path);
                        assert_eq!(
                            fs::read(original_path.join("dtm/tile.tif")).unwrap(),
                            b"terrain"
                        );
                        let revision = match (operation, committed) {
                            ("autosave", true) => 3,
                            ("recover", true) => 1,
                            _ => 2,
                        };
                        assert_eq!(current.plan, json!({"revision": revision}));
                        assert_eq!(current.archived, operation == "restore" && !committed);
                        let mirror: serde_json::Value = serde_json::from_slice(
                            &fs::read(original_path.join("plan.json")).unwrap(),
                        )
                        .unwrap();
                        assert_eq!(fs::read_dir(&original_path).unwrap().count(), 2);
                        if operation != "restore" || committed || point == "published" {
                            assert_eq!(mirror, current.plan, "{operation}/{point}");
                        }
                    }
                    let active = store.active(KEY).unwrap();
                    let created = committed && matches!(operation, "create" | "duplicate");
                    if created {
                        assert_eq!(active.name, "New");
                        let path = store.project_path(KEY, &active).unwrap();
                        let mirror: serde_json::Value =
                            serde_json::from_slice(&fs::read(path.join("plan.json")).unwrap())
                                .unwrap();
                        assert_eq!(mirror, active.plan);
                        assert_eq!(path.join("dtm/tile.tif").exists(), operation == "duplicate");
                    } else if operation == "restore" && committed {
                        assert_eq!(active.id, original.id);
                    } else {
                        assert_eq!(active.id, other.id);
                    }
                    let directories = fs::read_dir(original_path.parent().unwrap())
                        .unwrap()
                        .count();
                    assert_eq!(
                        directories,
                        3 + usize::from(created) - usize::from(operation == "delete" && committed)
                    );
                    assert_eq!(fs::read(loose.join("user.bin")).unwrap(), b"untouched");
                }
            }
        }
    }
}
