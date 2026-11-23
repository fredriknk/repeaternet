use std::{
    fs, io,
    path::{Path as FsPath, PathBuf},
};

use axum::{
    Json,
    extract::{
        Multipart, Path, Query, State,
        multipart::{MultipartError, MultipartRejection},
    },
    http::StatusCode,
};
use repeaternet_terrain::{
    Bounds, GeoTiff, Point, RasterTerrain, TerrainLayer, TerrainSource, Transformer,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tempfile::TempPath;
use tiff_reader::{Ifd, OpenOptions, TiffFile, constants::TAG_NEW_SUBFILE_TYPE};
use tokio::io::AsyncWriteExt;

use crate::{
    api::{ApiError, ApiResult, ObjectBody, Workspace, active_project, ensure_idle},
    catalog::{self, AppState, Session, TerrainFiles},
    route_inputs::{self, Location},
};

pub const UPLOAD_LIMIT: usize = 512 * 1024 * 1024;
const COVERAGE_GENERATIONS: &str = "coverage-terrain-generations";

pub struct TerrainContext {
    pub project_id: String,
    pub revision: u64,
    pub directory: PathBuf,
    pub cache_directory: PathBuf,
    pub route: TerrainFiles,
    pub supplemental: Option<TerrainFiles>,
    pub fingerprint: Value,
    pub plan_fingerprint: String,
}

impl TerrainContext {
    pub fn capture(
        app: &AppState,
        key: &str,
        session: &Session,
        supplemental: bool,
    ) -> ApiResult<Self> {
        let project = active_project(&app.store, key, session)?;
        let directory = app.store.project_path(key, &project)?;
        let cache_directory = repeaternet_providers::shared_cache_directory(&app.data_directory);
        let route = TerrainFiles::read(&directory, false);
        let supplemental = supplemental.then(|| TerrainFiles::read(&directory, true));
        let fingerprint = route
            .fingerprint(&directory, supplemental.as_ref())
            .ok_or_else(|| ApiError::conflict("Terrain files changed during the request"))?;
        Ok(Self {
            project_id: project.id,
            revision: session.input_revision,
            directory,
            cache_directory,
            route,
            supplemental,
            fingerprint,
            plan_fingerprint: catalog::plan_fingerprint(&project.plan, true),
        })
    }

    pub fn verify(&self, app: &AppState, key: &str, session: &Session) -> ApiResult<()> {
        if self.project_id != session.project_id || self.revision != session.input_revision {
            return Err(ApiError::conflict(
                "The active project or route inputs changed during the request",
            ));
        }
        self.verify_files_and_plan(app, key, session, &self.plan_fingerprint)
    }

    pub fn verify_files_and_plan(
        &self,
        app: &AppState,
        key: &str,
        session: &Session,
        plan_fingerprint: &str,
    ) -> ApiResult<()> {
        let current = Self::capture(app, key, session, self.supplemental.is_some())?;
        if self.directory != current.directory
            || self.fingerprint != current.fingerprint
            || plan_fingerprint != current.plan_fingerprint
        {
            return Err(ApiError::conflict(
                "Route inputs or terrain changed during the request",
            ));
        }
        Ok(())
    }

    pub fn open(&self) -> ApiResult<RasterTerrain> {
        let paths = |kind| {
            self.route
                .paths(kind)
                .iter()
                .chain(self.supplemental.iter().flat_map(|extra| extra.paths(kind)))
                .map(|path| self.directory.join(path))
                .collect::<Vec<_>>()
        };
        let dtm = paths("dtm");
        if dtm.is_empty() {
            return Err(ApiError::invalid("Prepare or upload ground terrain first"));
        }
        RasterTerrain::open(&dtm, &paths("dom")).map_err(terrain_invalid)
    }
}

pub fn terrain_invalid(error: impl std::fmt::Display) -> ApiError {
    ApiError::invalid(error.to_string())
}

fn kind(kind: &str) -> ApiResult<&'static str> {
    match kind {
        "dtm" => Ok("dtm"),
        "dom" => Ok("dom"),
        _ => Err(ApiError::missing("Unknown terrain product")),
    }
}

