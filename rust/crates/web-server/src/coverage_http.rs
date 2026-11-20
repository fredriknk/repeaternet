use std::time::Instant;

use axum::{Json, extract::State};
use repeaternet_coverage::{
    CoverageEvaluator, CoverageSettings, CoverageState, MapBounds, RunOptions, area_bounds_wgs84,
    cell_footprint, resolve_area, select_sources,
};
use repeaternet_terrain::{RasterTerrain, Transformer};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{
    api::{ApiError, ApiResult, ObjectBody, Workspace, active_project},
    catalog::AppState,
    route_http, route_inputs,
    terrain_http::{TerrainContext, terrain_invalid},
};

pub async fn estimate(
    State(app): State<AppState>,
    workspace: Workspace,
    ObjectBody(body): ObjectBody,
) -> ApiResult<Json<Value>> {
    evaluate(app, workspace, body, false).await
}

pub async fn preview(
    State(app): State<AppState>,
    workspace: Workspace,
    ObjectBody(body): ObjectBody,
) -> ApiResult<Json<Value>> {
    evaluate(app, workspace, body, true).await
}

/// Bind the opened mosaic to the same project and file revision as the job.
pub(crate) fn open_coverage_terrain(
    context: &TerrainContext,
) -> ApiResult<(RasterTerrain, [u8; 32])> {
    let terrain = context.open()?;
    let current = context
        .route
        .fingerprint(&context.directory, context.supplemental.as_ref());
    if current.as_ref() != Some(&context.fingerprint) {
        return Err(ApiError::conflict(
            "Terrain files changed while opening coverage terrain",
        ));
    }
    let mut digest = Sha256::new();
    let path = context.directory.as_os_str().as_encoded_bytes();
    for part in [
        path,
        context.project_id.as_bytes(),
        context.fingerprint.to_string().as_bytes(),
    ] {
        digest.update(part.len().to_le_bytes());
        digest.update(part);
    }
    Ok((terrain, digest.finalize().into()))
}

async fn evaluate(
    app: AppState,
    workspace: Workspace,
    body: serde_json::Map<String, Value>,
    preview: bool,
) -> ApiResult<Json<Value>> {
    let (context, route, sites, corridor, request, permit) =
        capture(app.clone(), workspace.clone(), &body, true).await?;
    let CoverageRequest {
        settings,
        map_bounds,
        snapshot,
    } = request;
    let coverage_cache = app.coverage_cache.clone();
    let (context, route, response) = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let start = Instant::now();
        let (terrain, revision) = open_coverage_terrain(&context)?;
        let transform = Transformer::new(terrain.crs()).map_err(terrain_invalid)?;
        let rf = route_inputs::rf(&route["rf_settings"])?;
        let evaluator = CoverageEvaluator::new(&terrain, &rf, &transform);
        let sources = select_sources(&sites, &sites, &settings).map_err(terrain_invalid)?;
        let area = resolve_area(&settings, &transform, &sources, &sites, corridor, map_bounds)
            .map_err(terrain_invalid)?;
        let mut response = if preview {
            let grid = evaluator.with_cache(&coverage_cache, revision)
                .calculate(&sources, &settings.preview_settings(), &area, RunOptions::default())
                .map_err(terrain_invalid)?;
            let mut cells = Vec::with_capacity(grid.cells.len());
            for cell in &grid.cells {
                let mut value = serde_json::to_value(cell).map_err(ApiError::internal)?;
                if let Some(outline) = cell_footprint(cell, grid.bounds, grid.effective_cell_size_m, &transform)
                    .map_err(terrain_invalid)?
                {
                    value["footprint_latlng"] = json!(outline);
                }
                cells.push(value);
            }
            json!({
                "preview": true, "approximate": true,
                "source_count": sources.len(), "source_ids": sources.iter().map(|site| &site.id).collect::<Vec<_>>(),
                "requested_cells": grid.requested_cells, "requested_area_cells": grid.inside_area_cells,
                "outside_area_cells": grid.outside_area_cells, "effective_cell_size_m": grid.effective_cell_size_m,
                "area_bounds_wgs84": area_bounds_wgs84(grid.bounds, &transform).map_err(terrain_invalid)?,
                "terrain_crs": terrain.crs().to_string(), "grid_bounds_projected": grid.bounds,
                "settings": settings, "snapshot_version": snapshot, "alternative_id": route["active_alternative_id"],
                "terrain_available_cells": grid.terrain_available_cells, "evaluated_cells": grid.evaluated_cells,
                "covered_cells": grid.cells.iter().filter(|cell| cell.state == CoverageState::Covered).count(),
                "unknown_cells": grid.unknown_cells, "unresolved_cells": grid.unresolved_cells,
                "unresolved_source_evaluations": grid.unresolved_source_evaluations,
                "cells": cells, "elapsed_seconds": start.elapsed().as_secs_f64(),
            })
        } else {
            let estimate = evaluator.estimate(&sources, &settings, &area, &|| false).map_err(terrain_invalid)?;
            serde_json::to_value(estimate).map_err(ApiError::internal)?
        };
        response["route_search_complete"] = route["search_complete"].clone();
        response["requested_path_count"] = route["requested_path_count"].clone();
        response["achieved_path_count"] = route["achieved_path_count"].clone();
        Ok::<_, ApiError>((context, route, response))
    }).await.map_err(ApiError::internal)??;
    workspace
        .run(app, move |app, key, session| {
            context.verify(app, key, session)?;
            let current = route_http::current(app, key, session)?;
            if current["job_id"] != route["job_id"] || current["snapshot_version"] != snapshot {
                return Err(ApiError::conflict(
                    "The selected route changed; refresh before calculating coverage",
                ));
            }
            Ok(Json(response))
        })
        .await
}

