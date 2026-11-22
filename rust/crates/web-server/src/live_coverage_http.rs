use std::{io::Cursor, time::Instant};

use axum::{
    Json,
    body::Body,
    extract::{State, rejection::JsonRejection},
    http::{StatusCode, header},
    response::Response,
};
use repeaternet_coverage::{
    ClientRadioProfile,
    live::{self, ElevationLookup, LiveMode, MercatorRaster, SweepGrid},
};
use repeaternet_rf_core::{Site, SiteKind};
use repeaternet_terrain::{LonLat, Transformer};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;
use tokio_util::io::ReaderStream;

use crate::{
    api::{ApiError, ApiResult, Workspace, active_project},
    catalog::AppState,
    route_inputs,
    terrain_http::{TerrainContext, terrain_invalid},
};

#[derive(Deserialize)]
pub(crate) struct Request {
    #[serde(default, deserialize_with = "optional_project")]
    project_id: Option<String>,
    site: LiveSite,
    #[serde(default)]
    client: ClientRadioProfile,
    #[serde(default)]
    mode: LiveMode,
    #[serde(default = "default_radius")]
    radius_m: f64,
    #[serde(default = "default_resolution")]
    resolution_m: f64,
    #[serde(default = "default_floor")]
    floor_db: f64,
}

fn optional_project<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    String::deserialize(deserializer).map(Some)
}

fn default_radius() -> f64 {
    20_000.0
}
fn default_resolution() -> f64 {
    30.0
}
fn default_floor() -> f64 {
    -20.0
}

#[derive(Deserialize)]
struct LiveSite {
    id: String,
    kind: LiveSiteKind,
    latitude: f64,
    longitude: f64,
    antenna_height_m: f64,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum LiveSiteKind {
    EndpointA,
    EndpointB,
    Router,
}

impl From<LiveSiteKind> for SiteKind {
    fn from(kind: LiveSiteKind) -> Self {
        match kind {
            LiveSiteKind::EndpointA => Self::EndpointA,
            LiveSiteKind::EndpointB => Self::EndpointB,
            LiveSiteKind::Router => Self::Router,
        }
    }
}

#[derive(Serialize)]
struct Header<'a> {
    site_id: &'a str,
    #[serde(flatten)]
    raster: &'a MercatorRaster,
    floor_db: f64,
    step_db: f64,
    radius_m: f64,
    resolution_m: f64,
    rays: usize,
    samples_per_ray: usize,
    site_ground_m: f64,
    antenna_elevation_m: f64,
    elapsed_ms: f64,
}

pub(crate) const NO_TERRAIN: &str = "No terrain yet. Download terrain to see coverage.";

fn bad_request(detail: impl std::fmt::Display) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, detail.to_string())
}

pub(crate) fn validate_radius(radius: f64) -> ApiResult<()> {
    if !(1_000.0..=80_000.0).contains(&radius) {
        return Err(bad_request(
            "Coverage radius must be between 1,000 and 80,000 m",
        ));
    }
    Ok(())
}

pub(crate) fn coordinates(latitude: f64, longitude: f64) -> ApiResult<LonLat> {
    if !(-90.0 < latitude && latitude < 90.0) || !(-180.0..=180.0).contains(&longitude) {
        return Err(bad_request("Invalid site coordinates"));
    }
    Ok(LonLat {
        latitude,
        longitude,
    })
}

