#![forbid(unsafe_code)]

//! The RepeaterNet v1 SQLite catalog and workspace-local project files.

mod files;
mod recovery;

use recovery::Operation;

use std::{
    fmt::Write as _,
    fs, io,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use rusqlite::{
    params, types::Type, Connection, OptionalExtension, Row, Transaction, TransactionBehavior,
};
use serde::Serialize;
use serde_json::Value;
use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("Open the planner to initialize a workspace")]
    InvalidWorkspace,
    #[error("Project name must be 1–80 printable characters")]
    InvalidName,
    #[error("Plan must be a JSON object")]
    InvalidPlan,
    #[error("A project with that name already exists")]
    NameInUse,
    #[error("Project not found")]
    ProjectNotFound,
    #[error("Project storage path escaped its workspace or is unsafe")]
    UnsafeStoragePath,
    #[error("Switch to another project before archiving this one")]
    ArchiveActive,
    #[error("Switch to another project before deleting this one")]
    DeleteActive,
    #[error("A project stored at the workspace root cannot be permanently deleted")]
    DeleteWorkspaceRoot,
    #[error("Unsupported project catalog schema version {0}")]
    UnsupportedSchema(i64),
    #[error("Project was deleted from the catalog, but cleanup of {path:?} failed: {source}")]
    DeleteCleanup { path: PathBuf, source: io::Error },
    #[error(
        "Catalog deletion failed ({catalog}) and restoring files from {path:?} failed: {source}"
    )]
    DeleteRollback {
        path: PathBuf,
        catalog: rusqlite::Error,
        source: io::Error,
    },
    #[error("Catalog creation failed ({catalog}) and removing {path:?} failed: {source}")]
    CreateRollback {
        path: PathBuf,
        catalog: rusqlite::Error,
        source: io::Error,
    },
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Project {
    pub id: String,
    pub workspace_key: String,
    pub name: String,
    pub directory: String,
    pub plan: Value,
    pub previous_plan: Option<Value>,
    pub result_summary: Option<Value>,
    pub run_state: String,
    pub archived: bool,
    pub created_at: f64,
    pub updated_at: f64,
}

/// A disk-backed catalog. Each operation uses its own SQLite connection.
#[derive(Clone, Debug)]
pub struct ProjectStore {
    database: PathBuf,
    data_root: PathBuf,
}

