use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fmt::Write,
    fs, io,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::UNIX_EPOCH,
};

use repeaternet_coverage::CoverageMetricsCache;
use repeaternet_optimizer::LinkMetricsCache;
use repeaternet_project_store::{Project, ProjectStore};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex as AsyncMutex;

type SessionLock = Arc<AsyncMutex<Option<Session>>>;

#[derive(Clone)]
pub(crate) struct AppState {
    pub store: ProjectStore,
    pub data_directory: PathBuf,
    pub terrain_limits: repeaternet_providers::sources::TileLimits,
    pub providers: Arc<Mutex<Arc<repeaternet_providers::sources::ProviderRegistry>>>,
    #[cfg(test)]
    pub overpass_endpoint: Option<String>,
    #[cfg(test)]
    pub coverage_terrain_resolution: Option<f64>,
    pub corescope_endpoint: String,
    pub defaults: Arc<Value>,
    pub scheduler: Arc<crate::route_http::Scheduler>,
    pub uploads: Arc<tokio::sync::Semaphore>,
    pub cache: Arc<LinkMetricsCache>,
    pub coverage_cache: Arc<CoverageMetricsCache>,
    pub live_terrain: Arc<crate::live_terrain::LiveTerrainCache>,
    pub live_coverage_workers: Arc<tokio::sync::Semaphore>,
    // Acquire a workspace lock before submitting blocking work, so waiting
    // requests do not occupy blocking threads or serialize other workspaces.
    sessions: Arc<Mutex<HashMap<String, SessionLock>>>,
}

impl AppState {
    pub fn new(store: ProjectStore, data_directory: &Path) -> Self {
        use repeaternet_providers::sources::ProviderRegistry;
        let providers = ProviderRegistry::load(data_directory).unwrap_or_else(|error| {
            eprintln!("Could not open data sources: {error}");
            ProviderRegistry::builtin()
        });
        Self {
            store,
            data_directory: data_directory.into(),
            terrain_limits: Default::default(),
            providers: Arc::new(Mutex::new(Arc::new(providers))),
            #[cfg(test)]
            overpass_endpoint: None,
            #[cfg(test)]
            coverage_terrain_resolution: None,
            corescope_endpoint: repeaternet_providers::corescope::DEFAULT_BASE_URL.into(),
            defaults: Arc::new(
                serde_json::from_str(include_str!("dashboard_defaults.json"))
                    .expect("embedded dashboard defaults are valid JSON"),
            ),
            sessions: Arc::default(),
            scheduler: Arc::new(crate::route_http::Scheduler::from_env()),
            uploads: Arc::new(tokio::sync::Semaphore::new(2)),
            live_terrain: Arc::new(crate::live_terrain::LiveTerrainCache::from_env()),
            live_coverage_workers: Arc::new(tokio::sync::Semaphore::new(2)),
            cache: Arc::new(LinkMetricsCache::new(
                std::env::var("RF_PLANNER_RF_CACHE_ENTRIES")
                    .ok()
                    .and_then(|value| value.parse::<usize>().ok())
                    .unwrap_or(50_000)
                    .max(1),
            )),
            coverage_cache: Arc::new(CoverageMetricsCache::new(
                std::env::var("RF_PLANNER_COVERAGE_CACHE_ENTRIES")
                    .ok()
                    .and_then(|value| value.trim().parse::<i128>().ok())
                    .and_then(|value| usize::try_from(value.max(1)).ok())
                    .unwrap_or(50_000),
            )),
        }
    }

    pub fn provider_snapshot(
        &self,
    ) -> crate::api::ApiResult<Arc<repeaternet_providers::sources::ProviderRegistry>> {
        let mut registry = self.providers.lock().expect("provider registry lock");
        *registry = Arc::new(
            repeaternet_providers::sources::ProviderRegistry::load(&self.data_directory)
                .map_err(crate::api::ApiError::internal)?,
        );
        Ok(registry.clone())
    }

    pub fn session(&self, key: &str) -> SessionLock {
        self.sessions
            .lock()
            .expect("workspace registry poisoned")
            .entry(key.to_owned())
            .or_default()
            .clone()
    }
}

