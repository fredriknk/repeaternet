use std::{
    collections::{BTreeSet, HashMap},
    net::{Ipv4Addr, TcpListener},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use axum::{
    Json, Router,
    extract::{Query, State},
    http::{StatusCode, header},
    response::Html,
    routing::get,
};
use chrono::Utc;
use rusqlite::{Connection, OpenFlags, named_params, params};
use serde::{Deserialize, Serialize};
use sondehub_import::{DATABASE, GB, SUSPECT_FLAGS, database_bytes};
use tokio::sync::oneshot;

pub struct Viewer {
    stop: Option<oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Viewer {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

const LINE_LIMIT: usize = 15_000;
const SONDE_HOST: &str = "history.sondehub.org";
const SONDE_NETWORK: &str = "Radiosonde";
const SONDE_BAND: &str = "400 MHz";

pub fn directory_globs(dir: &Path) -> Vec<String> {
    let dir = PathBuf::from(glob::Pattern::escape(&dir.to_string_lossy()));
    ["*capture*.sqlite3", DATABASE]
        .into_iter()
        .map(|pattern| dir.join(pattern).to_string_lossy().into_owned())
        .collect()
}

pub fn start(
    patterns: Vec<String>,
    port: u16,
    budget_gb: f64,
    directory: Option<PathBuf>,
) -> Result<Viewer> {
    for pattern in &patterns {
        glob::Pattern::new(pattern)?;
    }
    let listener =
        TcpListener::bind((Ipv4Addr::LOCALHOST, port)).context("Bind the measured links viewer")?;
    let address = listener.local_addr()?;
    listener.set_nonblocking(true)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()?;
    let (stop, shutdown) = oneshot::channel();
    let state = Arc::new(ViewerState {
        patterns,
        directory,
        budget_gb,
        cache: Mutex::new(HashMap::new()),
    });
    let router = Router::new()
        .route("/", get(|| async { Html(include_str!("viewer.html")) }))
        .route(
            "/leaflet.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript")],
                    include_str!("../../web-server/assets/leaflet/leaflet.js"),
                )
            }),
        )
        .route(
            "/leaflet.css",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/css")],
                    include_str!("../../web-server/assets/leaflet/leaflet.css"),
                )
            }),
        )
        .route("/api/links", get(api))
        .route("/api/sondes", get(sonde_api))
        .with_state(state);
    let thread = thread::Builder::new()
        .name("capture-viewer".into())
        .spawn(move || {
            let result = runtime.block_on(async {
                axum::serve(tokio::net::TcpListener::from_std(listener)?, router)
                    .with_graceful_shutdown(async {
                        let _ = shutdown.await;
                    })
                    .await
            });
            if let Err(error) = result {
                eprintln!("Viewer stopped · {error:#}");
            }
        })?;
    println!("Measured links viewer · http://{address}/");
    Ok(Viewer {
        stop: Some(stop),
        thread: Some(thread),
    })
}

#[derive(Clone, Copy, Default, Deserialize, Hash, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum Window {
    #[default]
    Day,
    Week,
    All,
}

#[derive(Deserialize)]
struct Filters {
    #[serde(default)]
    window: Window,
}

impl Window {
    fn since(self) -> i64 {
        let now = Utc::now().timestamp();
        match self {
            Self::Day => now - 86_400,
            Self::Week => now - 7 * 86_400,
            Self::All => 0,
        }
    }
}

struct ViewerState {
    patterns: Vec<String>,
    directory: Option<PathBuf>,
    budget_gb: f64,
    cache: Mutex<HashMap<Window, (Instant, Arc<MapData>)>>,
}

