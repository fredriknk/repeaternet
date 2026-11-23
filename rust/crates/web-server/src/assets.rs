use axum::{
    body::Body,
    http::header,
    response::{IntoResponse, Response},
};

macro_rules! dashboard {
    ($name:literal) => {
        include_bytes!(concat!("../assets/dashboard/", $name))
    };
}

macro_rules! leaflet {
    ($name:literal) => {
        include_bytes!(concat!("../assets/leaflet/", $name))
    };
}

const HTML: &str = "text/html; charset=utf-8";
const CSS: &str = "text/css; charset=utf-8";
const JS: &str = "text/javascript; charset=utf-8";
const PNG: &str = "image/png";

fn response(content_type: &'static str, bytes: &'static [u8]) -> Response {
    let mut response = ([(header::CONTENT_TYPE, content_type)], Body::from(bytes)).into_response();
    response
        .headers_mut()
        .insert(header::CONTENT_LENGTH, bytes.len().into());
    response
}

pub fn index() -> Response {
    response(HTML, dashboard!("index.html"))
}

pub fn login() -> Response {
    response(HTML, dashboard!("login.html"))
}

pub fn lookup(path: &str) -> Option<Response> {
    let (content_type, bytes): (&str, &[u8]) = match path {
        "/openapi.json" => ("application/json", include_bytes!("discovery/openapi.json")),
        "/docs" => (HTML, include_bytes!("discovery/docs.html")),
        "/docs/oauth2-redirect" => (HTML, include_bytes!("discovery/oauth2-redirect.html")),
        "/redoc" => (HTML, include_bytes!("discovery/redoc.html")),
        "/assets/index.html" => (HTML, dashboard!("index.html")),
        "/assets/login.html" => (HTML, dashboard!("login.html")),
        "/assets/style.css" => (CSS, dashboard!("style.css")),
        "/assets/meshcore.css" => (CSS, dashboard!("meshcore.css")),
        "/assets/coverage.css" => (CSS, dashboard!("coverage.css")),
        "/assets/workspace.css" => (CSS, dashboard!("workspace.css")),
        "/assets/map.css" => (CSS, dashboard!("map.css")),
        "/assets/main_view_navigation.js" => (JS, dashboard!("main_view_navigation.js")),
        "/assets/app.js" => (JS, dashboard!("app.js")),
        "/assets/site_selection.js" => (JS, dashboard!("site_selection.js")),
        "/assets/mesh_mode.js" => (JS, dashboard!("mesh_mode.js")),
        "/assets/coverage_inspection.js" => (JS, dashboard!("coverage_inspection.js")),
        "/assets/coverage_raster.js" => (JS, dashboard!("coverage_raster.js")),
        "/assets/live_coverage.js" => (JS, dashboard!("live_coverage.js")),
        "/leaflet/leaflet.css" => (CSS, leaflet!("leaflet.css")),
        "/leaflet/leaflet.js" => (JS, leaflet!("leaflet.js")),
        "/leaflet/images/marker-icon-2x.png" => (PNG, leaflet!("images/marker-icon-2x.png")),
        "/leaflet/images/marker-icon.png" => (PNG, leaflet!("images/marker-icon.png")),
        "/leaflet/images/marker-shadow.png" => (PNG, leaflet!("images/marker-shadow.png")),
        _ => return None,
    };
    Some(response(content_type, bytes))
}