fn multipart_error(error: MultipartError) -> ApiError {
    if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
        return ApiError::new(StatusCode::BAD_REQUEST, "Tile exceeds 512 MiB");
    }
    ApiError::new(StatusCode::BAD_REQUEST, error.body_text())
}

pub async fn upload(
    State(app): State<AppState>,
    workspace: Workspace,
    Path(product): Path<String>,
    multipart: Result<Multipart, MultipartRejection>,
) -> ApiResult<Json<Value>> {
    upload_bounded(app, workspace, &product, multipart, UPLOAD_LIMIT).await
}

pub(crate) async fn upload_bounded(
    app: AppState,
    workspace: Workspace,
    product: &str,
    multipart: Result<Multipart, MultipartRejection>,
    limit: usize,
) -> ApiResult<Json<Value>> {
    let kind = kind(product)?;
    let mut form =
        multipart.map_err(|error| ApiError::new(StatusCode::BAD_REQUEST, error.body_text()))?;
    let (context, staged, admission) = workspace
        .clone()
        .run(app.clone(), |app, key, session| {
            ensure_idle(session)?;
            let busy = |_| {
                ApiError::new(
                    StatusCode::TOO_MANY_REQUESTS,
                    "Terrain uploads are busy; retry shortly",
                )
            };
            let local = session.upload.clone().try_acquire_owned().map_err(busy)?;
            let global = app.uploads.clone().try_acquire_owned().map_err(busy)?;
            let context = TerrainContext::capture(app, key, session, false)?;
            let staged = tempfile::Builder::new()
                .prefix(".terrain-upload-")
                .tempfile_in(&context.directory)
                .map_err(ApiError::internal)?;
            Ok((context, staged, (local, global)))
        })
        .await?;
    let (file, staged) = staged.into_parts();
    let mut file = tokio::fs::File::from_std(file);
    let mut found = false;
    let mut size = 0;
    while let Some(mut field) = form.next_field().await.map_err(multipart_error)? {
        if field.name() != Some("file") || field.file_name().is_none() {
            continue;
        }
        if found {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "Upload one terrain tile at a time",
            ));
        }
        found = true;
        while let Some(chunk) = field.chunk().await.map_err(multipart_error)? {
            size += chunk.len();
            if size > limit {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "Tile exceeds 512 MiB",
                ));
            }
            file.write_all(&chunk).await.map_err(ApiError::internal)?;
        }
    }
    if !found {
        return Err(ApiError::invalid("Field required: file"));
    }
    file.sync_all().await.map_err(ApiError::internal)?;
    drop(file);

    let (staged, bounds, admission) = tokio::task::spawn_blocking(move || {
        let (staged, bounds) = validate_upload(staged)?;
        Ok::<_, ApiError>((staged, bounds, admission))
    })
    .await
    .map_err(ApiError::internal)??;
    workspace
        .run(app, move |app, key, session| {
            let _admission = admission;
            ensure_idle(session)?;
            context.verify(app, key, session)?;
            let destination = context.directory.join(kind);
            create_plain_directory(&destination)?;
            let name = format!("{}.tif", catalog::random_id::<8>()?);
            let path = destination.join(&name);
            staged
                .persist_noclobber(&path)
                .map_err(ApiError::internal)?;
            if let Err(error) = change_terrain(app, key, session, &context, &[], || Ok(())) {
                if let Err(rollback) = fs::remove_file(&path) {
                    eprintln!("Could not roll back uploaded terrain: {rollback}");
                }
                return Err(error);
            }
            Ok(Json(json!({"name": name, "bounds": bounds})))
        })
        .await
}

pub(crate) fn create_plain_directory(path: &FsPath) -> ApiResult<()> {
    match fs::create_dir(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let metadata = fs::symlink_metadata(path).map_err(ApiError::internal)?;
            if metadata.is_dir() && !catalog::linked(&metadata) {
                Ok(())
            } else {
                Err(ApiError::conflict(
                    "Terrain product directory is not a regular directory",
                ))
            }
        }
        Err(error) => Err(ApiError::internal(error)),
    }
}

