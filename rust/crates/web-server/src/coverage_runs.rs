use std::{collections::BTreeMap, fs, io::Write, path::Path as FsPath, sync::Arc, time::Instant};

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use repeaternet_coverage::{
    CoverageArea, CoverageCell, CoverageError, CoverageEvaluator, CoverageNetwork,
    CoverageSettings, CoverageState, MODEL_VERSION, RunOptions, apply_height_overrides,
    cell_footprint, resolve_area, select_sources,
};
use repeaternet_coverage_store::{
    CurrentIdentity, Error, FORMAT, LegacyIdentity, Store, fingerprint, route_fingerprint,
};
use repeaternet_optimizer::Cancellation;
use repeaternet_rf_core::{RfSettings, Site};
use repeaternet_terrain::{RasterTerrain, Transformer};
use serde_json::{Value, json};
use tokio::sync::OwnedSemaphorePermit;

use crate::{
    api::{ApiError, ApiResult, ObjectBody, Workspace, active_project, ensure_idle},
    catalog::{AppState, Session, random_id},
    coverage_history, coverage_http,
    route_http::{self, Control},
    route_inputs,
    terrain_http::{TerrainContext, terrain_invalid},
};

impl From<Error> for ApiError {
    fn from(error: Error) -> Self {
        match error {
            Error::NotFound | Error::UnsafePath => {
                Self::missing("Coverage job not found in this workspace")
            }
            Error::Io(ref cause) if cause.kind() == std::io::ErrorKind::NotFound => {
                Self::missing("Coverage result pages are unavailable")
            }
            Error::ByteLimit => Self::new(StatusCode::PAYLOAD_TOO_LARGE, error.to_string()),
            Error::ReportLimit => Self::new(
                StatusCode::TOO_MANY_REQUESTS,
                "This coverage run already has 20 saved target reports",
            ),
            Error::Conflict | Error::Invalid(_) | Error::Json(_) => {
                Self::new(StatusCode::CONFLICT, error.to_string())
            }
            _ => Self::internal(error),
        }
    }
}

pub(crate) fn store(app: &AppState, key: &str, session: &mut Session) -> ApiResult<Store> {
    let project = active_project(&app.store, key, session)?;
    let storage = Store::open(app.store.project_path(key, &project)?, &project.id)?;
    if !session.coverage_recovered {
        if let Err(error) = storage.recover_interrupted() {
            eprintln!(
                "Could not recover coverage for project {}: {error}",
                project.id
            );
        }
        session.coverage_recovered = true;
    }
    Ok(storage)
}

pub(crate) fn legacy(terrain: &TerrainContext) -> LegacyIdentity {
    LegacyIdentity::from_fingerprint(&terrain.project_id, terrain.fingerprint.clone())
}

const REUSED_SELECTION: &str = "coverage-reused-selection.json";

/// The store owns the latest-started manifest. Tie a reused selection to that
/// manifest so a subsequent start supersedes the selection without a second write.
fn remember_reuse(storage: &Store, directory: &FsPath, reused: &Value) -> ApiResult<()> {
    let latest = storage
        .current()?
        .ok_or_else(|| ApiError::conflict("Coverage selection is no longer available"))?;
    if latest["job_id"] == reused["job_id"] && !directory.join(REUSED_SELECTION).exists() {
        return Ok(());
    }
    let selection = json!({"latest_job_id":latest["job_id"],"selected_job_id":reused["job_id"]});
    let mut file = tempfile::NamedTempFile::new_in(directory).map_err(ApiError::internal)?;
    serde_json::to_writer(&mut file, &selection).map_err(ApiError::internal)?;
    file.flush().map_err(ApiError::internal)?;
    file.as_file().sync_all().map_err(ApiError::internal)?;
    file.persist(directory.join(REUSED_SELECTION))
        .map_err(ApiError::internal)?;
    Ok(())
}

