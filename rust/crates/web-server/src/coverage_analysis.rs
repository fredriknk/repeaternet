use axum::{
    Json,
    extract::{Path, State},
};
use repeaternet_coverage::{
    ComparisonRun, CoverageCell, CoverageEvaluator, CoverageGrid, CoverageNetwork, CoverageState,
    SavedTargetReport, TargetAssessmentInput, analyze_node_failures, compare_runs,
    compare_target_reports, normalize_targets,
};
use repeaternet_coverage_store::{
    CurrentIdentity, Error, Store, comparison_mismatch, fingerprint, reason,
};
use repeaternet_rf_core::{HeightReference, Site, SiteKind, calibration::CERTIFICATION_MAX_STEP_M};
use repeaternet_terrain::{Point, Transformer};
use serde_json::{Map, Value, json};

use crate::{
    api::{ApiError, ApiResult, ObjectBody, Workspace, active_project},
    catalog::{AppState, random_id},
    coverage_http, coverage_runs, route_http, route_inputs,
    terrain_http::{TerrainContext, terrain_invalid},
};

pub(crate) fn id(value: &Value) -> Option<&str> {
    value.as_str().filter(|id| {
        id.len() == 32
            && id
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    })
}

pub(crate) fn text<'a>(value: &'a Value, detail: &'static str) -> ApiResult<&'a str> {
    value.as_str().ok_or_else(|| ApiError::invalid(detail))
}

pub(crate) fn current_complete(job: &Value, current: Option<&CurrentIdentity>) -> ApiResult<()> {
    if job["state"] != "complete" {
        return Err(ApiError::conflict(
            "Complete the coverage calculation first",
        ));
    }
    if !current.is_some_and(|current| current.matches(job)) {
        return Err(ApiError::conflict(
            "Coverage run is stale; recalculate before using it",
        ));
    }
    Ok(())
}

pub(crate) fn source_sites(job: &Value) -> ApiResult<Vec<Site>> {
    let sites = job["source_sites"]
        .as_array()
        .filter(|sites| (1..=repeaternet_coverage::MAX_SOURCES).contains(&sites.len()))
        .ok_or_else(|| ApiError::conflict("Saved coverage sources are unavailable"))?;
    let invalid = || ApiError::conflict("Saved coverage source is malformed");
    sites
        .iter()
        .map(|site| {
            let number = |field| {
                site[field]
                    .as_f64()
                    .filter(|value| value.is_finite())
                    .ok_or_else(invalid)
            };
            Ok(Site {
                id: site["id"].as_str().ok_or_else(invalid)?.to_owned(),
                position: Point {
                    x: number("x")?,
                    y: number("y")?,
                },
                kind: match site["kind"].as_str() {
                    Some("endpoint_a") => SiteKind::EndpointA,
                    Some("endpoint_b") => SiteKind::EndpointB,
                    Some("client") => SiteKind::Client,
                    Some("router") => SiteKind::Router,
                    Some("candidate") => SiteKind::Candidate,
                    _ => return Err(invalid()),
                },
                ground_elevation_m: number("ground_elevation_m")?,
                surface_elevation_m: site["surface_elevation_m"].as_f64(),
                antenna_height_m: number("antenna_height_m")?,
                terminal_clutter: site
                    .get("terminal_clutter")
                    .map(|model| serde_json::from_value(model.clone()).map_err(|_| invalid()))
                    .transpose()?
                    .unwrap_or_default(),
                clutter_height_m: site["clutter_height_m"].as_f64(),
                height_reference: match site["height_reference"].as_str() {
                    Some("ground_dtm") | None => HeightReference::GroundDtm,
                    Some("surface_dom") => HeightReference::SurfaceDom,
                    _ => return Err(invalid()),
                },
            })
        })
        .collect()
}

pub(crate) fn network(job: &Value) -> ApiResult<CoverageNetwork> {
    serde_json::from_value(job.clone())
        .map_err(|_| ApiError::conflict("Saved coverage network is malformed"))
}

pub(crate) fn stored_cells(store: &Store, job_id: &str) -> ApiResult<Vec<Value>> {
    let mut cells = Vec::new();
    store.visit_cells(job_id, |cell| {
        if cells.len() == repeaternet_coverage::MAX_CELLS {
            return Err(Error::Invalid("too many coverage cells"));
        }
        cells.push(cell);
        Ok(())
    })?;
    Ok(cells)
}