fn validate_upload(path: TempPath) -> ApiResult<(TempPath, [[f64; 2]; 2])> {
    let bad_tile = |error: &dyn std::fmt::Display| {
        eprintln!("Rejected terrain tile: {error}");
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "Use a readable projected GeoTIFF with metre coordinates in a supported CRS",
        )
    };
    let tile = GeoTiff::open(&path).map_err(|error| bad_tile(&error))?;
    validate_blocks(&path, tile.metadata().has_internal_mask).map_err(|error| bad_tile(&error))?;
    let coords = outline(*tile.metadata()).map_err(|error| bad_tile(&error))?;
    let bounds = [
        coords
            .iter()
            .fold([f64::INFINITY; 2], |a, b| [a[0].min(b[0]), a[1].min(b[1])]),
        coords.iter().fold([f64::NEG_INFINITY; 2], |a, b| {
            [a[0].max(b[0]), a[1].max(b[1])]
        }),
    ];
    drop(tile);
    Ok((path, bounds))
}

// GeoTiff validates storage layout and the first block. Decode one pixel in
// every native strip/tile thereafter, including the selected internal mask.
fn validate_blocks(path: &FsPath, masked: bool) -> tiff_reader::error::Result<()> {
    let tiff = TiffFile::open_with_options(
        path,
        OpenOptions {
            decode_output_bytes: 32 * 1024 * 1024,
            ..OpenOptions::default()
        },
    )?;
    let image = tiff.ifd(0)?;
    let decode = |ifd: &Ifd| -> tiff_reader::error::Result<()> {
        let (width, height) = if ifd.is_tiled() {
            (ifd.tile_width().unwrap(), ifd.tile_height().unwrap())
        } else {
            (ifd.width(), ifd.rows_per_strip())
        };
        for row in (0..ifd.height() as usize).step_by(height as usize) {
            for col in (0..ifd.width() as usize).step_by(width as usize) {
                tiff.read_window_bytes_from_ifd(ifd, row, col, 1, 1)?;
            }
        }
        Ok(())
    };
    decode(image)?;
    if masked {
        let is_mask = |ifd: &Ifd| {
            ifd.tag(TAG_NEW_SUBFILE_TYPE)
                .and_then(|tag| tag.value.as_u32())
                .is_some_and(|kind| kind & 5 == 4)
        };
        for ifd in tiff.ifds().iter().skip(1).filter(|ifd| is_mask(ifd)) {
            decode(ifd)?;
        }
        for offset in image.sub_ifd_offsets().unwrap_or_default() {
            let ifd = tiff.read_ifd_at_offset(offset)?;
            if is_mask(&ifd) {
                decode(&ifd)?;
            }
        }
    }
    Ok(())
}

pub async fn clear(
    State(app): State<AppState>,
    workspace: Workspace,
    Path(product): Path<String>,
) -> ApiResult<Json<Value>> {
    let kind = kind(&product)?;
    workspace
        .run(app, move |app, key, session| {
            ensure_idle(session)?;
            let context = TerrainContext::capture(app, key, session, false)?;
            change_terrain(
                app,
                key,
                session,
                &context,
                context.route.paths(kind),
                || Ok(()),
            )?;
            Ok(Json(json!({"ok": true})))
        })
        .await
}