/// Validate the exact workspace-cookie form used by the Python dashboard.
pub fn valid_workspace_key(key: &str) -> bool {
    key.len() == 48
        && key
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// Use the Python dashboard's strip, code-point limit, and control check.
pub fn normalize_project_name(value: &str) -> Result<&str> {
    let name = value.trim_matches(|c: char| c.is_whitespace() || matches!(c, '\u{1c}'..='\u{1f}'));
    if name.is_empty() || name.chars().count() > 80 || name.chars().any(|c| c < ' ') {
        Err(Error::InvalidName)
    } else {
        Ok(name)
    }
}

impl ProjectStore {
    pub fn new(database: impl AsRef<Path>, data_root: impl AsRef<Path>) -> Result<Self> {
        fs::create_dir_all(data_root.as_ref())?;
        let mut store = Self {
            database: std::path::absolute(database.as_ref())?,
            data_root: fs::canonicalize(data_root.as_ref())?,
        };
        let mut connection = store.connect()?;
        store.database = fs::canonicalize(&store.database)?;
        let version = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if !matches!(version, 0 | 1) {
            return Err(Error::UnsupportedSchema(version));
        }
        connection.pragma_update(None, "journal_mode", "WAL")?;
        let transaction = write_transaction(&mut connection)?;
        transaction.execute_batch(include_str!("schema.sql"))?;
        store.reconcile(&transaction)?;
        transaction.commit()?;
        Ok(store)
    }

    /// Resolve the active project when initializing a workspace. Call once
    /// per workspace on startup, since pending work is marked interrupted.
    pub fn active(&self, key: &str) -> Result<Project> {
        validate_workspace(key)?;
        let mut connection = self.connect()?;
        let transaction = self.transaction(&mut connection)?;
        let mut project = None;
        if let Some(active_id) = active_id(&transaction, key)? {
            project = get_project(&transaction, key, &active_id)?.filter(|row| !row.archived);
        }
        let project = match project {
            Some(mut project) => {
                if matches!(
                    project.run_state.as_str(),
                    "queued" | "running" | "preparing"
                ) {
                    project.run_state = "interrupted".to_owned();
                    project.updated_at = now();
                    transaction.execute(
                        "UPDATE projects SET run_state=?, updated_at=? WHERE workspace_key=? AND id=?",
                        params![project.run_state, project.updated_at, key, project.id],
                    )?;
                }
                project
            }
            None => {
                let project = transaction
                    .query_row(
                        "SELECT * FROM projects WHERE workspace_key=? AND archived=0
                         ORDER BY updated_at DESC LIMIT 1",
                        [key],
                        decode_project,
                    )
                    .optional()?;
                let project = match project {
                    Some(project) => project,
                    None => {
                        let project = insert_project(
                            &transaction,
                            key,
                            "Untitled plan",
                            &Value::Object(Default::default()),
                        )?;
                        return self.publish_new_project(transaction, project, None);
                    }
                };
                set_active(&transaction, key, &project.id)?;
                project
            }
        };
        transaction.commit()?;
        Ok(project)
    }

    pub fn list_active(&self, key: &str) -> Result<Vec<Project>> {
        self.list(key, false)
    }

    pub fn list_archived(&self, key: &str) -> Result<Vec<Project>> {
        self.list(key, true)
    }

    fn list(&self, key: &str, archived: bool) -> Result<Vec<Project>> {
        let connection = self.workspace_connection(key)?;
        let mut statement = connection.prepare(
            "SELECT * FROM projects WHERE workspace_key=? AND archived=? ORDER BY updated_at DESC",
        )?;
        let rows = statement.query_map(params![key, archived], decode_project)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn get(&self, key: &str, project_id: &str) -> Result<Option<Project>> {
        get_project(&self.workspace_connection(key)?, key, project_id)
    }

    pub fn activate(&self, key: &str, project_id: &str) -> Result<Option<Project>> {
        validate_workspace(key)?;
        let mut connection = self.connect()?;
        let transaction = self.transaction(&mut connection)?;
        let Some(project) = get_project(&transaction, key, project_id)?.filter(|row| !row.archived)
        else {
            return Ok(None);
        };
        self.directory(&project, false)?;
        set_active(&transaction, key, project_id)?;
        transaction.commit()?;
        Ok(Some(project))
    }

    pub fn create(&self, key: &str, name: &str, plan: &Value) -> Result<Project> {
        let name = normalize_project_name(name)?;
        require_plan(plan)?;
        validate_workspace(key)?;
        let mut connection = self.connect()?;
        let transaction = self.transaction(&mut connection)?;
        let project = insert_project(&transaction, key, name, plan)?;
        self.publish_new_project(transaction, project, None)
    }

    pub fn autosave(&self, key: &str, project_id: &str, plan: &Value) -> Result<bool> {
        require_plan(plan)?;
        validate_workspace(key)?;
        let mut connection = self.connect()?;
        let transaction = self.transaction(&mut connection)?;
        let Some(project) = get_project(&transaction, key, project_id)?.filter(|row| !row.archived)
        else {
            return Ok(false);
        };
        if project.plan != *plan {
            transaction.execute(
                "UPDATE projects SET previous_plan_json=plan_json, plan_json=?, updated_at=?
                 WHERE workspace_key=? AND id=?",
                params![serde_json::to_string(plan)?, now(), key, project_id],
            )?;
        }
        self.publish_plan(transaction, &project, plan)?;
        Ok(true)
    }

    /// Swap current and previous revisions and clear the saved result summary.
    pub fn recover_previous(&self, key: &str, project_id: &str) -> Result<Option<Project>> {
        validate_workspace(key)?;
        let mut connection = self.connect()?;
        let transaction = self.transaction(&mut connection)?;
        let Some(project) = get_project(&transaction, key, project_id)?.filter(|row| !row.archived)
        else {
            return Ok(None);
        };
        let Some(previous) = &project.previous_plan else {
            return Ok(None);
        };
        transaction.execute(
            "UPDATE projects SET plan_json=previous_plan_json, previous_plan_json=plan_json,
             result_summary_json=NULL, updated_at=? WHERE workspace_key=? AND id=?",
            params![now(), key, project_id],
        )?;
        let recovered = required_project(&transaction, key, project_id)?;
        self.publish_plan(transaction, &project, previous)?;
        Ok(Some(recovered))
    }

    pub fn rename(&self, key: &str, project_id: &str, name: &str) -> Result<bool> {
        let name = normalize_project_name(name)?;
        let connection = self.workspace_connection(key)?;
        let changed = connection.execute(
            "UPDATE projects SET name=?, updated_at=? WHERE workspace_key=? AND id=? AND archived=0",
            params![name, now(), key, project_id],
        );
        Ok(changed.map_err(name_conflict)? == 1)
    }

    pub fn duplicate(&self, key: &str, project_id: &str, name: &str) -> Result<Option<Project>> {
        let name = normalize_project_name(name)?;
        validate_workspace(key)?;
        let mut connection = self.connect()?;
        let transaction = self.transaction(&mut connection)?;
        let Some(source) = get_project(&transaction, key, project_id)?.filter(|row| !row.archived)
        else {
            return Ok(None);
        };
        let source_dir = self.directory(&source, false)?;
        let target = insert_project(&transaction, key, name, &source.plan)?;
        Ok(Some(self.publish_new_project(
            transaction,
            target,
            Some(&source_dir),
        )?))
    }

    pub fn archive(&self, key: &str, project_id: &str) -> Result<bool> {
        validate_workspace(key)?;
        let mut connection = self.connect()?;
        let transaction = self.transaction(&mut connection)?;
        if active_id(&transaction, key)?.as_deref() == Some(project_id)
            && get_project(&transaction, key, project_id)?.is_some()
        {
            return Err(Error::ArchiveActive);
        }
        let changed = transaction.execute(
            "UPDATE projects SET archived=1, updated_at=? WHERE workspace_key=? AND id=?",
            params![now(), key, project_id],
        )?;
        transaction.commit()?;
        Ok(changed == 1)
    }

    pub fn restore(&self, key: &str, project_id: &str, name: &str) -> Result<Option<Project>> {
        let name = normalize_project_name(name)?;
        validate_workspace(key)?;
        let mut connection = self.connect()?;
        let transaction = self.transaction(&mut connection)?;
        let changed = transaction
            .execute(
                "UPDATE projects SET name=?, archived=0, updated_at=?
             WHERE workspace_key=? AND id=? AND archived=1",
                params![name, now(), key, project_id],
            )
            .map_err(name_conflict)?;
        if changed != 1 {
            return Ok(None);
        }
        let project = required_project(&transaction, key, project_id)?;
        set_active(&transaction, key, project_id)?;
        self.publish_plan(transaction, &project, &project.plan)?;
        Ok(Some(project))
    }

    /// Delete an inactive project and its files. The files are first moved to
    /// a sibling staging directory so a failed catalog commit can be rolled back.
    pub fn delete(&self, key: &str, project_id: &str) -> Result<Option<Project>> {
        validate_workspace(key)?;
        let mut connection = self.connect()?;
        let transaction = self.transaction(&mut connection)?;
        let Some(project) = get_project(&transaction, key, project_id)? else {
            return Ok(None);
        };
        if active_id(&transaction, key)?.as_deref() == Some(project_id) {
            return Err(Error::DeleteActive);
        }
        if project.directory == "." {
            return Err(Error::DeleteWorkspaceRoot);
        }
        let directory = self.directory(&project, false)?;
        transaction.execute(
            "DELETE FROM projects WHERE workspace_key=? AND id=?",
            params![key, project_id],
        )?;
        match fs::symlink_metadata(&directory) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => transaction.commit()?,
            Err(error) => return Err(error.into()),
            Ok(metadata) => {
                files::require_directory(&metadata)?;
                self.reject_shared_directory(&transaction, key, &directory)?;
                files::check_delete_tree(&directory)?;
                self.publish(
                    transaction,
                    &project,
                    Operation::Delete,
                    &directory,
                    |stage, _| {
                        fs::rename(&directory, stage)?;
                        Ok(())
                    },
                )?;
            }
        }
        Ok(Some(project))
    }

    pub fn set_run_state(&self, key: &str, project_id: &str, state: &str) -> Result<()> {
        self.workspace_connection(key)?.execute(
            "UPDATE projects SET run_state=?, updated_at=? WHERE workspace_key=? AND id=?",
            params![state, now(), key, project_id],
        )?;
        Ok(())
    }

    pub fn save_result_summary(
        &self,
        key: &str,
        project_id: &str,
        summary: Option<&Value>,
    ) -> Result<()> {
        let encoded = summary.map(serde_json::to_string).transpose()?;
        self.workspace_connection(key)?.execute(
            "UPDATE projects SET result_summary_json=?, updated_at=? WHERE workspace_key=? AND id=?",
            params![encoded, now(), key, project_id],
        )?;
        Ok(())
    }

    /// Resolve and create a catalog-owned directory, using the current row.
    pub fn project_path(&self, key: &str, project: &Project) -> Result<PathBuf> {
        validate_workspace(key)?;
        if key != project.workspace_key {
            return Err(Error::ProjectNotFound);
        }
        let mut connection = self.connect()?;
        let transaction = self.transaction(&mut connection)?;
        let current = required_project(&transaction, key, &project.id)?;
        let path = self.directory(&current, true)?;
        transaction.commit()?;
        Ok(path)
    }

    /// Rebuild the project-local plan.json using the authoritative catalog.
    pub fn sync_plan_mirror(&self, key: &str, project_id: &str) -> Result<bool> {
        validate_workspace(key)?;
        let mut connection = self.connect()?;
        let transaction = self.transaction(&mut connection)?;
        let Some(project) = get_project(&transaction, key, project_id)? else {
            return Ok(false);
        };
        self.publish_plan(transaction, &project, &project.plan)?;
        Ok(true)
    }

    pub fn storage_usage(&self, key: &str) -> Result<u64> {
        let _connection = self.workspace_connection(key)?;
        files::storage_usage(&self.workspace_directory(key, false)?)
    }

    fn publish_new_project(
        &self,
        transaction: Transaction<'_>,
        project: Project,
        copy_from: Option<&Path>,
    ) -> Result<Project> {
        let workspace = self.workspace_directory(&project.workspace_key, true)?;
        files::checked_directory(&workspace, ["projects"], true)?;
        let destination = self.directory(&project, false)?;
        if files::metadata(&destination)?.is_some() {
            return Err(Error::UnsafeStoragePath);
        }
        set_active(&transaction, &project.workspace_key, &project.id)?;
        self.publish(
            transaction,
            &project,
            Operation::Create,
            &destination,
            |stage, ready| {
                fs::create_dir(stage)?;
                if let Some(source) = copy_from {
                    files::copy_terrain(source, stage)?;
                }
                files::write_new_plan(&stage.join("plan.json"), &project.plan)?;
                ready()?;
                fs::rename(stage, &destination)?;
                Ok(())
            },
        )?;
        Ok(project)
    }

    fn publish_plan(
        &self,
        transaction: Transaction<'_>,
        project: &Project,
        plan: &Value,
    ) -> Result<()> {
        let destination = self.directory(project, true)?.join("plan.json");
        self.publish(
            transaction,
            project,
            Operation::Plan,
            &destination,
            |stage, ready| {
                files::write_new_plan(stage, plan)?;
                ready()?;
                fs::rename(stage, &destination)?;
                Ok(())
            },
        )
    }

    fn reject_shared_directory(
        &self,
        connection: &Connection,
        key: &str,
        path: &Path,
    ) -> Result<()> {
        let path = fs::canonicalize(path)?;
        let workspace = self.workspace_directory(key, false)?;
        let container = workspace.join("projects");
        if let Ok(container) = fs::canonicalize(container) {
            if path == container {
                return Err(Error::UnsafeStoragePath);
            }
        }
        let mut statement = connection
            .prepare("SELECT directory FROM projects WHERE workspace_key=? AND directory<>'.'")?;
        let directories = statement.query_map([key], |row| row.get::<_, String>(0))?;
        for directory in directories {
            let directory = directory?;
            let checked = files::directory_parts(&directory)
                .and_then(|parts| files::checked_directory(&workspace, parts, false));
            let other = match checked {
                Ok(other) => other,
                Err(Error::UnsafeStoragePath) => continue,
                Err(error) => return Err(error),
            };
            match fs::canonicalize(other) {
                Ok(other) if path.starts_with(&other) || other.starts_with(&path) => {
                    return Err(Error::UnsafeStoragePath)
                }
                Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error.into()),
                _ => {}
            }
        }
        Ok(())
    }

    fn workspace_directory(&self, key: &str, create: bool) -> Result<PathBuf> {
        files::checked_directory(&self.data_root, [key], create)
    }

    fn directory(&self, project: &Project, create: bool) -> Result<PathBuf> {
        let parts = files::directory_parts(&project.directory)?;
        let workspace = self.workspace_directory(&project.workspace_key, create)?;
        files::checked_directory(&workspace, parts, create)
    }

    fn connect(&self) -> Result<Connection> {
        let connection = Connection::open(&self.database)?;
        connection.busy_timeout(Duration::from_secs(10))?;
        connection.pragma_update(None, "foreign_keys", true)?;
        Ok(connection)
    }

    fn workspace_connection(&self, key: &str) -> Result<Connection> {
        validate_workspace(key)?;
        let mut connection = self.connect()?;
        if self.journal_exists()? {
            self.transaction(&mut connection)?.commit()?;
        }
        Ok(connection)
    }
}