pub(crate) fn selected(
    storage: &Store,
    directory: &FsPath,
    identity: Option<&LegacyIdentity>,
) -> ApiResult<Option<Value>> {
    let latest = match identity {
        Some(identity) => storage.current_with_legacy(identity)?,
        None => storage.current()?,
    };
    let path = directory.join(REUSED_SELECTION);
    let selected = (|| {
        let current = latest.as_ref()?;
        if matches!(current["state"].as_str(), Some("queued" | "running")) {
            return None;
        }
        let metadata = fs::symlink_metadata(&path).ok()?;
        if !metadata.is_file() || crate::catalog::linked(&metadata) || metadata.len() > 256 {
            return None;
        }
        let value: Value = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
        if value["latest_job_id"] != current["job_id"] {
            return None;
        }
        storage
            .load(value["selected_job_id"].as_str()?)
            .ok()
            .filter(|job| job["state"] == "complete" && job["coverage_store_format"] == FORMAT)
    })();
    Ok(selected.or(latest))
}

pub(crate) fn radio(app: &AppState, plan: &Value, route: &Value) -> ApiResult<Value> {
    Ok(json!({
        "rf": route["rf_settings"],
        "candidates": route_inputs::merged(&app.defaults, plan, "candidates")?,
        "router_terminal_clutter_overrides": route_inputs::router_clutter_overrides(plan)?,
    }))
}

pub(crate) fn identity(app: &AppState, key: &str, session: &Session) -> Option<CurrentIdentity> {
    let route = route_http::current(app, key, session).ok()?;
    if route["found"] != true {
        return None;
    }
    let context = TerrainContext::capture(app, key, session, true).ok()?;
    let project = active_project(&app.store, key, session).ok()?;
    let settings = coverage_http::settings(project.plan.get("coverage")).ok()?;
    CurrentIdentity::new(json!({
        "project_id": project.id,
        "model_version": MODEL_VERSION,
        "route_fingerprint": route_fingerprint(&project.plan)?,
        "terrain_fingerprint": context.fingerprint,
        "alternative_id": route["active_alternative_id"],
        "snapshot_version": route["snapshot_version"],
        "settings_fingerprint": fingerprint(&json!(settings)),
        "radio_fingerprint": fingerprint(&radio(app, &project.plan, route).ok()?),
    }))
}

pub(crate) fn status(mut job: Value, current: Option<&CurrentIdentity>) -> Value {
    if job.is_object() {
        let route_current = current.is_some_and(|current| current.route_matches(&job));
        let reason = if job["coverage_store_format"] == FORMAT {
            current.map_or(
                Some(repeaternet_coverage_store::reason::CURRENT_UNAVAILABLE),
                |current| current.stale_reason(&job),
            )
        } else {
            job["stale_reason"].as_str()
        };
        if let Some(reason) = reason {
            job["stale_reason"] = json!(reason);
            job["stale"] = json!(true);
        } else {
            job["stale"] = json!(false);
        }
        job["route_current"] = json!(route_current);
    }
    job
}

struct Prepared {
    terrain: RasterTerrain,
    terrain_revision: [u8; 32],
    transform: Transformer,
    rf: RfSettings,
    sites: Vec<Site>,
    sources: Vec<Site>,
    settings: CoverageSettings,
    area: CoverageArea,
    network: CoverageNetwork,
    overrides: BTreeMap<String, f64>,
    manifest: Value,
}

