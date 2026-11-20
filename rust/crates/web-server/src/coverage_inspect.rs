use std::{collections::BTreeSet, sync::Arc};

use axum::{
    Json,
    extract::{Path, State},
};
use repeaternet_coverage::{
    CoverageEvaluator, CoverageMetricsCache, CoverageSettings, ExactPointOptions, select_sources,
};
use repeaternet_coverage_store::{FORMAT, fingerprint};
use repeaternet_optimizer::Cancellation;
use repeaternet_rf_core::Site;
use repeaternet_terrain::{LonLat, Transformer};
use serde_json::{Value, json};
use tokio::sync::OwnedSemaphorePermit;

use crate::{
    api::{ApiError, ApiResult, ObjectBody, Workspace, active_project},
    catalog::{AppState, Session, random_id},
    coverage_analysis::{final_sample_step, source_sites},
    coverage_http, coverage_runs,
    route_http::{self, Control},
    route_inputs, route_output,
    terrain_http::{TerrainContext, terrain_invalid},
};

const CHANGED: &str =
    "The selected route, terrain, or coverage run context changed; refresh before inspecting";

struct Inspection {
    context: TerrainContext,
    location: LonLat,
    settings: CoverageSettings,
    sources: Vec<Site>,
    source_display: Value,
    links: Value,
    radio: Value,
}

pub async fn start(
    State(app): State<AppState>,
    workspace: Workspace,
    ObjectBody(body): ObjectBody,
) -> ApiResult<Json<Value>> {
    let location = route_inputs::coordinate(
        body.get("latitude").unwrap_or(&Value::Null),
        body.get("longitude").unwrap_or(&Value::Null),
        "point",
    )?;
    let body = Value::Object(body);
    workspace
        .clone()
        .run(app, move |app, key, session| {
            if session.calculations_active() {
                return Err(ApiError::conflict(
                    "Wait for coverage calculation to finish before inspecting a point",
                ));
            }
            let saved = if body["coverage_job_id"].is_null() {
                None
            } else {
                let id = body["coverage_job_id"]
                    .as_str()
                    .ok_or_else(|| ApiError::invalid("Selected coverage run ID is invalid"))?;
                Some(coverage_runs::store(app, key, session)?.load(id)?)
            };
            let live = coverage_runs::identity(app, key, session).ok_or_else(|| {
                ApiError::conflict("Find a certified route before calculating coverage")
            })?;
            let route = route_http::current(app, key, session)?;
            let project = active_project(&app.store, key, session)?;
            let context = TerrainContext::capture(app, key, session, true)?;
            let mut guard = serde_json::to_value(&live).map_err(ApiError::internal)?;
            guard["coverage_store_format"] = json!(FORMAT);
            guard["stale"] = json!(false);
            let (settings, sources, source_display, links, radio) = if let Some(run) = &saved {
                if run["state"] != "complete" {
                    return Err(ApiError::conflict(
                        "Select a completed coverage run for exact-point inspection",
                    ));
                }
                if !live.route_matches(run) {
                    return Err(ApiError::conflict(
                        "Selected coverage run belongs to a different route or terrain snapshot",
                    ));
                }
                guard["settings_fingerprint"] = run["settings_fingerprint"].clone();
                (
                    coverage_http::settings(run.get("settings"))?,
                    source_sites(run)?,
                    run["source_sites"].clone(),
                    run["network_links"].clone(),
                    run["radio_settings"].clone(),
                )
            } else {
                let snapshot = body["snapshot_version"].as_u64().ok_or_else(|| {
                    ApiError::invalid("Coverage request needs a route snapshot version")
                })?;
                let settings = coverage_http::settings(body.get("settings"))?;
                if route["snapshot_version"] != snapshot
                    || route["active_alternative_id"] != body["alternative_id"]
                    || guard["settings_fingerprint"] != fingerprint(&json!(settings))
                {
                    return Err(ApiError::conflict(CHANGED));
                }
                let sites = &session
                    .route_alternatives
                    .iter()
                    .find(|item| item.summary["id"] == body["alternative_id"])
                    .ok_or_else(|| ApiError::conflict("The selected route alternative changed"))?
                    .rf_sites;
                (
                    settings.clone(),
                    select_sources(sites, sites, &settings).map_err(terrain_invalid)?,
                    route["route"].clone(),
                    route["links"].clone(),
                    coverage_runs::radio(app, &project.plan, route)?,
                )
            };
            guard["settings"] = json!(settings);
            guard["radio_settings"] = radio.clone();
            let inputs = Inspection {
                context,
                location,
                settings,
                sources,
                source_display,
                links,
                radio,
            };
            let outstanding = app.scheduler.reserve_job()?;
            let worker = app.scheduler.try_worker();
            let id = random_id::<16>()?;
            let mut job = guard;
            job["job_id"] = json!(id);
            job["coverage_job_id"] = body["coverage_job_id"].clone();
            job["state"] = json!(if worker.is_some() {
                "running"
            } else {
                "queued"
            });
            job["stage"] = json!(if worker.is_some() {
                "Opening terrain"
            } else {
                "Waiting for a planner worker"
            });
            job["created_at"] = json!(route_http::now());
            job["latitude"] = json!(location.latitude);
            job["longitude"] = json!(location.longitude);
            job["source_count"] = json!(inputs.sources.len());
            let control = Arc::new(Control::default());
            session.cancel_inspection();
            session.inspection_job = Some(job.clone());
            session.inspection_control = Some(control.clone());
            tokio::spawn(run(
                app.clone(),
                workspace,
                id,
                inputs,
                outstanding,
                worker,
                control,
            ));
            Ok(Json(public_job(job)))
        })
        .await
}

