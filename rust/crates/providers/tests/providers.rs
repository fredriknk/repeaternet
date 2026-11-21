use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

use chrono::{TimeZone, Utc};
use geo::{MultiPolygon, Polygon, Rect};
use repeaternet_providers::{
    corescope::{filter_corridor, CoreScopeClient},
    kartverket::{
        DownloadArea, DownloadTiming, KartverketProvider, PlanOptions, Product, StagedGeneration,
        TileLimits, WcsConfig, WcsService,
    },
    roads::{tiles_for_points, RoadAccessClasses, RoadAccessIndex, RoadTile, TiledRoadAccess},
    ProviderError,
};
use repeaternet_terrain::{Bounds, LonLat, Point, TerrainCrs, Transformer};
use reqwest::blocking::Client;
use serde_json::{json, Value};
use tempfile::tempdir;
use url::Url;

const CRS: TerrainCrs = TerrainCrs::Etrs89Utm33;
const WEST_TIF: &[u8] = include_bytes!("fixtures/tile-west.tif");
const EAST_TIF: &[u8] = include_bytes!("fixtures/tile-east.tif");
const EMPTY_TIF: &[u8] = include_bytes!("fixtures/tile-empty.tif");
const WRONG_CRS_TIF: &[u8] = include_bytes!("fixtures/tile-wrong-crs.tif");
const WEST: Bounds = Bounds {
    min_x: 500_000.0,
    min_y: 6_650_000.0,
    max_x: 500_100.0,
    max_y: 6_650_100.0,
};
const EAST: Bounds = Bounds {
    min_x: 500_100.0,
    max_x: 500_200.0,
    ..WEST
};
const MANUAL: PlanOptions = PlanOptions {
    auto_resolution: false,
    maximum_total_pixels: 50_000_000,
};

struct Reply {
    status: u16,
    body: Vec<u8>,
    retry_after: Option<&'static str>,
}
impl Reply {
    fn new(status: u16, bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            body: bytes.into(),
            retry_after: None,
        }
    }

    fn json(value: Value) -> Self {
        Self::new(200, serde_json::to_vec(&value).unwrap())
    }

    fn retry(status: u16, seconds: &'static str) -> Self {
        Self {
            status,
            body: vec![],
            retry_after: Some(seconds),
        }
    }
}

#[derive(Debug)]
struct Request {
    method: String,
    path: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
    at: Instant,
}

struct Mock {
    base: String,
    worker: thread::JoinHandle<Vec<Request>>,
}

impl Mock {
    fn start(replies: Vec<Reply>) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let worker = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut requests = Vec::new();
            for reply in replies {
                let connection = loop {
                    match listener.accept() {
                        Ok((connection, _)) => break connection,
                        Err(error)
                            if error.kind() == std::io::ErrorKind::WouldBlock
                                && Instant::now() < deadline =>
                        {
                            thread::sleep(Duration::from_millis(2));
                        }
                        Err(error) => panic!("waiting for request after {requests:?}: {error}"),
                    }
                };
                requests.push(serve(connection, reply));
            }
            requests
        });
        Self { base, worker }
    }

    fn finish(self) -> Vec<Request> {
        self.worker.join().unwrap()
    }
}

fn serve(mut connection: TcpStream, reply: Reply) -> Request {
    connection.set_nonblocking(false).unwrap();
    connection
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut reader = BufReader::new(&mut connection);
    let mut current = String::new();
    reader.read_line(&mut current).unwrap();
    let mut first = current.split_whitespace();
    let method = first.next().unwrap().to_owned();
    let path = first.next().unwrap().to_owned();
    let mut headers = BTreeMap::new();
    loop {
        current.clear();
        assert_ne!(reader.read_line(&mut current).unwrap(), 0);
        if current == "\r\n" {
            break;
        }
        let (key, value) = current.split_once(':').unwrap();
        headers.insert(key.to_ascii_lowercase(), value.trim().to_owned());
    }
    let mut body = vec![
        0;
        headers
            .get("content-length")
            .map_or(0, |value| value.parse().unwrap())
    ];
    reader.read_exact(&mut body).unwrap();
    let at = Instant::now();
    write!(
        connection,
        "HTTP/1.1 {} Mock\r\nContent-Length: {}\r\nConnection: close\r\n",
        reply.status,
        reply.body.len()
    )
    .unwrap();
    if let Some(seconds) = reply.retry_after {
        write!(connection, "Retry-After: {seconds}\r\n").unwrap();
    }
    connection.write_all(b"\r\n").unwrap();
    let _ = connection.write_all(&reply.body);
    Request {
        method,
        path,
        headers,
        body,
        at,
    }
}