pub async fn start(
    State(app): State<AppState>,
    workspace: Workspace,
    ObjectBody(body): ObjectBody,
) -> ApiResult<Json<Value>> {
    let (context, route, sites, corridor, request, _) =
        coverage_http::capture(app.clone(), workspace.clone(), &body, false).await?;
    let overrides = serde_json::from_value::<BTreeMap<String, f64>>(
        body.get("source_height_overrides")
            .cloned()
            .unwrap_or_else(|| json!({})),
    )
    .map_err(terrain_invalid)?;
    let outstanding = app.scheduler.reserve_job()?;
    let (context, route, prepared) = tokio::task::spawn_blocking(move || {
        let settings = request.settings;
        let (terrain, terrain_revision) = coverage_http::open_coverage_terrain(&context)?;
        let transform = Transformer::new(terrain.crs()).map_err(terrain_invalid)?;
        let rf = route_inputs::rf(&route["rf_settings"])?;
        let sources = select_sources(&sites, &sites, &settings).map_err(terrain_invalid)?;
        let mut sources = apply_height_overrides(&sources, &overrides).map_err(terrain_invalid)?;
        for source in &mut sources {
            source.sample_terminal_clutter(&terrain).map_err(terrain_invalid)?;
        }
        let area = resolve_area(&settings, &transform, &sources, &sites, corridor, request.map_bounds)
            .map_err(terrain_invalid)?;
        let evaluation = CoverageEvaluator::new(&terrain, &rf, &transform);
        let estimate = evaluation.estimate(&sources, &settings, &area, &|| false).map_err(terrain_invalid)?;
        let selected = route["alternatives"].as_array().into_iter().flatten()
            .find(|item| item["id"] == route["active_alternative_id"]);
        let router_ids = selected.and_then(|item| item.get("router_ids")).cloned()
            .unwrap_or_else(|| json!(sites.iter().filter(|site| {
                !matches!(site.kind, repeaternet_rf_core::SiteKind::EndpointA | repeaternet_rf_core::SiteKind::EndpointB)
            }).map(|site| &site.id).collect::<Vec<_>>()));
        let network_sites = route["route"].as_array().ok_or_else(|| ApiError::conflict("Route sites are unavailable"))?;
        let source_sites = sources.iter().map(|source| {
            let mut site = network_sites.iter().find(|item| item["id"] == source.id).cloned()
                .ok_or_else(|| ApiError::conflict("Route source is unavailable"))?;
            if overrides.contains_key(&source.id) {
                site["antenna_height_m"] = json!(source.antenna_height_m);
                site["height_override"] = json!(true);
            }
            site["clutter_height_m"] = json!(source.clutter_height_m);
            Ok(site)
        }).collect::<ApiResult<Vec<_>>>()?;
        let report_links: Vec<_> = route["links"].as_array().into_iter().flatten().map(|link| {
            json!({"source_id": link["source_id"], "target_id": link["target_id"],
                "valid": link["valid"], "distance_m": link["distance_m"], "worst_margin_db": link["worst_margin_db"],
                "los_clear": link["los_clear"], "fresnel_clear": link["fresnel_clear"]})
        }).collect();
        let network: CoverageNetwork = serde_json::from_value(json!({
            "router_ids": router_ids, "source_ids": estimate.source_ids, "network_links": report_links,
        })).map_err(ApiError::internal)?;
        let [left, bottom, right, top] = area.bounds();
        let size = estimate.effective_cell_size_m;
        let mut manifest = json!({
            "coverage_store_format": FORMAT, "model_version": MODEL_VERSION, "project_id": context.project_id,
            "terrain_fingerprint": context.fingerprint, "terrain_crs": terrain.crs().to_string(),
            "terrain_sources": context.route.sources(&context.directory).into_iter()
                .chain(context.supplemental.iter().flat_map(|files| files.sources(&context.directory)))
                .collect::<Vec<_>>(),
            "settings": settings, "settings_fingerprint": fingerprint(&json!(settings)),
            "source_ids": network.source_ids, "source_sites": source_sites, "network_sites": network_sites,
            "network_links": network.network_links, "report_links": report_links,
            "router_ids": network.router_ids, "source_height_overrides": overrides,
            "alternative_id": route["active_alternative_id"], "snapshot_version": route["snapshot_version"],
            "input_revision": route["input_revision"], "route_search_complete": route["search_complete"],
            "achieved_path_count": route["achieved_path_count"], "requested_path_count": route["requested_path_count"],
            "grid_bounds_projected": area.bounds(), "area_bounds_wgs84": estimate.area_bounds_wgs84,
            "rows": ((top - bottom) / size).ceil().max(1.0) as usize,
            "columns": ((right - left) / size).ceil().max(1.0) as usize,
            "requested_cells": estimate.requested_cells, "total": estimate.requested_cells,
            "requested_area_cells": estimate.requested_area_cells, "outside_area_cells": estimate.outside_area_cells,
            "effective_cell_size_m": size, "requested_cell_size_m": settings.cell_size_m,
            "surface_available": estimate.surface_available, "terrain_available_cells": estimate.terrain_available_cells,
            "mode": settings.mode, "stale": false, "done": 0, "covered_cells": 0, "evaluated_cells": 0,
            "unknown_cells": 0, "unresolved_cells": 0, "unresolved_source_evaluations": 0,
        });
        manifest["raster_mesh"] = coverage_http::raster_mesh(area.bounds(), size, &transform)?;
        Ok::<_, ApiError>((context, route, Prepared {
            terrain, terrain_revision, transform, rf, sites, sources, settings, area, network, overrides, manifest,
        }))
    }).await.map_err(ApiError::internal)??;
    workspace
        .clone()
        .run(app, move |app, key, session| {
            ensure_idle(session)?;
            if session.inspection_active() {
                return Err(ApiError::conflict(
                    "A coverage calculation or inspection is already running",
                ));
            }
            context.verify(app, key, session)?;
            if route_http::current(app, key, session)?["job_id"] != route["job_id"] {
                return Err(ApiError::conflict(
                    "The selected route changed; refresh before calculating coverage",
                ));
            }
            let project = active_project(&app.store, key, session)?;
            let mut prepared = prepared;
            prepared.manifest["route_fingerprint"] = json!(route_fingerprint(&project.plan));
            let radio = radio(app, &project.plan, &route)?;
            prepared.manifest["radio_fingerprint"] = json!(fingerprint(&radio));
            prepared.manifest["radio_settings"] = radio;
            let current = identity(app, key, session)
                .filter(|current| current.matches(&prepared.manifest))
                .ok_or_else(|| {
                    ApiError::conflict(
                        "Save coverage settings and refresh the route before calculating",
                    )
                })?;
            let storage = store(app, key, session)?;
            let validation = Instant::now();
            if let Some(reused) = storage.find_reusable(&prepared.manifest, &current)? {
                remember_reuse(&storage, &context.directory, &reused)?;
                let validation_seconds = validation.elapsed().as_secs_f64();
                session.coverage_job = Some(reused.clone());
                let mut response = reused;
                response["reused"] = json!(true);
                response["reuse_validation_seconds"] = json!(validation_seconds);
                return Ok(Json(response));
            }
            let worker = app.scheduler.try_worker();
            let id = random_id::<16>()?;
            prepared.manifest["job_id"] = json!(id);
            prepared.manifest["created_at"] = json!(route_http::now());
            prepared.manifest["state"] = json!(if worker.is_some() {
                "running"
            } else {
                "queued"
            });
            prepared.manifest["stage"] = json!(if worker.is_some() {
                "Opening terrain"
            } else {
                "Waiting for a planner worker"
            });
            let job = storage.start(prepared.manifest.clone())?;
            let control = Arc::new(Control::default());
            session.coverage_job = Some(job.clone());
            session.coverage_control = Some(control.clone());
            tokio::spawn(run(
                app.clone(),
                Workspace::worker(key, &session.project_id),
                Job {
                    context,
                    storage,
                    id,
                    prepared,
                    control,
                },
                outstanding,
                worker,
            ));
            Ok(Json(job))
        })
        .await
}