fn validate_workspace(key: &str) -> Result<()> {
    if valid_workspace_key(key) {
        Ok(())
    } else {
        Err(Error::InvalidWorkspace)
    }
}

fn require_plan(plan: &Value) -> Result<()> {
    if plan.is_object() {
        Ok(())
    } else {
        Err(Error::InvalidPlan)
    }
}

fn write_transaction(connection: &mut Connection) -> rusqlite::Result<Transaction<'_>> {
    connection.transaction_with_behavior(TransactionBehavior::Immediate)
}

fn active_id(connection: &Connection, key: &str) -> rusqlite::Result<Option<String>> {
    connection
        .query_row(
            "SELECT active_project_id FROM workspace_state WHERE workspace_key=?",
            [key],
            |row| row.get(0),
        )
        .optional()
}

fn set_active(connection: &Connection, key: &str, project_id: &str) -> rusqlite::Result<()> {
    connection.execute(
        "INSERT INTO workspace_state(workspace_key, active_project_id) VALUES (?, ?)
         ON CONFLICT(workspace_key) DO UPDATE SET active_project_id=excluded.active_project_id",
        params![key, project_id],
    )?;
    Ok(())
}

fn get_project(connection: &Connection, key: &str, project_id: &str) -> Result<Option<Project>> {
    Ok(connection
        .query_row(
            "SELECT * FROM projects WHERE workspace_key=? AND id=?",
            params![key, project_id],
            decode_project,
        )
        .optional()?)
}