fn complete_cells(cells: Vec<Value>, job: &Value) -> ApiResult<Vec<CoverageCell>> {
    if Some(cells.len() as u64)
        != job["requested_cells"]
            .as_u64()
            .or_else(|| job["total"].as_u64())
    {
        return Err(ApiError::conflict(
            "Coverage results are incomplete; calculate again first",
        ));
    }
    cells
        .into_iter()
        .enumerate()
        .map(|(index, value)| {
            let cell: CoverageCell = serde_json::from_value(value)
                .map_err(|_| ApiError::conflict("Coverage result pages are malformed"))?;
            if cell.index != index || cell.state == CoverageState::NotEvaluated {
                return Err(ApiError::conflict(
                    "Coverage results are incomplete; calculate again first",
                ));
            }
            Ok(cell)
        })
        .collect()
}

fn typed_cells(store: &Store, job: &Value) -> ApiResult<Vec<CoverageCell>> {
    complete_cells(
        stored_cells(store, text(&job["job_id"], "Coverage job ID is missing")?)?,
        job,
    )
}

pub async fn scenario(
    State(app): State<AppState>,
    workspace: Workspace,
    Path(job_id): Path<String>,
    ObjectBody(body): ObjectBody,
) -> ApiResult<Json<Value>> {
    let message = "Choose failed router IDs and one reference router";
    let failed = body.get("failed_ids").cloned().unwrap_or_else(|| json!([]));
    let failed: Vec<String> =
        serde_json::from_value(failed).map_err(|_| ApiError::invalid(message))?;
    let reference = text(body.get("reference_id").unwrap_or(&Value::Null), message)?.to_owned();
    if failed.len() > 64 {
        return Err(ApiError::invalid(message));
    }
    workspace
        .run(app, move |app, key, session| {
            let store = coverage_runs::store(app, key, session)?;
            let job = store.load(&job_id)?;
            let terrain = TerrainContext::capture(app, key, session, true)?;
            let selected = session.coverage_job.clone().or(coverage_runs::selected(
                &store,
                &terrain.directory,
                None,
            )?);
            if selected.is_none_or(|selected| selected["job_id"] != job_id) {
                return Err(ApiError::missing(
                    "Coverage job not found in this workspace",
                ));
            }
            current_complete(&job, coverage_runs::identity(app, key, session).as_ref())?;
            let mut rows = stored_cells(&store, &job_id)?;
            let outlines = if job["terrain_crs"] == "custom projected metre CRS" {
                coverage_runs::footprints(&job, &mut rows, Some(&terrain))?;
                rows.iter_mut()
                    .map(|cell| {
                        cell.as_object_mut()
                            .and_then(|cell| cell.remove("footprint_latlng"))
                    })
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            };
            let cells = complete_cells(rows, &job)?;
            let result = analyze_node_failures(&cells, &network(&job)?, &failed, &reference)
                .map_err(terrain_invalid)?;
            let mut result = serde_json::to_value(result).map_err(ApiError::internal)?;
            if let Some(cells) = result["cells"].as_array_mut() {
                for (cell, outline) in cells.iter_mut().zip(outlines) {
                    if let Some(outline) = outline {
                        cell["footprint_latlng"] = outline;
                    }
                }
            }
            result["job_id"] = json!(job_id);
            Ok(Json(result))
        })
        .await
}

pub(crate) fn compare_saved(
    store: &Store,
    baseline_id: &str,
    scenario_id: &str,
    reference: &str,
    context: &TerrainContext,
) -> ApiResult<Value> {
    let baseline = store.load(baseline_id)?;
    let scenario = store.load(scenario_id)?;
    if let Some(reason) = comparison_mismatch(
        &context.project_id,
        &baseline,
        &scenario,
        &context.fingerprint,
    ) {
        let message = match reason {
            reason::INCOMPLETE => "Both coverage runs must be complete",
            reason::COMPARISON_TERRAIN => {
                "Runs use different or changed terrain; recalculate on the same terrain"
            }
            reason::COMPARISON_RADIO => "Runs use different radio or propagation settings",
            reason::DOMAIN | reason::COMPARISON_MODEL => {
                "Runs were calculated with different coverage model versions"
            }
            reason::COMPARISON_GRID => "Coverage runs are incomplete and cannot be compared",
            _ => "Runs have incompatible or unverified coverage fingerprints",
        };
        return Err(ApiError::conflict(message));
    }
    let as_run = |job: &Value| -> ApiResult<ComparisonRun> {
        let mut data = job.clone();
        data["terrain_fingerprint"] = json!(fingerprint(&job["terrain_fingerprint"]));
        serde_json::from_value(data)
            .map_err(|_| ApiError::conflict("Coverage run lacks compatible comparison metadata"))
    };
    let before = as_run(&baseline)?;
    let after = as_run(&scenario)?;
    for run in [&before, &after] {
        if !run.network.router_ids.iter().any(|id| id == reference)
            || !run.network.source_ids.iter().any(|id| id == reference)
        {
            return Err(ApiError::invalid(
                "Reference router must be a selected repeater in both runs",
            ));
        }
    }
    let terrain = fingerprint(&context.fingerprint);
    before
        .check_compatibility(&after, &terrain, reference)
        .map_err(|error| ApiError::new(axum::http::StatusCode::CONFLICT, error.to_string()))?;
    let left = typed_cells(store, &baseline)?;
    let right = typed_cells(store, &scenario)?;
    let comparison = compare_runs(
        &before,
        &after,
        &terrain,
        reference,
        left.iter().map(Ok),
        right.iter().map(Ok),
    )
    .map_err(|error| ApiError::new(axum::http::StatusCode::CONFLICT, error.to_string()))?;
    let mut result = serde_json::to_value(comparison).map_err(ApiError::internal)?;
    result["rows"] = scenario["rows"].clone();
    result["columns"] = scenario["columns"].clone();
    result["raster_mesh"] = json!(coverage_runs::raster_mesh(&scenario, Some(context)));
    if let Some(cells) = result["cells"].as_array_mut() {
        for (cell, source) in cells.iter_mut().zip(&right) {
            cell["x"] = json!(source.x);
            cell["y"] = json!(source.y);
            cell["state"] = json!(source.state);
        }
        coverage_runs::footprints(&scenario, cells, Some(context))?;
        for cell in cells {
            if let Some(cell) = cell.as_object_mut() {
                for field in ["x", "y", "state"] {
                    cell.remove(field);
                }
            }
        }
    }
    Ok(result)
}

