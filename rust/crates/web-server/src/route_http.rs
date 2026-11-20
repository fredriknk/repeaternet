use std::{
    path::Path as FsPath,
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
    time::{Duration, Instant, UNIX_EPOCH},
};

use axum::{
    Json,
    body::Bytes,
    extract::{Path, State},
    http::StatusCode,
};
use repeaternet_optimizer::{
    Cancellation, InfrastructurePolicy, OptimizationRequest, PlanningSite, Progress,
    RouteOptimizer, RouterPolicy, SearchHooks, SiteOrigin, candidate_grid_spacing,
};
use repeaternet_project_store::{Project, ProjectStore};
use repeaternet_rf_core::{HeightReference, Site};
use repeaternet_terrain::{RasterTerrain, TerrainLayer, TerrainSource, Transformer};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};

use crate::{
    api::{ApiError, ApiResult, ObjectBody, Workspace, ensure_idle, save_plan},
    catalog::{self, AppState, Session, TerrainFiles},
    route_inputs::{self, RouteInputs},
    route_output::{self, RouteOutput},
    terrain_http::{self, TerrainContext},
};

#[derive(Serialize, Deserialize)]
struct SavedRoute<P, A> {
    payload: P,
    alternatives: A,
    job: Value,
    resolution_m: Option<f64>,
    surface_available: Option<bool>,
}

pub(crate) fn restore(
    store: &ProjectStore,
    key: &str,
    project: &Project,
    directory: &FsPath,
    session: &mut Session,
) -> ApiResult<()> {
    let Some(summary) = &project.result_summary else {
        return Ok(());
    };
    if !matches!(project.run_state.as_str(), "complete" | "stopped") {
        return Ok(());
    }
    let plan = catalog::plan_fingerprint(&project.plan, true);
    if summary["plan_fingerprint"].as_str() != Some(plan.as_str()) {
        return Ok(());
    }
    let terrain = TerrainFiles::read(directory, false);
    let Some(fingerprint) = terrain.fingerprint(directory, None) else {
        return Ok(());
    };
    if summary["terrain_fingerprint"] != fingerprint {
        return Ok(());
    }
    let Some(encoded) = store.route_result(key, &project.id)? else {
        return Ok(());
    };
    let SavedRoute {
        mut payload,
        alternatives,
        job,
        resolution_m,
        surface_available,
    } = match serde_json::from_str::<SavedRoute<Value, Vec<route_output::Alternative>>>(&encoded) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            eprintln!("Could not read the saved route snapshot: {error}");
            return Ok(());
        }
    };
    let (Some(version), Some(job_id)) = (
        payload["snapshot_version"].as_u64(),
        payload["job_id"].as_str(),
    ) else {
        return Ok(());
    };
    session.job_id = Some(job_id.to_owned());
    session.snapshot_version = version;
    payload["input_revision"] = json!(session.input_revision);
    session.route_job = job;
    session.route_result = Some(payload);
    session.route_alternatives = alternatives;
    session.route_identity = Some((plan, fingerprint));
    session.route_resolution_m = resolution_m;
    session.route_surface_available = surface_available;
    Ok(())
}

pub struct Scheduler {
    available: Arc<Semaphore>,
    outstanding: Arc<Semaphore>,
    #[cfg(test)]
    pub snapshot_gate: std::sync::Mutex<Option<SnapshotGate>>,
}

#[cfg(test)]
pub struct SnapshotGate {
    pub published: tokio::sync::oneshot::Sender<()>,
    pub resume: std::sync::mpsc::Receiver<()>,
}