fn public_job(mut job: Value) -> Value {
    if let Some(fields) = job.as_object_mut() {
        for field in ["coverage_store_format", "radio_settings", "settings"] {
            fields.remove(field);
        }
        if job["stale"] == false {
            job.as_object_mut().unwrap().remove("stale");
        }
    }
    job
}

fn current(app: &AppState, key: &str, session: &Session, job: &Value) -> bool {
    coverage_runs::identity(app, key, session).is_some_and(|identity| {
        if job["coverage_job_id"].is_string() {
            identity.route_matches(job)
        } else {
            identity.matches(job)
        }
    })
}

async fn run(
    app: AppState,
    workspace: Workspace,
    id: String,
    inputs: Inspection,
    outstanding: OwnedSemaphorePermit,
    worker: Option<OwnedSemaphorePermit>,
    control: Arc<Control>,
) {
    let worker = app.scheduler.wait_worker(worker, &control).await;
    let result = if let Some(worker) = worker {
        let mark_id = id.clone();
        let mark = workspace
            .clone()
            .run(app.clone(), move |_, _, session| {
                if let Some(job) = session
                    .inspection_job
                    .as_mut()
                    .filter(|job| job["job_id"] == mark_id)
                {
                    job["state"] = json!("running");
                    job["stage"] = json!("Evaluating client links and terrain profile");
                }
                Ok(())
            })
            .await;
        let signal = control.clone();
        let coverage_cache = app.coverage_cache.clone();
        match mark {
            Ok(()) => tokio::task::spawn_blocking(move || {
                let _worker = worker;
                compute(inputs, &signal, &coverage_cache)
            })
            .await
            .map_err(ApiError::internal)
            .and_then(|result| result),
            Err(error) => Err(error),
        }
    } else {
        Ok(Value::Null)
    };
    let _ = workspace
        .run(app, move |app, key, session| {
            let Some(mut job) = session
                .inspection_job
                .as_ref()
                .filter(|job| job["job_id"] == id)
                .cloned()
            else {
                return Ok(());
            };
            if control.cancellation() != Cancellation::Continue {
                job["state"] = json!("cancelled");
                job["stage"] = json!("Location inspection cancelled");
            } else if !current(app, key, session, &job) {
                job["state"] = json!("failed");
                job["stage"] = json!(
                    "Route, terrain, project, or coverage settings changed during inspection"
                );
            } else {
                match result {
                    Ok(result) => {
                        job["state"] = json!("complete");
                        job["stage"] = json!("Location inspection complete");
                        job["result"] = result;
                    }
                    Err(error) => {
                        job["state"] = json!("failed");
                        job["stage"] = json!(error.detail());
                    }
                }
            }
            job["finished_at"] = json!(route_http::now());
            session.inspection_job = Some(job);
            session.inspection_control = None;
            drop(outstanding);
            Ok(())
        })
        .await;
}