struct Job {
    context: TerrainContext,
    storage: Store,
    id: String,
    prepared: Prepared,
    control: Arc<Control>,
}

async fn run(
    app: AppState,
    workspace: Workspace,
    job: Job,
    outstanding: OwnedSemaphorePermit,
    worker: Option<OwnedSemaphorePermit>,
) {
    let Job {
        context,
        storage,
        id,
        prepared,
        control,
    } = job;
    let worker = app.scheduler.wait_worker(worker, &control).await;
    let started = Instant::now();
    let result = if let Some(worker) = worker {
        let app = app.clone();
        let key = workspace.key.clone();
        let storage = storage.clone();
        let id = id.clone();
        let control = control.clone();
        tokio::task::spawn_blocking(move || {
            let _worker = worker;
            compute(&app, &key, &storage, &id, prepared, &control)
        })
        .await
        .map_err(ApiError::internal)
        .and_then(|result| result)
    } else {
        Ok(())
    };
    let _ = workspace
        .run(app, move |app, key, session| {
            let cancelled = control.cancellation() != Cancellation::Continue;
            let current = context.verify(app, key, session).is_ok()
                && identity(app, key, session).is_some_and(|current| {
                    session
                        .coverage_job
                        .as_ref()
                        .is_some_and(|job| current.matches(job))
                });
            let (state, stage) = match result {
                _ if cancelled => ("cancelled", "Coverage calculation cancelled".to_owned()),
                _ if !current => (
                    "stale",
                    "Route inputs or terrain changed during coverage".to_owned(),
                ),
                Ok(()) => ("complete", "Coverage calculation complete".to_owned()),
                Err(error) => ("failed", error.detail().to_owned()),
            };
            let mut patch = json!({"state": state, "stage": stage, "stale": state == "stale",
                "elapsed_seconds": started.elapsed().as_secs_f64(), "finished_at": route_http::now()});
            let finished = storage.finish(&id, patch.clone(), &[]).or_else(|error| {
                patch["state"] = json!("failed");
                patch["stage"] = json!(error.to_string());
                storage.finish(&id, patch.clone(), &[])
            });
            if session
                .coverage_job
                .as_ref()
                .is_some_and(|job| job["job_id"] == id)
            {
                session.coverage_control = None;
                match finished {
                    Ok(job) => session.coverage_job = Some(job),
                    Err(error) => {
                        // Keep the last committed file as the durable source of truth.
                        session.coverage_recovered = false;
                        session.coverage_job = None;
                        eprintln!("Could not commit coverage terminal state: {error}");
                    }
                }
            }
            drop(outstanding);
            Ok(())
        })
        .await;
}