fn required_project(connection: &Connection, key: &str, project_id: &str) -> Result<Project> {
    get_project(connection, key, project_id)?.ok_or(Error::ProjectNotFound)
}

fn insert_project(connection: &Connection, key: &str, name: &str, plan: &Value) -> Result<Project> {
    let project_id = token()?;
    let directory = format!("projects/{}", token()?);
    let timestamp = now();
    connection.execute(
        "INSERT INTO projects (id, workspace_key, name, directory, plan_json, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
        params![project_id, key, name, directory, serde_json::to_string(plan)?, timestamp, timestamp],
    ).map_err(name_conflict)?;
    required_project(connection, key, &project_id)
}

fn decode_project(row: &Row<'_>) -> rusqlite::Result<Project> {
    let plan_index = row.as_ref().column_index("plan_json")?;
    Ok(Project {
        id: row.get("id")?,
        workspace_key: row.get("workspace_key")?,
        name: row.get("name")?,
        directory: row.get("directory")?,
        plan: decode_json(plan_index, &row.get::<_, String>(plan_index)?)?,
        previous_plan: optional_json(row, "previous_plan_json")?,
        result_summary: optional_json(row, "result_summary_json")?,
        run_state: row.get("run_state")?,
        archived: row.get("archived")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
}

fn optional_json(row: &Row<'_>, column: &str) -> rusqlite::Result<Option<Value>> {
    let index = row.as_ref().column_index(column)?;
    let value = row
        .get::<_, Option<String>>(index)?
        .filter(|text| !text.is_empty())
        .map(|text| decode_json(index, &text))
        .transpose()?;
    Ok(value.filter(|value| !value.is_null()))
}

fn decode_json(index: usize, text: &str) -> rusqlite::Result<Value> {
    serde_json::from_str(text).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(index, Type::Text, Box::new(error))
    })
}

fn name_conflict(error: rusqlite::Error) -> Error {
    match &error {
        rusqlite::Error::SqliteFailure(details, _)
            if details.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE =>
        {
            Error::NameInUse
        }
        _ => error.into(),
    }
}

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

fn token() -> Result<String> {
    let mut bytes = [0; 10];
    getrandom::fill(&mut bytes).map_err(|error| io::Error::other(error.to_string()))?;
    let mut hex = String::with_capacity(20);
    for byte in bytes {
        write!(hex, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Ok(hex)
}