fn client() -> Client {
    let _ = rustls::crypto::ring::default_provider().install_default();
    Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap()
}

fn provider(directory: &Path, base: &str, limits: TileLimits) -> KartverketProvider {
    let config = WcsConfig {
        services: BTreeMap::from([(
            CRS.to_string(),
            WcsService {
                dtm_endpoint: format!("{base}/dtm"),
                dom_endpoint: format!("{base}/dom"),
                dtm_coverage: "dtm".into(),
                dom_coverage: "dom".into(),
            },
        )]),
    };
    KartverketProvider::with_client(directory, config, limits, client())
        .unwrap()
        .with_timing(DownloadTiming {
            retry_base: Duration::ZERO,
            retry_cap: Duration::ZERO,
            inter_tile_pause: Duration::ZERO,
        })
}

fn bounds(a: f64, b: f64, c: f64, d: f64) -> Bounds {
    Bounds {
        min_x: a,
        min_y: b,
        max_x: c,
        max_y: d,
    }
}

fn pt(x: f64, y: f64) -> Point {
    Point { x, y }
}

fn ll(longitude: f64, latitude: f64) -> LonLat {
    LonLat {
        longitude,
        latitude,
    }
}

#[test]
fn wcs_alignment_corridor_budget_holes_and_defaults_match_planner() {
    let tmp = tempdir().unwrap();
    let wcs = provider(tmp.path(), "https://example.test", TileLimits::default());
    assert_eq!(
        WcsConfig::default()
            .services
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["EPSG:25832", "EPSG:25833", "EPSG:25835"]
    );
    let corridor = DownloadArea::Corridor {
        points: &[pt(0., 10_000.), pt(100_000., 10_000.)],
        half_width_m: 10_000.,
        extra_points: &[],
    };
    let plan = wcs
        .plan_download(bounds(0., 0., 100_000., 20_000.), 10., corridor, MANUAL)
        .unwrap();
    assert_eq!(plan.tiles.len(), 5);
    assert_eq!(plan.pixel_count(), 20_000_000);
    assert_eq!(plan.download_area_km2(), 2_000.0);
    assert_eq!(plan.tiles[4].bounds, bounds(80_000., 0., 100_000., 20_000.));

    let outer = Rect::new((0., 0.), (60_000., 60_000.)).to_polygon();
    let hole = Rect::new((20_000., 20_000.), (40_000., 40_000.)).to_polygon();
    let area = MultiPolygon::new(vec![Polygon::new(
        outer.exterior().clone(),
        vec![hole.exterior().clone()],
    )]);
    let plan = wcs
        .plan_download(
            bounds(0., 0., 60_000., 60_000.),
            10.,
            DownloadArea::Geometry(&area),
            PlanOptions {
                maximum_total_pixels: 6_000_000,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(plan.tiles.len(), 8);
    assert_eq!(plan.effective_resolution_m, 25.);
    assert_eq!(plan.pixel_count(), 5_120_000);
    assert!(!plan
        .tiles
        .iter()
        .any(|tile| tile.bounds == bounds(20_000., 20_000., 40_000., 40_000.)));

    let bent = DownloadArea::Corridor {
        points: &[pt(0., 10_000.), pt(80_000., 10_000.), pt(80_000., 80_000.)],
        half_width_m: 2_000.,
        extra_points: &[pt(10_000., 70_000.)],
    };
    let plan = wcs
        .plan_download(bounds(0., 0., 100_000., 100_000.), 25., bent, MANUAL)
        .unwrap();
    assert!(plan.tiles.len() < 15);
    assert!(plan
        .tiles
        .iter()
        .any(|tile| tile.bounds.contains(pt(10_000., 70_000.))));
    assert!(!plan
        .tiles
        .iter()
        .any(|tile| tile.bounds.contains(pt(30_000., 50_000.))));

    let limited = provider(
        tmp.path(),
        "https://example.test",
        TileLimits {
            maximum_tiles: 10,
            ..Default::default()
        },
    );
    let error = limited
        .plan_download(bounds(0., 0., 100_000., 20_000.), 1., corridor, MANUAL)
        .unwrap_err();
    assert!(error.to_string().contains("Enable automatic resolution"));
}

#[test]
fn wcs_coverage_extension_selects_each_missing_product_independently() {
    let tmp = tempdir().unwrap();
    let wcs = provider(tmp.path(), "https://example.test", TileLimits::default());
    let west = MultiPolygon::new(vec![Rect::new((0., 0.), (20_000., 20_000.)).to_polygon()]);
    let east = MultiPolygon::new(vec![
        Rect::new((20_000., 0.), (40_000., 20_000.)).to_polygon()
    ]);
    let extension = wcs
        .plan_coverage_extension(CRS, &west, Some(&east), 10.)
        .unwrap();
    assert_eq!(extension.estimate.missing_area_km2, 800.);
    assert_eq!(extension.estimate.tile_count, 2);
    assert_eq!(extension.estimate.cache_total, 2);
    assert_eq!(extension.estimate.cache_hits, 0);
    let dtm = extension.dtm.unwrap();
    let dom = extension.dom.unwrap();
    assert_eq!(dtm.tiles.len(), 1);
    assert_eq!(dom.tiles.len(), 1);
    assert_eq!(dtm.tiles[0].bounds, bounds(0., 0., 20_000., 20_000.));
    assert_eq!(dom.tiles[0].bounds, bounds(20_000., 0., 40_000., 20_000.));

    let empty = MultiPolygon::new(vec![]);
    let extension = wcs
        .plan_coverage_extension(CRS, &empty, Some(&empty), 10.)
        .unwrap();
    assert!(extension.dtm.is_none() && extension.dom.is_none());
    assert!(extension.estimate.include_dom);
    assert_eq!(extension.estimate.cache_total, 0);
}

#[test]
fn wcs_python_cache_keys_and_offline_estimates() {
    let tmp = tempdir().unwrap();
    let wcs = provider(tmp.path(), "https://example.test", TileLimits::default());
    let raster = wcs
        .cache_path(Product::Dtm, CRS, bounds(0., 0., 1000., 1000.), 10.)
        .unwrap();
    assert_eq!(
        raster.file_name().unwrap(),
        "kartverket_dtm_1937652cfbd858fe1189.tif"
    );
    let legacy = tmp.path().join("kartverket_dtm_057501c6105297a7f46a.tif");
    fs::write(&legacy, WEST_TIF).unwrap();
    assert_eq!(
        wcs.fetch(Product::Dtm, CRS, WEST, 10., &|| false, |_, _| panic!(
            "network"
        ))
        .unwrap(),
        legacy
    );
    let tiny = provider(
        tmp.path(),
        "https://example.test",
        TileLimits {
            maximum_pixels_per_tile: 100,
            ..Default::default()
        },
    );
    let plan = tiny
        .plan_download(WEST, 10., DownloadArea::Bounds, MANUAL)
        .unwrap();
    let estimate = tiny.estimate(CRS, &plan, true, tmp.path()).unwrap();
    assert_eq!(
        (
            estimate.cache_hits,
            estimate.cache_total,
            estimate.tile_count,
            estimate.pixel_count
        ),
        (1, 2, 1, 100)
    );
    assert_eq!(estimate.estimated_raw_mib, 800. / 1_048_576.);
    assert_eq!(estimate.estimated_disk_mib, estimate.estimated_raw_mib * 2.);
    assert!(estimate.available_disk_mib > 0.);
    assert!(estimate
        .summary
        .starts_with("1 tile(s), 0 km² downloaded, 10 m"));
}

#[test]
fn wcs_mock_request_retry_courtesy_pause_and_atomic_generation() {
    let tmp = tempdir().unwrap();
    let mock = Mock::start(vec![
        Reply::retry(503, "2"),
        Reply::new(200, WEST_TIF),
        Reply::new(200, EAST_TIF),
    ]);
    let limits = TileLimits {
        maximum_pixels_per_tile: 100,
        ..Default::default()
    };
    let wcs = provider(tmp.path(), &mock.base, limits).with_timing(DownloadTiming {
        retry_base: Duration::ZERO,
        retry_cap: Duration::from_millis(60),
        inter_tile_pause: Duration::from_millis(55),
    });
    let region = Bounds {
        max_x: EAST.max_x,
        ..WEST
    };
    let plan = wcs
        .plan_download(region, 10., DownloadArea::Bounds, MANUAL)
        .unwrap();
    let mut events = Vec::new();
    let sources = wcs
        .fetch_plan(Product::Dtm, CRS, &plan, &|| false, |a, b, c| {
            events.push((a, b, c))
        })
        .unwrap();
    assert_eq!(events, [(1, 2, false), (2, 2, false)]);
    assert_eq!(fs::read(&sources[0]).unwrap(), WEST_TIF);
    assert_eq!(fs::read(&sources[1]).unwrap(), EAST_TIF);
    let base = mock.base.clone();
    let requests = mock.finish();
    assert!(requests[1].at.duration_since(requests[0].at) >= Duration::from_millis(50));
    assert!(requests[2].at.duration_since(requests[1].at) >= Duration::from_millis(45));
    let first = Url::parse(&format!("{base}{}", requests[0].path)).unwrap();
    assert_eq!(requests[0].method, "GET");
    assert_eq!(first.path(), "/dtm");
    assert!(requests[0].headers["user-agent"].contains("Kartverket terrain client"));
    let params: BTreeMap<_, _> = first.query_pairs().into_owned().collect();
    assert_eq!(
        params,
        BTreeMap::from([
            (
                "bbox".into(),
                "500000.0,6650000.0,500100.0,6650100.0".into()
            ),
            ("coverage".into(), "dtm".into()),
            ("crs".into(), "EPSG:25833".into()),
            ("format".into(), "GeoTIFF".into()),
            ("height".into(), "10".into()),
            ("request".into(), "GetCoverage".into()),
            ("service".into(), "WCS".into()),
            ("version".into(), "1.0.0".into()),
            ("width".into(), "10".into()),
        ])
    );
    let again = provider(tmp.path(), &base, limits);
    assert_eq!(
        again
            .fetch_plan(Product::Dtm, CRS, &plan, &|| false, |_, _, cached| assert!(
                cached
            ))
            .unwrap(),
        sources
    );

    // A staged generation cannot appear under its final name until the complete rename.
    let root = tmp.path().join("project");
    let stage =
        StagedGeneration::copy_from_cache(&root, &sources, &sources[..1], &|| false).unwrap();
    assert!(!root.join("generation-1").exists());
    assert!(stage.path().join("dom/tile-0000.tif").exists());
    assert!(matches!(
        stage.publish("generation-1", &|| true),
        Err(ProviderError::Cancelled)
    ));
    assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
    let published = StagedGeneration::copy_from_cache(&root, &sources, &sources[..1], &|| false)
        .unwrap()
        .publish("generation-1", &|| false)
        .unwrap();
    assert_eq!(
        fs::read(published.join("dtm/tile-0001.tif")).unwrap(),
        EAST_TIF
    );
    assert_eq!(
        fs::read(published.join("dom/tile-0000.tif")).unwrap(),
        WEST_TIF
    );
    for name in ["generation-1", "../outside", "generation-2/"] {
        let stage = StagedGeneration::copy_from_cache(&root, &sources, &[], &|| false).unwrap();
        assert!(matches!(
            stage.publish(name, &|| false),
            Err(ProviderError::InvalidInput(_))
        ));
    }
    assert_eq!(fs::read_dir(&root).unwrap().count(), 1);
    assert_eq!(
        fs::read(published.join("dom/tile-0000.tif")).unwrap(),
        WEST_TIF
    );
}

#[test]
fn wcs_rejects_unreadable_nodata_wrong_crs_bounds_and_resolution_before_publish() {
    let tmp = tempdir().unwrap();
    let mock = Mock::start(
        [
            b"not TIFF".as_slice(),
            EMPTY_TIF,
            WRONG_CRS_TIF,
            EAST_TIF,
            WEST_TIF,
        ]
        .into_iter()
        .map(|bytes| Reply::new(200, bytes))
        .collect(),
    );
    let wcs = provider(tmp.path(), &mock.base, TileLimits::default());
    let error = wcs
        .fetch(Product::Dtm, CRS, WEST, 5., &|| false, |_, _| {})
        .unwrap_err();
    assert!(error.to_string().contains("requested resolution"));
    assert_eq!(mock.finish().len(), 5);
    assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 0);

    let mock = Mock::start(vec![Reply::new(404, b"missing".as_slice())]);
    let wcs = provider(tmp.path(), &mock.base, TileLimits::default());
    assert!(matches!(
        wcs.fetch(Product::Dtm, CRS, WEST, 10., &|| false, |_, _| {}),
        Err(ProviderError::Http { status: 404, .. })
    ));
    assert_eq!(mock.finish().len(), 1);
}

#[test]
fn wcs_cancellation_and_invalid_cache_replacement() {
    let tmp = tempdir().unwrap();
    let mock = Mock::start(vec![Reply::new(200, WEST_TIF), Reply::new(200, WEST_TIF)]);
    let wcs = provider(tmp.path(), &mock.base, TileLimits::default());
    let path = wcs.cache_path(Product::Dtm, CRS, WEST, 10.).unwrap();
    fs::write(&path, b"corrupt old file").unwrap();
    let cancel = AtomicBool::new(false);
    let stopped = || cancel.load(Ordering::Relaxed);
    assert!(matches!(
        wcs.fetch(Product::Dtm, CRS, WEST, 10., &stopped, |written, total| {
            assert_eq!(written, total);
            cancel.store(true, Ordering::Relaxed);
        }),
        Err(ProviderError::Cancelled)
    ));
    assert_eq!(fs::read(&path).unwrap(), b"corrupt old file");
    assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 1);
    assert_eq!(
        wcs.fetch(Product::Dtm, CRS, WEST, 10., &|| false, |_, _| {})
            .unwrap(),
        path
    );
    assert_eq!(fs::read(&path).unwrap(), WEST_TIF);
    assert_eq!(mock.finish().len(), 2);
}

