mod identity;
mod legacy;
mod reports;

pub use identity::{
    comparison_mismatch, fingerprint, job_stale_reason, reason, reuse_fingerprint,
    route_fingerprint, CurrentIdentity, FORMAT, REUSE_FIELDS,
};

pub use legacy::{
    python_canonical_json, python_fingerprint, python_mtime_ns, python_reuse_fingerprint,
    python_route_fingerprint, python_terrain_entry, python_terrain_fingerprint, LegacyIdentity,
    PYTHON_MODEL_VERSION,
};

use serde::Serialize;
use serde_json::{json, Value};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;

pub const MAX_RESULT_BYTES: u64 = 100 * 1024 * 1024;
pub const MAX_EXPORT_BYTES: u64 = 100 * 1024 * 1024;
pub const MAX_REPORT_BYTES: u64 = 5 * 1024 * 1024;
pub const MAX_REPORTS_PER_RUN: usize = 20;
const MAX_MANIFEST_BYTES: u64 = 5 * 1024 * 1024;
const CURRENT: &str = "coverage-job.json";
const BYTES: &str = "coverage_store_committed_bytes";
const LAST: &str = "coverage_store_last_index";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("coverage artifact not found")]
    NotFound,
    #[error("unsafe coverage artifact path")]
    UnsafePath,
    #[error("coverage artifact exceeds its byte limit")]
    ByteLimit,
    #[error("coverage run already has 20 reports")]
    ReportLimit,
    #[error("invalid coverage data: {0}")]
    Invalid(&'static str),
    #[error("coverage run state conflict")]
    Conflict,
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Debug)]
pub struct Store {
    root: PathBuf,
    project_id: String,
}

#[derive(Debug, Serialize)]
pub struct Page {
    pub cells: Vec<Value>,
    pub cursor: u64,
    pub next_cursor: Option<u64>,
    pub stored_cells: u64,
    pub requested_cells: u64,
    pub state: String,
    pub stale: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stale_reason: Option<&'static str>,
}

impl Store {
    /// Bind to a trusted project directory resolved by the authenticated catalog.
    pub fn open(root: impl AsRef<Path>, project_id: impl Into<String>) -> Result<Self> {
        let root = root.as_ref();
        check_path(root, true)?;
        let project_id = project_id.into();
        if project_id.is_empty() {
            return Err(Error::Invalid("missing project ID"));
        }
        Ok(Self {
            root: root.canonicalize()?,
            project_id,
        })
    }

    pub fn current(&self) -> Result<Option<Value>> {
        let _lock = self.lock()?;
        Ok(self.read_current()?.map(|job| legacy_status(job, None)))
    }

    pub fn load(&self, id: &str) -> Result<Value> {
        let _lock = self.lock()?;
        Ok(legacy_status(self.load_unlocked(id)?, None))
    }

    pub fn history(&self) -> Result<Vec<Value>> {
        let _lock = self.lock()?;
        Ok(self
            .history_unlocked(None)?
            .into_iter()
            .map(|job| legacy_status(job, None))
            .collect())
    }

    /// Load a manifest with status derived from independently verified Python inputs.
    pub fn load_with_legacy(&self, id: &str, legacy: &LegacyIdentity) -> Result<Value> {
        let _lock = self.lock()?;
        Ok(legacy_status(self.load_unlocked(id)?, Some(legacy)))
    }

    pub fn current_with_legacy(&self, legacy: &LegacyIdentity) -> Result<Option<Value>> {
        let _lock = self.lock()?;
        Ok(self
            .read_current()?
            .map(|job| legacy_status(job, Some(legacy))))
    }

    pub fn history_with_legacy(&self, legacy: &LegacyIdentity) -> Result<Vec<Value>> {
        let _lock = self.lock()?;
        Ok(self
            .history_unlocked(None)?
            .into_iter()
            .map(|job| legacy_status(job, Some(legacy)))
            .collect())
    }

    /// Load two complete runs for comparison on the live terrain fingerprint.
    /// Both runs must come from one fingerprint domain (native or Python) and
    /// one model version. Errors carry a `reason` constant in `Error::Invalid`.
    /// The caller still checks settings, grid and reference compatibility.
    pub fn comparison(
        &self,
        baseline: &str,
        scenario: &str,
        current_terrain: &Value,
    ) -> Result<(Value, Value)> {
        let (baseline, scenario) = self.comparison_unlocked(baseline, scenario, current_terrain)?;
        Ok((legacy_status(baseline, None), legacy_status(scenario, None)))
    }

