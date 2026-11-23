use std::net::IpAddr;

use axum::{
    Extension, Router,
    body::Body,
    extract::{DefaultBodyLimit, Request},
    http::{HeaderMap, Method, StatusCode},
    middleware,
    response::Response,
    routing::{get, post},
};
use repeaternet_project_store::ProjectStore;

use crate::{api, assets, catalog::AppState, security};

#[cfg(test)]
#[path = "project_http_tests.rs"]
mod project_http_tests;
#[cfg(test)]
#[path = "route_http_tests.rs"]
mod route_http_tests;

pub fn router(
    store: ProjectStore,
    data_directory: &std::path::Path,
    token: &str,
    loopback_only: bool,
    trusted_proxy: Option<IpAddr>,
) -> Router {
    build_router(
        AppState::new(store, data_directory),
        token,
        loopback_only,
        trusted_proxy,
    )
}

#[cfg(test)]
fn router_with_state(app: AppState, token: &str, loopback_only: bool) -> Router {
    build_router(app, token, loopback_only, None)
}

fn build_router(
    mut app: AppState,
    token: &str,
    loopback_only: bool,
    trusted_proxy: Option<IpAddr>,
) -> Router {
    app.shared_projects = !token.is_empty();
    let security = security::Security::new(token, loopback_only, trusted_proxy);
    Router::new()
        .route("/", get(index))
        .route("/health", get(api::health))
        .route(
            "/login",
            post(security::login)
                .with_state(security.clone())
                .layer(DefaultBodyLimit::max(16 * 1024)),
        )
        .merge(api::router(app))
        .fallback(fallback)
        .layer(middleware::from_fn_with_state(security, security::enforce))
}

async fn index(headers: HeaderMap, Extension(https): Extension<security::Https>) -> Response {
    security::initialize_workspace(assets::index(), &headers, https)
}

async fn fallback(request: Request) -> Response {
    let path = request.uri().path();
    if let Some(mut response) = assets::lookup(path) {
        return match *request.method() {
            Method::GET => response,
            Method::HEAD => {
                *response.body_mut() = Body::empty();
                response
            }
            _ => api::problem(StatusCode::METHOD_NOT_ALLOWED, "Method not allowed"),
        };
    }
    api::problem(StatusCode::NOT_FOUND, "Not found")
}

#[cfg(test)]
mod tests {
    use axum::{
        body::{Body, to_bytes},
        http::header,
    };
    use serde_json::Value;
    use tower::ServiceExt;

    use super::*;

    fn test_router(data: &std::path::Path, token: &str) -> Router {
        router(
            ProjectStore::new(data.join("projects.sqlite3"), data).unwrap(),
            data,
            token,
            true,
            None,
        )
    }

    fn request(method: Method, uri: &str) -> axum::http::request::Builder {
        Request::builder()
            .method(method)
            .uri(uri)
            .header(header::HOST, "127.0.0.1:8000")
    }

    fn set_cookie(headers: &HeaderMap) -> &str {
        let value = headers[header::SET_COOKIE].to_str().unwrap();
        assert!(value.contains("; HttpOnly; SameSite=Strict"));
        value.split(';').next().unwrap()
    }