fn roads(directory: &Path, endpoint: &str, classes: RoadAccessClasses) -> TiledRoadAccess {
    TiledRoadAccess::with_client(CRS, classes, 50., directory, endpoint, client())
        .unwrap()
        .with_retry_timing(Duration::ZERO, Duration::ZERO)
}

fn road_way() -> Value {
    json!({"elements": [{"type": "way", "id": 1, "geometry": [
        {"lon": 15.1999, "lat": 60.0}, {"lon": 15.1999, "lat": 60.1}
    ]}]})
}

#[test]
fn roads_query_bounds_cache_key_and_exact_distances() {
    let index = RoadAccessIndex::new(vec![vec![(0., 0.), (2000., 0.)].into()], 1_000.).unwrap();
    assert_eq!(
        index.allowed(&[
            pt(1000., 0.),
            pt(1000., 999.),
            pt(1000., 1000.),
            pt(1000., 1001.),
            pt(2500., 0.),
            pt(2600., 900.)
        ]),
        [true, true, true, false, true, false]
    );
    let tmp = tempdir().unwrap();
    let endpoint = "https://overpass.example/api/interpreter";
    let mut offline = roads(tmp.path(), endpoint, RoadAccessClasses::PublicRoads);
    let tile_a = RoadTile { row: 600, col: 75 };
    let tile_b = RoadTile { row: 600, col: 76 };
    let path_a = tmp.path().join(
        "overpass-tile-10832c9f2e1fd1779509359b54417d336becd6b205cd28e14a43aae963035ba7.json",
    );
    let path_b = tmp.path().join(
        "overpass-tile-3d8dd56bdcde4d85b794e14d2ffe96dd7ba2a8a121975db028598d8223a497e4.json",
    );
    assert_eq!(offline.cache_path(tile_a), path_a);
    assert_eq!(offline.cache_path(tile_b), path_b);
    fs::write(path_a, road_way().to_string()).unwrap();
    fs::write(path_b, json!({"elements": []}).to_string()).unwrap();
    let transform = Transformer::new(CRS).unwrap();
    let point = transform.forward(ll(15.2001, 60.05)).unwrap();
    assert_eq!(
        tiles_for_points(&[point], 50., &transform, &|| false).unwrap(),
        [tile_a, tile_b]
    );
    assert_eq!(
        offline.allowed(&[point], &|| false, |_, _, _| {}).unwrap(),
        [true]
    );
}

