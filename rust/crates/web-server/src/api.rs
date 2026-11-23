use std::time::UNIX_EPOCH;

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, FromRequest, FromRequestParts, Multipart, Path, Request, State},
    http::{Method, StatusCode, header, request::Parts},
    response::{IntoResponse, Response},
    routing::{get, patch, post, put},
};
use repeaternet_project_store::{
    Error as StoreError, Project, ProjectStore, SHARED_WORKSPACE_KEY, normalize_project_name,
};
use serde_json::{Map, Value, json};

use crate::{
    catalog::{self, AppState, Session, TerrainFiles},
    coverage_runs, route_http, security, targets, terrain_http,
};

pub fn router(app: AppState) -> Router {
    Router::new()
        .route("/api/state", get(state))
        .route("/api/projects", post(create))
        .route("/api/projects/autosave", post(autosave))
        .route("/api/projects/recover-previous", post(recover_previous))
        .route("/api/projects/{id}", patch(rename).delete(delete))
        .route("/api/projects/{id}/activate", post(activate))
        .route("/api/projects/{id}/duplicate", post(duplicate))
        .route("/api/projects/{id}/archive", post(archive))
        .route("/api/projects/{id}/restore", post(restore))
        .route("/api/export/{kind}", get(export))
        .route("/api/result", get(result))
        .route("/api/optimize", post(route_http::start))
        .route("/api/link/evaluate", post(crate::link_http::evaluate))
        .route(
            "/api/live-coverage",
            post(crate::live_coverage_http::evaluate),
        )
        .route("/api/cancel", post(route_http::cancel))
        .route("/api/stop-and-keep", post(route_http::keep))
        .route("/api/alternatives/{id}/select", post(route_http::select))
        .route("/api/cache", axum::routing::delete(route_http::clear_cache))
        .route("/api/terrain/coverage", post(terrain_http::coverage))
        .route(
            "/api/terrain/estimate",
            post(crate::provider_http::estimate),
        )
        .route("/api/terrain/prepare", post(crate::provider_http::prepare))
        .route(
            "/api/data-sources",
            get(crate::source_http::get).put(crate::source_http::put),
        )
        .route(
            "/api/data-sources/validate",
            post(crate::source_http::validate),
        )
        .route(
            "/api/data-sources/{id}/probe",
            post(crate::source_http::probe),
        )
        .route(
            "/api/data-sources/preview",
            post(crate::source_http::preview),
        )
        .route(
            "/api/terrain/{kind}",
            post(terrain_http::upload)
                .layer(DefaultBodyLimit::max(
                    terrain_http::UPLOAD_LIMIT + 64 * 1024,
                ))
                .delete(terrain_http::clear),
        )
        .route("/api/meshcore", post(crate::provider_http::meshcore))
        .route(
            "/api/coverage/terrain/estimate",
            post(crate::provider_http::estimate_extra),
        )
        .route(
            "/api/coverage/terrain/prepare",
            post(crate::provider_http::prepare_extra),
        )
        .route(
            "/api/coverage/terrain/{id}/cancel",
            post(crate::provider_http::cancel_extra),
        )
        .route(
            "/api/coverage/jobs",
            get(coverage_runs::history).post(coverage_runs::start),
        )
        .route(
            "/api/coverage/estimate",
            post(crate::coverage_http::estimate),
        )
        .route("/api/coverage/preview", post(crate::coverage_http::preview))
        .route("/api/coverage/jobs/{job_id}", get(coverage_runs::detail))
        .route(
            "/api/coverage/jobs/{job_id}/cells",
            get(coverage_runs::cells),
        )
        .route(
            "/api/coverage/jobs/{job_id}/cancel",
            post(coverage_runs::cancel),
        )
        .route(
            "/api/coverage/jobs/{job_id}/export.geojson",
            get(crate::downloads::coverage_geojson),
        )
        .route(
            "/api/coverage/jobs/{job_id}/export.json",
            get(crate::downloads::coverage_json),
        )
        .route(
            "/api/coverage/jobs/{job_id}/report.html",
            get(crate::downloads::coverage_html),
        )
        .route(
            "/api/coverage/jobs/{job_id}/scenario",
            post(crate::coverage_analysis::scenario),
        )
        .route(
            "/api/coverage/compare",
            post(crate::coverage_analysis::compare),
        )
        .route(
            "/api/coverage/jobs/{job_id}/targets",
            post(crate::coverage_analysis::assess),
        )
        .route(
            "/api/coverage/target-reports",
            get(crate::coverage_analysis::reports),
        )
        .route(
            "/api/coverage/target-reports/compare",
            post(crate::coverage_analysis::compare_reports),
        )
        .route(
            "/api/coverage/inspect",
            post(crate::coverage_inspect::start),
        )
        .route(
            "/api/coverage/inspect/{job_id}",
            get(crate::coverage_inspect::detail),
        )
        .route(
            "/api/coverage/inspect/{job_id}/cancel",
            post(crate::coverage_inspect::cancel),
        )
        .route("/api/coverage/targets", put(save_targets))
        .route(
            "/api/coverage/targets/import",
            post(import_targets).layer(DefaultBodyLimit::max(targets::IMPORT_LIMIT + 64 * 1024)),
        )
        .layer(DefaultBodyLimit::max(16 * 1024 * 1024))
        .with_state(app)
}