pub(crate) struct CoverageRequest {
    pub settings: CoverageSettings,
    pub map_bounds: Option<MapBounds>,
    pub snapshot: u64,
}

pub(crate) async fn capture(
    app: AppState,
    workspace: Workspace,
    body: &serde_json::Map<String, Value>,
    reserve_worker: bool,
) -> ApiResult<(
    TerrainContext,
    Value,
    Vec<repeaternet_rf_core::Site>,
    f64,
    CoverageRequest,
    Option<tokio::sync::OwnedSemaphorePermit>,
)> {
    let settings = settings(body.get("settings"))?;
    let snapshot = body
        .get("snapshot_version")
        .and_then(Value::as_u64)
        .ok_or_else(|| ApiError::invalid("Coverage request needs a route snapshot version"))?;
    let alternative = body.get("alternative_id").cloned().unwrap_or(Value::Null);
    let map_bounds: Option<MapBounds> =
        serde_json::from_value(body.get("map_bounds").cloned().unwrap_or(Value::Null))
            .map_err(terrain_invalid)?;
    let (context, route, sites, corridor, permit) = workspace
        .clone()
        .run(app.clone(), move |app, key, session| {
            if matches!(
                session.route_job["state"].as_str(),
                Some("queued" | "running" | "preparing")
            ) {
                return Err(ApiError::conflict(
                    "Wait until route search or terrain preparation finishes",
                ));
            }
            if session
                .route_result
                .as_ref()
                .is_none_or(|route| route["found"] != true)
            {
                return Err(ApiError::conflict(
                    "Find a certified route before calculating coverage",
                ));
            }
            let route = route_http::current(app, key, session)?;
            if route["snapshot_version"] != snapshot {
                return Err(ApiError::conflict(
                    "The selected route changed; refresh before calculating coverage",
                ));
            }
            if route["active_alternative_id"] != alternative {
                return Err(ApiError::conflict("The selected route alternative changed"));
            }
            let sites = session
                .route_alternatives
                .iter()
                .find(|item| item.summary["id"] == alternative)
                .ok_or_else(|| ApiError::conflict("The selected route alternative changed"))?
                .rf_sites
                .clone();
            let plan = active_project(&app.store, key, session)?.plan;
            let corridor = plan["candidates"]["corridor_width_m"]
                .as_f64()
                .unwrap_or_else(|| {
                    app.defaults["candidates"]["corridor_width_m"]
                        .as_f64()
                        .unwrap()
                });
            Ok((
                TerrainContext::capture(app, key, session, true)?,
                route.clone(),
                sites,
                corridor,
                reserve_worker
                    .then(|| app.scheduler.reserve_worker())
                    .transpose()?,
            ))
        })
        .await?;
    Ok((
        context,
        route,
        sites,
        corridor,
        CoverageRequest {
            settings,
            map_bounds,
            snapshot,
        },
        permit,
    ))
}

pub(crate) fn settings(value: Option<&Value>) -> ApiResult<CoverageSettings> {
    serde_json::from_value::<CoverageSettings>(value.cloned().unwrap_or_else(|| json!({})))
        .map_err(terrain_invalid)?
        .normalized()
        .map_err(terrain_invalid)
}