#[test]
fn roads_mock_cross_tile_cache_retry_and_shared_clipped_overlay() {
    let tmp = tempdir().unwrap();
    let mock = Mock::start(vec![
        Reply::retry(503, "0"),
        Reply::json(road_way()),
        Reply::json(road_way()),
    ]);
    let endpoint = format!("{}/api/interpreter", mock.base);
    let mut road = roads(tmp.path(), &endpoint, RoadAccessClasses::AllDrivable);
    let transform = Transformer::new(CRS).unwrap();
    let point = transform.forward(ll(15.2001, 60.05)).unwrap();
    let mut progress = Vec::new();
    assert_eq!(
        road.allowed(&[point], &|| false, |stage, done, total| progress.push((
            stage.to_owned(),
            done,
            total
        )))
        .unwrap(),
        [true]
    );
    assert_eq!(
        progress
            .iter()
            .map(|(_, a, b)| (*a, *b))
            .collect::<Vec<_>>(),
        [(0, 2), (1, 2), (2, 2)]
    );
    assert!(progress
        .iter()
        .all(|(stage, _, _)| stage == "Loading road network"));
    let requests = mock.finish();
    let query = |request: &Request| {
        url::form_urlencoded::parse(&request.body)
            .find(|(key, _)| key == "data")
            .unwrap()
            .1
            .into_owned()
    };
    assert_eq!(query(&requests[0]), query(&requests[1]));
    assert!(query(&requests[1]).ends_with("|track)$\"](60.0,15.0,60.1,15.2);out skel geom;"));
    assert!(query(&requests[2]).ends_with("(60.0,15.2,60.1,15.4);out skel geom;"));
    assert!(requests.iter().all(|req| req.method == "POST"
        && req.path == "/api/interpreter"
        && req.headers["user-agent"].starts_with("RepeaterNet/")
        && req.headers["user-agent"].contains("road access planner")));

    let alternatives = [
        BTreeMap::from([("K-one".into(), point)]),
        BTreeMap::from([("M-other".into(), point)]),
    ];
    let overlay = roads(tmp.path(), &endpoint, RoadAccessClasses::AllDrivable)
        .overlay(&alternatives, &|| false, |_, _, _| {})
        .unwrap();
    assert_eq!(overlay.polyline_indices, [vec![0], vec![0]]);
    assert_eq!(overlay.polylines.len(), 1);
    assert_eq!(overlay.polylines[0].len(), 2);
    assert!(overlay.polylines[0]
        .iter()
        .all(|[lat, lon]| (lat - 60.05).abs() < 0.0007 && (lon - 15.1999).abs() < 0.00001));
    let distance = overlay.nearest_distances_m[0]["K-one"].unwrap();
    assert!((distance - 11.14).abs() < 0.20);
    assert_eq!(overlay.nearest_distances_m[1]["M-other"], Some(distance));
}

