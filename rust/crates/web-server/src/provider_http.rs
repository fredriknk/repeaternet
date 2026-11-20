use std::{fs, path::PathBuf, sync::Arc};

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use geo::{BooleanOps, LineString, MultiPolygon, Polygon};
use repeaternet_optimizer::Cancellation;
use repeaternet_optimizer::InfrastructurePolicy;
use repeaternet_providers::{
    corescope,
    kartverket::{
        DownloadArea, DownloadPlan, KartverketProvider, PlanOptions, Product, StagedGeneration,
    },
};
use repeaternet_terrain::{Bounds, GeoTiff, LonLat, TerrainCrs, Transformer};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    api::{ApiError, ApiResult, ObjectBody, Workspace, ensure_idle, save_plan},
    catalog::{AppState, Session, random_id},
    route_http::{self, Control},
    route_inputs,
    terrain_http::{TerrainContext, change_terrain, create_plain_directory, terrain_invalid},
};

pub async fn meshcore(
    State(app): State<AppState>,
    workspace: Workspace,
    ObjectBody(body): ObjectBody,
) -> ApiResult<Json<Value>> {
    let (a, b) = (endpoint(body.get("a"), "a")?, endpoint(body.get("b"), "b")?);
    let width = body
        .get("corridor_width_m")
        .unwrap_or(&app.defaults["candidates"]["corridor_width_m"])
        .as_f64()
        .filter(|w| (1.0..=500_000.0).contains(w))
        .ok_or_else(|| ApiError::invalid("Corridor width must be between 1 and 500,000 m"))?;
    let endpoint = app.corescope_endpoint.clone();
    let permit = workspace
        .run(app, |app, _, _| app.scheduler.reserve_worker())
        .await?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let routers = corescope::CoreScopeClient::new(endpoint)
            .and_then(|client| client.fetch_repeaters(corescope::DEFAULT_MAXIMUM_PAGES, &|| false))
            .map_err(|error| {
                ApiError::new(
                    StatusCode::BAD_GATEWAY,
                    format!("Could not load MeshCore routers from CoreScope: {error}"),
                )
            })?;
        let result = corescope::filter_corridor(&routers, a, b, width, chrono::Utc::now())
            .map_err(terrain_invalid)?;
        Ok(Json(json!(result)))
    })
    .await
    .map_err(ApiError::internal)?
}

fn endpoint(value: Option<&Value>, name: &str) -> ApiResult<LonLat> {
    let pair = value
        .and_then(Value::as_array)
        .filter(|pair| pair.len() == 2)
        .ok_or_else(|| ApiError::invalid(format!("{name} must contain latitude and longitude")))?;
    route_inputs::coordinate(&pair[0], &pair[1], name)
}

fn download_locations(body: &Value, policy: InfrastructurePolicy) -> ApiResult<Vec<LonLat>> {
    let a = endpoint(body.get("a"), "a")?;
    let b = endpoint(body.get("b"), "b")?;
    if a == b {
        return Err(ApiError::invalid("Place endpoints at different locations"));
    }
    let mut locations = vec![a, b];
    for (field, enabled, malformed) in [
        (
            "known_routers",
            policy != InfrastructurePolicy::ProposedOnly,
            "known_routers must be a list",
        ),
        (
            "manual_routers",
            policy != InfrastructurePolicy::ExistingOnly,
            "manual_routers must be a list of router objects",
        ),
    ] {
        let Some(values) = body.get(field) else {
            continue;
        };
        let routers = values
            .as_array()
            .ok_or_else(|| ApiError::invalid(malformed))?;
        for router in routers {
            if !router.is_object() && (enabled || field == "manual_routers") {
                return Err(ApiError::invalid(if field == "known_routers" {
                    "Each MeshCore router must be an object"
                } else {
                    malformed
                }));
            }
            if enabled && router["policy"] != "excluded" {
                locations.push(route_inputs::coordinate(
                    &router["latitude"],
                    &router["longitude"],
                    "router",
                )?);
            }
        }
    }
    Ok(locations)
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Options {
    requested_resolution_m: f64,
    auto_resolution: bool,
    include_dom: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            requested_resolution_m: 10.0,
            auto_resolution: true,
            include_dom: true,
        }
    }
}