impl Scheduler {
    pub(crate) fn reserve_worker(&self) -> ApiResult<OwnedSemaphorePermit> {
        self.available.clone().try_acquire_owned().map_err(|_| {
            ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "All planner workers are busy; retry shortly",
            )
        })
    }

    pub(crate) fn try_worker(&self) -> Option<OwnedSemaphorePermit> {
        self.available.clone().try_acquire_owned().ok()
    }

    pub(crate) async fn wait_worker(
        &self,
        worker: Option<OwnedSemaphorePermit>,
        control: &Control,
    ) -> Option<OwnedSemaphorePermit> {
        match worker {
            Some(permit) => Some(permit),
            None => tokio::select! {
                biased;
                _ = control.changed.notified() => None,
                permit = self.available.clone().acquire_owned() => permit.ok(),
            },
        }
    }

    pub(crate) fn reserve_job(&self) -> ApiResult<OwnedSemaphorePermit> {
        self.outstanding.clone().try_acquire_owned().map_err(|_| {
            ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "The planner queue is full; retry shortly",
            )
        })
    }

    #[cfg(test)]
    fn pause_after_snapshot(&self) {
        let gate = self.snapshot_gate.lock().unwrap().take();
        if let Some(gate) = gate {
            let _ = gate.published.send(());
            let _ = gate.resume.recv_timeout(Duration::from_secs(10));
        }
    }

    pub fn from_env() -> Self {
        let workers = std::env::var("RF_PLANNER_MAX_ACTIVE_JOBS")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(1)
            .clamp(1, Semaphore::MAX_PERMITS / 2);
        Self::new(workers)
    }

    pub(crate) fn new(workers: usize) -> Self {
        Self {
            available: Arc::new(Semaphore::new(workers)),
            outstanding: Arc::new(Semaphore::new(workers * 2)),
            #[cfg(test)]
            snapshot_gate: std::sync::Mutex::default(),
        }
    }
}

#[derive(Default)]
pub struct Control {
    disposition: AtomicU8,
    changed: Notify,
}

impl Control {
    pub(crate) fn cancellation(&self) -> Cancellation {
        match self.disposition.load(Ordering::Acquire) {
            1 => Cancellation::Discard,
            2 => Cancellation::KeepCertified,
            _ => Cancellation::Continue,
        }
    }

    pub(crate) fn cancel(&self, keep: bool) {
        self.disposition
            .store(if keep { 2 } else { 1 }, Ordering::Release);
        self.changed.notify_one();
    }
}

struct Job {
    id: String,
    revision: u64,
    context: TerrainContext,
    plan_fingerprint: String,
    control: Arc<Control>,
}

impl Job {
    fn matches(&self, session: &Session) -> bool {
        session.project_id == self.context.project_id
            && session.input_revision == self.revision
            && session.job_id.as_deref() == Some(&self.id)
    }

    fn verify(&self, app: &AppState, key: &str, session: &Session) -> ApiResult<()> {
        if !self.matches(session) {
            return Err(ApiError::conflict(
                "This request refers to an older route job",
            ));
        }
        self.context
            .verify_files_and_plan(app, key, session, &self.plan_fingerprint)
    }

    fn stamp(&self, output: &mut Value, version: u64) {
        output["job_id"] = json!(self.id);
        output["input_revision"] = json!(self.revision);
        output["snapshot_version"] = json!(version);
    }
}

struct Prepared {
    terrain: RasterTerrain,
    transform: Transformer,
    request: OptimizationRequest,
    inputs: RouteInputs,
}

