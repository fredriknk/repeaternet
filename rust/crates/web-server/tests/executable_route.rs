use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpStream},
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use repeaternet_terrain::{Point, TerrainCrs, Transformer};
use serde_json::{Value, json};

struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Server {
    fn start(root: &Path) -> (Self, SocketAddr) {
        let child = Command::new(env!("CARGO_BIN_EXE_repeaternet"))
            .args(["--no-browser", "--port", "0", "--data"])
            .arg(root)
            .env("RF_PLANNER_TOKEN", "smoke-test")
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut server = Self(child);
        let mut stdout = BufReader::new(server.0.stdout.take().unwrap());
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        let address = line
            .trim()
            .split_once("http://")
            .unwrap_or_else(|| panic!("No listening address in {line:?}"))
            .1
            .trim_end_matches('/')
            .parse()
            .unwrap();
        (server, address)
    }
}

struct Response {
    status: u16,
    headers: String,
    body: Vec<u8>,
}

impl Response {
    fn json(self) -> Value {
        assert_eq!(self.status, 200, "{}", String::from_utf8_lossy(&self.body));
        serde_json::from_slice(&self.body).unwrap()
    }

    fn cookie(&self) -> &str {
        self.headers
            .lines()
            .find_map(|line| line.strip_prefix("set-cookie: "))
            .unwrap()
            .split(';')
            .next()
            .unwrap()
    }
}

fn send(
    address: SocketAddr,
    method: &str,
    path: &str,
    cookie: &str,
    media_type: &str,
    body: &[u8],
) -> Response {
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(5)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\nCookie: {cookie}\r\nContent-Type: {media_type}\r\nContent-Length: {}\r\n\r\n",
        body.len()
    ).unwrap();
    stream.write_all(body).unwrap();
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).unwrap();
    let split = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap();
    let headers = String::from_utf8(bytes[..split].to_vec()).unwrap();
    let status = headers.split_whitespace().nth(1).unwrap().parse().unwrap();
    Response {
        status,
        headers,
        body: bytes.split_off(split + 4),
    }
}