    fn comparison_unlocked(
        &self,
        baseline: &str,
        scenario: &str,
        current_terrain: &Value,
    ) -> Result<(Value, Value)> {
        let _lock = self.lock()?;
        let baseline = self.load_unlocked(baseline)?;
        let scenario = self.load_unlocked(scenario)?;
        if let Some(reason) =
            comparison_mismatch(&self.project_id, &baseline, &scenario, current_terrain)
        {
            return Err(Error::Invalid(reason));
        }
        if !self.complete_grid(&baseline)? || !self.complete_grid(&scenario)? {
            return Err(Error::Invalid(reason::COMPARISON_GRID));
        }
        Ok((baseline, scenario))
    }

    /// Python-to-Python comparison against a live legacy identity.
    pub fn legacy_comparison(
        &self,
        baseline: &str,
        scenario: &str,
        legacy: &LegacyIdentity,
    ) -> Result<(Value, Value)> {
        if legacy.project_id() != self.project_id {
            return Err(Error::Invalid(reason::COMPARISON_PROJECT));
        }
        let (baseline, scenario) =
            self.comparison_unlocked(baseline, scenario, legacy.terrain())?;
        if !legacy::is_legacy(&baseline) {
            return Err(Error::Invalid(reason::DOMAIN));
        }
        Ok((
            legacy_status(baseline, Some(legacy)),
            legacy_status(scenario, Some(legacy)),
        ))
    }

    /// Start a native run. The previous terminal current run is archived first.
    pub fn start(&self, mut job: Value) -> Result<Value> {
        let _lock = self.lock()?;
        let id = self.validate_job(&job)?.to_owned();
        if !identity::native_model(&job) {
            return Err(Error::Invalid(
                "native runs need a non-Python model version",
            ));
        }
        if !active(&job) {
            return Err(Error::Conflict);
        }
        let previous = self.read_current()?;
        if let Some(old) = &previous {
            if active(old) || old["job_id"] == id {
                return Err(Error::Conflict);
            }
        }
        let archive = self.path(Some("coverage-jobs"), &format!("{id}.json"), true)?;
        if archive.try_exists()? {
            return Err(Error::Conflict);
        }
        if let Some(old) = previous {
            let old_id = self.validate_job(&old)?;
            let path = self.path(Some("coverage-jobs"), &format!("{old_id}.json"), true)?;
            write_json(&path, &old, MAX_MANIFEST_BYTES)?;
        }
        job["result_file"] = json!(result_name(&id));
        job["coverage_store_format"] = json!(FORMAT);
        job["stale"] = json!(false);
        job[BYTES] = json!(0);
        job[LAST] = Value::Null;
        let path = self.result_path(&job, true)?;
        let result = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        let published = result
            .sync_all()
            .map_err(Error::from)
            .and_then(|()| self.publish(&job));
        drop(result);
        if let Err(error) = published {
            let _ = fs::remove_file(path);
            return Err(error);
        }
        Ok(job)
    }

    /// Commit a progress patch and optional row-major chunk under one manifest replacement.
    /// An uncommitted result tail is invisible and is discarded before the next append.
    pub fn progress(&self, id: &str, patch: Value, cells: &[Value]) -> Result<Value> {
        self.update(id, patch, cells, false)
    }

    /// Publish a terminal patch and optional final chunk. Complete runs must have a full grid.
    pub fn finish(&self, id: &str, patch: Value, cells: &[Value]) -> Result<Value> {
        self.update(id, patch, cells, true)
    }