fn preflight(context: &TerrainContext, inputs: RouteInputs) -> ApiResult<Prepared> {
    let terrain = context.open()?;
    let (transform, points, elevations) = terrain_http::ground(&terrain, &inputs.locations)?;
    let gaps = terrain_http::uncovered(&inputs.locations, &points, &elevations, terrain.bounds());
    if !gaps.is_empty() {
        let names: Vec<_> = gaps
            .iter()
            .filter_map(|item| item["name"].as_str())
            .collect();
        return Err(ApiError::invalid(format!(
            "Ground-terrain coverage is missing at: {}",
            names.join(", ")
        )));
    }
    if points[0] == points[1] {
        return Err(ApiError::invalid("Place endpoints at different locations"));
    }
    let settings = &inputs.candidates;
    let bounds = terrain.bounds();
    let span_x = bounds.max_x - bounds.min_x;
    let span_y = bounds.max_y - bounds.min_y;
    let distance = settings
        .maximum_link_distance_m
        .unwrap_or(f64::INFINITY)
        .min(span_x.hypot(span_y));
    let step = settings
        .coarse_sample_step_m
        .min(settings.medium_sample_step_m)
        .min(settings.final_sample_step_m);
    if !repeaternet_terrain::profile::bounded_profile_step(
        distance,
        step,
        terrain.resolution_m(),
        65_536,
    )
    .map_err(terrain_http::terrain_invalid)?
    .fits()
    {
        return Err(ApiError::invalid(
            "Route profiles exceed 65,536 samples with the selected terrain and link settings",
        ));
    }
    if settings.infrastructure_policy != InfrastructurePolicy::ExistingOnly
        && inputs.locations.len() - 2 < settings.maximum_candidates
    {
        let mut extrema = [points[0].x, points[0].y, points[0].x, points[0].y];
        for (location, point) in inputs.locations.iter().zip(&points) {
            if location.origin == SiteOrigin::Optimized || location.policy == RouterPolicy::Required
            {
                extrema = [
                    extrema[0].min(point.x),
                    extrema[1].min(point.y),
                    extrema[2].max(point.x),
                    extrema[3].max(point.y),
                ];
            }
        }
        let padding = settings.corridor_width_m * std::f64::consts::SQRT_2;
        let spacing = candidate_grid_spacing(&terrain, settings);
        let width = bounds.max_x.min(extrema[2] + padding) - bounds.min_x.max(extrema[0] - padding);
        let height =
            bounds.max_y.min(extrema[3] + padding) - bounds.min_y.max(extrema[1] - padding);
        let cells = (width / spacing + 1.0) * (height / spacing + 1.0);
        if cells > 2_000_000.0 {
            return Err(ApiError::invalid(
                "Candidate grid exceeds 2,000,000 points; increase grid spacing or narrow the corridor",
            ));
        }
    }
    let surfaces = if terrain.has_surface() {
        terrain
            .sample(&points, TerrainLayer::SurfaceDom)
            .map_err(terrain_http::terrain_invalid)?
    } else {
        vec![f64::NAN; points.len()]
    };
    let mut sites = inputs
        .locations
        .iter()
        .zip(points)
        .zip(elevations)
        .zip(surfaces)
        .map(|(((location, position), ground), surface)| PlanningSite {
            rf: Site {
                id: location.id.clone(),
                position,
                kind: location.kind,
                ground_elevation_m: ground,
                surface_elevation_m: (location.origin != SiteOrigin::Optimized
                    && surface.is_finite())
                .then_some(surface),
                antenna_height_m: location.height,
                height_reference: HeightReference::GroundDtm,
            },
            origin: location.origin,
            policy: location.policy,
            enabled: true,
            locked: location.origin != SiteOrigin::Optimized,
            height_override: location.height_override,
            terrain_slope: 0.0,
            site_quality: 0.0,
        })
        .collect::<Vec<_>>();
    let fixed_routers = sites.split_off(2);
    let request = OptimizationRequest {
        clients: sites,
        fixed_routers,
        terrain_bounds: Some(bounds.as_array()),
        ..Default::default()
    };
    Ok(Prepared {
        terrain,
        transform,
        request,
        inputs,
    })
}