// Project selection and publication of route/coverage state share this lock.
// Workers must reacquire it and verify their captured identity before publishing.
pub(crate) struct Session {
    pub project_id: String,
    pub input_revision: u64,
    pub snapshot_version: u64,
    pub job_id: Option<String>,
    pub route_job: Value,
    pub route_result: Option<Value>,
    pub route_alternatives: Vec<crate::route_output::Alternative>,
    pub route_control: Option<Arc<crate::route_http::Control>>,
    pub route_identity: Option<(String, Value)>,
    pub route_resolution_m: Option<f64>,
    pub route_surface_available: Option<bool>,
    pub coverage_job: Option<Value>,
    pub coverage_control: Option<Arc<crate::route_http::Control>>,
    pub coverage_recovered: bool,
    pub upload: Arc<tokio::sync::Semaphore>,
    pub coverage_terrain_control: Option<Arc<crate::route_http::Control>>,
    pub coverage_terrain_job: Option<Value>,
    pub route_terrain_estimate_registry: Option<String>,
    pub coverage_terrain_estimate_registry: Option<String>,
    pub inspection_job: Option<Value>,
    pub inspection_cancel: Option<Arc<AtomicBool>>,
    pub inspection_control: Option<Arc<crate::route_http::Control>>,
}

impl Session {
    pub fn open(project: &Project, previous_revision: Option<u64>) -> Self {
        let (state, stage) = match project.run_state.as_str() {
            "interrupted" | "queued" | "running" | "preparing" => (
                "interrupted",
                "Previous search interrupted by restart; rerun to recover.",
            ),
            _ if previous_revision.is_some() => ("idle", "Project opened"),
            _ => ("idle", "Ready to plan"),
        };
        Self {
            project_id: project.id.clone(),
            input_revision: previous_revision.map_or_else(
                || {
                    u64::from(
                        project
                            .plan
                            .as_object()
                            .is_some_and(|plan| !plan.is_empty()),
                    )
                },
                |revision| revision + 1,
            ),
            snapshot_version: 0,
            job_id: None,
            route_job: json!({"state": state, "stage": stage, "done": 0, "total": 1}),
            route_result: None,
            route_alternatives: Vec::new(),
            route_control: None,
            route_identity: None,
            route_resolution_m: None,
            route_surface_available: None,
            coverage_job: None,
            coverage_control: None,
            coverage_recovered: false,
            upload: Arc::new(tokio::sync::Semaphore::new(1)),
            coverage_terrain_job: None,
            route_terrain_estimate_registry: None,
            coverage_terrain_estimate_registry: None,
            coverage_terrain_control: None,
            inspection_job: None,
            inspection_cancel: None,
            inspection_control: None,
        }
    }

    pub fn calculations_active(&self) -> bool {
        job_active(Some(&self.route_job), &["queued", "running", "preparing"])
            || job_active(self.coverage_terrain_job.as_ref(), &["queued", "preparing"])
            || job_active(self.coverage_job.as_ref(), &["queued", "running"])
    }

    pub fn invalidate_route(&mut self) {
        self.input_revision += 1;
        self.snapshot_version = 0;
        self.job_id = None;
        self.route_job = json!({"state": "idle", "stage": "Ready to plan", "done": 0, "total": 1});
        self.route_result = None;
        self.route_alternatives.clear();
        self.route_control = None;
        self.route_identity = None;
        self.route_resolution_m = None;
        self.route_surface_available = None;
    }

    pub fn inspection_active(&self) -> bool {
        job_active(self.inspection_job.as_ref(), &["queued", "running"])
    }

    pub fn cancel_inspection(&self) {
        if self.inspection_active() {
            if let Some(cancel) = &self.inspection_cancel {
                cancel.store(true, Ordering::Relaxed);
            }
            if let Some(control) = &self.inspection_control {
                control.cancel(false);
            }
        }
    }

    pub fn job_payload(&self) -> Value {
        let mut job = self.route_job.clone();
        job["job_id"] = json!(self.job_id);
        job["input_revision"] = json!(self.input_revision);
        job["snapshot_version"] = json!(self.snapshot_version);
        job
    }
}

fn job_active(job: Option<&Value>, states: &[&str]) -> bool {
    job.and_then(|job| job["state"].as_str())
        .is_some_and(|state| states.contains(&state))
}

pub(crate) fn project_item(project: Project) -> Value {
    json!({"id": project.id, "name": project.name, "updated_at": project.updated_at})
}

pub(crate) fn opened_project(project: Project) -> Value {
    json!({"id": project.id, "name": project.name, "plan": project.plan})
}