    fn update(&self, id: &str, patch: Value, cells: &[Value], terminal: bool) -> Result<Value> {
        let _lock = self.lock()?;
        let mut job = self
            .read_current()?
            .filter(|job| job["job_id"] == id)
            .ok_or(Error::NotFound)?;
        if !active(&job) || job["coverage_store_format"] != FORMAT {
            return Err(Error::Conflict);
        }
        for (key, value) in patch.as_object().ok_or(Error::Invalid("progress patch"))? {
            if [
                "job_id",
                "project_id",
                "result_file",
                "coverage_store_format",
                BYTES,
                LAST,
            ]
            .contains(&key.as_str())
                || ((identity::REUSE_FIELDS.contains(&key.as_str())
                    || ["settings", "radio_settings"].contains(&key.as_str()))
                    && job
                        .get(key)
                        .is_some_and(|previous| !previous.is_null() && previous != value))
            {
                return Err(Error::Invalid("immutable run identity"));
            }
            job[key] = value.clone();
        }
        let state = job["state"].as_str().ok_or(Error::Invalid("state"))?;
        if (terminal
            && !matches!(
                state,
                "complete" | "cancelled" | "failed" | "stale" | "interrupted"
            ))
            || (!terminal && !active(&job))
        {
            return Err(Error::Conflict);
        }
        let committed = job[BYTES]
            .as_u64()
            .ok_or(Error::Invalid("result watermark"))?;
        if committed > MAX_RESULT_BYTES {
            return Err(Error::ByteLimit);
        }
        let mut last = job[LAST].as_u64();
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(self.result_path(&job, false)?)?;
        if file.metadata()?.len() < committed {
            return Err(Error::Invalid("truncated committed results"));
        }
        if !cells.is_empty() {
            let grid = identity::grid_size(&job).ok_or(Error::Invalid("grid dimensions"))?;
            for cell in cells {
                let index = native_cell_index(cell)?;
                if index >= grid || last.is_some_and(|last| last >= index) {
                    return Err(Error::Invalid("cell index order or range"));
                }
                last = Some(index);
            }
            file.set_len(committed)?;
            file.seek(SeekFrom::Start(committed))?;
            let mut writer = LimitedWriter::new(&mut file, MAX_RESULT_BYTES - committed);
            let written = match writer.json_line(cells) {
                Ok(written) => written,
                Err(error) => {
                    file.set_len(committed)?;
                    return Err(error);
                }
            };
            file.sync_all()?;
            job[BYTES] = json!(committed + written);
            job[LAST] = json!(last);
        }
        if job["state"] == "complete"
            && (reuse_fingerprint(&job).is_none() || !self.complete_grid(&job)?)
        {
            return Err(Error::Invalid("incomplete coverage grid"));
        }
        self.publish(&job)?;
        Ok(job)
    }

    /// Mark the persisted active run interrupted after acquiring worker ownership on startup.
    pub fn recover_interrupted(&self) -> Result<Option<Value>> {
        let _lock = self.lock()?;
        let Some(mut job) = self.read_current()? else {
            return Ok(None);
        };
        if active(&job) {
            if job["coverage_store_format"] == FORMAT {
                self.discard_interrupted_tail(&job)?;
            }
            job["state"] = json!("interrupted");
            if job["coverage_store_format"] != FORMAT {
                job["stale"] = json!(true);
            }
            job["stage"] =
                json!("Coverage job interrupted by restart; calculate again to continue.");
            self.publish(&job)?;
        }
        Ok(Some(legacy_status(job, None)))
    }

    fn discard_interrupted_tail(&self, job: &Value) -> Result<()> {
        let Some(committed) = job[BYTES].as_u64().filter(|size| *size <= MAX_RESULT_BYTES) else {
            return Ok(());
        };
        let path = match self.result_path(job, false) {
            Ok(path) => path,
            Err(Error::UnsafePath) => return Ok(()),
            Err(error) => return Err(error),
        };
        let file = match OpenOptions::new().write(true).open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        if file.metadata()?.len() > committed {
            file.set_len(committed)?;
            file.sync_all()?;
        }
        Ok(())
    }
    pub fn page(
        &self,
        id: &str,
        cursor: u64,
        limit: usize,
        current: Option<&CurrentIdentity>,
    ) -> Result<Page> {
        self.page_with_identities(id, cursor, limit, current, None)
    }

