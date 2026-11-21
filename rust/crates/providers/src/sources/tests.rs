use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    ops::RangeInclusive,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
};

use geo::MultiPolygon;
use repeaternet_terrain::{
    write_elevation_tiff, Bounds, GeoTiff, LonLat, Point, SourceRaster, TerrainCrs, Transformer,
};
use reqwest::header::HeaderMap;
use serde_json::{json, Value};

use super::*;
use super::{
    downloader::{FetchContext, PROVIDER},
    schema::rectangle,
};

const COG: &[u8] = include_bytes!("../../tests/fixtures/geographic-cog.tif");
const RIO: LonLat = LonLat {
    longitude: -43.21,
    latitude: -22.95,
};

fn area([x, y]: [f64; 2]) -> MultiPolygon<f64> {
    MultiPolygon(vec![rectangle([x - 0.01, y - 0.01, x + 0.01, y + 0.01])])
}

fn custom(id: &str, url: &str) -> Value {
    json!({
        "id": id, "name": id, "kind": "cog",
        "products": {"dtm": {"url": url}},
        "coverage": {"type":"global"}, "resolution_m": 10, "attribution": "Fixture"
    })
}

fn registry(dir: &Path, terrain: Vec<Value>, disabled: &[&str]) -> Arc<ProviderRegistry> {
    let file = ProviderRegistry::validate_custom(&json!({
        "version":1, "terrain": terrain, "disabled_builtins":disabled
    }))
    .unwrap();
    ProviderRegistry::save_custom(dir, &file).unwrap();
    Arc::new(ProviderRegistry::load(dir).unwrap())
}

#[test]
fn definitions_validate_with_paths_and_round_trip() {
    let mut a = custom("bad-id", "https://example.com/elev.tif");
    a["coverage"] = json!({"type":"bbox","bbox":[170.0,0.0,-170.0,1.0]});
    a["resolution_m"] = json!(0);
    let bad_template = json!({
        "id":"tile", "name":"Tiles", "kind":"cog_tiles",
        "products":{"dtm":{"url_template":"https://example.com/{foo}/{lat2}.tif","tile_degrees":1}},
        "coverage":{"type":"global"}, "resolution_m":30, "attribution":"Test"
    });
    let mut extra = custom("typo", "https://example.com/a.tif");
    extra["products"]["dtm"]["extra"] = json!(true);
    let issues = ProviderRegistry::validate_custom(&json!({
        "version": 2, "terrain": [a,bad_template,extra], "disabled_builtins": ["missing"],
    }))
    .unwrap_err();
    for path in [
        "version",
        "disabled_builtins[0]",
        "terrain[0].coverage.bbox",
        "terrain[0].resolution_m",
        "terrain[1].products.dtm.url_template",
        "terrain[2].products.dtm.extra",
    ] {
        assert!(
            issues.iter().any(|issue| issue.path == path),
            "{path}: {issues:?}"
        );
    }

    let root = tempfile::tempdir().unwrap();
    let mut provider = custom("custom-test", "http://127.0.0.1/a.tif");
    provider["headers"] = json!({"authorization":"sensitive","x-env":"${SOURCE_TOKEN}"});
    let registry = registry(root.path(), vec![provider], &["copernicus-glo90"]);
    let view = registry.view();
    assert_eq!(
        view["custom"]["terrain"][0]["headers"]["authorization"],
        "***"
    );
    assert_eq!(
        view["custom"]["terrain"][0]["headers"]["x-env"],
        "${SOURCE_TOKEN}"
    );
    assert_eq!(
        registry.validate_edit(&view["custom"]).unwrap().terrain[0].headers["authorization"],
        "sensitive"
    );
    assert!(
        !registry
            .terrain()
            .iter()
            .find(|p| p.id == "copernicus-glo90")
            .unwrap()
            .enabled
    );
}

#[test]
fn expands_tile_names_on_each_side_of_zero_and_at_the_date_line() {
    let config = CogTiles {
        url_template: "https://example.com/{NS}{lat2}{EW}{lon3}/{lat_signed}/{lon_signed}".into(),
        tile_degrees: 1.0,
    };
    for (latitude, longitude, expected) in [
        (59.9, 10.8, "N59E010/59/10"),
        (-0.5, -0.5, "S01W001/-1/-1"),
        (0.0, 0.0, "N00E000/0/0"),
        (89.999, 179.999, "N89E179/89/179"),
        (-90.0, -180.0, "S90W180/-90/-180"),
        (0.3, 180.0, "N00W180/0/-180"),
    ] {
        assert!(config
            .url_for_cell(config.cell(LonLat {
                latitude,
                longitude
            }))
            .unwrap()
            .ends_with(expected));
    }
    let config = CogTiles {
        url_template: "{ns}{lat_n2}{ew}{lon3}".into(),
        tile_degrees: 1.0,
    };
    assert_eq!(
        config
            .url_for_cell(config.cell(LonLat {
                latitude: 39.3,
                longitude: -105.2
            }))
            .unwrap(),
        "n40w106"
    );
}

