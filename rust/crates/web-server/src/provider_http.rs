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
    kartverket::StagedGeneration,
    sources::{
        DownloadArea, DownloadCancellation, DownloadPlan, PlanOptions, Product, ProviderRegistry,
        SourceChoice, TerrainDownloader, geographic_footprint,
    },
};
use repeaternet_terrain::{
    Bounds, GeoTiff, LonLat, Point, TerrainCrs, Transformer, corridor::corridor_geometry,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    api::{ApiError, ApiResult, ObjectBody, Workspace, active_project, ensure_idle, save_plan},
    catalog::{self, AppState, Session, random_id},
    live_coverage_http,
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
    source: String,
    fallback: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            requested_resolution_m: 10.0,
            auto_resolution: true,
            include_dom: true,
            source: "auto".into(),
            fallback: true,
        }
    }
}

struct Download {
    context: TerrainContext,
    provider: TerrainDownloader,
    choice: SourceChoice,
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
        cancelled: DownloadCancellation,
    ) -> ApiResult<DownloadStage> {
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
                "Preparing ground terrain"
            });
        }
        if !self.supplemental() {
            for kind in ["dtm", "dom"] {
                for path in self.context.route.paths(kind) {
                    if cancelled() {
                        return Err(ApiError::conflict("Terrain preparation cancelled"));
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
        let mut warnings = std::collections::BTreeSet::new();
        let mut providers_used = std::collections::BTreeSet::new();
        let mut source_info = Vec::new();
        let mut fetch = |kind: Product, plan: &Option<DownloadPlan>| -> ApiResult<Vec<PathBuf>> {
            let Some(plan) = plan else {
                return Ok(Vec::new());
            };
            let paths = self
                .provider
                .fetch_plan(
                    kind,
                    self.crs,
                    plan,
                    &self.choice,
                    cancelled.clone(),
                    |tile| {
                        let lock = app.session(key);
                        let mut guard = lock.blocking_lock();
                        if let Some(session) = guard.as_mut().filter(|s| self.matches(s, id)) {
                            let status = self.status(session);
                            let product = kind.name().to_uppercase();
                            let provider = tile
                                .provider
                                .map_or_else(String::new, |name| format!(" · {name}"));
                            let suffix = if tile.cached { " (cache hit)" } else { "" };
                            status["stage"] = json!(format!(
                                "{product} tile {}/{}{provider}{suffix}",
                                tile.index, tile.count
                            ));
                            status["done"] = json!(done + tile.index.saturating_sub(1));
                        }
                    },
                )
                .map_err(terrain_invalid)?;
            done += paths.paths.len();
            warnings.extend(paths.warnings);
            source_info.extend(self.provider.source_info(kind, &paths.providers_used));
            providers_used.extend(paths.providers_used);
            Ok(paths.paths)
        };
        let dtm = fetch(Product::Dtm, &self.dtm)?;
        let dom = fetch(Product::Dom, &self.dom)?;
        let generation = StagedGeneration::copy_from_cache(&root, &dtm, &dom, cancelled.as_ref())
            .map_err(terrain_invalid)?;
        let metadata = serde_json::to_vec(&source_info).map_err(ApiError::internal)?;
        fs::write(generation.path().join("sources.json"), metadata).map_err(ApiError::internal)?;
        Ok(DownloadStage {
            generation,
            warnings,
            providers_used,
        })
    }
}

struct DownloadStage {
    generation: StagedGeneration,
    warnings: std::collections::BTreeSet<String>,
    providers_used: std::collections::BTreeSet<String>,
}

impl Options {
    fn read(value: Option<&Value>) -> ApiResult<Self> {
        serde_json::from_value(value.cloned().unwrap_or_else(|| json!({}))).map_err(terrain_invalid)
    }

    fn choice(&self) -> SourceChoice {
        SourceChoice {
            source: self.source.clone(),
            fallback: self.fallback,
        }
    }
}

fn require_choice(registry: &ProviderRegistry, choice: &SourceChoice) -> ApiResult<()> {
    registry
        .check_choice(choice)
        .map_err(|error| ApiError::new(StatusCode::BAD_REQUEST, error.to_string()))
}

fn estimate_slot(session: &mut Session, supplemental: bool) -> &mut Option<String> {
    if supplemental {
        &mut session.coverage_terrain_estimate_registry
    } else {
        &mut session.route_terrain_estimate_registry
    }
}

fn check_estimate(expected: Option<&str>, snapshot: &ProviderRegistry) -> ApiResult<()> {
    if expected.is_some_and(|expected| expected != snapshot.fingerprint()) {
        return Err(ApiError::conflict("Data sources changed; estimate again"));
    }
    Ok(())
}

fn route_area(
    transform: &Transformer,
    locations: &[LonLat],
    width: f64,
) -> ApiResult<(Vec<Point>, MultiPolygon<f64>)> {
    let points = locations
        .iter()
        .map(|&site| transform.forward(site))
        .collect::<Result<Vec<_>, _>>()
        .map_err(terrain_invalid)?;
    let corridor = corridor_geometry(&points[..2], width, &points[2..]).map_err(terrain_invalid)?;
    Ok((points, corridor))
}

fn route_crs(
    registry: &ProviderRegistry,
    context: &TerrainContext,
    locations: &[LonLat],
    width: f64,
    choice: &SourceChoice,
) -> ApiResult<TerrainCrs> {
    if let Some(path) = ["dtm", "dom"]
        .into_iter()
        .find_map(|kind| context.route.paths(kind).first())
    {
        return Ok(GeoTiff::open(context.directory.join(path))
            .map_err(terrain_invalid)?
            .metadata()
            .crs);
    }
    let origin = locations[0].longitude;
    let center = LonLat {
        longitude: (origin
            + locations
                .iter()
                .map(|p| (p.longitude - origin + 180.0).rem_euclid(360.0) - 180.0)
                .sum::<f64>()
                / locations.len() as f64
            + 180.0)
            .rem_euclid(360.0)
            - 180.0,
        latitude: locations.iter().map(|p| p.latitude).sum::<f64>() / locations.len() as f64,
    };
    let provisional = TerrainCrs::for_wgs84_location(center).map_err(terrain_invalid)?;
    let transform = Transformer::new(provisional).map_err(terrain_invalid)?;
    let (_, geometry) = route_area(&transform, locations, width)?;
    let footprint = geographic_footprint(&transform, &geometry).map_err(terrain_invalid)?;
    registry
        .target_crs(&footprint, choice)
        .map_err(terrain_invalid)
}

pub async fn preview_sources(
    app: AppState,
    workspace: Workspace,
    body: Value,
) -> ApiResult<Json<Value>> {
    let options = Options::read(body.get("terrain"))?;
    let choice = options.choice();
    let candidates = route_inputs::terrain_candidates(&body, &app.defaults)?;
    let locations = download_locations(&body, candidates.infrastructure_policy)?;
    let width = candidates.corridor_width_m;
    workspace
        .run(app, move |app, key, session| {
            let registry = app.provider_snapshot()?;
            require_choice(&registry, &choice)?;
            let context = TerrainContext::capture(app, key, session, false)?;
            let crs = route_crs(&registry, &context, &locations, width, &choice)?;
            let transform = Transformer::new(crs).map_err(terrain_invalid)?;
            let (_, geometry) = route_area(&transform, &locations, width)?;
            let footprint = geographic_footprint(&transform, &geometry).map_err(terrain_invalid)?;
            let mut result =
                json!(registry.describe_area(&footprint, options.include_dom, &choice));
            result["crs"] = json!(crs.to_string());
            Ok(Json(result))
        })
        .await
}

async fn route_plan(
    app: AppState,
    workspace: Workspace,
    mut body: Value,
    preparing: bool,
) -> ApiResult<Download> {
    let options = Options::read(body.get("terrain"))?;
    if !(1.0..=1000.0).contains(&options.requested_resolution_m) {
        return Err(ApiError::invalid(
            "Terrain resolution must be between 1 and 1,000 m",
        ));
    }
    let choice = options.choice();
    let candidates = route_inputs::terrain_candidates(&body, &app.defaults)?;
    let locations = download_locations(&body, candidates.infrastructure_policy)?;
    let width = candidates.corridor_width_m;
    let limits = app.terrain_limits;
    let requested_fingerprint = body
        .as_object_mut()
        .and_then(|body| body.remove("registry_fingerprint"));
    let plan_choice = choice.clone();
    let (context, registry) = workspace
        .run(app, move |app, key, session| {
            let registry = app.provider_snapshot()?;
            if preparing {
                let fingerprint = requested_fingerprint
                    .as_ref()
                    .and_then(Value::as_str)
                    .or(estimate_slot(session, false).as_deref());
                check_estimate(fingerprint, &registry)?;
            }
            require_choice(&registry, &plan_choice)?;
            Ok((TerrainContext::capture(app, key, session, false)?, registry))
        })
        .await?;
    tokio::task::spawn_blocking(move || {
        let crs = route_crs(&registry, &context, &locations, width, &choice)?;
        let transform = Transformer::new(crs).map_err(terrain_invalid)?;
        let (points, geometry) = route_area(&transform, &locations, width)?;
        if choice.source != "auto" && !choice.fallback {
            let footprint = geographic_footprint(&transform, &geometry).map_err(terrain_invalid)?;
            if registry.rank(Product::Dtm, &footprint, &choice).is_empty() {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    format!(
                        "Terrain source {} has no ground terrain in this area",
                        choice.source
                    ),
                ));
            }
        }
        let bounds = Bounds {
            min_x: points.iter().map(|p| p.x).fold(f64::INFINITY, f64::min) - width,
            min_y: points.iter().map(|p| p.y).fold(f64::INFINITY, f64::min) - width,
            max_x: points.iter().map(|p| p.x).fold(f64::NEG_INFINITY, f64::max) + width,
            max_y: points.iter().map(|p| p.y).fold(f64::NEG_INFINITY, f64::max) + width,
        };
        let provider = TerrainDownloader::new(registry, &context.cache_directory, limits)
            .map_err(terrain_invalid)?;
        let plan = provider
            .plan_download(
                crs,
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
                options.include_dom,
                choice.clone(),
            )
            .map_err(terrain_invalid)?;
        let estimate = provider
            .estimate(crs, &plan, &context.directory)
            .map_err(terrain_invalid)?;
        Ok(Download {
            context,
            provider,
            choice,
            crs,
            dom: plan.dom,
            dtm: plan.dtm,
            estimate: json!(estimate),
            plan: Some(body),
            route_identity: None,
        })
    })
    .await
    .map_err(ApiError::internal)?
}