    pub fn page_with_identities(
        &self,
        id: &str,
        cursor: u64,
        limit: usize,
        current: Option<&CurrentIdentity>,
        legacy: Option<&LegacyIdentity>,
    ) -> Result<Page> {
        if !(1..=256).contains(&limit) {
            return Err(Error::Invalid("page size must be 1 through 256"));
        }
        let _lock = self.lock()?;
        let job = self.load_unlocked(id)?;
        let mut cells = Vec::new();
        let mut stored = 0;
        self.scan(&job, |cell| {
            if stored >= cursor && cells.len() < limit {
                cells.push(cell);
            }
            stored += 1;
            Ok(())
        })?;
        let end = cursor.saturating_add(cells.len() as u64);
        let stale_reason = job_stale_reason(&job, current, legacy);
        Ok(Page {
            cells,
            cursor,
            next_cursor: (end < stored).then_some(end),
            stored_cells: stored,
            requested_cells: job["requested_cells"]
                .as_u64()
                .or_else(|| job["total"].as_u64())
                .unwrap_or(0),
            state: job["state"]
                .as_str()
                .ok_or(Error::Invalid("state"))?
                .to_owned(),
            stale: stale_reason.is_some(),
            stale_reason,
        })
    }

    /// Visit stored cells under the project lock. The visitor must not reenter this store.
    pub fn visit_cells(&self, id: &str, visitor: impl FnMut(Value) -> Result<()>) -> Result<()> {
        let _lock = self.lock()?;
        self.scan(&self.load_unlocked(id)?, visitor)
    }

    /// Look at current and up to 50 recent archived runs, skipping unusable results.
    pub fn find_reusable(
        &self,
        expected: &Value,
        current: &CurrentIdentity,
    ) -> Result<Option<Value>> {
        if expected["project_id"] != self.project_id || !current.matches(expected) {
            return Ok(None);
        }
        let Some(fingerprint) = reuse_fingerprint(expected) else {
            return Ok(None);
        };
        let _lock = self.lock()?;
        Ok(self.history_unlocked(Some(50))?.into_iter().find(|job| {
            job["state"] == "complete"
                && current.matches(job)
                && reuse_fingerprint(job).as_ref() == Some(&fingerprint)
                && self.complete_grid(job).unwrap_or(false)
        }))
    }

    fn complete_grid(&self, job: &Value) -> Result<bool> {
        let Some(expected) = identity::grid_size(job) else {
            return Ok(false);
        };
        let mut count = 0;
        let mut complete = true;
        self.scan(job, |cell| {
            if job["coverage_store_format"] != FORMAT {
                native_cell_index(&cell)?;
            }
            complete &= cell["index"].as_u64() == Some(count) && cell["state"] != "not_evaluated";
            count += 1;
            Ok(())
        })?;
        Ok(complete && count == expected)
    }

    fn scan(&self, job: &Value, mut visitor: impl FnMut(Value) -> Result<()>) -> Result<()> {
        let path = self.result_path(job, false)?;
        let file = File::open(path)?;
        let size = file.metadata()?.len();
        let committed = if job["coverage_store_format"] == FORMAT {
            job[BYTES]
                .as_u64()
                .ok_or(Error::Invalid("result watermark"))?
        } else {
            size
        };
        if size > MAX_RESULT_BYTES || committed > MAX_RESULT_BYTES {
            return Err(Error::ByteLimit);
        }
        if committed > size {
            return Err(Error::Invalid("truncated committed results"));
        }
        let mut reader = BufReader::new(file.take(committed));
        let mut line = Vec::new();
        let mut previous = None;
        while reader.read_until(b'\n', &mut line)? != 0 {
            if !line.ends_with(b"\n") {
                return Err(Error::Invalid("unterminated JSONL chunk"));
            }
            let cells: Vec<Value> = serde_json::from_slice(&line)?;
            for cell in cells {
                let index = if job["coverage_store_format"] == FORMAT {
                    native_cell_index(&cell)?
                } else {
                    cell_index(&cell)?
                };
                if previous.is_some_and(|last| last >= index)
                    || job["requested_cells"]
                        .as_u64()
                        .is_some_and(|count| index >= count)
                {
                    return Err(Error::Invalid("cell index order or range"));
                }
                previous = Some(index);
                visitor(cell)?;
            }
            line.clear();
        }
        Ok(())
    }