#[test]
fn ranking_pinning_and_target_zones() {
    let root = tempfile::tempdir().unwrap();
    let mut regional = custom("touching", "https://example.com/r.tif");
    regional["coverage"] = json!({"type":"bbox","bbox":[10.005,59.0,11.0,60.0]});
    regional["resolution_m"] = json!(1);
    let lower = custom("lower", "https://example.com/lower.tif");
    let mut higher = custom("higher", "https://example.com/higher.tif");
    higher["priority"] = json!(50);
    let registry = registry(
        root.path(),
        vec![regional, lower, higher],
        &["kartverket-nhm"],
    );
    let footprint = area([10.0, 59.9]);
    let ids = |choice| {
        registry
            .rank(Product::Dtm, &footprint, &choice)
            .into_iter()
            .map(|p| p.id.as_str())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        ids(SourceChoice::default()),
        [
            "higher",
            "lower",
            "copernicus-glo30",
            "copernicus-glo90",
            "touching"
        ]
    );
    assert_eq!(
        ids(SourceChoice {
            source: "touching".into(),
            fallback: false
        }),
        ["touching"]
    );
    assert_eq!(
        ids(SourceChoice {
            source: "lower".into(),
            fallback: true
        })[0],
        "lower"
    );
    assert!(registry
        .check_choice(&SourceChoice {
            source: "kartverket-nhm".into(),
            fallback: true
        })
        .is_err());
    let registry = ProviderRegistry::builtin();
    for (x, y, epsg) in [
        (10.75, 59.91, 25832),
        (18.95, 69.65, 25833),
        (30.04, 69.73, 25835),
        (18.07, 59.33, 32633),
        (-43.21, -22.95, 32723),
        (16.0, 78.0, 32633),
        (0.0, 85.0, 3413),
        (0.0, -85.0, 3031),
    ] {
        assert_eq!(
            registry.select_target_crs(&area([x, y])).unwrap().epsg(),
            epsg,
            "{x}, {y}"
        );
    }
}

#[test]
fn source_resolution_can_reduce_an_initial_grid_over_the_tile_limit() {
    let root = tempfile::tempdir().unwrap();
    let registry = Arc::new(ProviderRegistry::builtin());
    let limits = TileLimits {
        maximum_tiles: 4,
        ..Default::default()
    };
    let downloader = TerrainDownloader::new(registry, root.path(), limits).unwrap();
    let crs = TerrainCrs::from_epsg(32723).unwrap();
    let bounds = Bounds {
        min_x: 683_000.0,
        min_y: 7_453_000.0,
        max_x: 693_000.0,
        max_y: 7_463_000.0,
    };
    let options = PlanOptions {
        auto_resolution: false,
        ..Default::default()
    };
    let plan = downloader
        .plan_download(
            crs,
            bounds,
            1.0,
            DownloadArea::Bounds,
            options,
            false,
            SourceChoice::default(),
        )
        .unwrap();
    let estimate = downloader.estimate(crs, &plan, root.path()).unwrap();
    assert_eq!(estimate.requested_resolution_m, 1.0);
    assert_eq!(estimate.effective_resolution_m, 30.0);
    assert!(estimate.tile_count <= limits.maximum_tiles);
}

#[derive(Clone, Debug)]
struct Request {
    path: String,
    range: Option<RangeInclusive<usize>>,
}

type Requests = Arc<Mutex<Vec<Request>>>;

struct Server {
    address: SocketAddr,
    requests: Requests,
    stopping: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Server {
    fn start(handler: impl Fn(&Request, &mut TcpStream) + Send + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let stopping = Arc::new(AtomicBool::new(false));
        let requests = Requests::default();
        let worker = {
            let stopping = stopping.clone();
            let requests = requests.clone();
            thread::spawn(move || {
                for stream in listener.incoming() {
                    if stopping.load(Ordering::Acquire) {
                        break;
                    }
                    let mut stream = stream.unwrap();
                    let mut lines = BufReader::new(&stream).lines();
                    let first = lines.next().unwrap().unwrap();
                    let path = first.split_ascii_whitespace().nth(1).unwrap().to_owned();
                    let mut range = None;
                    for line in lines {
                        let line = line.unwrap();
                        if line.is_empty() {
                            break;
                        }
                        if let Some(value) = line.to_ascii_lowercase().strip_prefix("range: bytes=")
                        {
                            let (start, end) = value.split_once('-').unwrap();
                            range = Some(start.parse().unwrap()..=end.parse().unwrap());
                        }
                    }
                    let request = Request { path, range };
                    requests.lock().unwrap().push(request.clone());
                    handler(&request, &mut stream);
                }
            })
        };
        Self {
            address,
            requests,
            stopping,
            worker: Some(worker),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.address)
    }
    fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        let _ = TcpStream::connect(self.address);
        self.worker.take().unwrap().join().unwrap();
    }
}