fn compute(
    app: &AppState,
    key: &str,
    storage: &Store,
    id: &str,
    prepared: Prepared,
    control: &Control,
) -> ApiResult<()> {
    let cancelled = || control.cancellation() != Cancellation::Continue;
    if cancelled() {
        return Ok(());
    }
    let evaluation = CoverageEvaluator::new(&prepared.terrain, &prepared.rf, &prepared.transform)
        .with_cache(&app.coverage_cache, prepared.terrain_revision);
    let backbone = evaluation.revalidate_backbone(
        &prepared.network,
        &prepared.sites,
        &prepared.sources,
        &prepared.overrides,
        &prepared.settings,
        &cancelled,
    );
    let backbone = match backbone {
        Err(CoverageError::Cancelled) => return Ok(()),
        result => result.map_err(terrain_invalid)?,
    };
    let mut progress = serde_json::to_value(backbone).map_err(ApiError::internal)?;
    if !prepared.overrides.is_empty() {
        let mut report_links = prepared.manifest["report_links"].clone();
        if let (Some(report), Some(network)) = (
            report_links.as_array_mut(),
            progress["network_links"].as_array(),
        ) {
            for (link, edge) in report.iter_mut().zip(network) {
                for field in ["valid", "unresolved", "rejection", "rejection_detail"] {
                    link[field] = edge[field].clone();
                }
                if edge["unresolved"] == true {
                    for field in ["worst_margin_db", "los_clear", "fresnel_clear"] {
                        link[field] = Value::Null;
                    }
                }
            }
        }
        progress["report_links"] = report_links;
    }
    progress["state"] = json!("running");
    progress["stage"] = json!("Calculating coverage");
    let mut manifest = storage.progress(id, progress, &[])?;
    let session = app.session(
        key,
        prepared.manifest["project_id"]
            .as_str()
            .expect("coverage project ID"),
    );
    let update = |manifest: &Value| {
        if let Some(session) = session.blocking_lock().as_mut() {
            session.coverage_job = Some(manifest.clone());
        }
    };
    update(&manifest);
    let mut failure = None;
    let mut chunk = |cells: &[CoverageCell]| {
        let publish = || -> ApiResult<Value> {
            let mut patch = json!({});
            for (field, count) in [
                ("done", cells.len()),
                (
                    "covered_cells",
                    cells
                        .iter()
                        .filter(|cell| cell.state == CoverageState::Covered)
                        .count(),
                ),
                (
                    "evaluated_cells",
                    cells
                        .iter()
                        .filter(|cell| {
                            matches!(
                                cell.state,
                                CoverageState::Covered | CoverageState::Uncovered
                            )
                        })
                        .count(),
                ),
                (
                    "unknown_cells",
                    cells
                        .iter()
                        .filter(|cell| cell.state == CoverageState::UnknownTerrain)
                        .count(),
                ),
                (
                    "unresolved_cells",
                    cells
                        .iter()
                        .filter(|cell| cell.state == CoverageState::Unresolved)
                        .count(),
                ),
                (
                    "unresolved_source_evaluations",
                    cells.iter().map(|cell| cell.unresolved_sources).sum(),
                ),
            ] {
                patch[field] = json!(manifest[field].as_u64().unwrap_or(0) + count as u64);
            }
            let cells = cells
                .iter()
                .map(|cell| {
                    let mut value = serde_json::to_value(cell).map_err(ApiError::internal)?;
                    if prepared.terrain.crs().epsg_code().is_none()
                        && let Some(outline) = cell_footprint(
                            cell,
                            prepared.area.bounds(),
                            prepared.manifest["effective_cell_size_m"]
                                .as_f64()
                                .unwrap_or_default(),
                            &prepared.transform,
                        )
                        .map_err(terrain_invalid)?
                    {
                        value["footprint_latlng"] = json!(outline);
                    }
                    Ok(value)
                })
                .collect::<ApiResult<Vec<_>>>()?;
            Ok(storage.progress(id, patch, &cells)?)
        };
        match publish() {
            Ok(job) => {
                manifest = job;
                update(&manifest);
                Ok(())
            }
            Err(error) => {
                failure = Some(error);
                Err(CoverageError::Cancelled)
            }
        }
    };
    let result = evaluation.calculate(
        &prepared.sources,
        &prepared.settings,
        &prepared.area,
        RunOptions {
            cancelled: &cancelled,
            on_chunk: Some(&mut chunk),
            chunk_size: 20,
            progress: None,
        },
    );
    if let Some(error) = failure {
        return Err(error);
    }
    match result {
        Ok(_) | Err(CoverageError::Cancelled) => Ok(()),
        Err(error) => Err(terrain_invalid(error)),
    }
}