pub async fn health() -> Json<Value> {
    Json(json!({"status": "ok", "runtime": "rust"}))
}

pub fn problem(status: StatusCode, detail: &str) -> Response {
    (status, Json(json!({"detail": detail}))).into_response()
}

fn workspace_required() -> Response {
    problem(
        StatusCode::UNAUTHORIZED,
        "Open the planner to initialize a workspace",
    )
}

#[derive(Debug)]
pub(crate) struct ApiError(StatusCode, String);
pub(crate) type ApiResult<T> = Result<T, ApiError>;

impl ApiError {
    pub(crate) fn detail(&self) -> &str {
        &self.1
    }
    pub(crate) fn new(status: StatusCode, detail: impl Into<String>) -> Self {
        Self(status, detail.into())
    }

    pub(crate) fn invalid(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::UNPROCESSABLE_ENTITY, detail)
    }

    pub(crate) fn missing(detail: &'static str) -> Self {
        Self::new(StatusCode::NOT_FOUND, detail)
    }

    pub(crate) fn conflict(detail: &'static str) -> Self {
        Self::new(StatusCode::CONFLICT, detail)
    }

    pub(crate) fn internal(error: impl std::fmt::Display) -> Self {
        eprintln!("Project API error: {error}");
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not access project data",
        )
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        problem(self.0, &self.1)
    }
}

impl From<StoreError> for ApiError {
    fn from(error: StoreError) -> Self {
        let status = match error {
            StoreError::InvalidWorkspace => StatusCode::UNAUTHORIZED,
            StoreError::InvalidName | StoreError::InvalidPlan => StatusCode::UNPROCESSABLE_ENTITY,
            StoreError::NameInUse
            | StoreError::ArchiveActive
            | StoreError::DeleteActive
            | StoreError::DeleteWorkspaceRoot => StatusCode::CONFLICT,
            StoreError::ProjectNotFound => StatusCode::NOT_FOUND,
            _ => return Self::internal(error),
        };
        Self::new(status, error.to_string())
    }
}

const PROJECT_HEADER: &str = "x-repeaternet-project";
const PLAN_REVISION_HEADER: &str = "x-repeaternet-plan-revision";
const PROJECT_CHANGED: &str = "The active project changed. Reload the project.";
const PLAN_CHANGED: &str =
    "The plan changed in another browser. Reload the project before editing.";

#[derive(Clone)]
pub(crate) struct Workspace {
    pub(crate) key: String,
    browser: Option<String>,
    worker_project: String,
    project: Option<String>,
    plan_revision: Option<String>,
}

impl FromRequestParts<AppState> for Workspace {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        app: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // The random workspace cookie identifies this browser profile (including
        // its tabs): its private workspace, or its selection in the shared catalog.
        let browser = security::workspace(&parts.headers).ok_or_else(workspace_required)?;
        if !app.shared_projects {
            return Ok(Self::worker(&browser, ""));
        }
        let read = |name| {
            parts
                .headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        };
        let project = read(PROJECT_HEADER);
        let plan_revision = read(PLAN_REVISION_HEADER);
        let writes_plan = matches!(
            (&parts.method, parts.uri.path()),
            (
                &Method::POST,
                "/api/projects/autosave"
                    | "/api/projects/recover-previous"
                    | "/api/optimize"
                    | "/api/terrain/prepare"
            ) | (&Method::PUT, "/api/coverage/targets")
        );
        if writes_plan && (project.is_none() || plan_revision.is_none()) {
            return Err(problem(
                StatusCode::PRECONDITION_REQUIRED,
                "Open the project to get its current plan revision",
            ));
        }
        Ok(Self {
            key: SHARED_WORKSPACE_KEY.to_owned(),
            browser: Some(browser),
            worker_project: String::new(),
            project,
            plan_revision,
        })
    }
}