fn reply(stream: &mut TcpStream, status: &str, headers: &str, body: &[u8]) {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nConnection: close\r\nContent-Length: {}\r\n{headers}\r\n",
        body.len()
    )
    .unwrap();
    stream.write_all(body).unwrap();
}

fn respond_range(request: &Request, stream: &mut TcpStream, bytes: &[u8]) {
    let range = request.range.as_ref().expect("expected Range header");
    let start = *range.start();
    let end = (*range.end()).min(bytes.len() - 1);
    let headers = format!("Content-Type: image/tiff\r\nContent-Range: bytes {start}-{end}/{}\r\nETag: \"fixture\"\r\n", bytes.len());
    reply(stream, "206 Partial Content", &headers, &bytes[start..=end]);
}

fn context() -> FetchContext {
    FetchContext::test_context()
}

fn u32_at(bytes: &[u8], offset: usize) -> usize {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize
}

fn tag_offset(bytes: &[u8], ifd: usize, tag: u16) -> usize {
    let n = u16::from_le_bytes(bytes[ifd..ifd + 2].try_into().unwrap());
    (0..usize::from(n))
        .find_map(|index| {
            let offset = ifd + 2 + index * 12;
            (bytes[offset..offset + 2] == tag.to_le_bytes()).then(|| u32_at(bytes, offset + 8))
        })
        .unwrap()
}

#[test]
fn cog_overview_deflate_predictor_range_cache_and_warp() {
    let base_ifd = 8;
    let overview_ifd = u32_at(COG, base_ifd + 2 + 16 * 12);
    let offsets = tag_offset(COG, overview_ifd, 324);
    let counts = tag_offset(COG, overview_ifd, 325);
    let block_start = u32_at(COG, offsets);
    let count = u32_at(COG, counts);
    let relocated = 3 * 256 * 1024 + 1024;
    let mut bytes = COG.to_vec();
    bytes.resize(8 * 256 * 1024, 0);
    bytes[relocated..relocated + count].copy_from_slice(&COG[block_start..block_start + count]);
    bytes[offsets..offsets + 4].copy_from_slice(&(relocated as u32).to_le_bytes());
    let server = Server::start(move |request, stream| respond_range(request, stream, &bytes));
    let directory = tempfile::tempdir().unwrap();
    let client = crate::transport::client(PROVIDER, 1, 5).unwrap();
    let url = server.url("/terrain.tif");
    for _ in 0..2 {
        let raster = range::open(
            &client,
            directory.path(),
            &url,
            &HeaderMap::new(),
            false,
            &context(),
        )
        .unwrap()
        .unwrap();
        let raster = SourceRaster::open(raster, 250.0, RIO.latitude).unwrap();
        assert_eq!(raster.crs().to_string(), "EPSG:4326");
        assert!((215.0..225.0).contains(&raster.resolution_m()));
        let transform = Transformer::new(TerrainCrs::from_epsg(32723).unwrap()).unwrap();
        let center = transform.forward(RIO).unwrap();
        let locations: Vec<_> = (-10..10)
            .map(|i| {
                transform
                    .reverse(Point {
                        x: center.x + f64::from(i) * 10.0,
                        y: center.y,
                    })
                    .unwrap()
            })
            .collect();
        let values = raster.sample_wgs84(&locations).unwrap();
        for (location, value) in locations.iter().zip(values) {
            let expected =
                100.0 + (location.longitude + 43.4) * 300.0 + (location.latitude + 23.212) * 200.0;
            assert!(
                (f64::from(value) - expected).abs() < 0.5,
                "{value} vs {expected}"
            );
        }
        let requests = server.requests();
        assert_eq!(requests.len(), 2, "{requests:?}");
        assert_eq!(requests[0].range, Some(0..=262_143));
        assert_eq!(requests[1].range, Some(786_432..=1_048_575));
    }
}