    fn validate_job<'a>(&self, job: &'a Value) -> Result<&'a str> {
        if job["project_id"] != self.project_id {
            return Err(Error::NotFound);
        }
        let id = job["job_id"]
            .as_str()
            .ok_or(Error::Invalid("missing job ID"))?;
        validate_id(id)?;
        if !job["state"].is_string() {
            return Err(Error::Invalid("missing state"));
        }
        Ok(id)
    }

    fn read_current(&self) -> Result<Option<Value>> {
        let path = self.path(None, CURRENT, false)?;
        match read_json(&path, MAX_MANIFEST_BYTES) {
            Ok(job) => {
                self.validate_job(&job)?;
                Ok(Some(job))
            }
            Err(Error::Io(error)) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn load_unlocked(&self, id: &str) -> Result<Value> {
        validate_id(id)?;
        if let Some(job) = self.read_current()?.filter(|job| job["job_id"] == id) {
            return Ok(job);
        }
        self.read_archive(id)
    }

    fn read_archive(&self, id: &str) -> Result<Value> {
        let path = self.path(Some("coverage-jobs"), &format!("{id}.json"), false)?;
        let job = read_json(&path, MAX_MANIFEST_BYTES)?;
        if self.validate_job(&job)? != id {
            return Err(Error::NotFound);
        }
        Ok(job)
    }

    fn history_unlocked(&self, max_archives: Option<usize>) -> Result<Vec<Value>> {
        let mut jobs = Vec::new();
        if let Some(job) = self.read_current()? {
            jobs.push(job);
        }
        let current_id = jobs
            .first()
            .and_then(|job| job["job_id"].as_str())
            .map(str::to_owned);
        let mut entries = self.entries("coverage-jobs")?;
        entries.sort_by_key(|entry| {
            std::cmp::Reverse(entry.metadata().and_then(|meta| meta.modified()).ok())
        });
        for entry in entries
            .into_iter()
            .filter(|entry| {
                let name = entry.file_name();
                name.to_str()
                    .and_then(|name| name.strip_suffix(".json"))
                    .is_some_and(|id| validate_id(id).is_ok() && current_id.as_deref() != Some(id))
            })
            .take(max_archives.unwrap_or(usize::MAX))
        {
            let name = entry.file_name();
            let Some(id) = name.to_str().and_then(|name| name.strip_suffix(".json")) else {
                continue;
            };
            if let Ok(job) = self.read_archive(id) {
                jobs.push(job);
            }
        }
        Ok(jobs)
    }

    fn result_path(&self, job: &Value, create_dir: bool) -> Result<PathBuf> {
        let id = self.validate_job(job)?;
        if job["result_file"] != result_name(id) {
            return Err(Error::UnsafePath);
        }
        self.path(Some("coverage-results"), &format!("{id}.jsonl"), create_dir)
    }

    fn publish(&self, job: &Value) -> Result<()> {
        write_json(&self.path(None, CURRENT, false)?, job, MAX_MANIFEST_BYTES)
    }

    fn lock(&self) -> Result<File> {
        let path = self.path(None, ".coverage-store.lock", false)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        file.lock()?;
        Ok(file)
    }

    fn path(&self, directory: Option<&str>, name: &str, create_dir: bool) -> Result<PathBuf> {
        check_path(&self.root, true)?;
        let parent = match directory {
            Some(directory) => {
                let parent = self.root.join(directory);
                if create_dir && !check_path(&parent, true)? {
                    match fs::create_dir(&parent) {
                        Ok(()) => (),
                        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => (),
                        Err(error) => return Err(error.into()),
                    }
                }
                check_path(&parent, true)?;
                parent
            }
            None => self.root.clone(),
        };
        let path = parent.join(name);
        check_path(&path, false)?;
        Ok(path)
    }

    fn entries(&self, directory: &str) -> Result<Vec<fs::DirEntry>> {
        check_path(&self.root, true)?;
        let path = self.root.join(directory);
        if !check_path(&path, true)? {
            return Ok(Vec::new());
        }
        Ok(fs::read_dir(path)?.collect::<io::Result<_>>()?)
    }
}

fn check_path(path: &Path, directory: bool) -> Result<bool> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return Err(Error::UnsafePath);
        }
    }
    if !(if directory {
        metadata.is_dir()
    } else {
        metadata.is_file()
    }) {
        return Err(Error::UnsafePath);
    }
    Ok(true)
}

/// Python runs are always reported stale with a reason. Native runs are untouched;
/// derive their status with `job_stale_reason` at response time.
fn legacy_status(mut job: Value, identity: Option<&LegacyIdentity>) -> Value {
    if job["coverage_store_format"] != FORMAT {
        let reason = job_stale_reason(&job, None, identity);
        job["stale"] = json!(true);
        job["stale_reason"] = json!(reason.unwrap_or(reason::UNSUPPORTED_FORMAT));
    }
    job
}