pub async fn start(
    State(app): State<AppState>,
    workspace: Workspace,
    ObjectBody(mut body): ObjectBody,
) -> ApiResult<Json<Value>> {
    let project = body.remove("project_id").filter(|value| !value.is_null());
    let revision = body
        .remove("input_revision")
        .filter(|value| !value.is_null());
    let mut plan = Value::Object(body);
    if let Some(targets) = plan.get_mut("coverage_targets") {
        *targets = crate::targets::normalize(targets).map_err(ApiError::invalid)?;
    }
    let inputs = route_inputs::parse(plan, &app.defaults)?;
    let (captured, outstanding) = workspace
        .clone()
        .run(app.clone(), move |app, key, session| {
            ensure_idle(session)?;
            if project
                .as_ref()
                .is_some_and(|id| id.as_str() != Some(&session.project_id))
                || revision
                    .as_ref()
                    .is_some_and(|revision| revision.as_u64() != Some(session.input_revision))
            {
                return Err(ApiError::conflict(
                    "This route request refers to an older project or input revision",
                ));
            }
            Ok((
                TerrainContext::capture(app, key, session, false)?,
                app.scheduler.reserve_job()?,
            ))
        })
        .await?;
    let (captured, prepared, outstanding) = tokio::task::spawn_blocking(move || {
        let prepared = preflight(&captured, inputs)?;
        Ok::<_, ApiError>((captured, prepared, outstanding))
    })
    .await
    .map_err(ApiError::internal)??;
    workspace.run(app, move |app, key, session| {
        ensure_idle(session)?;
        captured.verify(app, key, session)?;
        let worker = app.scheduler.try_worker();
        let state = if worker.is_some() { "running" } else { "queued" };
        let id = catalog::random_id::<12>()?;
        let plan = &prepared.inputs.plan;
        let plan_fingerprint = catalog::plan_fingerprint(plan, true);
        save_plan(&app.store, key, &session.project_id, plan)?;
        app.store.save_result_summary(key, &session.project_id, None)?;
        app.store.set_run_state(key, &session.project_id, state)?;
        session.invalidate_route();
        let control = Arc::new(Control::default());
        let job = Job {
            id: id.clone(), revision: session.input_revision, context: captured, plan_fingerprint,
            control: control.clone(),
        };
        session.job_id = Some(id);
        session.route_control = Some(control);
        session.route_resolution_m = Some(prepared.terrain.resolution_m());
        session.route_surface_available = Some(prepared.terrain.has_surface());
        session.route_job = json!({
            "state": state, "stage": if worker.is_some() { "Opening terrain" } else { "Waiting for a planner worker" },
            "done": 0, "total": 1, "started_at": now(),
            "search_effort": plan["search_effort"], "resolved_search": plan["resolved_search"],
        });
        tokio::spawn(run(app.clone(), Workspace(key.to_owned()), Arc::new(job), prepared, outstanding, worker));
        Ok(Json(session.job_payload()))
    }).await
}

pub fn now() -> f64 {
    UNIX_EPOCH.elapsed().unwrap_or_default().as_secs_f64()
}

async fn run(
    app: AppState,
    workspace: Workspace,
    job: Arc<Job>,
    prepared: Prepared,
    outstanding: OwnedSemaphorePermit,
    worker: Option<OwnedSemaphorePermit>,
) {
    let worker = app.scheduler.wait_worker(worker, &job.control).await;
    let outcome = if worker.is_some() && job.control.cancellation() == Cancellation::Continue {
        let worker_job = job.clone();
        let ready = workspace
            .clone()
            .run(app.clone(), move |app, key, session| {
                worker_job.verify(app, key, session)?;
                app.store
                    .set_run_state(key, &session.project_id, "running")?;
                session.route_job["state"] = json!("running");
                session.route_job["stage"] = json!("Opening terrain");
                Ok(())
            })
            .await;
        match ready {
            Ok(()) => {
                let computation_app = app.clone();
                let computation_workspace = workspace.clone();
                let computation_job = job.clone();
                tokio::task::spawn_blocking(move || {
                    compute(
                        computation_app,
                        &computation_workspace.0,
                        computation_job,
                        prepared,
                    )
                })
                .await
                .unwrap_or_else(|error| Err(ApiError::internal(error)))
                .map(Some)
            }
            Err(error) => Err(error),
        }
    } else {
        Ok(None)
    };
    let finished = workspace
        .run(app, move |app, key, session| {
            // Keep the reservation through persistence and release it under the
            // workspace lock before clients can observe the terminal state.
            let _reservation = (worker, outstanding);
            if !job.matches(session) {
                return Ok(());
            }
            if let Err(error) = finish(app, key, session, &job, outcome) {
                eprintln!("Could not publish route result: {error:?}");
                session.route_result = None;
                session.route_alternatives.clear();
                session.route_identity = None;
                session.snapshot_version = 0;
                session.route_control = None;
                session.route_job["state"] = json!("failed");
                session.route_job["stage"] = json!("Could not save the route result");
                let _ = app
                    .store
                    .save_result_summary(key, &session.project_id, None);
                let _ = app.store.set_run_state(key, &session.project_id, "failed");
            }
            Ok(())
        })
        .await;
    if let Err(error) = finished {
        eprintln!("Could not finish route job: {error:?}");
    }
}