    #[tokio::test]
    async fn token_and_workspace_protect_dashboard_api() {
        let data = tempfile::tempdir().unwrap();
        let app = test_router(data.path(), "shared key");
        let response = app
            .clone()
            .oneshot(
                request(Method::GET, "/api/state")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(response.headers()[header::X_FRAME_OPTIONS], "DENY");

        for (origin, form, expected, remember) in [
            (
                "https://other.example",
                "token=shared+key&remember=1",
                StatusCode::FORBIDDEN,
                false,
            ),
            (
                "http://127.0.0.1:8000",
                "token=wrong&remember=1",
                StatusCode::UNAUTHORIZED,
                false,
            ),
            (
                "http://127.0.0.1:8000",
                "token=shared+key",
                StatusCode::SEE_OTHER,
                false,
            ),
            (
                "http://127.0.0.1:8000",
                "token=shared+key&remember=1",
                StatusCode::SEE_OTHER,
                true,
            ),
        ] {
            let response = app
                .clone()
                .oneshot(
                    request(Method::POST, "/login")
                        .header(header::ORIGIN, origin)
                        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                        .body(Body::from(form))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), expected);
            if expected != StatusCode::SEE_OTHER {
                assert!(!response.headers().contains_key(header::SET_COOKIE));
                continue;
            }
            let access = set_cookie(response.headers());
            let attributes = response.headers()[header::SET_COOKIE].to_str().unwrap();
            if remember {
                assert!(attributes.contains("; Max-Age=2592000"));
            } else {
                assert!(!attributes.contains("; Max-Age="));
            }
            let dashboard = app
                .clone()
                .oneshot(
                    request(Method::GET, "/")
                        .header(header::COOKIE, access)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(dashboard.status(), StatusCode::OK);
            let cookies = format!("{access}; {}", set_cookie(dashboard.headers()));
            let response = app
                .clone()
                .oneshot(
                    request(Method::GET, "/api/state")
                        .header(header::COOKIE, cookies)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let data: Value =
                serde_json::from_slice(&to_bytes(response.into_body(), 16 * 1024).await.unwrap())
                    .unwrap();
            assert_eq!(data["rf"]["frequency_mhz"], 869.5);
            assert_eq!(
                data["projects"]["active_id"],
                data["projects"]["items"][0]["id"]
            );
            assert_eq!(data["plan"], Value::Null);
        }
    }

    #[tokio::test]
    async fn multipart_login_accepts_remember_option() {
        let data = tempfile::tempdir().unwrap();
        let app = test_router(data.path(), "shared-key");
        let form = concat!(
            "--login\r\nContent-Disposition: form-data; name=\"remember\"\r\n\r\n1\r\n",
            "--login\r\nContent-Disposition: form-data; name=\"token\"\r\n\r\nshared-key\r\n",
            "--login--\r\n",
        );
        let response = app
            .oneshot(
                request(Method::POST, "/login")
                    .header(header::CONTENT_TYPE, "multipart/form-data; boundary=login")
                    .body(Body::from(form))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert!(
            response.headers()[header::SET_COOKIE]
                .to_str()
                .unwrap()
                .contains("; Max-Age=2592000")
        );
    }

    #[tokio::test]
    async fn loopback_listener_rejects_an_external_host() {
        let data = tempfile::tempdir().unwrap();
        let app = test_router(data.path(), "");
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .header(header::HOST, "external.example")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn embedded_api_discovery_requires_token_and_lists_coverage_operations() {
        let data = tempfile::tempdir().unwrap();
        let app = test_router(data.path(), "secret");
        let gated = app
            .clone()
            .oneshot(
                request(Method::GET, "/openapi.json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            gated.headers()[header::CONTENT_TYPE],
            "text/html; charset=utf-8"
        );
        for (path, expected) in [
            ("/openapi.json", "application/json"),
            ("/docs", "text/html; charset=utf-8"),
            ("/docs/oauth2-redirect", "text/html; charset=utf-8"),
            ("/redoc", "text/html; charset=utf-8"),
        ] {
            let response = app
                .clone()
                .oneshot(
                    request(Method::GET, path)
                        .header(header::COOKIE, "planner_access=secret")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.headers()[header::CONTENT_TYPE], expected);
            let body = to_bytes(response.into_body(), 128 * 1024).await.unwrap();
            if path == "/openapi.json" {
                let spec: Value = serde_json::from_slice(&body).unwrap();
                assert!(
                    spec["paths"]["/api/coverage/jobs/{job_id}/report.html"]["get"].is_object()
                );
                assert!(spec["paths"]["/api/coverage/inspect"]["post"].is_object());
            }
        }
    }

    #[tokio::test]
    async fn trusted_proxy_https_sign_in_autosave_and_spoofing() {
        use axum::extract::ConnectInfo;
        use std::net::SocketAddr;

        let data = tempfile::tempdir().unwrap();
        let store = ProjectStore::new(data.path().join("projects.sqlite3"), data.path()).unwrap();
        let trusted: SocketAddr = "127.0.0.2:5000".parse().unwrap();
        let direct: SocketAddr = "127.0.0.1:5000".parse().unwrap();
        let app = router(store, data.path(), "proxy-token", true, Some(trusted.ip()));
        let forwarded = |method, path, peer| {
            request(method, path)
                .extension(ConnectInfo(peer))
                .header("x-forwarded-host", "planner.example:8443")
                .header("x-forwarded-proto", "https")
        };
        let login = |peer, origin| {
            forwarded(Method::POST, "/login", peer)
                .header(header::ORIGIN, origin)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from("token=proxy-token"))
                .unwrap()
        };
        let public_origin = "https://planner.example:8443";
        let response = app
            .clone()
            .oneshot(login(direct, public_origin))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let response = app
            .clone()
            .oneshot(login(direct, "http://127.0.0.1:8000"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert!(
            !response.headers()[header::SET_COOKIE]
                .to_str()
                .unwrap()
                .contains("; Secure")
        );
        let response = app
            .clone()
            .oneshot(login(trusted, public_origin))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert!(
            response.headers()[header::SET_COOKIE]
                .to_str()
                .unwrap()
                .contains("; Secure")
        );
        let access = set_cookie(response.headers());
        let index = app
            .clone()
            .oneshot(
                forwarded(Method::GET, "/", trusted)
                    .header(header::COOKIE, access)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(index.status(), StatusCode::OK);
        assert!(
            index.headers()[header::SET_COOKIE]
                .to_str()
                .unwrap()
                .contains("; Secure")
        );
        let cookie = format!("{access}; {}", set_cookie(index.headers()));
        let state = app
            .clone()
            .oneshot(
                forwarded(Method::GET, "/api/state", trusted)
                    .header(header::COOKIE, &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let state: Value =
            serde_json::from_slice(&to_bytes(state.into_body(), 16 * 1024).await.unwrap()).unwrap();
        for (origin, expected) in [
            (public_origin, StatusCode::OK),
            ("https://elsewhere.example", StatusCode::FORBIDDEN),
        ] {
            let response = app
                .clone()
                .oneshot(
                    forwarded(Method::POST, "/api/projects/autosave", trusted)
                        .header(header::ORIGIN, origin)
                        .header(header::COOKIE, &cookie)
                        .header(
                            "x-repeaternet-project",
                            state["projects"]["active_id"].as_str().unwrap(),
                        )
                        .header(
                            "x-repeaternet-plan-revision",
                            state["projects"]["plan_revision"].as_str().unwrap(),
                        )
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(r#"{"plan":{"a":[59,10]}}"#))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), expected);
        }
        let invalid = app
            .clone()
            .oneshot(
                forwarded(Method::GET, "/health", trusted)
                    .header("x-forwarded-host", "evil.example")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
        let invalid = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .extension(ConnectInfo(direct))
                    .header(header::HOST, "evil.example")
                    .header("X-Forwarded-Host", "localhost")
                    .header("X-Forwarded-Proto", "https")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
    }
}
