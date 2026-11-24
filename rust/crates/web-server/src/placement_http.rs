use std::{collections::BTreeSet, sync::Arc};

use axum::{
    Json,
    extract::{Path, State},
};
use repeaternet_optimizer::{
    Cancellation, OptimizationError, PlacementRequest, PlacementSite, Progress, RouteOptimizer,
    SearchHooks, SiteOrigin,
};
use repeaternet_rf_core::SiteKind;
use repeaternet_terrain::Point;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::OwnedSemaphorePermit;

use crate::{
    api::{ApiError, ApiResult, Workspace, active_project, ensure_idle},
    catalog::{self, AppState, Session},
    route_http::{self, Control},
    route_inputs::{self, RouteInputs},
    terrain_http::{TerrainContext, terrain_invalid},
};

#[derive(Deserialize)]
pub struct Request {
    site_ids: BTreeSet<String>,
    radius_m: Option<f64>,
}

pub(crate) struct Job {
    identity: Arc<Identity>,
    pub control: Arc<Control>,
    pub status: Value,
}

impl Job {
    pub fn active(&self) -> bool {
        matches!(self.status["state"].as_str(), Some("queued" | "running"))
    }
}

struct Identity {
    id: String,
    context: TerrainContext,
    snapshot_version: u64,
}

impl Identity {
    fn verify(&self, app: &AppState, key: &str, session: &Session) -> ApiResult<()> {
        if self.snapshot_version != session.snapshot_version {
            return Err(ApiError::conflict(
                "The selected route changed during placement optimization",
            ));
        }
        self.context.verify(app, key, session)
    }

    fn job<'a>(&self, session: &'a mut Session) -> Option<&'a mut Job> {
        session
            .placement_job
            .as_mut()
            .filter(|job| job.identity.id == self.id)
    }
}

struct Selection {
    request: Request,
    inputs: RouteInputs,
    // Search-found routers and mesh clients come from the certified snapshot.
    snapshot_sites: Vec<PlacementSite>,
    links: Option<Vec<[String; 2]>>,
}

pub async fn start(
    State(app): State<AppState>,
    workspace: Workspace,
    Json(request): Json<Request>,
) -> ApiResult<Json<Value>> {
    if !(2..=32).contains(&request.site_ids.len()) {
        return Err(ApiError::invalid(
            "Select between 2 and 32 sites to optimize placement",
        ));
    }
    if request.radius_m.is_some_and(|r| !(0.0 < r && r <= 2000.0)) {
        return Err(ApiError::invalid(
            "Move radius must be greater than 0 and at most 2,000 m",
        ));
    }
    workspace.run(app, move |app, key, session| {
        ensure_idle(session)?;
        let project = active_project(&app.store, key, session)?;
        let context = TerrainContext::capture(app, key, session, crate::mesh_paint::uses_paint(&project.plan))?;
        let inputs = route_inputs::parse_for_placement(project.plan, &app.defaults)?;
        let active = session.route_identity.as_ref()
            .filter(|(plan, terrain)| plan == &context.plan_fingerprint && terrain == &context.fingerprint)
            .and(session.route_result.as_ref())
            .and_then(|result| session.route_alternatives.iter().find(|alt| alt.summary["id"] == result["active_alternative_id"]));
        let known_ids: BTreeSet<_> = inputs.locations.iter().map(|site| site.id.as_str()).collect();
        let mut snapshot_sites = Vec::new();
        let links = active.map(|alt| {
            snapshot_sites.extend(alt.rf_sites.iter().filter(|site| {
                request.site_ids.contains(&site.id) && !known_ids.contains(site.id.as_str())
            }).map(|site| PlacementSite { rf: site.clone(), movable: site.kind == SiteKind::Candidate }));
            alt.links.iter().filter_map(|link| {
                let source = link["source_id"].as_str()?;
                let target = link["target_id"].as_str()?;
                (request.site_ids.contains(source) && request.site_ids.contains(target))
                    .then(|| [source.to_owned(), target.to_owned()])
            }).collect()
        });
        if request.site_ids.iter().any(|id| {
            !known_ids.contains(id.as_str()) && !snapshot_sites.iter().any(|site| site.rf.id == *id)
        }) {
            return Err(ApiError::invalid("A selected site is no longer in the plan or current route"));
        }
        let outstanding = app.scheduler.reserve_job()?;
        let worker = app.scheduler.try_worker();
        let identity = Arc::new(Identity {
            id: catalog::random_id::<12>()?, context, snapshot_version: session.snapshot_version,
        });
        let status = json!({
            "job_id": identity.id,
            "state": if worker.is_some() { "running" } else { "queued" },
            "stage": if worker.is_some() { "Checking terrain" } else { "Waiting for a planner worker" },
            "started_at": route_http::now(),
        });
        let control = Arc::new(Control::default());
        session.placement_job = Some(Job { identity: identity.clone(), control: control.clone(), status: status.clone() });
        tokio::spawn(run(app.clone(), key.to_owned(), identity, control,
            Selection { request, inputs, snapshot_sites, links }, outstanding, worker));
        Ok(Json(status))
    }).await
}

pub async fn get(
    State(app): State<AppState>,
    workspace: Workspace,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    workspace
        .run(app, move |app, key, session| {
            let job = session
                .placement_job
                .as_ref()
                .filter(|job| job.identity.id == id)
                .ok_or_else(|| {
                    ApiError::missing("This placement preview is no longer available")
                })?;
            if job.status["state"] == "complete" {
                job.identity.verify(app, key, session)?;
            }
            Ok(Json(job.status.clone()))
        })
        .await
}