pub(crate) fn change_terrain(
    app: &AppState,
    key: &str,
    session: &mut Session,
    context: &TerrainContext,
    paths: &[PathBuf],
    commit: impl FnOnce() -> ApiResult<()>,
) -> ApiResult<()> {
    let staging = tempfile::Builder::new()
        .prefix(".terrain-removal-")
        .tempdir_in(&context.directory)
        .map_err(ApiError::internal)?;
    let supplemental = context.directory.join(COVERAGE_GENERATIONS);
    let mut removed = Vec::new();
    let change = (|| {
        let mut move_aside = |path: PathBuf| -> ApiResult<()> {
            let target = staging.path().join(removed.len().to_string());
            fs::rename(&path, &target).map_err(ApiError::internal)?;
            removed.push((path, target));
            Ok(())
        };
        for path in paths {
            move_aside(context.directory.join(path))?;
        }
        match fs::symlink_metadata(&supplemental) {
            Ok(metadata) if metadata.is_dir() && !catalog::linked(&metadata) => {
                move_aside(supplemental)?
            }
            Ok(_) => {
                return Err(ApiError::conflict(
                    "Coverage terrain directory is not a regular directory",
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(ApiError::internal(error)),
        }
        app.store
            .save_result_summary(key, &session.project_id, None)?;
        commit()
    })();
    if let Err(error) = change {
        let mut rollback_failed = false;
        for (path, target) in removed.into_iter().rev() {
            if let Err(rollback) = fs::rename(&target, &path) {
                eprintln!("Could not restore removed terrain: {rollback}");
                rollback_failed = true;
            }
        }
        if rollback_failed {
            let _ = staging.keep();
        }
        return Err(error);
    }
    session.invalidate_route();
    session.coverage_terrain_job = None;
    Ok(())
}

#[derive(Deserialize)]
pub struct CoverageQuery {
    #[serde(default)]
    route_only: bool,
}

pub async fn coverage(
    State(app): State<AppState>,
    workspace: Workspace,
    Query(query): Query<CoverageQuery>,
    ObjectBody(body): ObjectBody,
) -> ApiResult<Json<Value>> {
    let plan = Value::Object(body);
    let locations = route_inputs::readiness_locations(&plan, &app.defaults)?;
    let mesh = route_inputs::mesh_mode(&plan)?;
    let width = route_inputs::terrain_candidates(&plan, &app.defaults)?.corridor_width_m;
    let paint = crate::mesh_paint::MeshPaint::active(&plan)?;
    let captured = workspace
        .clone()
        .run(app.clone(), move |app, key, session| {
            TerrainContext::capture(app, key, session, !query.route_only)
        })
        .await?;
    let (captured, report) = tokio::task::spawn_blocking(move || {
        let terrain = captured.open()?;
        let (transform, points, elevations) = ground(&terrain, &locations)?;
        let surface = terrain.has_surface();
        let line = if mesh {
            Vec::new()
        } else {
            let [start, end] = [points[0], points[1]];
            let resolution = (4.0 * terrain.resolution_m()).max(100.0);
            let count =
                ((start.distance_to(end) / resolution).ceil() + 1.0).clamp(2.0, 2001.0) as usize;
            (0..count)
                .map(|i| {
                    let f = i as f64 / (count - 1) as f64;
                    Point {
                        x: start.x + (end.x - start.x) * f,
                        y: start.y + (end.y - start.y) * f,
                    }
                })
                .collect()
        };
        let line_ground = terrain
            .sample(&line, TerrainLayer::GroundDtm)
            .map_err(terrain_invalid)?;
        let mut gaps = Vec::new();
        let mut gap = Vec::new();
        for (point, elevation) in line.into_iter().zip(line_ground) {
            if !elevation.is_finite() {
                let coordinate = transform.reverse(point).map_err(terrain_invalid)?;
                gap.push([coordinate.latitude, coordinate.longitude]);
            } else if !gap.is_empty() {
                gaps.push(std::mem::take(&mut gap));
            }
        }
        if !gap.is_empty() {
            gaps.push(gap);
        }
        let painted = paint
            .as_ref()
            .map(|paint| paint.projected(&transform))
            .transpose()?
            .unwrap_or_default();
        let paint_gaps = terrain
            .sample(&painted, TerrainLayer::GroundDtm)
            .map_err(terrain_invalid)?
            .iter()
            .filter(|height| !height.is_finite())
            .count();
        let (area_samples, area_gaps) = if mesh {
            let mut anchors: Vec<_> = locations
                .iter()
                .zip(&points)
                .filter(|(site, _)| {
                    site.kind == repeaternet_rf_core::SiteKind::Client
                        || site.policy == repeaternet_optimizer::RouterPolicy::Required
                })
                .map(|(_, &point)| point)
                .collect();
            anchors.extend(painted.iter().copied());
            mesh_area_gaps(&terrain, &anchors, width)?
        } else {
            (0, 0)
        };
        let uncovered = uncovered(&locations, &points, &elevations, terrain.bounds());
        let outlines = |tiles: &[GeoTiff]| {
            tiles
                .iter()
                .map(|tile| outline(*tile.metadata()).map_err(terrain_invalid))
                .collect::<ApiResult<Vec<_>>>()
        };
        let report = json!({
            "covered": uncovered.is_empty() && (painted.is_empty() || paint_gaps < painted.len()),
            "uncovered_sites": uncovered,
            "paint_cells": painted.len(),
            "paint_gaps": paint_gaps,
            "line_gaps": gaps,
            "area_samples": area_samples,
            "area_gaps": area_gaps,
            "dtm_outlines": outlines(terrain.ground_tiles())?,
            "dom_outlines": outlines(terrain.surface_tiles())?,
            "surface_available": surface,
        });
        Ok::<_, ApiError>((captured, report))
    })
    .await
    .map_err(ApiError::internal)??;
    workspace
        .run(app, move |app, key, session| {
            captured.verify(app, key, session)?;
            Ok(Json(report))
        })
        .await
}

fn mesh_area_gaps(
    terrain: &RasterTerrain,
    anchors: &[Point],
    width: f64,
) -> ApiResult<(usize, usize)> {
    let hull = repeaternet_terrain::corridor::MeshArea::new(anchors);
    let bounds = anchors.iter().fold(
        [
            f64::INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NEG_INFINITY,
        ],
        |[left, bottom, right, top], point| {
            [
                left.min(point.x),
                bottom.min(point.y),
                right.max(point.x),
                top.max(point.y),
            ]
        },
    );
    let [left, bottom, right, top] = [
        bounds[0] - width,
        bounds[1] - width,
        bounds[2] + width,
        bounds[3] + width,
    ];
    let mut samples = Vec::new();
    for row in 0..=32 {
        for column in 0..=32 {
            let point = Point {
                x: left + (right - left) * f64::from(column) / 32.0,
                y: bottom + (top - bottom) * f64::from(row) / 32.0,
            };
            if hull.distance_to(point) <= width {
                samples.push(point);
            }
        }
    }
    let elevations = terrain
        .sample(&samples, TerrainLayer::GroundDtm)
        .map_err(terrain_invalid)?;
    Ok((
        samples.len(),
        elevations.iter().filter(|value| !value.is_finite()).count(),
    ))
}

pub fn ground(
    terrain: &RasterTerrain,
    locations: &[Location],
) -> ApiResult<(Transformer, Vec<Point>, Vec<f64>)> {
    let transformer = Transformer::new(terrain.crs()).map_err(terrain_invalid)?;
    let points = locations
        .iter()
        .map(|site| {
            transformer
                .forward(site.coordinate)
                .map_err(terrain_invalid)
        })
        .collect::<ApiResult<Vec<_>>>()?;
    let elevations = terrain
        .sample(&points, TerrainLayer::GroundDtm)
        .map_err(terrain_invalid)?;
    Ok((transformer, points, elevations))
}

pub fn uncovered(
    locations: &[Location],
    points: &[Point],
    elevations: &[f64],
    bounds: Bounds,
) -> Vec<Value> {
    locations
        .iter()
        .zip(points)
        .zip(elevations)
        .filter(|(_, elevation)| !elevation.is_finite())
        .map(|((site, point), _)| {
            json!({
                "name": site.name,
                "latitude": site.coordinate.latitude,
                "longitude": site.coordinate.longitude,
                "reason": if bounds.contains(*point) { "no_data" } else { "outside_coverage" },
            })
        })
        .collect()
}

fn outline(
    metadata: repeaternet_terrain::RasterMetadata,
) -> Result<[[f64; 2]; 4], repeaternet_terrain::TerrainError> {
    let transform = Transformer::new(metadata.crs)?;
    let mut outline = [[0.0; 2]; 4];
    for (corner, projected) in outline.iter_mut().zip(metadata.footprint()) {
        let coordinate = transform.reverse(projected)?;
        *corner = [coordinate.latitude, coordinate.longitude];
    }
    Ok(outline)
}