fn compute(app: AppState, key: &str, job: Arc<Job>, prepared: Prepared) -> ApiResult<RouteOutput> {
    let Prepared {
        terrain,
        transform,
        request,
        inputs,
    } = prepared;
    let session = app.session(key);
    let mut last_progress: Option<(repeaternet_optimizer::Stage, Instant)> = None;
    let mut progress = |progress: Progress| {
        if last_progress.is_some_and(|(stage, when)| {
            stage == progress.stage
                && when.elapsed() < Duration::from_millis(80)
                && progress.done != progress.total
        }) {
            return;
        }
        last_progress = Some((progress.stage, Instant::now()));
        if let Some(session) = session
            .blocking_lock()
            .as_mut()
            .filter(|session| job.matches(session))
        {
            session.route_job["stage"] = json!(progress.stage.label());
            session.route_job["done"] = json!(progress.done);
            session.route_job["total"] = json!(progress.total);
        }
    };
    let cancellation = || job.control.cancellation();
    use repeaternet_providers::roads::{RoadAccessClasses, TiledRoadAccess};
    let roads = inputs
        .candidates
        .road_access_enabled
        .then(|| {
            let classes = match inputs.candidates.road_access_classes {
                repeaternet_optimizer::RoadAccessClasses::PublicRoads => {
                    RoadAccessClasses::PublicRoads
                }
                repeaternet_optimizer::RoadAccessClasses::AllDrivable => {
                    RoadAccessClasses::AllDrivable
                }
            };
            #[cfg(test)]
            if let Some(endpoint) = &app.overpass_endpoint {
                let _ = rustls::crypto::ring::default_provider().install_default();
                return TiledRoadAccess::with_client(
                    terrain.crs(),
                    classes,
                    inputs.candidates.maximum_road_distance_m,
                    job.context.cache_directory.join("overpass"),
                    endpoint,
                    reqwest::blocking::Client::builder()
                        .no_proxy()
                        .build()
                        .unwrap(),
                )
                .map(std::cell::RefCell::new);
            }
            TiledRoadAccess::new(
                terrain.crs(),
                classes,
                inputs.candidates.maximum_road_distance_m,
                job.context.cache_directory.join("overpass"),
            )
            .map(std::cell::RefCell::new)
        })
        .transpose()
        .map_err(crate::terrain_http::terrain_invalid)?;
    let cancelled = || cancellation() != Cancellation::Continue;
    let road_progress = |stage: &str, done, total| {
        if let Some(session) = session.blocking_lock().as_mut().filter(|s| job.matches(s)) {
            session.route_job["stage"] = json!(stage);
            session.route_job["done"] = json!(done);
            session.route_job["total"] = json!(total);
        }
    };
    let site_filter = |points: &[repeaternet_terrain::Point]| {
        roads
            .as_ref()
            .expect("road filter enabled")
            .borrow_mut()
            .allowed(points, &cancelled, road_progress)
            .map_err(|error| {
                repeaternet_optimizer::OptimizationError::InvalidInput(error.to_string())
            })
    };
    let add_overlay = |output: &mut RouteOutput| -> ApiResult<()> {
        if let Some(roads) = &roads {
            if output.alternatives.is_empty() {
                return Ok(());
            }
            let locations = output
                .alternatives
                .iter()
                .map(|alt| {
                    alt.rf_sites
                        .iter()
                        .filter(|site| site.kind == repeaternet_rf_core::SiteKind::Router)
                        .map(|site| (site.id.clone(), site.position))
                        .collect()
                })
                .collect::<Vec<_>>();
            let overlay = roads
                .borrow_mut()
                .overlay(&locations, &cancelled, road_progress)
                .map_err(crate::terrain_http::terrain_invalid)?;
            output.payload["road_polylines"] = json!(overlay.polylines);
            for (index, alt) in output.alternatives.iter_mut().enumerate() {
                alt.summary["road_polyline_indices"] = json!(overlay.polyline_indices[index]);
                alt.summary["nearest_road_distance_m"] = json!(overlay.nearest_distances_m[index]);
            }
            if let Some(index) = output
                .alternatives
                .iter()
                .position(|alt| alt.summary["id"] == output.payload["active_alternative_id"])
            {
                route_output::select(&mut output.payload, &output.alternatives, index);
            }
        }
        Ok(())
    };
    let mut publish = |snapshot: repeaternet_optimizer::OptimizationResult| {
        if snapshot.search_complete || !snapshot.found() || cancellation() != Cancellation::Continue
        {
            return;
        }
        let mut output = route_output::build(snapshot, &transform, &inputs.rf_payload);
        if add_overlay(&mut output).is_err() {
            return;
        }
        {
            let mut guard = session.blocking_lock();
            let Some(session) = guard.as_mut() else {
                return;
            };
            if cancellation() != Cancellation::Continue || job.verify(&app, key, session).is_err() {
                return;
            }
            session.snapshot_version += 1;
            job.stamp(&mut output.payload, session.snapshot_version);
            session.route_result = Some(output.payload);
            session.route_alternatives = output.alternatives;
            session.route_identity = Some((
                job.plan_fingerprint.clone(),
                job.context.fingerprint.clone(),
            ));
            session.route_job["stage"] = json!("Route found; broader search continues");
            session.route_job["search_complete"] = json!(false);
        }
        #[cfg(test)]
        app.scheduler.pause_after_snapshot();
    };
    let cache_version = json!([job.context.fingerprint, inputs.rf_payload]).to_string();
    let optimizer = RouteOptimizer::new(&terrain, &inputs.rf, &inputs.candidates).with_cache(
        &app.cache,
        &job.context.project_id,
        &cache_version,
    );
    let optimizer = if roads.is_some() {
        optimizer.with_site_filter(&site_filter)
    } else {
        optimizer
    };
    let result = optimizer
        .optimize(
            request,
            SearchHooks {
                cancellation: Some(&cancellation),
                progress: Some(&mut progress),
                solution_progress: Some(&mut publish),
            },
        )
        .map_err(|error| ApiError::invalid(error.to_string()))?;
    let mut output = route_output::build(result, &transform, &inputs.rf_payload);
    if !cancelled() {
        add_overlay(&mut output)?;
    }
    Ok(output)
}