async fn run(
    app: AppState,
    key: String,
    identity: Arc<Identity>,
    control: Arc<Control>,
    selection: Selection,
    _outstanding: OwnedSemaphorePermit,
    worker: Option<OwnedSemaphorePermit>,
) {
    let worker = app.scheduler.wait_worker(worker, &control).await;
    let workspace = Workspace::worker(&key, &identity.context.project_id);
    let outcome = if worker.is_some() && control.cancellation() == Cancellation::Continue {
        let app = app.clone();
        let identity = identity.clone();
        let control = control.clone();
        tokio::task::spawn_blocking(move || optimize(&app, &key, &identity, &control, selection))
            .await
            .map_err(ApiError::internal)
            .and_then(|outcome| outcome)
    } else {
        Ok(None)
    };
    let published = workspace
        .run(app, move |app, key, session| {
            let outcome = if control.cancellation() == Cancellation::Continue {
                outcome.and_then(|preview| identity.verify(app, key, session).map(|()| preview))
            } else {
                Ok(None)
            };
            if let Some(job) = identity.job(session) {
                let (state, stage) = match outcome {
                    Ok(Some(preview)) => {
                        job.status["preview"] = preview;
                        ("complete", "Placement preview ready".to_owned())
                    }
                    Ok(None) => ("cancelled", "Placement optimization cancelled".to_owned()),
                    Err(error) => ("failed", error.detail().to_owned()),
                };
                job.status["state"] = json!(state);
                job.status["stage"] = json!(stage);
            }
            Ok(())
        })
        .await;
    if let Err(error) = published {
        eprintln!("Could not publish placement job result: {}", error.detail());
    }
}

fn optimize(
    app: &AppState,
    key: &str,
    identity: &Identity,
    control: &Control,
    mut selection: Selection,
) -> ApiResult<Option<Value>> {
    let session = app.session(key, &identity.context.project_id);
    let update = |stage: &str, done, total| {
        if let Some(job) = session
            .blocking_lock()
            .as_mut()
            .and_then(|s| identity.job(s))
        {
            job.status["state"] = json!("running");
            job.status["stage"] = json!(stage);
            job.status["done"] = json!(done);
            job.status["total"] = json!(total);
        }
    };
    update("Checking terrain", 0, 0);
    let terrain = identity.context.open()?;
    selection
        .inputs
        .locations
        .retain(|site| selection.request.site_ids.contains(&site.id));
    let (transform, projected) = route_http::project_sites(&terrain, &selection.inputs.locations)?;
    let mut sites: Vec<_> = projected
        .into_iter()
        .map(|site| PlacementSite {
            movable: site.origin == SiteOrigin::Manual,
            rf: site.rf,
        })
        .chain(selection.snapshot_sites)
        .collect();
    sites.sort_by(|a, b| a.rf.id.cmp(&b.rf.id));
    let settings = &selection.inputs.candidates;
    let radius_m = selection.request.radius_m.unwrap_or({
        if settings.refine_radius_m > 0.0 {
            settings.refine_radius_m
        } else {
            250.0
        }
    });
    let exclusions = route_inputs::project_exclusions(&selection.inputs.plan, &transform)?;
    let index = |id: &str| {
        sites
            .binary_search_by(|site| site.rf.id.as_str().cmp(id))
            .expect("selected route site")
    };
    let links = selection.links.as_ref().map(|links| {
        links
            .iter()
            .map(|[a, b]| (index(a), index(b)))
            .collect::<Vec<_>>()
    });
    let roads = route_http::road_access(
        app,
        terrain.crs(),
        settings,
        &identity.context.cache_directory,
    )?
    .map(std::cell::RefCell::new);
    let cancellation = || control.cancellation();
    let allowed = |points: &[Point]| {
        roads
            .as_ref()
            .expect("road access enabled")
            .borrow_mut()
            .allowed(points, &|| cancellation() != Cancellation::Continue, update)
            .map_err(|error| OptimizationError::InvalidInput(error.to_string()))
    };
    let mut progress =
        |progress: Progress| update(progress.stage.label(), progress.done, progress.total);
    let mut optimizer = RouteOptimizer::new(&terrain, &selection.inputs.rf, settings);
    if roads.is_some() {
        optimizer = optimizer.with_site_filter(&allowed);
    }
    let preview = optimizer
        .optimize_placement(
            PlacementRequest {
                sites: &sites,
                links: links.as_deref(),
                exclusions: &exclusions,
                radius_m,
            },
            SearchHooks {
                cancellation: Some(&cancellation),
                progress: Some(&mut progress),
                ..Default::default()
            },
        )
        .map_err(terrain_invalid)?;
    let Some(preview) = preview else {
        return Ok(None);
    };
    let positions = sites
        .iter()
        .zip(preview.sites)
        .map(|(old, new)| {
            let from = transform
                .reverse(old.rf.position)
                .map_err(terrain_invalid)?;
            let to = transform.reverse(new.position).map_err(terrain_invalid)?;
            Ok(
                json!({"id": new.id, "moved": old.rf.position != new.position,
            "before": [from.latitude, from.longitude], "after": [to.latitude, to.longitude]}),
            )
        })
        .collect::<ApiResult<Vec<_>>>()?;
    let links = preview
        .links
        .iter()
        .map(|link| {
            let before = link
                .before
                .map(|margin| json!({"db": margin.db, "valid": margin.valid}));
            let after = link
                .after
                .map(|margin| json!({"db": margin.db, "valid": margin.valid}));
            json!({"source_id": sites[link.source].rf.id, "target_id": sites[link.target].rf.id,
            "before": before, "after": after})
        })
        .collect::<Vec<_>>();
    Ok(Some(json!({"sites": positions, "links": links,
        "link_scope": if selection.links.is_some() { "topology" } else { "range" }})))
}