pub(crate) fn historical_result(
    project: &Project,
    directory: &Path,
    terrain: &TerrainFiles,
) -> Option<Value> {
    let mut summary = project.result_summary.clone()?;
    let summary_fields = summary.as_object_mut()?;
    let recorded_plan = summary_fields
        .get("plan_fingerprint")
        .and_then(Value::as_str);
    let stale = (recorded_plan != Some(&plan_fingerprint(&project.plan, true))
        && recorded_plan != Some(&plan_fingerprint(&project.plan, false)))
        || terrain
            .fingerprint(directory, None)
            .is_none_or(|current| summary_fields.get("terrain_fingerprint") != Some(&current));
    summary_fields.insert("stale".into(), json!(stale));
    Some(summary)
}

pub(crate) fn route_inputs_changed(before: &Value, after: &Value) -> bool {
    plan_fingerprint(before, true) != plan_fingerprint(after, true)
}

// The Python catalog hashes json.dumps(sort_keys=True, separators=(",", ":"))
// with ASCII escaping and Python float formatting. Preserve arbitrary integers.
pub(crate) fn plan_fingerprint(plan: &Value, route_only: bool) -> String {
    lower_hex(&Sha256::digest(canonical_json(plan, route_only).as_bytes()))
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

pub(crate) fn canonical_json(plan: &Value, route_only: bool) -> String {
    let mut encoded = String::new();
    write_canonical_json(&mut encoded, plan, route_only);
    encoded
}

pub(crate) fn random_id<const N: usize>() -> Result<String, crate::api::ApiError> {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes).map_err(crate::api::ApiError::internal)?;
    let mut id = String::with_capacity(N * 2);
    for byte in bytes {
        write!(&mut id, "{byte:02x}").unwrap();
    }
    Ok(id)
}

fn write_canonical_json(output: &mut String, value: &Value, route_only: bool) {
    match value {
        Value::Object(fields) => {
            let mut fields: Vec<_> = fields
                .iter()
                .filter(|(key, _)| {
                    !route_only
                        || !matches!(
                            key.as_str(),
                            "coverage" | "coverage_targets" | "resolved_search"
                        )
                })
                .collect();
            fields.sort_unstable_by_key(|(key, _)| *key);
            output.push('{');
            for (index, (key, value)) in fields.into_iter().enumerate() {
                if index != 0 {
                    output.push(',');
                }
                write_ascii_string(output, key);
                output.push(':');
                write_canonical_json(output, value, false);
            }
            output.push('}');
        }
        Value::Array(items) => {
            output.push('[');
            for (index, item) in items.iter().enumerate() {
                if index != 0 {
                    output.push(',');
                }
                write_canonical_json(output, item, false);
            }
            output.push(']');
        }
        Value::String(value) => write_ascii_string(output, value),
        Value::Number(number) => {
            let raw = number.as_str();
            if raw.contains(['.', 'e', 'E'])
                && let Ok(value) = raw.parse::<f64>()
            {
                let formatted = format!("{value:?}");
                if let Some((mantissa, exponent)) = formatted.split_once('e') {
                    let exponent: i32 = exponent.parse().expect("float exponent is an integer");
                    write!(output, "{mantissa}e{exponent:+03}").unwrap();
                } else {
                    output.push_str(match formatted.as_str() {
                        "inf" => "Infinity",
                        "-inf" => "-Infinity",
                        _ => &formatted,
                    });
                }
            } else {
                output.push_str(if raw == "-0" { "0" } else { raw });
            }
        }
        _ => write!(output, "{value}").unwrap(),
    }
}

fn write_ascii_string(output: &mut String, value: &str) {
    // serde_json already escapes ASCII controls, quotes, and backslashes.
    for unit in serde_json::to_string(value)
        .expect("a string serializes")
        .encode_utf16()
    {
        if unit < 0x7f {
            output.push(char::from(unit as u8));
        } else {
            write!(output, "\\u{unit:04x}").unwrap();
        }
    }
}

#[derive(Default)]
pub(crate) struct TerrainFiles {
    dtm: Vec<PathBuf>,
    dom: Vec<PathBuf>,
}

