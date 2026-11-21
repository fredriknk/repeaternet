use std::sync::Arc;

use axum::{
    Json,
    body::Bytes,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use repeaternet_providers::{
    shared_cache_directory,
    sources::{ProviderRegistry, TerrainDownloader, ValidationIssue},
};
use serde_json::{Value, json};

use crate::{
    api::{ApiError, ApiResult, ObjectBody, Workspace},
    catalog::AppState,
    provider_http, route_inputs,
    terrain_http::terrain_invalid,
};

fn decode(body: &[u8]) -> Result<Value, Vec<ValidationIssue>> {
    serde_json::from_slice(body).map_err(|error| {
        vec![ValidationIssue {
            path: ".".into(),
            message: format!(
                "Invalid JSON at line {}, column {}",
                error.line(),
                error.column()
            ),
        }]
    })
}

pub async fn get(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    tokio::task::spawn_blocking(move || Ok(Json(app.provider_snapshot()?.view())))
        .await
        .map_err(ApiError::internal)?
}

pub async fn put(State(app): State<AppState>, body: Bytes) -> ApiResult<Response> {
    tokio::task::spawn_blocking(move || {
        let mut registry = app.providers.lock().expect("provider registry lock");
        let current = ProviderRegistry::load(&app.data_directory).map_err(ApiError::internal)?;
        let custom = match decode(&body).and_then(|json| current.validate_edit(&json)) {
            Ok(custom) => custom,
            Err(issues) => {
                return Ok((
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "error": "Invalid data source definitions", "issues": issues
                    })),
                )
                    .into_response());
            }
        };
        ProviderRegistry::save_custom(&app.data_directory, &custom).map_err(ApiError::internal)?;
        *registry =
            Arc::new(ProviderRegistry::load(&app.data_directory).map_err(ApiError::internal)?);
        Ok(Json(registry.view()).into_response())
    })
    .await
    .map_err(ApiError::internal)?
}

pub async fn validate(State(app): State<AppState>, body: Bytes) -> ApiResult<Json<Value>> {
    tokio::task::spawn_blocking(move || {
        let registry = app.provider_snapshot()?;
        let issues = decode(&body)
            .and_then(|json| registry.validate_edit(&json))
            .err()
            .unwrap_or_default();
        Ok(Json(json!({"ok": issues.is_empty(), "issues": issues})))
    })
    .await
    .map_err(ApiError::internal)?
}

pub async fn probe(
    State(app): State<AppState>,
    Path(id): Path<String>,
    ObjectBody(body): ObjectBody,
) -> ApiResult<Json<Value>> {
    let at = route_inputs::coordinate(
        body.get("latitude").unwrap_or(&Value::Null),
        body.get("longitude").unwrap_or(&Value::Null),
        "Test point",
    )
    .map_err(|error| ApiError::new(StatusCode::BAD_REQUEST, error.detail()))?;
    let permit = app.scheduler.reserve_worker()?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let snapshot = app.provider_snapshot()?;
        snapshot
            .provider(&id)
            .map_err(|error| ApiError::new(StatusCode::BAD_REQUEST, error.to_string()))?;
        let downloader = TerrainDownloader::new(
            snapshot,
            shared_cache_directory(&app.data_directory),
            app.terrain_limits,
        )
        .map_err(terrain_invalid)?;
        Ok(Json(json!(
            downloader.probe(&id, at).map_err(terrain_invalid)?
        )))
    })
    .await
    .map_err(ApiError::internal)?
}

pub async fn preview(
    State(app): State<AppState>,
    workspace: Workspace,
    ObjectBody(body): ObjectBody,
) -> ApiResult<Json<Value>> {
    provider_http::preview_sources(app, workspace, Value::Object(body)).await
}