pub async fn cancel(
    State(app): State<AppState>,
    workspace: Workspace,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    workspace
        .run(app, move |app, key, session| {
            let storage = store(app, key, session)?;
            let context = TerrainContext::capture(app, key, session, true)?;
            let job =
                session
                    .coverage_job
                    .clone()
                    .or(selected(&storage, &context.directory, None)?);
            if job.as_ref().is_none_or(|job| job["job_id"] != id) {
                return Err(ApiError::missing(
                    "Coverage job not found in this workspace",
                ));
            }
            let control = session
                .coverage_control
                .as_ref()
                .ok_or_else(|| ApiError::conflict("There is no active coverage job"))?;
            control.cancel(false);
            session.coverage_job =
                Some(storage.progress(&id, json!({"stage":"Cancelling coverage"}), &[])?);
            Ok(Json(json!({"ok": true})))
        })
        .await
}

pub async fn history(State(app): State<AppState>, workspace: Workspace) -> ApiResult<Json<Value>> {
    workspace
        .run(app, |app, key, session| {
            let current = identity(app, key, session);
            let terrain = TerrainContext::capture(app, key, session, true).ok();
            let storage = store(app, key, session)?;
            let history = match terrain.as_ref() {
                Some(terrain) => storage.history_with_legacy(&legacy(terrain))?,
                None => storage.history()?,
            };
            let jobs: Vec<_> = history
                .into_iter()
                .take(50)
                .map(|job| {
                    coverage_history::job_summary(
                        status(job, current.as_ref()),
                        terrain.as_ref().map(|context| &context.fingerprint),
                    )
                })
                .collect();
            Ok(Json(json!({"jobs": jobs})))
        })
        .await
}

pub async fn detail(
    State(app): State<AppState>,
    workspace: Workspace,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    workspace
        .run(app, move |app, key, session| {
            let storage = store(app, key, session)?;
            let terrain = TerrainContext::capture(app, key, session, true).ok();
            let mut job = match terrain.as_ref() {
                Some(terrain) => storage.load_with_legacy(&id, &legacy(terrain))?,
                None => storage.load(&id)?,
            };
            if job["raster_mesh"].is_null()
                && let Some(mesh) = raster_mesh(&job, terrain.as_ref())
            {
                job["raster_mesh"] = mesh;
            }
            Ok(Json(status(job, identity(app, key, session).as_ref())))
        })
        .await
}