impl Workspace {
    pub fn worker(key: &str, project_id: &str) -> Self {
        Self {
            key: key.to_owned(),
            browser: None,
            worker_project: project_id.to_owned(),
            project: None,
            plan_revision: None,
        }
    }

    pub async fn run<T: Send + 'static>(
        self,
        app: AppState,
        action: impl FnOnce(&AppState, &str, &mut Session) -> ApiResult<T> + Send + 'static,
    ) -> ApiResult<T> {
        self.run_with_selection(app, move |app, key, session, _| action(app, key, session))
            .await
    }

    async fn run_with_selection<T: Send + 'static>(
        self,
        app: AppState,
        action: impl FnOnce(&AppState, &str, &mut Session, Option<&str>) -> ApiResult<T>
        + Send
        + 'static,
    ) -> ApiResult<T> {
        let Self {
            key,
            browser,
            worker_project,
            project,
            plan_revision,
        } = self;
        let selection = match &browser {
            Some(browser) => Some(app.browser_selection(browser).lock_owned().await),
            None => None,
        };
        let project_id = if let Some(browser) = &browser {
            let store = app.store.clone();
            let browser = browser.clone();
            tokio::task::spawn_blocking(move || -> ApiResult<String> {
                Ok(store.shared_selection(&browser)?.id)
            })
            .await
            .map_err(ApiError::internal)??
        } else {
            worker_project
        };
        let mut session = app.session(&key, &project_id).lock_owned().await;
        tokio::task::spawn_blocking(move || {
            let _selection = selection;
            let selected = if browser.is_some() {
                Some(
                    app.store
                        .get(&key, &project_id)?
                        .filter(|project| !project.archived)
                        .ok_or_else(|| ApiError::conflict(PROJECT_CHANGED))?,
                )
            } else {
                None
            };
            if session.is_none() {
                let active = match selected {
                    Some(project) => project,
                    None if app.shared_projects => return Err(ApiError::conflict(PROJECT_CHANGED)),
                    None => app.store.active(&key)?,
                };
                let mut opened = Session::open(&active, None);
                restore_project_route(&app, &key, &active, &mut opened)?;
                *session = Some(opened);
            }
            let session = session.as_mut().expect("initialized workspace");
            if project.as_ref().is_some_and(|id| id != &session.project_id) {
                return Err(ApiError::conflict(PROJECT_CHANGED));
            }
            if let Some(expected) = plan_revision {
                let current = active_project(&app.store, &key, session)?;
                if expected != catalog::plan_fingerprint(&current.plan, false) {
                    return Err(ApiError::conflict(PLAN_CHANGED));
                }
            }
            action(&app, &key, session, browser.as_deref())
        })
        .await
        .map_err(ApiError::internal)?
    }
}

// Project routes accept a Python dict body, then validate the relevant fields.
pub(crate) struct ObjectBody(pub Map<String, Value>);

impl FromRequest<AppState> for ObjectBody {
    type Rejection = Response;

    async fn from_request(request: Request, _: &AppState) -> Result<Self, Self::Rejection> {
        let media_type = request.headers().get(header::CONTENT_TYPE).cloned();
        let body = axum::body::Bytes::from_request(request, &())
            .await
            .map_err(|error| problem(error.status(), &error.body_text()))?;
        let validation = |error: Value| {
            (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({"detail": [error]})),
            )
                .into_response()
        };
        if body.is_empty() {
            return Err(validation(
                json!({"type": "missing", "loc": ["body"], "msg": "Field required", "input": null}),
            ));
        }
        let is_json = media_type.is_none()
            || media_type
                .as_ref()
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| {
                    let media_type = value
                        .split(';')
                        .next()
                        .unwrap_or_default()
                        .trim()
                        .to_ascii_lowercase();
                    media_type == "application/json"
                        || (media_type.starts_with("application/") && media_type.ends_with("+json"))
                });
        let value = if is_json {
            serde_json::from_slice(&body)
                .map_err(|error| validation(json_decode_error(&body, &error)))?
        } else {
            json!(String::from_utf8_lossy(&body))
        };
        match value {
            Value::Object(fields) => Ok(Self(fields)),
            Value::Null => Err(validation(
                json!({"type": "missing", "loc": ["body"], "msg": "Field required", "input": null}),
            )),
            _ => Err(validation(
                json!({"type": "dict_type", "loc": ["body"], "msg": "Input should be a valid dictionary", "input": value}),
            )),
        }
    }
}