struct Download {
    context: TerrainContext,
    provider: KartverketProvider,
    crs: TerrainCrs,
    dtm: Option<DownloadPlan>,
    dom: Option<DownloadPlan>,
    estimate: Value,
    plan: Option<Value>,
    route_identity: Option<(Value, Value)>,
}

impl Download {
    fn supplemental(&self) -> bool {
        self.plan.is_none()
    }

    fn verify(&self, app: &AppState, key: &str, session: &Session) -> ApiResult<()> {
        self.context.verify(app, key, session)?;
        if let Some((snapshot, alternative)) = &self.route_identity {
            let route = route_http::current(app, key, session)?;
            if &route["snapshot_version"] != snapshot
                || &route["active_alternative_id"] != alternative
            {
                return Err(ApiError::conflict(
                    "The selected route changed; estimate again",
                ));
            }
        }
        Ok(())
    }

    fn status<'a>(&self, session: &'a mut Session) -> &'a mut Value {
        if self.supplemental() {
            session
                .coverage_terrain_job
                .as_mut()
                .expect("active terrain job")
        } else {
            &mut session.route_job
        }
    }

    fn matches(&self, session: &Session, id: &str) -> bool {
        session.project_id == self.context.project_id
            && if self.supplemental() {
                session
                    .coverage_terrain_job
                    .as_ref()
                    .and_then(|j| j["job_id"].as_str())
                    == Some(id)
            } else {
                session.job_id.as_deref() == Some(id)
            }
    }

    fn stage(
        &self,
        app: &AppState,
        key: &str,
        id: &str,
        cancelled: &dyn Fn() -> bool,
    ) -> ApiResult<StagedGeneration> {
        let root = self.context.directory.join(if self.supplemental() {
            "coverage-terrain-generations"
        } else {
            "terrain-generations"
        });
        {
            let entry = app.session(key);
            let mut guard = entry.blocking_lock();
            let session = guard
                .as_mut()
                .filter(|s| self.matches(s, id))
                .ok_or_else(|| ApiError::conflict("Terrain job changed"))?;
            self.verify(app, key, session)?;
            create_plain_directory(&root)?;
            let status = self.status(session);
            status["state"] = json!("preparing");
            status["stage"] = json!(if self.supplemental() {
                "Preparing coverage terrain"
            } else {
                "Downloading ground terrain"
            });
        }
        if !self.supplemental() {
            for kind in ["dtm", "dom"] {
                for path in self.context.route.paths(kind) {
                    if cancelled() {
                        return Err(ApiError::conflict("Terrain download cancelled"));
                    }
                    let tile = GeoTiff::open(self.context.directory.join(path))
                        .map_err(terrain_invalid)?;
                    if tile.metadata().crs != self.crs {
                        return Err(ApiError::invalid(
                            "Existing terrain uses a different CRS. Clear it before preparing terrain in this zone.",
                        ));
                    }
                }
            }
        }
        let mut done = 0;
        let mut fetch = |kind: Product, plan: &Option<DownloadPlan>| -> ApiResult<Vec<PathBuf>> {
            let Some(plan) = plan else {
                return Ok(Vec::new());
            };
            let paths = self
                .provider
                .fetch_plan(kind, self.crs, plan, cancelled, |index, count, cached| {
                    let lock = app.session(key);
                    let mut guard = lock.blocking_lock();
                    if let Some(session) = guard.as_mut().filter(|s| self.matches(s, id)) {
                        let status = self.status(session);
                        let product = kind.name().to_uppercase();
                        let suffix = if cached { " (cache hit)" } else { "" };
                        status["stage"] = json!(format!("{product} tile {index}/{count}{suffix}"));
                        status["done"] = json!(done + index.saturating_sub(1));
                    }
                })
                .map_err(terrain_invalid)?;
            done += paths.len();
            Ok(paths)
        };
        let dtm = fetch(Product::Dtm, &self.dtm)?;
        let dom = fetch(Product::Dom, &self.dom)?;
        StagedGeneration::copy_from_cache(&root, &dtm, &dom, cancelled).map_err(terrain_invalid)
    }
}

