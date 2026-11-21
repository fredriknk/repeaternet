use std::{
    fs::File,
    io::{BufRead, BufReader, BufWriter, Seek, Write},
};

use axum::{
    body::Body,
    extract::{Path as RoutePath, Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use repeaternet_coverage_store::{Error, Store};
use repeaternet_exports::{
    ExportError,
    coverage::{
        CoverageAttachments, CoverageFormat, CoverageGrid, CoverageOverlay, MetadataOptions,
        build_route_overlay, write_coverage,
    },
    route::{
        CoverageOmission, RouteReportOptions, write_route_csv, write_route_geojson,
        write_route_html,
    },
    spool,
};
use repeaternet_terrain::{LonLat, Point, Transformer};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::io::ReaderStream;

use crate::{
    api::{ApiError, ApiResult, Workspace, active_project},
    catalog::{self, AppState},
    coverage_analysis::compare_saved,
    coverage_runs, route_http, route_inputs,
    terrain_http::TerrainContext,
};

impl From<ExportError> for ApiError {
    fn from(error: ExportError) -> Self {
        match error {
            ExportError::LimitExceeded => {
                Self::new(StatusCode::PAYLOAD_TOO_LARGE, error.to_string())
            }
            ExportError::Invalid(_) | ExportError::Json(_) => {
                Self::new(StatusCode::CONFLICT, error.to_string())
            }
            _ => Self::internal(error),
        }
    }
}

#[derive(Deserialize, Default)]
pub struct Options {
    include_partial: Option<String>,
    include_stale: Option<String>,
    target_report_id: Option<String>,
    scenario_job_id: Option<String>,
    reference_id: Option<String>,
}

fn boolean(value: Option<&str>, field: &str) -> ApiResult<bool> {
    match value.map(str::to_ascii_lowercase).as_deref() {
        Some("1" | "true" | "on" | "yes" | "y" | "t") => Ok(true),
        None | Some("0" | "false" | "off" | "no" | "n" | "f") => Ok(false),
        _ => Err(ApiError::invalid(format!("Invalid {field}"))),
    }
}

pub async fn coverage_geojson(
    State(app): State<AppState>,
    workspace: Workspace,
    RoutePath(id): RoutePath<String>,
    Query(options): Query<Options>,
) -> ApiResult<Response> {
    coverage(app, workspace, id, options, CoverageFormat::GeoJson).await
}

pub async fn coverage_json(
    State(app): State<AppState>,
    workspace: Workspace,
    RoutePath(id): RoutePath<String>,
    Query(options): Query<Options>,
) -> ApiResult<Response> {
    coverage(app, workspace, id, options, CoverageFormat::Json).await
}

pub async fn coverage_html(
    State(app): State<AppState>,
    workspace: Workspace,
    RoutePath(id): RoutePath<String>,
    Query(options): Query<Options>,
) -> ApiResult<Response> {
    coverage(app, workspace, id, options, CoverageFormat::Html).await
}

struct CoverageDownload {
    store: Store,
    job: Value,
    context: TerrainContext,
}

impl CoverageDownload {
    fn job_id(&self) -> &str {
        self.job["job_id"].as_str().expect("store validates job ID")
    }

    fn transform(&self) -> ApiResult<Transformer> {
        coverage_runs::transform(&self.job, Some(&self.context))
    }

    fn target_report(&self, selected: Option<&str>) -> ApiResult<Option<Value>> {
        if let Some(selected) = selected.filter(|id| !id.is_empty()) {
            return self
                .store
                .load_report(self.job_id(), selected)
                .map(Some)
                .map_err(|error| match error {
                    Error::NotFound | Error::UnsafePath => {
                        ApiError::missing("Target report not found for this coverage run")
                    }
                    _ => error.into(),
                });
        }
        let prefix = format!("{}-", self.job_id());
        let folder = self.context.directory.join("coverage-target-reports");
        Ok(catalog::entries(&folder, false)
            .filter_map(|entry| {
                let name = entry.file_name();
                let report = name
                    .to_str()?
                    .strip_prefix(&prefix)?
                    .strip_suffix(".json")?;
                self.store.load_report(self.job_id(), report).ok()
            })
            .max_by(|a, b| {
                a["assessed_at"]
                    .as_f64()
                    .unwrap_or_default()
                    .total_cmp(&b["assessed_at"].as_f64().unwrap_or_default())
            }))
    }

    fn cell_snapshot(&self) -> ApiResult<File> {
        let mut file = tempfile::tempfile().map_err(ApiError::internal)?;
        let mut writer = BufWriter::new(&mut file);
        self.store.visit_cells(self.job_id(), |cell| {
            serde_json::to_writer(&mut writer, &cell)?;
            writer.write_all(b"\n")?;
            Ok(())
        })?;
        writer.flush().map_err(ApiError::internal)?;
        drop(writer);
        file.rewind().map_err(ApiError::internal)?;
        Ok(file)
    }
}

fn cell_rows(file: &mut File) -> impl Iterator<Item = Result<Value, ExportError>> {
    BufReader::new(file)
        .lines()
        .map(|line| serde_json::from_str(&line?).map_err(ExportError::from))
}

fn reverse(transform: &Transformer, [x, y]: [f64; 2]) -> Result<[f64; 2], ExportError> {
    let point = transform
        .reverse(Point { x, y })
        .map_err(|error| ExportError::Invalid(error.to_string()))?;
    Ok([point.longitude, point.latitude])
}

fn forward(
    transform: &Transformer,
    [longitude, latitude]: [f64; 2],
) -> Result<[f64; 2], ExportError> {
    let point = transform
        .forward(LonLat {
            longitude,
            latitude,
        })
        .map_err(|error| ExportError::Invalid(error.to_string()))?;
    Ok([point.x, point.y])
}

async fn coverage(
    app: AppState,
    workspace: Workspace,
    job_id: String,
    options: Options,
    format: CoverageFormat,
) -> ApiResult<Response> {
    let include_partial = boolean(options.include_partial.as_deref(), "include_partial")?;
    let include_stale = boolean(options.include_stale.as_deref(), "include_stale")?;
    let download = workspace
        .run(app, move |app, key, session| {
            let store = coverage_runs::store(app, key, session)?;
            let job = store.load(&job_id)?;
            let status =
                coverage_runs::status(job, coverage_runs::identity(app, key, session).as_ref());
            Ok(CoverageDownload {
                store,
                job: status,
                context: TerrainContext::capture(app, key, session, true)?,
            })
        })
        .await?;
    tokio::task::spawn_blocking(move || {
        let job = &download.job;
        if matches!(job["state"].as_str(), Some("queued" | "running")) {
            return Err(ApiError::conflict("Wait for the coverage run to stop before exporting"));
        }
        if job["state"] != "complete" && !include_partial {
            return Err(ApiError::conflict("This run is partial; set include_partial=true to export it explicitly"));
        }
        if job["stale"] == true && !include_stale {
            return Err(ApiError::conflict("This run is stale; set include_stale=true to export it explicitly"));
        }
        let transform = download.transform()?;
        let reverse = |xy| reverse(&transform, xy);
        let forward = |xy| forward(&transform, xy);
        let grid = CoverageGrid::new(job, &reverse, &forward)?;
        let mut rows = download.cell_snapshot()?;
        let counts = grid.summarize(cell_rows(&mut rows))?;
        if counts.has_incomplete_cells() && !include_partial {
            return Err(ApiError::conflict("This run contains unresolved or partial cells; set include_partial=true to export it explicitly"));
        }
        let target_report = download.target_report(options.target_report_id.as_deref())?;
        let comparison = if matches!(format, CoverageFormat::GeoJson) {
            None
        } else {
            match (options.scenario_job_id.as_deref(), options.reference_id.as_deref()) {
                (None, None) => None,
                (Some(scenario), Some(reference)) if !scenario.is_empty() && !reference.is_empty() => {
                    let report = compare_saved(&download.store, download.job_id(), scenario, reference, &download.context)?;
                    Some(Value::Object([
                        "baseline_job_id", "scenario_job_id", "reference_id", "effective_cell_size_m",
                        "approximate_sampled_area_km2", "baseline_connected_source_ids", "scenario_connected_source_ids", "counts",
                    ].into_iter().map(|field| (field.to_owned(), report[field].clone())).collect()))
                }
                _ => return Err(ApiError::invalid("Scenario export needs both a scenario run and a reference router")),
            }
        };
        let mut metadata = grid.metadata(&counts, MetadataOptions {
            stale: job["stale"] == true, target_report: target_report.as_ref(), exported_at: Some(route_http::now()),
        });
        if let Some(sources) = job.get("terrain_sources") {
            metadata["terrain_sources"] = sources.clone();
        }
        let attachments = CoverageAttachments {
            target_report: target_report.as_ref(), scenario_comparison: comparison.as_ref(), ..Default::default()
        };
        let file = spool(|file| {
            rows.rewind()?;
            let mut writer = BufWriter::new(file);
            write_coverage(&mut writer, format, &grid, cell_rows(&mut rows), &metadata, &attachments)?;
            writer.flush()?;
            Ok(())
        })?;
        attachment(file, format.media_type(), &format.filename(download.job_id()))
    }).await.map_err(ApiError::internal)?
}

fn attachment(file: File, media_type: &'static str, filename: &str) -> ApiResult<Response> {
    let size = file.metadata().map_err(ApiError::internal)?.len();
    let body = Body::from_stream(ReaderStream::new(tokio::fs::File::from_std(file)));
    Ok((
        [
            (header::CONTENT_TYPE, media_type.to_owned()),
            (header::CONTENT_LENGTH, size.to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{filename}\""),
            ),
        ],
        body,
    )
        .into_response())
}

fn route_overlay(download: CoverageDownload) -> ApiResult<Option<CoverageOverlay>> {
    let transform = download.transform()?;
    let reverse = |xy| reverse(&transform, xy);
    let forward = |xy| forward(&transform, xy);
    let grid = CoverageGrid::new(&download.job, &reverse, &forward)?;
    let mut rows = download.cell_snapshot()?;
    let counts = grid.summarize(cell_rows(&mut rows))?;
    if counts.has_incomplete_cells() {
        return Ok(None);
    }
    let report = download.target_report(None)?;
    let metadata = grid.metadata(
        &counts,
        MetadataOptions {
            target_report: report.as_ref(),
            ..Default::default()
        },
    );
    rows.rewind().map_err(ApiError::internal)?;
    Ok(Some(build_route_overlay(
        &grid,
        cell_rows(&mut rows),
        &metadata,
        report.as_ref(),
    )?))
}

struct RouteDownload {
    route: Value,
    project_id: String,
    rf: Value,
    candidates: Value,
    endpoints: [Option<[f64; 2]>; 2],
    resolution: Option<f64>,
    surface: Option<bool>,
    paths: Vec<Vec<String>>,
}

pub async fn route(app: AppState, workspace: Workspace, kind: &str) -> ApiResult<Response> {
    let format = match kind {
        "csv" => ("text/csv; charset=utf-8", "route.csv"),
        "geojson" => ("application/geo+json", "route.geojson"),
        "html" => ("text/html; charset=utf-8", "route-report.html"),
        _ => return Err(ApiError::missing("No result to export")),
    };
    let html = kind == "html";
    let (download, overlay) = workspace
        .clone()
        .run(app.clone(), move |app, key, session| {
            let route = route_http::current(app, key, session)?.clone();
            let project = active_project(&app.store, key, session)?;
            let paths = session
                .route_alternatives
                .iter()
                .find(|alternative| alternative.summary["id"] == route["active_alternative_id"])
                .map(|alternative| alternative.client_paths.clone())
                .unwrap_or_default();
            let mut rf = route["rf_settings"].clone();
            if let Ok(settings) = route_inputs::rf(&rf) {
                rf["effective_sensitivity_dbm"] = json!(settings.effective_sensitivity_dbm().ok());
            }
            let download = RouteDownload {
                route,
                project_id: project.id,
                rf,
                paths,
                candidates: route_inputs::merged(&app.defaults, &project.plan, "candidates")?,
                endpoints: ["a", "b"]
                    .map(|endpoint| serde_json::from_value(project.plan[endpoint].clone()).ok()),
                resolution: session.route_resolution_m,
                surface: session.route_surface_available,
            };
            let coverage = if html {
                let captured = (|| -> ApiResult<_> {
                    let store = coverage_runs::store(app, key, session)?;
                    let history = store.history()?;
                    let Some(job) = history.iter().find(|job| job["state"] == "complete") else {
                        return Ok(Err(if history.is_empty() {
                            CoverageOmission::NoCompletedRun
                        } else {
                            CoverageOmission::Incomplete
                        }));
                    };
                    if !coverage_runs::identity(app, key, session)
                        .is_some_and(|identity| identity.matches(job))
                    {
                        return Ok(Err(CoverageOmission::Stale));
                    }
                    Ok(Ok(CoverageDownload {
                        store,
                        job: job.clone(),
                        context: TerrainContext::capture(app, key, session, true)?,
                    }))
                })();
                captured.unwrap_or(Err(CoverageOmission::Unavailable))
            } else {
                Err(CoverageOmission::NoCompletedRun)
            };
            Ok((download, coverage))
        })
        .await?;
    let coverage_job = overlay.as_ref().ok().map(|coverage| coverage.job.clone());
    let mut overlay = match overlay {
        Ok(coverage) => tokio::task::spawn_blocking(move || match route_overlay(coverage) {
            Ok(Some(overlay)) => Ok(overlay),
            Ok(None) => Err(CoverageOmission::Incomplete),
            Err(_) => Err(CoverageOmission::Unavailable),
        })
        .await
        .unwrap_or(Err(CoverageOmission::Unavailable)),
        Err(reason) => Err(reason),
    };
    if overlay.is_ok() {
        let (project, job, snapshot) = (
            download.project_id.clone(),
            download.route["job_id"].clone(),
            download.route["snapshot_version"].clone(),
        );
        let reason = workspace
            .run(app, move |app, key, session| {
                let unchanged = session.project_id == project
                    && route_http::current(app, key, session).is_ok_and(|route| {
                        route["job_id"] == job && route["snapshot_version"] == snapshot
                    });
                Ok(if !unchanged {
                    Some(CoverageOmission::RouteChanged)
                } else if coverage_job.as_ref().is_some_and(|job| {
                    !coverage_runs::identity(app, key, session)
                        .is_some_and(|identity| identity.matches(job))
                }) {
                    Some(CoverageOmission::Stale)
                } else {
                    None
                })
            })
            .await?;
        if let Some(reason) = reason {
            overlay = Err(reason);
        }
    }
    tokio::task::spawn_blocking(move || {
        let file = spool(|file| {
            let mut writer = BufWriter::new(file);
            match format.1 {
                "route.csv" => write_route_csv(&mut writer, &download.route)?,
                "route.geojson" => write_route_geojson(&mut writer, &download.route, None)?,
                _ => write_route_html(
                    &mut writer,
                    &download.route,
                    RouteReportOptions {
                        generated_at: &chrono::Utc::now().to_rfc3339(),
                        endpoint_coordinates: download.endpoints,
                        terrain_resolution_m: download.resolution,
                        surface_available: download.surface,
                        rf_settings: Some(&download.rf),
                        candidate_settings: Some(&download.candidates),
                        client_paths: &download.paths,
                        coverage: overlay.as_ref().ok(),
                        coverage_omission: overlay.as_ref().err().copied(),
                    },
                )?,
            }
            writer.flush()?;
            Ok(())
        })?;
        attachment(file, format.0, format.1)
    })
    .await
    .map_err(ApiError::internal)?
}