fn json_decode_error(body: &[u8], error: &serde_json::Error) -> Value {
    let diagnostic = error.to_string();
    let message = diagnostic.split(" at line ").next().unwrap_or(&diagnostic);
    let mut offset = if error.is_eof() {
        body.len()
    } else {
        body.split(|byte| *byte == b'\n')
            .take(error.line().saturating_sub(1))
            .map(|line| line.len() + 1)
            .sum::<usize>()
            + error.column().saturating_sub(1)
    }
    .min(body.len());
    let message = match message {
        "key must be a string" | "EOF while parsing an object" => {
            "Expecting property name enclosed in double quotes"
        }
        "expected `:`" => "Expecting ':' delimiter",
        "expected `,` or `]`" | "expected `,` or `}`" => "Expecting ',' delimiter",
        "expected value" | "EOF while parsing a value" | "EOF while parsing a list" => {
            "Expecting value"
        }
        "trailing characters" => "Extra data",
        "trailing comma" => {
            let closing = body.get(offset);
            if let Some(comma) = body[..offset]
                .iter()
                .rposition(|byte| !byte.is_ascii_whitespace())
            {
                offset = comma;
            }
            if closing == Some(&b']') {
                "Illegal trailing comma before end of array"
            } else {
                "Illegal trailing comma before end of object"
            }
        }
        _ => message,
    };
    let position = String::from_utf8_lossy(&body[..offset]).chars().count();
    json!({"type":"json_invalid","loc":["body",position],"msg":"JSON decode error","input":{},"ctx":{"error":message}})
}

pub(crate) fn active_project(
    store: &ProjectStore,
    key: &str,
    session: &Session,
) -> ApiResult<Project> {
    store
        .get(key, &session.project_id)?
        .filter(|project| !project.archived)
        .ok_or_else(|| ApiError::missing("Active project is no longer available"))
}

pub(crate) fn ensure_idle(session: &Session) -> ApiResult<()> {
    if session.calculations_active() || session.inspection_active() {
        return Err(ApiError::conflict(
            "Wait for the current job before switching projects",
        ));
    }
    Ok(())
}

fn project_name(body: &Map<String, Value>) -> ApiResult<&str> {
    let name = body
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::invalid("Project name must be text"))?;
    Ok(normalize_project_name(name)?)
}

fn restore_project_route(
    app: &AppState,
    key: &str,
    project: &Project,
    session: &mut Session,
) -> ApiResult<()> {
    let directory = app.store.project_path(key, project)?;
    route_http::restore(&app.store, key, project, &directory, session)
}

fn open_project(
    app: &AppState,
    key: &str,
    session: &mut Session,
    project: Project,
) -> ApiResult<Json<Value>> {
    let mut opened = Session::open(&project, Some(session.input_revision));
    opened.upload = session.upload.clone();
    *session = opened;
    restore_project_route(app, key, &project, session)?;
    Ok(Json(catalog::opened_project(project)))
}

fn select_project(
    app: &AppState,
    key: &str,
    session: &mut Session,
    browser: Option<&str>,
    project: Project,
) -> ApiResult<Json<Value>> {
    if let Some(browser) = browser {
        let project = app
            .store
            .select_shared(browser, &project.id)?
            .ok_or_else(|| ApiError::missing("Project not found"))?;
        return Ok(Json(catalog::opened_project(project)));
    }
    open_project(app, key, session, project)
}