async fn route_plan(app: AppState, workspace: Workspace, body: Value) -> ApiResult<Download> {
    let options: Options =
        serde_json::from_value(body.get("terrain").cloned().unwrap_or_else(|| json!({})))
            .map_err(terrain_invalid)?;
    if !(1.0..=1000.0).contains(&options.requested_resolution_m) {
        return Err(ApiError::invalid(
            "Terrain resolution must be between 1 and 1,000 m",
        ));
    }
    let candidates = route_inputs::terrain_candidates(&body, &app.defaults)?;
    let locations = download_locations(&body, candidates.infrastructure_policy)?;
    let width = candidates.corridor_width_m;
    let config = app.wcs_config.clone();
    let limits = app.wcs_limits;
    let context = workspace
        .run(app, |app, key, session| {
            TerrainContext::capture(app, key, session, false)
        })
        .await?;
    tokio::task::spawn_blocking(move || {
        let longitude =
            locations.iter().map(|site| site.longitude).sum::<f64>() / locations.len() as f64;
        let transform = Transformer::for_norway_longitude(longitude).map_err(terrain_invalid)?;
        let crs = transform.crs();
        let points = locations
            .iter()
            .map(|site| transform.forward(*site))
            .collect::<Result<Vec<_>, _>>()
            .map_err(terrain_invalid)?;
        let bounds = Bounds {
            min_x: points.iter().map(|p| p.x).fold(f64::INFINITY, f64::min) - width,
            min_y: points.iter().map(|p| p.y).fold(f64::INFINITY, f64::min) - width,
            max_x: points.iter().map(|p| p.x).fold(f64::NEG_INFINITY, f64::max) + width,
            max_y: points.iter().map(|p| p.y).fold(f64::NEG_INFINITY, f64::max) + width,
        };
        let provider =
            KartverketProvider::new(context.cache_directory.join("kartverket"), config, limits)
                .map_err(terrain_invalid)?;
        let plan = provider
            .plan_download(
                bounds,
                options.requested_resolution_m,
                DownloadArea::Corridor {
                    points: &points[..2],
                    half_width_m: width,
                    extra_points: &points[2..],
                },
                PlanOptions {
                    auto_resolution: options.auto_resolution,
                    ..PlanOptions::default()
                },
            )
            .map_err(terrain_invalid)?;
        let estimate = provider
            .estimate(crs, &plan, options.include_dom, &context.directory)
            .map_err(terrain_invalid)?;
        Ok(Download {
            context,
            provider,
            crs,
            dom: options.include_dom.then(|| plan.clone()),
            dtm: Some(plan),
            estimate: json!(estimate),
            plan: Some(body),
            route_identity: None,
        })
    })
    .await
    .map_err(ApiError::internal)?
}

async fn extra_plan(app: AppState, workspace: Workspace, body: Value) -> ApiResult<Download> {
    let config = app.wcs_config.clone();
    let limits = app.wcs_limits;
    let resolution = std::env::var("RF_PLANNER_COVERAGE_TERRAIN_RESOLUTION_M")
        .ok()
        .map(|value| {
            value
                .parse::<f64>()
                .ok()
                .filter(|r| (1.0..=1000.0).contains(r))
        })
        .unwrap_or(Some(25.0))
        .ok_or_else(|| {
            ApiError::invalid("Coverage terrain resolution must be between 1 and 1,000 m")
        })?;
    #[cfg(test)]
    let resolution = app.coverage_terrain_resolution.unwrap_or(resolution);
    let (context, route, sites, corridor, request, _permit) =
        crate::coverage_http::capture(app, workspace, body.as_object().unwrap(), false).await?;
    tokio::task::spawn_blocking(move || {
        let terrain = context.open()?;
        let crs = terrain.crs();
        let transform = Transformer::new(crs).map_err(terrain_invalid)?;
        let sources = repeaternet_coverage::select_sources(&sites, &sites, &request.settings)
            .map_err(terrain_invalid)?;
        let area = repeaternet_coverage::resolve_area(
            &request.settings,
            &transform,
            &sources,
            &sites,
            corridor,
            request.map_bounds,
        )
        .map_err(terrain_invalid)?;
        let missing = |tiles: &[repeaternet_terrain::GeoTiff]| {
            let footprints: Vec<_> = tiles
                .iter()
                .map(|tile| {
                    let vertices = tile.metadata().footprint();
                    Polygon::new(
                        LineString::from(vertices.map(|p| (p.x, p.y)).to_vec()),
                        vec![],
                    )
                })
                .collect();
            if footprints.is_empty() {
                area.geometry().clone()
            } else {
                area.geometry().difference(&geo::unary_union(&footprints))
            }
        };
        let dtm: MultiPolygon<f64> = missing(terrain.ground_tiles());
        let dom =
            (!context.route.paths("dom").is_empty()).then(|| missing(terrain.surface_tiles()));
        let provider =
            KartverketProvider::new(context.cache_directory.join("kartverket"), config, limits)
                .map_err(terrain_invalid)?;
        let plans = provider
            .plan_coverage_extension(crs, &dtm, dom.as_ref(), resolution)
            .map_err(terrain_invalid)?;
        Ok(Download {
            context,
            provider,
            crs,
            dtm: plans.dtm,
            dom: plans.dom,
            estimate: json!(plans.estimate),
            plan: None,
            route_identity: Some((
                route["snapshot_version"].clone(),
                route["active_alternative_id"].clone(),
            )),
        })
    })
    .await
    .map_err(ApiError::internal)?
}