fn active(job: &Value) -> bool {
    matches!(job["state"].as_str(), Some("queued" | "running"))
}

fn hex_digit(byte: u8) -> bool {
    byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
}

fn validate_id(id: &str) -> Result<()> {
    if id.len() != 32 || !id.bytes().all(hex_digit) {
        return Err(Error::UnsafePath);
    }
    Ok(())
}

fn result_name(id: &str) -> String {
    format!("coverage-results/{id}.jsonl")
}

fn native_cell_index(cell: &Value) -> Result<u64> {
    if !["x", "y", "latitude", "longitude"]
        .iter()
        .all(|key| cell[key].as_f64().is_some())
        || !cell["sources"].as_array().is_some_and(|sources| {
            sources.len() <= 64 && sources.iter().all(|source| source["source_id"].is_string())
        })
    {
        return Err(Error::Invalid("malformed coverage cell"));
    }
    cell_index(cell)
}

fn cell_index(cell: &Value) -> Result<u64> {
    if !matches!(
        cell["state"].as_str(),
        Some(
            "covered"
                | "uncovered"
                | "unknown_terrain"
                | "unresolved"
                | "not_evaluated"
                | "outside_area"
        )
    ) {
        return Err(Error::Invalid("unknown coverage cell state"));
    }
    cell["index"]
        .as_u64()
        .ok_or(Error::Invalid("missing cell index"))
}

fn read_json(path: &Path, cap: u64) -> Result<Value> {
    let file = File::open(path)?;
    let mut bytes = Vec::new();
    file.take(cap + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > cap {
        return Err(Error::ByteLimit);
    }
    match serde_json::from_slice(&bytes) {
        Ok(value) => Ok(value),
        // Python artifacts may hold non-finite floats. Native artifacts never
        // do, so any file carrying the native format marker stays strict.
        Err(error) => legacy::python_nonfinite_to_null(&bytes)
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .filter(|value| value.is_object() && value.get("coverage_store_format").is_none())
            .ok_or(Error::Json(error)),
    }
}

fn write_json(path: &Path, value: &Value, cap: u64) -> Result<()> {
    atomic_write(path, cap, |writer| writer.json_line(value).map(|_| ()))
}

/// Atomically publish an export at a trusted caller-selected destination, enforcing 100 MiB.
pub fn write_export(
    path: &Path,
    generate: impl FnOnce(&mut dyn Write) -> io::Result<()>,
) -> Result<()> {
    atomic_write(path, MAX_EXPORT_BYTES, |writer| Ok(generate(writer)?))
}

fn atomic_write(
    path: &Path,
    cap: u64,
    generate: impl FnOnce(&mut LimitedWriter<&mut File>) -> Result<()>,
) -> Result<()> {
    let parent = path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    check_path(parent, true)?;
    check_path(path, false)?;
    let mut temp = NamedTempFile::new_in(parent)?;
    let mut writer = LimitedWriter::new(temp.as_file_mut(), cap);
    let result = generate(&mut writer);
    if writer.exceeded {
        return Err(Error::ByteLimit);
    }
    result?;
    temp.as_file().sync_all()?;
    temp.persist(path).map_err(|error| error.error)?;
    Ok(())
}

struct LimitedWriter<W> {
    inner: W,
    cap: u64,
    written: u64,
    exceeded: bool,
}

impl<W: Write> LimitedWriter<W> {
    fn new(inner: W, cap: u64) -> Self {
        Self {
            inner,
            cap,
            written: 0,
            exceeded: false,
        }
    }

    fn json_line(&mut self, value: &(impl Serialize + ?Sized)) -> Result<u64> {
        let result = serde_json::to_writer(&mut *self, value)
            .map_err(Error::from)
            .and_then(|()| self.write_all(b"\n").map_err(Error::from));
        if self.exceeded {
            return Err(Error::ByteLimit);
        }
        result?;
        Ok(self.written)
    }
}

impl<W: Write> Write for LimitedWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() as u64 > self.cap - self.written {
            self.exceeded = true;
            return Err(io::Error::other("coverage byte limit"));
        }
        let written = self.inner.write(bytes)?;
        self.written += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Lowercase hexadecimal encoding of a digest.
pub(crate) fn lower_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}