async fn state(State(app): State<AppState>, workspace: Workspace) -> ApiResult<Json<Value>> {
    workspace.run(app, |app, key, session| {
        let store = &app.store;
        let project = active_project(store, key, session)?;
        let directory = store.project_path(key, &project)?;
        let route_terrain = TerrainFiles::read(&directory, false);
        let coverage_terrain = TerrainFiles::read(&directory, true);
        let previous_result = catalog::historical_result(&project, &directory, &route_terrain);
        let mut payload = (*app.defaults).clone();
        payload["job"] = session.job_payload();
        let coverage_job = session.coverage_job.clone().or_else(|| {
            let storage = coverage_runs::store(app, key, session).ok()?;
            let legacy = route_terrain.fingerprint(&directory, Some(&coverage_terrain))
                .map(|terrain| repeaternet_coverage_store::LegacyIdentity::from_fingerprint(&project.id, terrain));
            coverage_runs::selected(&storage, &directory, legacy.as_ref()).ok().flatten()
        });
        payload["coverage_job"] = coverage_job.map_or(Value::Null, |job| {
            coverage_runs::status(job, coverage_runs::identity(app, key, session).as_ref())
        });
        payload["coverage_terrain_job"] = json!(session.coverage_terrain_job);
        let stats = app.cache.stats(&project.id);
        payload["rf_cache"] = json!({"entries": stats.entries, "hits": stats.hits, "misses": stats.misses,
            "evictions": stats.evictions, "capacity": stats.capacity});
        payload["terrain"] = route_terrain.names();
        payload["coverage_terrain"] = coverage_terrain.names();
        payload["terrain_sources"] = json!(route_terrain.sources(&directory));
        payload["coverage_terrain_sources"] = json!(coverage_terrain.sources(&directory));
        if let Some(settings) = project.plan.get("coverage") {
            payload["coverage_settings"] = settings.clone();
        }
        let plan_revision = catalog::plan_fingerprint(&project.plan, false);
        payload["plan"] = if project.plan.as_object().is_some_and(Map::is_empty) { Value::Null } else { project.plan };
        payload["projects"] = json!({
            "active_id": project.id,
            "active_name": project.name,
            "shared": app.shared_projects,
            "plan_revision": plan_revision,
            "has_previous_revision": project.previous_plan.is_some(),
            "items": store.list_active(key)?.into_iter().map(catalog::project_item).collect::<Vec<_>>(),
            "archived_items": store.list_archived(key)?.into_iter().map(catalog::project_item).collect::<Vec<_>>(),
            "storage_usage_bytes": store.storage_usage(key)?,
            "previous_result": previous_result,
        });
        Ok(Json(payload))
    }).await
}

async fn autosave(
    State(app): State<AppState>,
    workspace: Workspace,
    ObjectBody(mut body): ObjectBody,
) -> ApiResult<Json<Value>> {
    workspace
        .run(app, move |app, key, session| {
            let mut plan = body
                .remove("plan")
                .filter(Value::is_object)
                .ok_or_else(|| ApiError::invalid("Plan must be a JSON object"))?;
            if let Some(collection) = plan.get_mut("coverage_targets") {
                *collection = targets::normalize(collection).map_err(ApiError::invalid)?;
            }
            if let Some(id) = body.get("project_id").filter(|id| !id.is_null())
                && id.as_str() != Some(&session.project_id)
            {
                return Err(ApiError::conflict(
                    "Autosave belongs to a project that is no longer active",
                ));
            }
            if session.calculations_active() {
                return Err(ApiError::conflict(
                    "Plan inputs are locked while a job is active",
                ));
            }
            session.cancel_inspection();
            let current = active_project(&app.store, key, session)?;
            save_plan(&app.store, key, &current.id, &plan)?;
            let saved = active_project(&app.store, key, session)?;
            if catalog::route_inputs_changed(&current.plan, &plan) {
                session.invalidate_route();
                if matches!(saved.run_state.as_str(), "complete" | "stopped") {
                    // Keep the last snapshot so undo can restore it if plan and terrain match.
                    restore_project_route(app, key, &saved, session)?;
                } else if saved.result_summary.is_some() {
                    app.store.save_result_summary(key, &saved.id, None)?;
                }
            }
            Ok(Json(json!({
                "ok": true, "project_id": saved.id,
                "updated_at": UNIX_EPOCH.elapsed().unwrap_or_default().as_secs_f64(),
                "has_previous_revision": saved.previous_plan.is_some(),
                "plan_revision": catalog::plan_fingerprint(&saved.plan, false),
                "route_result_available": session.route_result.is_some(),
            })))
        })
        .await
}