pub async fn estimate(
    State(app): State<AppState>,
    workspace: Workspace,
    ObjectBody(body): ObjectBody,
) -> ApiResult<Json<Value>> {
    Ok(Json(
        route_plan(app, workspace, Value::Object(body))
            .await?
            .estimate,
    ))
}

pub async fn estimate_extra(
    State(app): State<AppState>,
    workspace: Workspace,
    ObjectBody(body): ObjectBody,
) -> ApiResult<Json<Value>> {
    let download = extra_plan(app.clone(), workspace.clone(), Value::Object(body)).await?;
    workspace
        .run(app, move |app, key, session| {
            download.verify(app, key, session)?;
            Ok(Json(download.estimate))
        })
        .await
}

pub async fn prepare(
    State(app): State<AppState>,
    workspace: Workspace,
    ObjectBody(body): ObjectBody,
) -> ApiResult<Json<Value>> {
    let download = route_plan(app.clone(), workspace.clone(), Value::Object(body)).await?;
    enqueue(app, workspace, download).await
}

pub async fn prepare_extra(
    State(app): State<AppState>,
    workspace: Workspace,
    ObjectBody(body): ObjectBody,
) -> ApiResult<Json<Value>> {
    let download = extra_plan(app.clone(), workspace.clone(), Value::Object(body)).await?;
    enqueue(app, workspace, download).await
}

async fn enqueue(
    app: AppState,
    workspace: Workspace,
    mut download: Download,
) -> ApiResult<Json<Value>> {
    workspace.run(app, move |app, key, session| {
        ensure_idle(session)?;
        download.verify(app, key, session)?;
        let total = download.estimate["cache_total"].clone();
        if total == 0 {
            return Ok(Json(json!({
                "job_id": null, "state": "complete", "stage": "Coverage terrain is already available",
                "done": 0, "total": 0, "terrain_estimate": download.estimate,
            })));
        }
        let outstanding = app.scheduler.reserve_job()?;
        let worker = app.scheduler.try_worker();
        let state = if worker.is_some() { "preparing" } else { "queued" };
        let stage = match (worker.is_some(), download.supplemental()) {
            (false, _) => "Waiting for a terrain worker",
            (true, true) => "Preparing coverage terrain",
            (true, false) => "Preparing Kartverket terrain",
        };
        let id = random_id::<12>()?;
        let control = Arc::new(Control::default());
        let status = json!({
            "job_id": id, "state": state, "stage": stage, "done": 0, "total": total,
            "started_at": route_http::now(), "terrain_estimate": download.estimate,
        });
        let response = if download.supplemental() {
            session.coverage_terrain_job = Some(status.clone());
            session.coverage_terrain_control = Some(control.clone());
            status
        } else {
            app.store.save_result_summary(key, &session.project_id, None)?;
            app.store.set_run_state(key, &session.project_id, state)?;
            session.invalidate_route();
            download.context.revision = session.input_revision;
            session.job_id = Some(id.clone());
            session.route_control = Some(control.clone());
            session.route_job = status;
            session.job_payload()
        };
        let job = TerrainJob { id, control, download };
        tokio::spawn(run(app.clone(), Workspace(key.to_owned()), Arc::new(job), outstanding, worker));
        Ok(Json(response))
    }).await
}

struct TerrainJob {
    id: String,
    control: Arc<Control>,
    download: Download,
}

impl TerrainJob {
    fn cancelled(&self) -> bool {
        self.control.cancellation() != Cancellation::Continue
    }