fn finish(
    app: &AppState,
    key: &str,
    session: &mut Session,
    job: &Job,
    outcome: ApiResult<Option<RouteOutput>>,
) -> ApiResult<()> {
    let identity = job.verify(app, key, session);
    let (state, stage, mut payload, alternatives) =
        match (identity, job.control.cancellation(), outcome) {
            (Err(_), _, _) => (
                "failed",
                "Route inputs or terrain changed during the search".to_owned(),
                None,
                Vec::new(),
            ),
            (Ok(()), Cancellation::Discard, _) | (Ok(()), Cancellation::Continue, Ok(None)) => (
                "cancelled",
                "Optimization cancelled".to_owned(),
                None,
                Vec::new(),
            ),
            (Ok(()), Cancellation::KeepCertified, _) => (
                "stopped",
                "Stopped with the latest certified route".to_owned(),
                session.route_result.take(),
                std::mem::take(&mut session.route_alternatives),
            ),
            (Ok(()), Cancellation::Continue, Ok(Some(output))) => {
                let stage = if output.payload["found"] == true {
                    "Route found"
                } else {
                    "No route found"
                };
                (
                    "complete",
                    stage.to_owned(),
                    Some(output.payload),
                    output.alternatives,
                )
            }
            (Ok(()), Cancellation::Continue, Err(error)) => {
                eprintln!("Route optimization failed: {error:?}");
                ("failed", error.detail().to_owned(), None, Vec::new())
            }
        };
    let version = if state == "stopped" {
        session.snapshot_version
    } else if payload.is_some() {
        session.snapshot_version + 1
    } else {
        0
    };
    let mut terminal_job = session.route_job.clone();
    terminal_job["state"] = json!(state);
    terminal_job["stage"] = json!(stage);
    terminal_job["search_complete"] = json!(state == "complete");
    if state == "complete" {
        terminal_job["done"] = terminal_job["total"].clone();
    }
    if let Some(payload) = &mut payload {
        job.stamp(payload, version);
        let summary =
            route_output::summary(payload, &job.plan_fingerprint, &job.context.fingerprint);
        app.store.save_route_result(
            key,
            &session.project_id,
            state,
            &summary,
            &SavedRoute {
                payload: &*payload,
                alternatives: &alternatives,
                job: terminal_job.clone(),
                resolution_m: session.route_resolution_m,
                surface_available: session.route_surface_available,
            },
        )?;
    } else {
        app.store
            .save_result_summary(key, &session.project_id, None)?;
        app.store.set_run_state(key, &session.project_id, state)?;
    }
    session.snapshot_version = version;
    session.route_identity = payload.as_ref().map(|_| {
        (
            job.plan_fingerprint.clone(),
            job.context.fingerprint.clone(),
        )
    });
    session.route_result = payload;
    session.route_alternatives = alternatives;
    session.route_control = None;
    session.route_job = terminal_job;
    Ok(())
}