pub(crate) fn save_plan(store: &ProjectStore, key: &str, id: &str, plan: &Value) -> ApiResult<()> {
    if !store.autosave(key, id, plan)? {
        return Err(ApiError::missing("Active project is no longer available"));
    }
    Ok(())
}

async fn recover_previous(
    State(app): State<AppState>,
    workspace: Workspace,
) -> ApiResult<Json<Value>> {
    workspace
        .run(app, |app, key, session| {
            ensure_idle(session)?;
            let project = app
                .store
                .recover_previous(key, &session.project_id)?
                .ok_or_else(|| ApiError::missing("No previous autosaved revision is available"))?;
            open_project(app, key, session, project)
        })
        .await
}

async fn create(
    State(app): State<AppState>,
    workspace: Workspace,
    ObjectBody(mut body): ObjectBody,
) -> ApiResult<Json<Value>> {
    workspace
        .run_with_selection(app, move |app, key, session, browser| {
            if browser.is_none() {
                ensure_idle(session)?;
            }
            let plan = body.remove("plan").unwrap_or_else(|| json!({}));
            let project = app.store.create(key, project_name(&body)?, &plan)?;
            select_project(app, key, session, browser, project)
        })
        .await
}

async fn activate(
    State(app): State<AppState>,
    workspace: Workspace,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    workspace
        .run_with_selection(app, move |app, key, session, browser| {
            if let Some(browser) = browser {
                let project = app
                    .store
                    .select_shared(browser, &id)?
                    .ok_or_else(|| ApiError::missing("Project not found"))?;
                return Ok(Json(catalog::opened_project(project)));
            }
            ensure_idle(session)?;
            let project = app
                .store
                .activate(key, &id)?
                .ok_or_else(|| ApiError::missing("Project not found"))?;
            open_project(app, key, session, project)
        })
        .await
}

async fn duplicate(
    State(app): State<AppState>,
    workspace: Workspace,
    Path(id): Path<String>,
    ObjectBody(body): ObjectBody,
) -> ApiResult<Json<Value>> {
    workspace
        .run_with_selection(app, move |app, key, session, browser| {
            if browser.is_none() {
                ensure_idle(session)?;
            }
            let project = app
                .store
                .duplicate(key, &id, project_name(&body)?)?
                .ok_or_else(|| ApiError::missing("Project not found"))?;
            select_project(app, key, session, browser, project)
        })
        .await
}

async fn rename(
    State(app): State<AppState>,
    workspace: Workspace,
    Path(id): Path<String>,
    ObjectBody(body): ObjectBody,
) -> ApiResult<Json<Value>> {
    workspace
        .run(app, move |app, key, _| {
            let name = project_name(&body)?;
            if !app.store.rename(key, &id, name)? {
                return Err(ApiError::missing("Project not found"));
            }
            Ok(Json(json!({"id": id, "name": name})))
        })
        .await
}

fn idle_project_lock(
    app: &AppState,
    key: &str,
    id: &str,
) -> ApiResult<Option<tokio::sync::OwnedMutexGuard<Option<Session>>>> {
    if !app.shared_projects {
        return Ok(None);
    }
    // The current project is already locked. Never wait with two project
    // locks, since another browser could be archiving in the opposite order.
    let guard = app
        .session(key, id)
        .try_lock_owned()
        .map_err(|_| ApiError::conflict("The project is in use. Try again."))?;
    if let Some(session) = guard.as_ref()
        && (session.calculations_active()
            || session.inspection_active()
            || session.upload.available_permits() == 0)
    {
        return Err(ApiError::conflict(
            "Wait for the project's active work to finish",
        ));
    }
    Ok(Some(guard))
}

async fn archive(
    State(app): State<AppState>,
    workspace: Workspace,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    workspace
        .run(app, move |app, key, session| {
            ensure_idle(session)?;
            if id == session.project_id {
                return Err(StoreError::ArchiveActive.into());
            }
            let _target = idle_project_lock(app, key, &id)?;
            if !app.store.archive(key, &id)? {
                return Err(ApiError::missing("Project not found"));
            }
            Ok(Json(json!({"ok": true})))
        })
        .await
}