#[derive(Deserialize)]
struct LiveTerrainArea {
    sites: Vec<LiveTerrainSite>,
    radius_m: f64,
}

#[derive(Deserialize)]
struct LiveTerrainSite {
    latitude: f64,
    longitude: f64,
}

enum ExtraArea {
    Live(Vec<LonLat>, f64),
    Route {
        sites: Vec<repeaternet_rf_core::Site>,
        corridor: f64,
        request: Box<crate::coverage_http::CoverageRequest>,
    },
}

fn live_area(body: &Value) -> ApiResult<Option<ExtraArea>> {
    let Some(value) = body.get("live") else {
        return Ok(None);
    };
    let LiveTerrainArea { sites, radius_m } =
        serde_json::from_value(value.clone()).map_err(|error| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                format!("Invalid live area ({error})"),
            )
        })?;
    live_coverage_http::validate_radius(radius_m)?;
    if !(1..=256).contains(&sites.len()) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "Live coverage requires 1 to 256 sites",
        ));
    }
    let sites = sites
        .into_iter()
        .map(|site| live_coverage_http::coordinates(site.latitude, site.longitude))
        .collect::<ApiResult<Vec<_>>>()?;
    Ok(Some(ExtraArea::Live(sites, radius_m)))
}

fn circle_union(
    transform: &Transformer,
    sites: Vec<LonLat>,
    radius: f64,
) -> ApiResult<MultiPolygon<f64>> {
    use std::f64::consts::{PI, TAU};
    const VERTICES: usize = 256;
    // Circumscribe each circle so the download includes its edge.
    let outer = radius / (PI / VERTICES as f64).cos();
    let offsets: Vec<_> = (0..VERTICES)
        .map(|i| (i as f64 * TAU / VERTICES as f64).sin_cos())
        .collect();
    let circles: Vec<_> = sites
        .into_iter()
        .map(|site| {
            let center = transform.forward(site).map_err(terrain_invalid)?;
            let ring: Vec<_> = offsets
                .iter()
                .map(|&(sin, cos)| (center.x + outer * cos, center.y + outer * sin))
                .collect();
            Ok(Polygon::new(LineString::from(ring), vec![]))
        })
        .collect::<ApiResult<_>>()?;
    Ok(geo::unary_union(&circles))
}