pub async fn compare(
    State(app): State<AppState>,
    workspace: Workspace,
    ObjectBody(body): ObjectBody,
) -> ApiResult<Json<Value>> {
    let message = "Choose completed runs and a shared reference router";
    let field = |key| text(body.get(key).unwrap_or(&Value::Null), message).map(str::to_owned);
    let (baseline, scenario, reference) = (
        field("baseline_job_id")?,
        field("scenario_job_id")?,
        field("reference_id")?,
    );
    workspace
        .run(app, move |app, key, session| {
            let store = coverage_runs::store(app, key, session)?;
            let context = TerrainContext::capture(app, key, session, true)?;
            Ok(Json(compare_saved(
                &store, &baseline, &scenario, &reference, &context,
            )?))
        })
        .await
}

pub async fn assess(
    State(app): State<AppState>,
    workspace: Workspace,
    Path(job_id): Path<String>,
    ObjectBody(body): ObjectBody,
) -> ApiResult<Json<Value>> {
    let spacing = body
        .get("road_spacing_m")
        .map_or(Some(100.0), Value::as_f64)
        .filter(|value| (25.0..=5000.0).contains(value))
        .ok_or_else(|| ApiError::invalid("Road sample spacing must be between 25 and 5,000 m"))?;
    let (store, job, context, targets, permit) = workspace
        .clone()
        .run(app.clone(), move |app, key, session| {
            let store = coverage_runs::store(app, key, session)?;
            let job = store.load(&job_id)?;
            current_complete(&job, coverage_runs::identity(app, key, session).as_ref())?;
            let plan = active_project(&app.store, key, session)?.plan;
            let targets = plan
                .get("coverage_targets")
                .filter(|targets| {
                    targets["features"]
                        .as_array()
                        .is_some_and(|features| !features.is_empty())
                })
                .cloned()
                .ok_or_else(|| {
                    ApiError::invalid("Add or import at least one saved coverage target first")
                })?;
            Ok((
                store,
                job,
                TerrainContext::capture(app, key, session, true)?,
                targets,
                app.scheduler.reserve_worker()?,
            ))
        })
        .await?;
    let coverage_cache = app.coverage_cache.clone();
    let (store, job, context, report) = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let targets_typed =
            normalize_targets(&serde_json::to_vec(&targets).map_err(ApiError::internal)?)
                .map_err(terrain_invalid)?;
        let settings = coverage_http::settings(job.get("settings"))?;
        let (terrain, revision) = coverage_http::open_coverage_terrain(&context)?;
        let transform = Transformer::new(terrain.crs()).map_err(terrain_invalid)?;
        let rf = route_inputs::rf(&job["radio_settings"]["rf"])?;
        let cells = typed_cells(&store, &job)?;
        let grid = CoverageGrid {
            bounds: serde_json::from_value(job["grid_bounds_projected"].clone())
                .map_err(terrain_invalid)?,
            effective_cell_size_m: job["effective_cell_size_m"].as_f64().unwrap_or(0.0),
            requested_cells: cells.len(),
            ..CoverageGrid::default()
        };
        let mut assessment = CoverageEvaluator::new(&terrain, &rf, &transform)
            .with_cache(&coverage_cache, revision)
            .assess_targets(TargetAssessmentInput {
                targets: &targets_typed,
                grid: &grid,
                cells: &cells,
                network: &network(&job)?,
                sources: &source_sites(&job)?,
                settings: &settings,
                final_sample_step_m: final_sample_step(&job["radio_settings"]),
                road_spacing_m: spacing,
                cancelled: &|| false,
            })
            .map_err(terrain_invalid)?;
        assessment.assumptions.coverage_job_id = job["job_id"].as_str().map(str::to_owned);
        let mut report = serde_json::to_value(assessment).map_err(ApiError::internal)?;
        for field in [
            "project_id",
            "terrain_fingerprint",
            "radio_fingerprint",
            "model_version",
        ] {
            report[field] = job[field].clone();
        }
        report["report_id"] = json!(random_id::<16>()?);
        report["coverage_job_id"] = job["job_id"].clone();
        report["target_fingerprint"] = json!(fingerprint(&targets));
        report["target_features"] = targets["features"].clone();
        report["assessed_at"] = json!(route_http::now());
        report["client_profile"] = job["settings"]["client"].clone();
        report["profile_step_m"] = job["settings"]["profile_step_m"].clone();
        report["maximum_profile_samples"] = job["settings"]["maximum_profile_samples"].clone();
        report["grid_compatibility"] = Value::Object(
            [
                "rows",
                "columns",
                "effective_cell_size_m",
                "area_bounds_wgs84",
            ]
            .into_iter()
            .map(|field| (field.to_owned(), job[field].clone()))
            .collect::<Map<_, _>>(),
        );
        Ok::<_, ApiError>((store, job, context, report))
    })
    .await
    .map_err(ApiError::internal)??;
    workspace
        .run(app, move |app, key, session| {
            context.verify(app, key, session)?;
            let current = coverage_runs::identity(app, key, session)
                .filter(|current| current.matches(&job))
                .ok_or_else(|| {
                    ApiError::conflict(
                        "Route, terrain, or radio settings changed during target assessment",
                    )
                })?;
            let plan = active_project(&app.store, key, session)?.plan;
            if report["target_fingerprint"] != fingerprint(&plan["coverage_targets"]) {
                return Err(ApiError::conflict(
                    "Coverage targets changed during assessment",
                ));
            }
            store.save_report(
                text(&job["job_id"], "Coverage job ID is missing")?,
                &report,
                &current,
            )?;
            Ok(Json(report))
        })
        .await
}