#[test]
fn binary_serves_an_uploaded_terrain_route_and_restores_its_snapshot() {
    let root = tempfile::tempdir().unwrap();
    let (server, address) = Server::start(root.path());
    assert_eq!(
        send(address, "GET", "/api/state", "", "application/json", &[]).status,
        401
    );
    let login = send(
        address,
        "POST",
        "/login",
        "",
        "application/x-www-form-urlencoded",
        b"token=smoke-test",
    );
    assert_eq!(login.status, 303);
    let access = login.cookie();
    let index = send(address, "GET", "/", access, "text/html", &[]);
    assert_eq!(index.status, 200);
    let cookie = format!("{access}; {}", index.cookie());
    let get = |path: &str| send(address, "GET", path, &cookie, "application/json", &[]);
    assert_eq!(get("/api/result").status, 404);

    let mut upload = b"--smoke\r\nContent-Disposition: form-data; name=\"file\"; filename=\"terrain.tif\"\r\n\r\n".to_vec();
    upload.extend(include_bytes!("../../terrain/tests/fixtures/fallback.tif"));
    upload.extend(b"\r\n--smoke--\r\n");
    assert_eq!(
        send(
            address,
            "POST",
            "/api/terrain/dtm",
            &cookie,
            "multipart/form-data; boundary=smoke",
            &upload
        )
        .status,
        200
    );
    let transform = Transformer::new(TerrainCrs::from_epsg(25833).unwrap()).unwrap();
    let coord = |x| {
        let coordinate = transform.reverse(Point { x, y: 6650850.0 }).unwrap();
        [coordinate.latitude, coordinate.longitude]
    };
    let plan = json!({
        "a": coord(500050.0), "b": coord(500290.0),
        "candidates": {"infrastructure_policy": "existing_only", "refine_radius_m": 0, "parallel_workers": 2},
        "coverage": {"include_endpoints": true, "area_buffer_m": 20.0, "cell_size_m": 50.0, "maximum_cells": 24},
    })
    .to_string();
    let start = send(
        address,
        "POST",
        "/api/optimize",
        &cookie,
        "application/json",
        plan.as_bytes(),
    )
    .json();
    let wait_until = Instant::now() + Duration::from_secs(10);
    let complete = loop {
        let state = get("/api/state").json();
        if !matches!(state["job"]["state"].as_str(), Some("queued" | "running")) {
            break state;
        }
        assert!(Instant::now() < wait_until, "{state}");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(complete["job"]["state"], "complete", "{complete}");
    let route = get("/api/result").json();
    assert_eq!(route["job_id"], start["job_id"]);
    assert_eq!(route["found"], true);
    assert!(
        route["links"][0]["profile"]["distances_m"]
            .as_array()
            .unwrap()
            .len()
            > 3
    );
    assert_eq!(
        get("/api/export/geojson").json()["type"],
        "FeatureCollection"
    );
    let coverage_request = json!({
        "snapshot_version": route["snapshot_version"],
        "alternative_id": route["active_alternative_id"],
        "settings": {"include_endpoints": true, "area_buffer_m": 20.0, "cell_size_m": 50.0,
            "maximum_cells": 24}
    });
    let coverage = |path, body: &serde_json::Value| {
        send(
            address,
            "POST",
            path,
            &cookie,
            "application/json",
            body.to_string().as_bytes(),
        )
    };
    let estimate = coverage("/api/coverage/estimate", &coverage_request).json();
    assert_eq!(estimate["source_count"], 2, "{estimate}");
    let preview = coverage("/api/coverage/preview", &coverage_request).json();
    assert_eq!(preview["preview"], true, "{preview}");
    assert!(
        preview["cells"]
            .as_array()
            .is_some_and(|cells| !cells.is_empty() && cells.len() <= 24)
    );
    assert_eq!(get("/api/coverage/jobs").json()["jobs"], json!([]));
    let started = coverage("/api/coverage/jobs", &coverage_request).json();
    let id = started["job_id"].as_str().unwrap();
    let job_path = format!("/api/coverage/jobs/{id}");
    let wait_until = Instant::now() + Duration::from_secs(10);
    let completed = loop {
        let job = get(&job_path).json();
        if !matches!(job["state"].as_str(), Some("queued" | "running")) {
            break job;
        }
        assert!(Instant::now() < wait_until, "{job}");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(completed["state"], "complete", "{completed}");
    assert_eq!(completed["stale"], false);
    let cells = get(&format!("{job_path}/cells?limit=1")).json();
    assert_eq!(cells["cells"][0]["index"], 0);
    assert_eq!(cells["next_cursor"], 1);
    assert_eq!(cells["stored_cells"], completed["requested_cells"]);
    assert_eq!(
        coverage("/api/coverage/jobs", &coverage_request).json()["reused"],
        true
    );
    let exported = get(&format!("{job_path}/export.json"));
    assert!(
        exported
            .headers
            .contains(&format!("predicted-mesh-coverage-{id}.json"))
    );
    assert_eq!(exported.json()["metadata"]["job_id"], id);
    let report = get("/api/export/html");
    assert_eq!(report.status, 200);
    assert!(String::from_utf8_lossy(&report.body).contains("data:image/png;base64,"));
    let point = coverage(
        "/api/coverage/inspect",
        &json!({
            "latitude": coord(500140.0)[0], "longitude": coord(500140.0)[1], "coverage_job_id": id,
        }),
    )
    .json();
    let point_path = format!(
        "/api/coverage/inspect/{}",
        point["job_id"].as_str().unwrap()
    );
    let wait_until = Instant::now() + Duration::from_secs(10);
    loop {
        let inspection = get(&point_path).json();
        if !matches!(inspection["state"].as_str(), Some("queued" | "running")) {
            assert_eq!(inspection["state"], "complete", "{inspection}");
            assert_eq!(inspection["result"]["state"], "evaluated");
            assert!(inspection["result"]["profile_link"]["profile"].is_object());
            break;
        }
        assert!(Instant::now() < wait_until);
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut stale = coverage_request.clone();
    stale["snapshot_version"] = json!(0);
    assert_eq!(coverage("/api/coverage/preview", &stale).status, 409);

    drop(server);
    let (_restarted, address) = Server::start(root.path());
    let state = send(
        address,
        "GET",
        "/api/state",
        &cookie,
        "application/json",
        &[],
    )
    .json();
    assert_eq!(state["projects"]["previous_result"]["stale"], false);
    assert_eq!(state["projects"]["previous_result"]["found"], true);
    assert_eq!(state["coverage_job"]["job_id"], id);
    assert_eq!(state["coverage_job"]["stale"], false);
    assert_eq!(state["coverage_job"]["route_current"], true);
    let get = |path: &str| send(address, "GET", path, &cookie, "application/json", &[]);
    let response = get("/api/result");
    assert_eq!(response.status, 200);
    let restored = response.json();
    assert_eq!(restored["input_revision"], state["job"]["input_revision"]);
    let mut expected = route;
    expected["input_revision"] = state["job"]["input_revision"].clone();
    assert_eq!(restored, expected);
    assert_eq!(get(&format!("{job_path}/export.json")).status, 200);
    assert_eq!(get(&job_path).json()["stale"], false);
    let rerun = send(
        address,
        "POST",
        "/api/optimize",
        &cookie,
        "application/json",
        plan.as_bytes(),
    )
    .json();
    let wait_until = Instant::now() + Duration::from_secs(10);
    let new_route = loop {
        let state = get("/api/state").json();
        if state["job"]["state"] == "complete" {
            break get("/api/result").json();
        }
        assert!(Instant::now() < wait_until, "{state}");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_ne!(rerun["job_id"], start["job_id"]);
    let mut coverage_request = coverage_request;
    coverage_request["snapshot_version"] = new_route["snapshot_version"].clone();
    let reused = send(
        address,
        "POST",
        "/api/coverage/jobs",
        &cookie,
        "application/json",
        coverage_request.to_string().as_bytes(),
    )
    .json();
    assert_eq!(reused["job_id"], id, "{reused}");
    assert_eq!(reused["reused"], true);
}