pub async fn evaluate(
    State(app): State<AppState>,
    workspace: Workspace,
    body: Result<Json<Request>, JsonRejection>,
) -> ApiResult<Response> {
    let Json(request) = body.map_err(|error| bad_request(error.body_text()))?;
    let Request {
        project_id,
        site,
        client,
        mode,
        radius_m,
        resolution_m,
        floor_db,
    } = request;
    validate_radius(radius_m)?;
    if !(5.0..=500.0).contains(&resolution_m) {
        return Err(bad_request(
            "Coverage resolution must be between 5 and 500 m",
        ));
    }
    if !(-60.0..=0.0).contains(&floor_db) {
        return Err(bad_request(
            "Coverage margin floor must be between -60 and 0 dB",
        ));
    }
    if site.id.chars().count() > 64 {
        return Err(bad_request("Site ID must contain at most 64 characters"));
    }
    if !(site.antenna_height_m > 0.0 && site.antenna_height_m <= 1000.0) {
        return Err(bad_request(
            "The site antenna height must be above 0 m and at most 1,000 m",
        ));
    }
    let location = coordinates(site.latitude, site.longitude)?;
    client.validate().map_err(bad_request)?;
    let grid = SweepGrid::new(radius_m, resolution_m).map_err(bad_request)?;

    let (context, rf) = workspace
        .clone()
        .run(app.clone(), move |app, key, session| {
            if project_id.is_some_and(|id| id != session.project_id) {
                return Err(ApiError::conflict("The active project changed"));
            }
            let context = TerrainContext::capture(app, key, session, true)?;
            if context.route.paths("dtm").is_empty()
                && context
                    .supplemental
                    .as_ref()
                    .is_none_or(|files| files.paths("dtm").is_empty())
            {
                return Err(ApiError::conflict(NO_TERRAIN));
            }
            let plan = active_project(&app.store, key, session)?.plan;
            let rf = route_inputs::rf(&route_inputs::merged(&app.defaults, &plan, "rf")?)?;
            Ok((context, rf))
        })
        .await?;
    let permit = app
        .live_coverage_workers
        .clone()
        .acquire_owned()
        .await
        .map_err(ApiError::internal)?;
    let cache = app.live_terrain.clone();
    let used_rf = rf.clone();
    let (context, response) = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let start = Instant::now();
        let raster = cache.open(&context)?;
        let transform = Transformer::new(raster.raster.crs()).map_err(terrain_invalid)?;
        let position = transform.forward(location).map_err(bad_request)?;
        let site_tiles = raster.tiles(position, 0.0, resolution_m)?;
        let (ground, surface) = site_tiles.elevations(position);
        if !ground.is_finite() {
            return Err(ApiError::conflict("No ground terrain at this site"));
        }
        let site = Site {
            position,
            ground_elevation_m: ground,
            surface_elevation_m: site_tiles.has_surface().then_some(surface),
            antenna_height_m: site.antenna_height_m,
            kind: site.kind.into(),
            ..Site::new(site.id, position.x, position.y)
        };
        let tiles = raster.tiles(position, grid.sample_radius_m(), resolution_m)?;
        let polar = live::sweep(&tiles, &site, &rf, &client.as_radio_budget(), mode, grid)
            .map_err(terrain_invalid)?;
        let raster =
            live::resample(&polar, location, &transform, floor_db).map_err(terrain_invalid)?;
        let header = Header {
            site_id: &site.id,
            raster: &raster,
            floor_db,
            step_db: live::MARGIN_STEP_DB,
            radius_m,
            resolution_m,
            rays: polar.ray_count(),
            samples_per_ray: polar.samples_per_ray(),
            site_ground_m: ground,
            antenna_elevation_m: site.antenna_absolute_elevation_m(),
            elapsed_ms: start.elapsed().as_secs_f64() * 1_000.0,
        };
        let data = serde_json::to_vec(&header).map_err(ApiError::internal)?;
        let mut prefix = Vec::with_capacity(8 + data.len());
        prefix.extend_from_slice(b"RNLC");
        prefix.extend_from_slice(&(data.len() as u32).to_le_bytes());
        prefix.extend(data);
        let len = prefix.len() + raster.pixels.len();
        let reader = Cursor::new(prefix).chain(Cursor::new(raster.pixels));
        let response = Response::builder()
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .header(header::CACHE_CONTROL, "no-store")
            .header(header::CONTENT_LENGTH, len)
            .body(Body::from_stream(ReaderStream::with_capacity(
                reader,
                64 * 1024,
            )))
            .map_err(ApiError::internal)?;
        Ok::<_, ApiError>((context, response))
    })
    .await
    .map_err(ApiError::internal)??;
    // The raster depends only on the project, its terrain files and the radio
    // settings; other plan edits made meanwhile (such as moving a different
    // site) leave it valid.
    workspace
        .run(app, move |app, key, session| {
            let current = TerrainContext::capture(app, key, session, true)?;
            let plan = active_project(&app.store, key, session)?.plan;
            let rf = route_inputs::rf(&route_inputs::merged(&app.defaults, &plan, "rf")?)?;
            if context.project_id != session.project_id
                || context.directory != current.directory
                || context.fingerprint != current.fingerprint
                || rf != used_rf
            {
                return Err(ApiError::conflict(
                    "Radio settings or terrain changed during the request",
                ));
            }
            Ok(response)
        })
        .await
}