pub async fn cells(
    State(app): State<AppState>,
    workspace: Workspace,
    Path(id): Path<String>,
    axum::extract::Query(query): axum::extract::Query<BTreeMap<String, String>>,
) -> ApiResult<Json<Value>> {
    let number = |name, default| {
        query
            .get(name)
            .map(|value| value.parse::<u64>())
            .transpose()
            .map(|number| number.unwrap_or(default))
            .map_err(|_| ApiError::invalid(format!("Invalid {name}")))
    };
    let cursor = number("cursor", 0)?;
    let limit = number("limit", 128)?;
    if !(1..=256).contains(&limit) {
        return Err(ApiError::invalid("Page limit must be 1 through 256"));
    }
    workspace
        .run(app, move |app, key, session| {
            let storage = store(app, key, session)?;
            let job = storage.load(&id)?;
            let current = identity(app, key, session);
            let terrain = TerrainContext::capture(app, key, session, true).ok();
            let mut page = storage.page_with_identities(
                &id,
                cursor,
                limit as usize,
                current.as_ref(),
                terrain.as_ref().map(legacy).as_ref(),
            )?;
            footprints(&job, &mut page.cells, terrain.as_ref())?;
            Ok(Json(
                serde_json::to_value(page).map_err(ApiError::internal)?,
            ))
        })
        .await
}

pub(crate) fn transform(job: &Value, terrain: Option<&TerrainContext>) -> ApiResult<Transformer> {
    let crs = job["terrain_crs"]
        .as_str()
        .ok_or_else(|| ApiError::conflict("Coverage terrain CRS is unavailable"))?;
    if crs == "custom projected metre CRS"
        && let Some(terrain) =
            terrain.filter(|terrain| terrain.fingerprint == job["terrain_fingerprint"])
    {
        return Transformer::new(terrain.open()?.crs()).map_err(terrain_invalid);
    }
    let code = crs
        .strip_prefix("EPSG:")
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| ApiError::conflict("Coverage terrain CRS is unavailable"))?;
    let crs = repeaternet_terrain::TerrainCrs::from_epsg(code).map_err(terrain_invalid)?;
    Transformer::new(crs).map_err(terrain_invalid)
}

pub(crate) fn raster_mesh(job: &Value, terrain: Option<&TerrainContext>) -> Option<Value> {
    if !job["raster_mesh"].is_null() {
        return Some(job["raster_mesh"].clone());
    }
    let bounds = serde_json::from_value(job["grid_bounds_projected"].clone()).ok()?;
    let size = job["effective_cell_size_m"].as_f64()?;
    let transform = transform(job, terrain).ok()?;
    coverage_http::raster_mesh(bounds, size, &transform).ok()
}

pub(crate) fn footprints(
    job: &Value,
    cells: &mut [Value],
    terrain: Option<&TerrainContext>,
) -> ApiResult<()> {
    if cells.is_empty() || job["grid_bounds_projected"].is_null() || job["terrain_crs"].is_null() {
        return Ok(());
    }
    if cells
        .iter()
        .all(|cell| cell["state"] == "outside_area" || cell["footprint_latlng"].is_array())
    {
        return Ok(());
    }
    let transform = transform(job, terrain)?;
    let bounds =
        serde_json::from_value(job["grid_bounds_projected"].clone()).map_err(terrain_invalid)?;
    let size = job["effective_cell_size_m"]
        .as_f64()
        .ok_or_else(|| ApiError::conflict("Cell spacing is unavailable"))?;
    for cell in cells {
        if cell["footprint_latlng"].is_array() {
            continue;
        }
        let point = CoverageCell {
            x: cell["x"]
                .as_f64()
                .ok_or_else(|| ApiError::conflict("Coverage cell location is unavailable"))?,
            y: cell["y"]
                .as_f64()
                .ok_or_else(|| ApiError::conflict("Coverage cell location is unavailable"))?,
            state: if cell["state"] == "outside_area" {
                CoverageState::OutsideArea
            } else {
                CoverageState::Covered
            },
            ..CoverageCell::default()
        };
        if let Some(outline) =
            cell_footprint(&point, bounds, size, &transform).map_err(terrain_invalid)?
        {
            cell["footprint_latlng"] = json!(outline);
        }
    }
    Ok(())
}