#[test]
fn single_geographic_cog_can_span_both_hemispheres() {
    let mut world = COG.to_vec();
    let scale_x = tag_offset(COG, 8, 33550);
    let tiepoint_x = tag_offset(COG, 8, 33922) + 24;
    world[scale_x..scale_x + 8].copy_from_slice(&(360.0_f64 / 512.0).to_le_bytes());
    world[tiepoint_x..tiepoint_x + 8].copy_from_slice(&(-180.0_f64).to_le_bytes());
    let raster = SourceRaster::open(
        Arc::new(repeaternet_terrain::BytesSource::new(world)),
        100.0,
        RIO.latitude,
    )
    .unwrap();
    let values = raster
        .sample_wgs84(&[
            LonLat {
                longitude: -90.0,
                ..RIO
            },
            LonLat {
                longitude: 90.0,
                ..RIO
            },
        ])
        .unwrap();
    for (value, fraction) in values.into_iter().zip([0.25, 0.75]) {
        let expected = 100.0 + fraction * 0.512 * 300.0 + (RIO.latitude + 23.212) * 200.0;
        assert!((f64::from(value) - expected).abs() < 0.5);
    }
}

#[test]
fn cog_missing_marker_and_range_ignoring_response_limits() {
    let server = Server::start(|request, stream| match request.path.as_str() {
        "/absent" => reply(stream, "404 Not Found", "", &[]),
        "/small" => reply(stream, "200 OK", "Content-Type: image/tiff\r\n", COG),
        "/large" => write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: 67108865\r\nConnection: close\r\n\r\n"
        )
        .unwrap(),
        _ => unreachable!(),
    });
    let directory = tempfile::tempdir().unwrap();
    let client = crate::transport::client(PROVIDER, 1, 5).unwrap();
    let open = |path, tiled| {
        range::open(
            &client,
            directory.path(),
            &server.url(path),
            &HeaderMap::new(),
            tiled,
            &context(),
        )
    };
    for _ in 0..2 {
        assert!(open("/absent", true).unwrap().is_none());
        assert_eq!(
            open("/small", false).unwrap().unwrap().len(),
            COG.len() as u64
        );
    }
    assert!(open("/large", false)
        .err()
        .unwrap()
        .to_string()
        .contains("server does not support range requests"));
    assert_eq!(server.requests().len(), 3);
}

#[test]
fn wcs_surfaces_namespaced_exception_text() {
    let server = Server::start(|request, stream| {
        assert!(request.path.contains("version=2.0.1"), "{request:?}");
        assert!(request.path.contains("coverageId=ground"), "{request:?}");
        let body = br#"<ows:ExceptionReport xmlns:ows="urn:ows"><ows:Exception>
            <ows:ExceptionText>Invalid subset &amp; extent</ows:ExceptionText>
            </ows:Exception></ows:ExceptionReport>"#;
        reply(
            stream,
            "400 Bad Request",
            "Content-Type: application/xml\r\n",
            body,
        );
    });
    let directory = tempfile::tempdir().unwrap();
    let mut provider = custom("wcs-test", "unused");
    provider["kind"] = json!("wcs");
    provider["products"] = json!({"dtm":{"endpoint":server.url("/wcs"),
        "coverage":"ground","version":"2.0.1","native_crs":["EPSG:4326"],"format":"image/tiff"}});
    let registry = registry(directory.path(), vec![provider], &[]);
    let downloader =
        TerrainDownloader::new(registry, directory.path(), TileLimits::default()).unwrap();
    let probe = downloader.probe("wcs-test", RIO).unwrap();
    assert!(!probe.ok);
    assert!(
        probe.message.contains("Invalid subset & extent"),
        "{probe:?}"
    );
}