async fn api(
    State(state): State<Arc<ViewerState>>,
    Query(filters): Query<Filters>,
) -> Result<([(header::HeaderName, &'static str); 1], Json<Arc<MapData>>), StatusCode> {
    let data = tokio::task::spawn_blocking(move || {
        let mut cache = state
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((time, data)) = cache.get(&filters.window)
            && time.elapsed() < Duration::from_secs(55)
        {
            return Arc::clone(data);
        }
        let data = Arc::new(load(&state, filters.window));
        cache.insert(filters.window, (Instant::now(), Arc::clone(&data)));
        data
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(data)))
}

#[derive(Default, Serialize)]
struct MapData {
    sources: Vec<Inventory>,
    links: Vec<Link>,
    warnings: Vec<String>,
    database_bytes: u64,
    budget_bytes: u64,
}

#[derive(Serialize)]
struct Inventory {
    network: &'static str,
    host: String,
    measurements: i64,
    located_links: i64,
    nodes: i64,
    last_update: Option<i64>,
}

#[derive(Serialize)]
struct Link {
    source: usize,
    from: [f64; 2],
    to: [f64; 2],
    tx: String,
    rx: String,
    evidence: String,
    band: Option<String>,
    preset: Option<String>,
    snr: Option<f64>,
    rssi: Option<f64>,
    count: i64,
    last: i64,
    coarse: bool,
    unknown_precision: bool,
    suspect: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    agl: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    quality_flags: Option<u32>,
}

fn capture_paths(patterns: &[String], warnings: &mut Vec<String>) -> BTreeSet<PathBuf> {
    let mut paths = BTreeSet::new();
    for pattern in patterns {
        match glob::glob_with(
            pattern,
            glob::MatchOptions {
                require_literal_separator: true,
                case_sensitive: !cfg!(windows),
                ..Default::default()
            },
        ) {
            Ok(entries) => {
                for entry in entries {
                    match entry {
                        Ok(path) if capture_file(&path) => {
                            let path = path.canonicalize().unwrap_or(path);
                            if capture_file(&path) {
                                paths.insert(path);
                            }
                        }
                        Ok(_) => {}
                        Err(error) => warnings.push(error.to_string()),
                    }
                }
            }
            Err(error) => warnings.push(error.to_string()),
        }
    }
    paths
}

fn load(state: &ViewerState, window: Window) -> MapData {
    let since = window.since();
    let mut data = MapData {
        budget_bytes: (state.budget_gb * GB) as u64,
        ..MapData::default()
    };
    let paths = capture_paths(&state.patterns, &mut data.warnings);
    let dirs = if let Some(dir) = &state.directory {
        BTreeSet::from([dir.as_path()])
    } else {
        paths.iter().filter_map(|path| path.parent()).collect()
    };
    for dir in dirs {
        match database_bytes(dir) {
            Ok(bytes) => data.database_bytes = data.database_bytes.saturating_add(bytes),
            Err(error) => {
                eprintln!("Could not measure {} · {error}", dir.display());
                data.warnings
                    .push("Could not measure capture database size".into());
            }
        }
    }
    for path in paths {
        match read_capture(&path, since, data.sources.len()) {
            Ok(Some((inventory, links))) => {
                data.sources.push(inventory);
                data.links.extend(links);
            }
            Ok(None) => {}
            Err(err) => {
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                eprintln!("Viewer skipped {name} · {err:#}");
                data.warnings.push(format!("Could not read {name}"));
            }
        }
    }
    data
}

fn capture_file(path: &Path) -> bool {
    path.is_file()
        && path.file_name().is_some_and(|name| {
            let name = name.to_string_lossy().to_ascii_lowercase();
            name.ends_with(".sqlite3") && !name.contains("backup")
        })
}

fn read_capture(path: &Path, since: i64, source: usize) -> Result<Option<(Inventory, Vec<Link>)>> {
    // SQLITE_OPEN_READ_ONLY is the SQLite open flag equivalent of URI mode=ro.
    // WAL readers share SQLite's normal snapshot protocol with active writers.
    let mut db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    db.busy_timeout(Duration::from_millis(500))?;
    if is_sonde(path) {
        return Ok(Some((sonde_inventory(&db)?, Vec::new())));
    }
    let snapshot = db.transaction()?;
    let exists = |table| -> rusqlite::Result<bool> {
        snapshot.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name=?1)",
            [table],
            |r| r.get(0),
        )
    };
    if exists("meshtastic_instance")? {
        meshtastic(&snapshot, since, source).map(Some)
    } else if exists("observations")? && exists("observers_snapshot")? {
        meshcore(&snapshot, since, source).map(Some)
    } else {
        Ok(None)
    }
}