pub async fn cancel(
    State(app): State<AppState>,
    workspace: Workspace,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    cancel_job(app, workspace, &body, false).await
}

pub async fn keep(
    State(app): State<AppState>,
    workspace: Workspace,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    cancel_job(app, workspace, &body, true).await
}

async fn cancel_job(
    app: AppState,
    workspace: Workspace,
    body: &[u8],
    keep: bool,
) -> ApiResult<Json<Value>> {
    let payload = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice::<Value>(body)
            .map_err(|_| ApiError::invalid("Invalid cancellation request"))?
    };
    if !payload.is_null() && !payload.is_object() {
        return Err(ApiError::invalid("Invalid cancellation request"));
    }
    workspace
        .run(app, move |_, _, session| {
            if let Some(id) = payload.get("job_id").filter(|value| !value.is_null())
                && id.as_str() != session.job_id.as_deref()
            {
                return Err(ApiError::conflict("This request refers to an older job"));
            }
            if keep && (session.route_job["state"] != "running" || session.route_result.is_none()) {
                return Err(ApiError::conflict(
                    "A certified route is not available to keep",
                ));
            }
            let control = session
                .route_control
                .as_ref()
                .ok_or_else(|| ApiError::conflict("There is no active job to cancel"))?;
            control.cancel(keep);
            Ok(Json(json!({"ok": true})))
        })
        .await
}

pub fn current<'a>(app: &AppState, key: &str, session: &'a Session) -> ApiResult<&'a Value> {
    let result = session
        .route_result
        .as_ref()
        .ok_or_else(|| ApiError::missing("No certified route is available yet"))?;
    let Some((plan, terrain)) = &session.route_identity else {
        return Err(ApiError::missing("No certified route is available yet"));
    };
    let context = TerrainContext::capture(app, key, session, false)?;
    if &context.plan_fingerprint != plan || &context.fingerprint != terrain {
        return Err(ApiError::conflict(
            "The certified route belongs to older route inputs or terrain",
        ));
    }
    Ok(result)
}

pub async fn select(
    State(app): State<AppState>,
    workspace: Workspace,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    workspace
        .run(app, move |app, key, session| {
            ensure_idle(session)?;
            let mut result = current(app, key, session)?.clone();
            let index = session
                .route_alternatives
                .iter()
                .position(|alternative| alternative.summary["id"] == id)
                .ok_or_else(|| {
                    ApiError::missing("That route alternative is no longer available")
                })?;
            route_output::select(&mut result, &session.route_alternatives, index);
            let version = session.snapshot_version + 1;
            result["snapshot_version"] = json!(version);
            let (plan, terrain) = session.route_identity.as_ref().unwrap();
            let summary = route_output::summary(&result, plan, terrain);
            app.store.save_route_result(
                key,
                &session.project_id,
                session.route_job["state"].as_str().unwrap_or("complete"),
                &summary,
                &SavedRoute {
                    payload: &result,
                    alternatives: &session.route_alternatives,
                    job: session.route_job.clone(),
                    resolution_m: session.route_resolution_m,
                    surface_available: session.route_surface_available,
                },
            )?;
            session.snapshot_version = version;
            session.route_result = Some(result.clone());
            Ok(Json(result))
        })
        .await
}

pub async fn clear_cache(
    State(app): State<AppState>,
    workspace: Workspace,
) -> ApiResult<Json<Value>> {
    workspace
        .run(app, |app, _, session| {
            ensure_idle(session)?;
            Ok(Json(
                json!({"ok": true, "deleted_entries": app.cache.clear(&session.project_id)}),
            ))
        })
        .await
}