#[test]
fn fills_gaps_rejects_value_ranges_and_cancels() {
    let root = tempfile::tempdir().unwrap();
    let crs = TerrainCrs::from_epsg(32723).unwrap();
    let bounds = Bounds {
        min_x: 680_000.0,
        min_y: 7_460_000.0,
        max_x: 680_100.0,
        max_y: 7_460_100.0,
    };
    let raster = |value, gap| {
        let mut values = vec![value; 100];
        if gap {
            for row in values.as_chunks_mut::<10>().0 {
                row[5..].fill(f32::NAN);
            }
        }
        let mut bytes = Vec::new();
        write_elevation_tiff(&mut bytes, crs, bounds, 10, 10, &values).unwrap();
        bytes
    };
    let fixtures = BTreeMap::from([
        ("/bad".to_string(), raster(11_000.0, false)),
        ("/half".to_string(), raster(123.0, true)),
        ("/good".to_string(), raster(61.0, false)),
    ]);
    let server = Server::start(move |request, stream| {
        respond_range(request, stream, fixtures.get(&request.path).unwrap());
    });
    let definitions = [("bad", 3), ("half", 2), ("good", 1)]
        .into_iter()
        .map(|(id, priority)| {
            let mut p = custom(id, &server.url(&format!("/{id}")));
            p["priority"] = json!(priority);
            p
        })
        .collect();
    let registry = registry(
        root.path(),
        definitions,
        &[
            "copernicus-glo30",
            "copernicus-glo90",
            "usgs-3dep-13",
            "kartverket-nhm",
        ],
    );
    let downloader = TerrainDownloader::new(
        registry,
        root.path(),
        TileLimits {
            maximum_tile_area_km2: 0.01,
            ..Default::default()
        },
    )
    .unwrap();
    let plan = downloader
        .plan_download(
            crs,
            bounds,
            10.0,
            DownloadArea::Bounds,
            PlanOptions::default(),
            true,
            SourceChoice::default(),
        )
        .unwrap();
    let dtm = plan.dtm.as_ref().unwrap();
    let estimate = downloader.estimate(crs, &plan, root.path()).unwrap();
    assert!(estimate.dom_unavailable_reason.is_some());
    assert_eq!(estimate.sources[0].source.id, "bad");
    let cancelled = downloader.fetch_plan(
        Product::Dtm,
        crs,
        dtm,
        &SourceChoice::default(),
        Arc::new(|| true),
        |_| {},
    );
    assert!(matches!(cancelled, Err(crate::ProviderError::Cancelled)));
    assert!(server.requests().is_empty());
    let result = downloader
        .fetch_plan(
            Product::Dtm,
            crs,
            dtm,
            &SourceChoice::default(),
            Arc::new(|| false),
            |_| {},
        )
        .unwrap();
    assert_eq!(
        result.providers_used,
        ["good".into(), "half".into()].into_iter().collect()
    );
    assert_eq!(result.warnings.len(), 1);
    let tile = GeoTiff::open(&result.paths[0]).unwrap();
    assert_eq!(tile.metadata().crs, crs);
    assert_eq!(
        tile.sample(&[
            Point {
                x: 680_015.0,
                y: 7_460_015.0
            },
            Point {
                x: 680_085.0,
                y: 7_460_015.0
            }
        ])
        .unwrap(),
        [123.0, 61.0]
    );
    assert_eq!(
        downloader
            .estimate(crs, &plan, root.path())
            .unwrap()
            .cache_hits,
        1
    );
}

#[test]
fn writer_round_trips_partial_tiles_and_nodata() {
    let crs = TerrainCrs::from_epsg(32723).unwrap();
    let bounds = Bounds {
        min_x: 500_000.0,
        min_y: 7_500_000.0,
        max_x: 500_257.0,
        max_y: 7_500_259.0,
    };
    let mut values = vec![56.25; 257 * 259];
    values[0] = f32::NAN;
    *values.last_mut().unwrap() = 789.5;
    let mut file = tempfile::NamedTempFile::new().unwrap();
    write_elevation_tiff(&mut file, crs, bounds, 257, 259, &values).unwrap();
    let tile = GeoTiff::open(file.path()).unwrap();
    tile.validate_pixels(&|| false).unwrap();
    assert_eq!(tile.metadata().bounds, bounds);
    assert_eq!(tile.metadata().crs, crs);
    let values = tile
        .sample(&[
            Point {
                x: 500_000.5,
                y: 7_500_258.5,
            },
            Point {
                x: 500_001.5,
                y: 7_500_258.5,
            },
            Point {
                x: 500_256.5,
                y: 7_500_000.5,
            },
        ])
        .unwrap();
    assert!(values[0].is_nan());
    assert_eq!(values[1..], [56.25, 789.5]);
}

#[test]
#[ignore = "uses live Copernicus and Kartverket endpoints"]
fn live_rio_and_oslo() {
    let dir = tempfile::tempdir().unwrap();
    let registry = Arc::new(ProviderRegistry::builtin());
    let downloader = TerrainDownloader::new(registry, dir.path(), TileLimits::default()).unwrap();
    let mut passed = true;
    for (id, at) in [
        ("copernicus-glo30", RIO),
        (
            "kartverket-nhm",
            LonLat {
                longitude: 10.7522,
                latitude: 59.9139,
            },
        ),
    ] {
        let result = downloader.probe_with_span(id, at, 2000.0).unwrap();
        println!("{id}: {}", serde_json::to_string(&result).unwrap());
        passed &= result.ok && result.valid_fraction > 0.5;
    }
    assert!(passed);
}