fn compute(
    inputs: Inspection,
    control: &Control,
    coverage_cache: &CoverageMetricsCache,
) -> ApiResult<Value> {
    let (terrain, revision) = coverage_http::open_coverage_terrain(&inputs.context)?;
    let transform = Transformer::new(terrain.crs()).map_err(terrain_invalid)?;
    let rf = route_inputs::rf(&inputs.radio["rf"])?;
    let point = CoverageEvaluator::new(&terrain, &rf, &transform)
        .with_cache(coverage_cache, revision)
        .evaluate_point(
            inputs.location,
            &inputs.sources,
            &inputs.settings,
            ExactPointOptions {
                final_sample_step_m: final_sample_step(&inputs.radio),
                include_serving_profile: true,
                cancelled: &|| control.cancellation() != Cancellation::Continue,
            },
        )
        .map_err(terrain_invalid)?;
    let mut result = serde_json::to_value(&point.coverage).map_err(ApiError::internal)?;
    if point.coverage.ground_elevation_m.is_none() {
        if let Some(result) = result.as_object_mut() {
            result.remove("surface_elevation_m");
            result.remove("selected_source_id");
        }
        return Ok(result);
    }
    let selected = point.coverage.selected_source_id.as_deref().and_then(|id| {
        inputs
            .source_display
            .as_array()?
            .iter()
            .find(|site| site["id"] == id)
    });
    result["selected_source"] = json!(selected);
    result["selected_source_component"] = Value::Null;
    result["selected_source_component_size"] = Value::Null;
    if let Some(id) = point.coverage.selected_source_id.as_deref() {
        let mut connected = BTreeSet::new();
        let mut pending = vec![id];
        while let Some(id) = pending.pop() {
            if !connected.insert(id) {
                continue;
            }
            for link in inputs
                .links
                .as_array()
                .into_iter()
                .flatten()
                .filter(|link| link["valid"] == true)
            {
                if let (Some(a), Some(b)) = (link["source_id"].as_str(), link["target_id"].as_str())
                {
                    if a == id {
                        pending.push(b);
                    }
                    if b == id {
                        pending.push(a);
                    }
                }
            }
        }
        result["selected_source_component"] = json!(connected.first());
        result["selected_source_component_size"] = json!(connected.len());
    }
    result["profile_link"] = json!(point.profile_link.as_ref().map(route_output::link));
    Ok(result)
}

pub async fn detail(
    State(app): State<AppState>,
    workspace: Workspace,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    workspace
        .run(app, move |app, key, session| {
            let mut job = job(session, &id)?.clone();
            if job["state"] == "complete" && !current(app, key, session, &job) {
                job["stale"] = json!(true);
            }
            Ok(Json(public_job(job)))
        })
        .await
}

pub async fn cancel(
    State(app): State<AppState>,
    workspace: Workspace,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    workspace
        .run(app, move |_, _, session| {
            job(session, &id)?;
            if !session.inspection_active() {
                return Err(ApiError::conflict(
                    "Location inspection is no longer active",
                ));
            }
            session.cancel_inspection();
            Ok(Json(json!({"ok": true})))
        })
        .await
}

fn job<'a>(session: &'a Session, id: &str) -> ApiResult<&'a Value> {
    session
        .inspection_job
        .as_ref()
        .filter(|job| job["job_id"] == id)
        .ok_or_else(|| ApiError::missing("Location inspection was superseded or is unavailable"))
}