fn meshtastic(db: &Connection, since: i64, source: usize) -> Result<(Inventory, Vec<Link>)> {
    crate::db::register_geometry(db)?;
    let (host, measurements, nodes, last_update) = db.query_row(
        "SELECT host,
          (SELECT count(*) FROM measurements),
          (SELECT count(*) FROM (SELECT node FROM radios UNION SELECT node FROM positions
                                  UNION SELECT tx FROM measurements WHERE tx IS NOT NULL UNION SELECT rx FROM measurements)),
          (SELECT max(t)/1000 FROM (SELECT max(seen_ms) AS t FROM measurements
                          UNION ALL SELECT max(observed_ms) FROM positions UNION ALL SELECT max(observed_ms) FROM radios))
         FROM meshtastic_instance",
        [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    let mut query = db.prepare(
        "SELECT tx, rx, tx_lat, tx_lon, rx_lat, rx_lon, evidence, link_region, modem_preset,
                avg(snr_db), avg(rssi_dbm), count(*), max(seen_ms)/1000, coarse, unknown_precision, max(mqtt_suspect),
                CASE WHEN link_region IS NULL THEN COALESCE(
                     (SELECT frequency_mhz FROM radios WHERE node=m.tx AND frequency_mhz IS NOT NULL ORDER BY observed_ms DESC LIMIT 1),
                     (SELECT frequency_mhz FROM radios WHERE node=m.rx AND frequency_mhz IS NOT NULL ORDER BY observed_ms DESC LIMIT 1)
                ) END AS band_frequency
         FROM located_measurements m WHERE seen_ms >= ?1
         GROUP BY tx, rx, evidence, link_region, modem_preset, band_frequency",
    )?;
    let links = query
        .query_map([since * 1000], |r| {
            let band: Option<String> = r.get(7)?;
            let frequency = r.get(16)?;
            Ok(Link {
                source,
                tx: format!("!{:08x}", r.get::<_, u32>(0)?),
                rx: format!("!{:08x}", r.get::<_, u32>(1)?),
                from: [r.get(2)?, r.get(3)?],
                to: [r.get(4)?, r.get(5)?],
                evidence: r.get(6)?,
                band: band.or_else(|| frequency_band(frequency)),
                preset: r.get(8)?,
                snr: r.get(9)?,
                rssi: r.get(10)?,
                count: r.get(11)?,
                last: r.get(12)?,
                coarse: r.get(13)?,
                unknown_precision: r.get(14)?,
                suspect: r.get(15)?,
                agl: None,
                quality_flags: None,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let (_, located_links) = crate::db::stats(db)?;
    Ok((
        Inventory {
            network: "Meshtastic",
            host,
            measurements,
            located_links,
            nodes,
            last_update,
        },
        links,
    ))
}

fn meshcore(db: &Connection, since: i64, source: usize) -> Result<(Inventory, Vec<Link>)> {
    let endpoint: String = db.query_row(
        "SELECT endpoint FROM capture_runs ORDER BY id DESC LIMIT 1",
        [],
        |r| r.get(0),
    )?;
    let url = reqwest::Url::parse(&endpoint)?;
    let host = url.host_str().unwrap_or(&endpoint).to_owned();
    let version: u32 = db.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let inventory = include_str!("../../corescope-capture/src/dashboard_inventory.sql");
    let inventory = if version == 1 {
        inventory.replace("seen_unix", "unixepoch(timestamp)")
    } else {
        inventory.into()
    };
    let mut query = db.prepare(&inventory)?;
    let items = query.query_map([&endpoint], |r| {
        Ok((
            r.get::<_, bool>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, Option<f64>>(3)?.zip(r.get::<_, Option<f64>>(4)?),
            r.get::<_, Option<String>>(5)?,
        ))
    })?;
    let mut nodes = HashMap::new();
    let mut observers = HashMap::new();
    let mut identities = BTreeSet::new();
    let mut observer_keys = HashMap::new();
    for item in items {
        let (observer, id, label, coords, key) = item?;
        identities.insert(key.as_ref().unwrap_or(&id).clone());
        let coords = coords.filter(|&(lat, lon)| {
            (-90.0..=90.0).contains(&lat)
                && (-180.0..=180.0).contains(&lon)
                && (lat != 0.0 || lon != 0.0)
        });
        let label = label.unwrap_or_else(|| id.clone());
        if observer {
            if let Some(key) = &key {
                observer_keys.insert(id.clone(), key.clone());
            }
            let coords = coords.or_else(|| {
                nodes
                    .get(key.as_ref().unwrap_or(&id))
                    .map(|&(_, pos): &(String, _)| pos)
            });
            if let Some(pos) = coords {
                observers.insert(id, (label, pos));
            }
        } else if let Some(pos) = coords {
            nodes.insert(id, (label, pos));
        }
    }
    let (measurements, last_update) = db.query_row(
        "SELECT count(*), (SELECT max(unixepoch(ended_at)) FROM capture_runs WHERE endpoint=?1)
         FROM observations WHERE endpoint=?1",
        [&endpoint],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let mut query = db.prepare(
        "SELECT observer_id, json_extract(raw_json, '$.radio') FROM observers_snapshot
         WHERE run_id = (SELECT id FROM capture_runs WHERE endpoint=?1 AND status='complete'
                         AND mode IN ('watch-topology','snapshot','run') ORDER BY id DESC LIMIT 1)
         AND json_type(raw_json, '$.radio')='text'",
    )?;
    let mut observer_radio = HashMap::new();
    for row in query.query_map([&endpoint], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })? {
        let (id, radio) = row?;
        observer_radio.insert(id, meshcore_radio(&radio));
    }
    let hops = if version == 1 {
        "FROM (SELECT endpoint, observer_id, snr, CASE WHEN rssi < 0 THEN rssi END AS rssi, unixepoch(timestamp) AS seen_unix,
           CASE WHEN json_array_length(path_prefixes)=0 THEN sender_pubkey
                ELSE json_extract(resolved_path, '$[#-1]') END AS last_hop FROM observations WHERE direction='rx')"
    } else {
        "FROM measured_hops"
    };
    let mut query = db.prepare(&format!(
        "SELECT last_hop, observer_id, count(*) FILTER (WHERE seen_unix >= ?2),
                avg(snr) FILTER (WHERE seen_unix >= ?2), avg(rssi) FILTER (WHERE seen_unix >= ?2), max(seen_unix)
         {hops} WHERE endpoint=?1 AND last_hop IS NOT NULL AND observer_id IS NOT NULL AND last_hop<>observer_id
         GROUP BY last_hop, observer_id",
    ))?;
    let mut rows = query.query(params![endpoint, since])?;
    let mut links = Vec::new();
    let mut located_links = 0;
    while let Some(r) = rows.next()? {
        let tx: String = r.get(0)?;
        let rx: String = r.get(1)?;
        identities.insert(observer_keys.get(&rx).unwrap_or(&rx).clone());
        identities.insert(observer_keys.get(&tx).unwrap_or(&tx).clone());
        let (Some((sender, from)), Some((receiver, to))) = (
            nodes.get(&tx).or_else(|| observers.get(&tx)),
            observers.get(&rx),
        ) else {
            continue;
        };
        located_links += 1;
        let count = r.get(2)?;
        if count == 0 {
            continue;
        }
        let (band, preset) = observer_radio.get(&rx).cloned().unwrap_or_default();
        links.push(Link {
            source,
            from: [from.0, from.1],
            to: [to.0, to.1],
            tx: sender.clone(),
            rx: receiver.clone(),
            evidence: "gateway_rx".into(),
            band,
            preset,
            snr: r.get(3)?,
            rssi: r.get(4)?,
            count,
            last: r.get(5)?,
            coarse: false,
            unknown_precision: true,
            suspect: false,
            agl: None,
            quality_flags: None,
        });
    }
    Ok((
        Inventory {
            network: "MeshCore",
            host,
            measurements,
            located_links,
            nodes: identities.len() as i64,
            last_update,
        },
        links,
    ))
}

fn frequency_band(frequency: Option<f64>) -> Option<String> {
    let mhz = frequency.filter(|f| f.is_finite() && *f > 0.0)?;
    let label = match mhz {
        420.0..450.0 => "433 MHz",
        850.0..880.0 => "868 MHz",
        900.0..930.0 => "915 MHz",
        2400.0..2485.0 => "2.4 GHz",
        _ => return Some(format!("{mhz:.3} MHz")),
    };
    Some(label.into())
}

fn meshcore_radio(radio: &str) -> (Option<String>, Option<String>) {
    let mut fields = radio.split(',').map(str::trim);
    let band = frequency_band(fields.next().and_then(|f| f.parse().ok()));
    let preset = (|| {
        let bw: f64 = fields.next()?.parse().ok()?;
        let sf: u32 = fields.next()?.parse().ok()?;
        let cr: u32 = fields.next()?.parse().ok()?;
        (bw.is_finite() && bw > 0.0 && (5..=12).contains(&sf) && (5..=8).contains(&cr))
            .then(|| format!("SF{sf} / {bw} kHz / CR 4/{cr}"))
    })();
    (band, preset)
}

fn is_sonde(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case(DATABASE))
}

fn sonde_inventory(db: &Connection) -> Result<Inventory> {
    Ok(db.query_row(
        "SELECT coalesce(sum(kept_rows), 0), coalesce(sum(located_links), 0),
                (SELECT count(*) FROM receiver), max(last_observed)
           FROM source_object WHERE status = 'complete'",
        [],
        |r| {
            Ok(Inventory {
                network: SONDE_NETWORK,
                host: SONDE_HOST.into(),
                measurements: r.get(0)?,
                located_links: r.get(1)?,
                nodes: r.get(2)?,
                last_update: r.get(3)?,
            })
        },
    )?)
}

#[derive(Deserialize)]
struct SondeFilters {
    #[serde(default)]
    window: Window,
    west: f64,
    east: f64,
    south: f64,
    north: f64,
    agl: Option<u32>,
    evidence: Option<String>,
    #[serde(default = "hide_suspect_by_default")]
    hide_suspect: bool,
}

fn hide_suspect_by_default() -> bool {
    true
}

#[derive(Default, Serialize)]
struct SondeData {
    links: Vec<Link>,
    total: usize,
}

async fn sonde_api(
    State(state): State<Arc<ViewerState>>,
    Query(filters): Query<SondeFilters>,
) -> Result<([(header::HeaderName, &'static str); 1], Json<SondeData>), StatusCode> {
    if ![filters.west, filters.east, filters.south, filters.north]
        .into_iter()
        .all(f64::is_finite)
        || filters.east < filters.west
        || filters.north < filters.south
        || !matches!(filters.agl, None | Some(1_000 | 3_000))
        || !matches!(
            filters.evidence.as_deref(),
            None | Some("sonde_snr_auto_rx" | "sonde_rssi_rdz")
        )
    {
        return Err(StatusCode::BAD_REQUEST);
    }
    let data = tokio::task::spawn_blocking(move || -> Result<SondeData> {
        let mut data = SondeData::default();
        let paths = capture_paths(&state.patterns, &mut Vec::new());
        for path in paths.iter().filter(|path| is_sonde(path)) {
            let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            db.busy_timeout(Duration::from_millis(500))?;
            let result = sonde_links(&db, &filters)?;
            data.total += result.total;
            data.links.extend(result.links);
        }
        data.links
            .sort_by_key(|link| std::cmp::Reverse((link.count, link.last)));
        data.links.truncate(LINE_LIMIT);
        Ok(data)
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    .map_err(|error| {
        eprintln!("Could not read SondeHub links · {error:#}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(data)))
}

fn sonde_links(db: &Connection, filters: &SondeFilters) -> Result<SondeData> {
    let since = filters.window.since();
    let span = filters.east - filters.west;
    let (west, east) = if span >= 360.0 {
        (-180.0, 180.0)
    } else {
        let west = (filters.west + 180.0).rem_euclid(360.0) - 180.0;
        (west, west + span)
    };
    let wrap = east > 180.0;
    let wrapped_east = east - 360.0;
    let east = east.min(180.0);
    let south = filters.south.clamp(-90.0, 90.0);
    let north = filters.north.clamp(-90.0, 90.0);
    let agl_bands = filters
        .agl
        .map(|height| if height == 1_000 { 1 } else { 2 });
    let mut query = db.prepare(include_str!("viewer_sonde.sql"))?;
    let mut rows = query.query(named_params! {
        ":west": west, ":east": east, ":south": south, ":north": north,
        ":since_ms": since * 1000, ":until_ms": Utc::now().timestamp_millis(),
        ":wrap": wrap, ":wrapped_east": wrapped_east,
        ":evidence": filters.evidence, ":agl_bands": agl_bands,
        ":hide_suspect": filters.hide_suspect, ":suspect_flags": SUSPECT_FLAGS,
        ":limit": LINE_LIMIT as i64,
    })?;
    let mut data = SondeData::default();
    while let Some(row) = rows.next()? {
        let evidence: String = row.get(8)?;
        let mean = row.get::<_, Option<f64>>(9)?;
        let flags: u32 = row.get(7)?;
        data.total = row.get::<_, i64>(12)? as usize;
        data.links.push(Link {
            source: 0,
            from: [row.get(2)?, row.get(3)?],
            to: [row.get(4)?, row.get(5)?],
            tx: row.get(0)?,
            rx: row.get(1)?,
            snr: if evidence == "sonde_snr_auto_rx" {
                mean
            } else {
                None
            },
            rssi: if evidence == "sonde_rssi_rdz" {
                mean
            } else {
                None
            },
            evidence,
            band: Some(SONDE_BAND.into()),
            preset: None,
            count: row.get(10)?,
            last: row.get::<_, i64>(11)?.div_euclid(1000),
            coarse: false,
            unknown_precision: true,
            suspect: flags & SUSPECT_FLAGS != 0,
            agl: row.get(6)?,
            quality_flags: Some(flags),
        });
    }
    Ok(data)
}