async fn restore(
    State(app): State<AppState>,
    workspace: Workspace,
    Path(id): Path<String>,
    ObjectBody(body): ObjectBody,
) -> ApiResult<Json<Value>> {
    workspace
        .run_with_selection(app, move |app, key, session, browser| {
            if browser.is_none() {
                ensure_idle(session)?;
            }
            let project = app
                .store
                .restore(key, &id, project_name(&body)?)?
                .ok_or_else(|| ApiError::missing("Archived project not found"))?;
            select_project(app, key, session, browser, project)
        })
        .await
}

async fn delete(
    State(app): State<AppState>,
    workspace: Workspace,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    workspace
        .run(app, move |app, key, session| {
            if id == session.project_id {
                return Err(StoreError::DeleteActive.into());
            }
            let _target = idle_project_lock(app, key, &id)?;
            let target = app
                .store
                .get(key, &id)?
                .ok_or_else(|| ApiError::missing("Project not found"))?;
            if target.directory == "." {
                return Err(StoreError::DeleteWorkspaceRoot.into());
            }
            if app.store.delete(key, &id)?.is_none() {
                return Err(ApiError::missing("Project not found"));
            }
            Ok(Json(json!({"ok": true})))
        })
        .await
}

async fn export(
    State(app): State<AppState>,
    workspace: Workspace,
    Path(kind): Path<String>,
) -> ApiResult<Response> {
    if kind != "project" {
        return crate::downloads::route(app, workspace, &kind).await;
    }
    workspace
        .run(app, |app, key, session| {
            let project = active_project(&app.store, key, session)?;
            if project.plan.as_object().is_none_or(Map::is_empty) {
                return Err(ApiError::missing("Run a plan first"));
            }
            if !app.store.sync_plan_mirror(key, &project.id)? {
                return Err(ApiError::missing("Active project is no longer available"));
            }
            let plan = serde_json::to_vec_pretty(&project.plan).map_err(ApiError::internal)?;
            Ok((
                [
                    (header::CONTENT_TYPE, "application/json"),
                    (
                        header::CONTENT_DISPOSITION,
                        "attachment; filename=\"route.webplan.json\"",
                    ),
                ],
                plan,
            )
                .into_response())
        })
        .await
}

async fn result(State(app): State<AppState>, workspace: Workspace) -> ApiResult<Json<Value>> {
    workspace
        .run(app, |app, key, session| {
            route_http::current(app, key, session).cloned().map(Json)
        })
        .await
}

async fn save_targets(
    State(app): State<AppState>,
    workspace: Workspace,
    ObjectBody(body): ObjectBody,
) -> ApiResult<Json<Value>> {
    workspace.run(app, move |app, key, session| {
        let targets = targets::normalize(&Value::Object(body)).map_err(ApiError::invalid)?;
        if session.calculations_active() {
            return Err(ApiError::conflict("Wait for active calculations before editing coverage targets"));
        }
        let mut project = active_project(&app.store, key, session)?;
        project.plan["coverage_targets"] = targets.clone();
        save_plan(&app.store, key, &project.id, &project.plan)?;
        Ok(Json(json!({"ok": true, "count": targets["features"].as_array().map_or(0, Vec::len), "targets": targets,
            "plan_revision": catalog::plan_fingerprint(&project.plan, false)})))
    }).await
}

async fn import_targets(
    State(app): State<AppState>,
    workspace: Workspace,
    multipart: Result<Multipart, axum::extract::multipart::MultipartRejection>,
) -> ApiResult<Json<Value>> {
    let mut form = multipart.map_err(|error| ApiError::invalid(error.body_text()))?;
    while let Some(mut field) = form
        .next_field()
        .await
        .map_err(|error| ApiError::invalid(error.body_text()))?
    {
        if field.name() != Some("file") || field.file_name().is_none() {
            continue;
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = field
            .chunk()
            .await
            .map_err(|error| ApiError::invalid(error.body_text()))?
        {
            if bytes.len() + chunk.len() > targets::IMPORT_LIMIT {
                return Err(ApiError::invalid("GeoJSON import exceeds the 5 MiB limit"));
            }
            bytes.extend_from_slice(&chunk);
        }
        return workspace
            .run(app, move |_, _, _| {
                let collection = serde_json::from_slice(&bytes)
                    .map_err(|error| ApiError::invalid(error.to_string()))?;
                Ok(Json(
                    targets::normalize(&collection).map_err(ApiError::invalid)?,
                ))
            })
            .await;
    }
    Err(ApiError::invalid("Field required: file"))
}