#[test]
fn roads_mock_reports_failure_and_resumes_after_cancel_between_tiles() {
    let tmp = tempdir().unwrap();
    let transform = Transformer::new(CRS).unwrap();
    let point = transform.forward(ll(15.2001, 60.05)).unwrap();
    let mock = Mock::start(
        (0..4)
            .map(|_| Reply::new(504, b"timeout".as_slice()))
            .collect(),
    );
    let error = roads(tmp.path(), &mock.base, RoadAccessClasses::PublicRoads)
        .allowed(&[point], &|| false, |_, _, _| {})
        .unwrap_err();
    assert!(matches!(
        error,
        ProviderError::RoadTile {
            total: 2,
            row: 600,
            col: 75,
            ..
        }
    ));
    assert!(error.to_string().contains("HTTP 504"));
    assert_eq!(mock.finish().len(), 4);
    assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 0);

    let mock = Mock::start(vec![
        Reply::json(road_way()),
        Reply::json(json!({"elements": [], "remark": "runtime error"})),
        Reply::json(json!({"elements": []})),
    ]);
    let mut road = roads(tmp.path(), &mock.base, RoadAccessClasses::PublicRoads);
    let cancel = AtomicBool::new(false);
    assert!(matches!(
        road.allowed(
            &[point],
            &|| cancel.load(Ordering::Relaxed),
            |_, done, _| {
                if done == 1 {
                    cancel.store(true, Ordering::Relaxed);
                }
            }
        ),
        Err(ProviderError::Cancelled)
    ));
    assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 1);
    let error = road.allowed(&[point], &|| false, |_, _, _| {}).unwrap_err();
    assert!(error.to_string().contains("runtime error"));
    assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 1);
    assert_eq!(
        road.allowed(&[point], &|| false, |_, _, _| {}).unwrap(),
        [true]
    );
    assert_eq!(mock.finish().len(), 3);
}