pub(crate) fn final_sample_step(radio: &Value) -> f64 {
    radio["candidates"]["final_sample_step_m"]
        .as_f64()
        .unwrap_or(CERTIFICATION_MAX_STEP_M)
}

pub async fn reports(State(app): State<AppState>, workspace: Workspace) -> ApiResult<Json<Value>> {
    workspace
        .run(app, |app, key, session| {
            let store = coverage_runs::store(app, key, session)?;
            let current = coverage_runs::identity(app, key, session);
            let mut reports = store.reports()?;
            for report in &mut reports {
                report["stale"] = json!(!current.as_ref().is_some_and(|current| {
                    report["coverage_job_id"]
                        .as_str()
                        .and_then(|id| store.load(id).ok())
                        .is_some_and(|job| current.matches(&job))
                }));
            }
            Ok(Json(json!({"reports": reports})))
        })
        .await
}

pub async fn compare_reports(
    State(app): State<AppState>,
    workspace: Workspace,
    ObjectBody(body): ObjectBody,
) -> ApiResult<Json<Value>> {
    let message = "Choose two different saved target assessment reports";
    let field = |key| {
        body.get(key)
            .and_then(id)
            .map(str::to_owned)
            .ok_or_else(|| ApiError::invalid(message))
    };
    let (baseline, scenario) = (field("baseline_report_id")?, field("scenario_report_id")?);
    if baseline == scenario {
        return Err(ApiError::invalid(message));
    }
    workspace
        .run(app, move |app, key, session| {
            let store = coverage_runs::store(app, key, session)?;
            let load = |id| -> ApiResult<SavedTargetReport> {
                let mut report = store.find_report(id)?;
                report["terrain_fingerprint"] = json!(fingerprint(&report["terrain_fingerprint"]));
                serde_json::from_value(report).map_err(|_| {
                    ApiError::conflict("Target report lacks compatible comparison metadata")
                })
            };
            let comparison =
                compare_target_reports(&load(&baseline)?, &load(&scenario)?).map_err(|error| {
                    ApiError::new(axum::http::StatusCode::CONFLICT, error.to_string())
                })?;
            Ok(Json(
                serde_json::to_value(comparison).map_err(ApiError::internal)?,
            ))
        })
        .await
}