impl TerrainFiles {
    pub fn paths(&self, kind: &str) -> &[PathBuf] {
        match kind {
            "dtm" => &self.dtm,
            "dom" => &self.dom,
            _ => unreachable!("validated terrain product"),
        }
    }
    pub fn read(project: &Path, supplemental: bool) -> Self {
        let mut terrain = Self::default();
        let generations = if supplemental {
            "coverage-terrain-generations"
        } else {
            "terrain-generations"
        };
        let dirs = entries(&project.join(generations), true).filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("generation-")
        });
        for generation in dirs {
            terrain.add(&generation.path(), project);
        }
        if !supplemental {
            terrain.add(project, project);
        }
        terrain.dtm.sort();
        terrain.dom.sort();
        terrain
    }

    fn add(&mut self, directory: &Path, project: &Path) {
        for (kind, paths) in [("dtm", &mut self.dtm), ("dom", &mut self.dom)] {
            paths.extend(
                entries(&directory.join(kind), false)
                    .filter(|entry| {
                        entry.path().extension().is_some_and(|ext| {
                            ext == "tif" || (cfg!(windows) && ext.eq_ignore_ascii_case("tif"))
                        })
                    })
                    .map(|entry| {
                        entry
                            .path()
                            .strip_prefix(project)
                            .expect("path is in project")
                            .to_owned()
                    }),
            );
        }
    }

    pub fn names(&self) -> Value {
        fn names(paths: &[PathBuf]) -> Vec<String> {
            paths
                .iter()
                .filter_map(|path| path.file_name())
                .map(|name| name.to_string_lossy().into_owned())
                .collect()
        }
        json!({"dtm": names(&self.dtm), "dom": names(&self.dom)})
    }

    pub fn sources(&self, project: &Path) -> Vec<repeaternet_providers::sources::SourceInfo> {
        use repeaternet_providers::sources::{Product, SourceInfo};
        let mut sources = BTreeMap::new();
        for (product, paths) in [(Product::Dtm, &self.dtm), (Product::Dom, &self.dom)] {
            let roots: BTreeSet<_> = paths
                .iter()
                .filter_map(|path| path.parent()?.parent())
                .filter(|root| !root.as_os_str().is_empty())
                .collect();
            for root in roots {
                let file = project.join(root).join("sources.json");
                let Some(bytes) = fs::symlink_metadata(&file)
                    .ok()
                    .filter(|meta| meta.is_file() && !linked(meta) && meta.len() <= 2 * 1024 * 1024)
                    .and_then(|_| fs::read(file).ok())
                else {
                    continue;
                };
                let Ok(entries) = serde_json::from_slice::<Vec<SourceInfo>>(&bytes) else {
                    continue;
                };
                for source in entries
                    .into_iter()
                    .filter(|source| source.product == product)
                {
                    sources.insert(
                        (
                            source.product,
                            source.id.clone(),
                            source.attribution.clone(),
                        ),
                        source,
                    );
                }
            }
        }
        sources.into_values().collect()
    }

    pub fn fingerprint(&self, project: &Path, supplemental: Option<&Self>) -> Option<Value> {
        let mut entries = Vec::new();
        let sources = std::iter::once(self).chain(supplemental);
        for path in sources.flat_map(|terrain| terrain.dtm.iter().chain(&terrain.dom)) {
            let metadata = fs::symlink_metadata(project.join(path)).ok()?;
            if !metadata.is_file() || linked(&metadata) {
                return None;
            }
            let modified = metadata.modified().ok()?;
            let nanos = match modified.duration_since(UNIX_EPOCH) {
                Ok(duration) => duration.as_nanos() as i128,
                Err(error) => -(error.duration().as_nanos() as i128),
            };
            entries.push((
                path.iter()
                    .map(|part| part.to_string_lossy())
                    .collect::<Vec<_>>()
                    .join("/"),
                metadata.len(),
                nanos,
            ));
        }
        entries.sort_unstable();
        Some(json!(entries))
    }
}

pub(crate) fn entries(
    directory: &Path,
    directories: bool,
) -> impl Iterator<Item = fs::DirEntry> + use<> {
    fs::symlink_metadata(directory)
        .ok()
        .filter(|metadata| metadata.is_dir() && !linked(metadata))
        .and_then(|_| fs::read_dir(directory).ok())
        .into_iter()
        .flatten()
        .filter_map(io::Result::ok)
        .filter(move |entry| {
            fs::symlink_metadata(entry.path()).is_ok_and(|metadata| {
                !linked(&metadata)
                    && if directories {
                        metadata.is_dir()
                    } else {
                        metadata.is_file()
                    }
            })
        })
}

pub(crate) fn linked(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0 // FILE_ATTRIBUTE_REPARSE_POINT
    }
    #[cfg(not(windows))]
    metadata.file_type().is_symlink()
}