async fn extra_plan(
    app: AppState,
    workspace: Workspace,
    body: Value,
    preparing: bool,
) -> ApiResult<Download> {
    let limits = app.terrain_limits;
    let live = live_area(&body)?;
    let for_live = live.is_some();
    let terrain = body.get("terrain").cloned();
    let requested_fingerprint = body.get("registry_fingerprint").cloned();
    let (registry, choice, live_context) = workspace
        .clone()
        .run(app.clone(), move |app, key, session| {
            let registry = app.provider_snapshot()?;
            if preparing {
                let fingerprint = requested_fingerprint
                    .as_ref()
                    .and_then(Value::as_str)
                    .or(estimate_slot(session, true).as_deref());
                check_estimate(fingerprint, &registry)?;
            }
            let saved = active_project(&app.store, key, session)?.plan;
            let choice = Options::read(terrain.as_ref().or_else(|| saved.get("terrain")))?.choice();
            require_choice(&registry, &choice)?;
            let context = if for_live {
                let context = TerrainContext::capture(app, key, session, true)?;
                if context.route.paths("dtm").is_empty() {
                    return Err(ApiError::conflict(live_coverage_http::NO_TERRAIN));
                }
                Some(context)
            } else {
                None
            };
            Ok((registry, choice, context))
        })
        .await?;
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
    let (context, area, route_identity) = if let (Some(context), Some(area)) = (live_context, live)
    {
        (context, area, None)
    } else {
        let (context, route, sites, corridor, request, _) =
            crate::coverage_http::capture(app, workspace, body.as_object().unwrap(), false).await?;
        (
            context,
            ExtraArea::Route {
                sites,
                corridor,
                request: Box::new(request),
            },
            Some((
                route["snapshot_version"].clone(),
                route["active_alternative_id"].clone(),
            )),
        )
    };
    tokio::task::spawn_blocking(move || {
        let terrain = context.open()?;
        let crs = terrain.crs();
        let transform = Transformer::new(crs).map_err(terrain_invalid)?;
        let area = match area {
            ExtraArea::Live(sites, radius) => circle_union(&transform, sites, radius)?,
            ExtraArea::Route {
                sites,
                corridor,
                request,
            } => {
                let sources =
                    repeaternet_coverage::select_sources(&sites, &sites, &request.settings)
                        .map_err(terrain_invalid)?;
                repeaternet_coverage::resolve_area(
                    &request.settings,
                    &transform,
                    &sources,
                    &sites,
                    corridor,
                    request.map_bounds,
                )
                .map_err(terrain_invalid)?
                .geometry()
                .clone()
            }
        };
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
                area.clone()
            } else {
                area.difference(&geo::unary_union(&footprints))
            }
        };
        let dtm: MultiPolygon<f64> = missing(terrain.ground_tiles());
        let dom =
            (!context.route.paths("dom").is_empty()).then(|| missing(terrain.surface_tiles()));
        let provider = TerrainDownloader::new(registry, &context.cache_directory, limits)
            .map_err(terrain_invalid)?;
        let plans = provider
            .plan_coverage_extension(crs, &dtm, dom.as_ref(), resolution, choice.clone())
            .map_err(terrain_invalid)?;
        let estimate = provider
            .estimate(crs, &plans, &context.directory)
            .map_err(terrain_invalid)?;
        Ok(Download {
            context,
            provider,
            choice,
            crs,
            dtm: plans.dtm,
            dom: plans.dom,
            estimate: json!(estimate),
            plan: None,
            route_identity,
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
    let download = route_plan(app.clone(), workspace.clone(), Value::Object(body), false).await?;
    record_estimate(app, workspace, download).await
}

pub async fn estimate_extra(
    State(app): State<AppState>,
    workspace: Workspace,
    ObjectBody(body): ObjectBody,
) -> ApiResult<Json<Value>> {
    let download = extra_plan(app.clone(), workspace.clone(), Value::Object(body), false).await?;
    record_estimate(app, workspace, download).await
}

async fn record_estimate(
    app: AppState,
    workspace: Workspace,
    download: Download,
) -> ApiResult<Json<Value>> {
    workspace
        .run(app, move |app, key, session| {
            download.verify(app, key, session)?;
            *estimate_slot(session, download.supplemental()) =
                Some(download.provider.fingerprint().into());
            Ok(Json(download.estimate))
        })
        .await
}

pub async fn prepare(
    State(app): State<AppState>,
    workspace: Workspace,
    ObjectBody(body): ObjectBody,
) -> ApiResult<Json<Value>> {
    let download = route_plan(app.clone(), workspace.clone(), Value::Object(body), true).await?;
    enqueue(app, workspace, download).await
}

pub async fn prepare_extra(
    State(app): State<AppState>,
    workspace: Workspace,
    ObjectBody(body): ObjectBody,
) -> ApiResult<Json<Value>> {
    let download = extra_plan(app.clone(), workspace.clone(), Value::Object(body), true).await?;
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
        let registry = app.provider_snapshot()?;
        check_estimate(Some(download.provider.fingerprint()), &registry)?;
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
            (true, false) => "Preparing route terrain",
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
        tokio::spawn(run(app.clone(), Workspace::worker(key), Arc::new(job), outstanding, worker));
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

    fn stage(self: &Arc<Self>, app: &AppState, key: &str) -> ApiResult<DownloadStage> {
        let job = self.clone();
        self.download
            .stage(app, key, &self.id, Arc::new(move || job.cancelled()))
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
        outcome: ApiResult<Option<DownloadStage>>,
    ) -> ApiResult<()> {
        if !self.download.matches(session, &self.id) {
            return Ok(());
        }
        let supplemental = self.download.supplemental();
        let mut status = self.download.status(session).clone();
        let published = outcome.and_then(|stage| {
            if let Some(stage) = stage.filter(|_| !self.cancelled()) {
                self.publish(app, key, session, stage.generation)?;
                status["warnings"] = json!(stage.warnings);
                status["providers_used"] = json!(stage.providers_used);
                if let Some(plan) = &self.download.plan {
                    status["plan_revision"] = json!(catalog::plan_fingerprint(plan, false));
                }
                return Ok(true);
            }
            Ok(false)
        });
        let (state, stage) = match published {
            Ok(true) if supplemental => ("complete", "Coverage terrain ready".to_owned()),
            Ok(true) => ("terrain_ready", "Terrain prepared and ready".to_owned()),
            _ if self.cancelled() && supplemental => (
                "cancelled",
                "Coverage terrain preparation cancelled".to_owned(),
            ),
            _ if self.cancelled() => ("cancelled", "Terrain preparation cancelled".to_owned()),
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
        let (app, key, job) = (app.clone(), workspace.key.clone(), job.clone());
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