#[test]
fn corescope_mock_pagination_normalization_and_corridor_wire_format() {
    let hill = json!({
        "public_key": "  ABCDEFGHIJKL ", "role": "repeater", "name": "Zebra",
        "lat": "60.0", "lon": 10.2, "last_seen": "2026-11-19T04:15:00+01:00",
        "relay_active": true, "relay_count_24h": "12",
    });
    let unknown = json!({
        "public_key": "123456789012", "role": "repeater", "name": "ångström",
        "lat": 60.005, "lon": 10.15, "last_heard": "invalid", "relay_active": "true", "relay_count_24h": null,
    });
    let far = json!({"public_key": "farawaytower", "role": "repeater", "lat": 71, "lon": 29});
    let mock = Mock::start(vec![
        Reply::retry(429, "0"),
        Reply::json(
            json!({"nodes": [hill, {"role": "companion"}, {"role": "repeater", "public_key": "out", "lat": 60, "lon": 3}]}),
        ),
        Reply::json(json!({"nodes": [hill, unknown, far]})),
        Reply::json(json!({"nodes": [hill, far, unknown]})),
    ]);
    let corescope = CoreScopeClient::with_client(
        format!("{}/", mock.base),
        client(),
        3,
        Duration::from_secs(2),
    )
    .unwrap();
    let repeaters = corescope.fetch_repeaters(20, &|| false).unwrap();
    assert_eq!(
        repeaters
            .iter()
            .map(|r| r.name.as_str())
            .collect::<Vec<_>>(),
        ["farawaytow", "Zebra", "ångström"]
    );
    let wire = filter_corridor(
        &repeaters,
        ll(10., 60.),
        ll(10.4, 60.),
        900.,
        Utc.with_ymd_and_hms(2026, 11, 19, 8, 0, 0).unwrap(),
    )
    .unwrap();
    assert_eq!(
        serde_json::to_value(wire).unwrap(),
        json!({
            "count": 2,
            "routers": [
                {"id":"K-ABCDEFGHIJ","name":"Zebra","latitude":60.0,"longitude":10.2,
                  "corridor_distance_m":0,"freshness":"heard 4 hour(s) ago","last_heard":"2026-11-19T04:15:00+01:00",
                  "relay_active":true,"relay_count_24h":12,"provenance":"CoreScope"},
                {"id":"K-1234567890","name":"ångström","latitude":60.005,"longitude":10.15,
                  "corridor_distance_m":557,"freshness":"last heard unknown","last_heard":null,
                  "relay_active":null,"relay_count_24h":0,"provenance":"CoreScope"}
            ]
        })
    );
    let requests = mock.finish();
    assert!(requests.iter().all(|req| req.method == "GET"
        && req.headers["user-agent"].starts_with("RepeaterNet/")
        && req.headers["user-agent"].contains("read-only CoreScope router import")));
    assert_eq!(
        requests
            .iter()
            .map(|req| req.path.as_str())
            .collect::<Vec<_>>(),
        [
            "/api/nodes?role=repeater&limit=3&offset=0",
            "/api/nodes?role=repeater&limit=3&offset=0",
            "/api/nodes?role=repeater&limit=3&offset=3",
            "/api/nodes?role=repeater&limit=3&offset=6",
        ]
    );
}

#[test]
fn corescope_respects_reported_total_and_nonretryable_status() {
    let mock = Mock::start(vec![
        Reply::json(
            json!({"nodes": [{"role": "repeater", "public_key": "abc", "lat": 60, "lon": 10}], "total": 1}),
        ),
        Reply::new(401, b"denied".as_slice()),
    ]);
    let corescope =
        CoreScopeClient::with_client(&mock.base, client(), 1, Duration::from_secs(2)).unwrap();
    assert_eq!(corescope.fetch_repeaters(20, &|| false).unwrap().len(), 1);
    assert!(matches!(
        corescope.fetch_repeaters(20, &|| false),
        Err(ProviderError::Http { status: 401, .. })
    ));
    let requests = mock.finish();
    assert_eq!(requests[0].path, requests[1].path);
}