    fn stage(&self, app: &AppState, key: &str) -> ApiResult<StagedGeneration> {
        self.download
            .stage(app, key, &self.id, &|| self.cancelled())
    }

    fn publish(
        &self,
        app: &AppState,
        key: &str,
        session: &mut Session,
        stage: StagedGeneration,
    ) -> ApiResult<()> {
        self.download.verify(app, key, session)?;
        let root = stage.path().parent().expect("stage has a generation root");
        create_plain_directory(root)?;
        let destination = stage
            .publish(&format!("generation-{}", self.id), &|| self.cancelled())
            .map_err(terrain_invalid)?;
        if let Some(plan) = &self.download.plan {
            let project = session.project_id.clone();
            let commit = change_terrain(app, key, session, &self.download.context, &[], || {
                save_plan(&app.store, key, &project, plan)
            });
            if let Err(error) = commit {
                if let Err(rollback) = fs::remove_dir_all(destination) {
                    eprintln!("Could not remove uncommitted generation: {rollback}");
                }
                return Err(error);
            }
            session.job_id = Some(self.id.clone());
        }
        Ok(())
    }

    fn finish(
        &self,
        app: &AppState,
        key: &str,
        session: &mut Session,
        outcome: ApiResult<Option<StagedGeneration>>,
    ) -> ApiResult<()> {
        if !self.download.matches(session, &self.id) {
            return Ok(());
        }
        let supplemental = self.download.supplemental();
        let mut status = self.download.status(session).clone();
        let published = outcome.and_then(|stage| {
            if let Some(stage) = stage.filter(|_| !self.cancelled()) {
                self.publish(app, key, session, stage)?;
                return Ok(true);
            }
            Ok(false)
        });
        let (state, stage) = match published {
            Ok(true) if supplemental => ("complete", "Coverage terrain ready".to_owned()),
            Ok(true) => ("terrain_ready", "Terrain prepared and ready".to_owned()),
            _ if self.cancelled() && supplemental => (
                "cancelled",
                "Coverage terrain download cancelled".to_owned(),
            ),
            _ if self.cancelled() => ("cancelled", "Terrain download cancelled".to_owned()),
            Err(error) => ("failed", error.detail().to_owned()),
            Ok(false) => ("failed", "Terrain worker unavailable".to_owned()),
        };
        status["state"] = json!(state);
        status["stage"] = json!(stage);
        if matches!(state, "terrain_ready" | "complete") {
            status["done"] = status["total"].clone();
        }
        *self.download.status(session) = status;
        if supplemental {
            session.coverage_terrain_control = None;
        } else {
            session.route_control = None;
            app.store.set_run_state(key, &session.project_id, state)?;
        }
        Ok(())
    }
}

async fn run(
    app: AppState,
    workspace: Workspace,
    job: Arc<TerrainJob>,
    outstanding: tokio::sync::OwnedSemaphorePermit,
    worker: Option<tokio::sync::OwnedSemaphorePermit>,
) {
    let worker = app.scheduler.wait_worker(worker, &job.control).await;
    let outcome = if worker.is_some() && !job.cancelled() {
        let (app, key, job) = (app.clone(), workspace.0.clone(), job.clone());
        tokio::task::spawn_blocking(move || job.stage(&app, &key))
            .await
            .unwrap_or_else(|error| Err(ApiError::internal(error)))
            .map(Some)
    } else {
        Ok(None)
    };
    if let Err(error) = workspace
        .run(app, move |app, key, session| {
            let _reservation = (worker, outstanding);
            job.finish(app, key, session, outcome)
        })
        .await
    {
        eprintln!("Could not finish terrain job: {error:?}");
    }
}

pub async fn cancel_extra(
    State(app): State<AppState>,
    workspace: Workspace,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    workspace
        .run(app, move |_, _, session| {
            if session
                .coverage_terrain_job
                .as_ref()
                .and_then(|job| job["job_id"].as_str())
                != Some(&id)
            {
                return Err(ApiError::missing(
                    "Coverage terrain job not found in this workspace",
                ));
            }
            session
                .coverage_terrain_control
                .as_ref()
                .ok_or_else(|| ApiError::conflict("Coverage terrain download already finished"))?
                .cancel(false);
            Ok(Json(json!({"ok": true})))
        })
        .await
}
