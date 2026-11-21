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
use rusqlite::{Connection, OpenFlags, params};
use serde::{Deserialize, Serialize};
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

pub fn directory_glob(dir: &Path) -> String {
    PathBuf::from(glob::Pattern::escape(&dir.to_string_lossy()))
        .join("*capture*.sqlite3")
        .to_string_lossy()
        .into_owned()
}

pub fn start(patterns: Vec<String>, port: u16) -> Result<Viewer> {
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

struct ViewerState {
    patterns: Vec<String>,
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
        let data = Arc::new(load(&state.patterns, filters.window));
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
}

fn load(patterns: &[String], window: Window) -> MapData {
    let now = Utc::now().timestamp();
    let since = match window {
        Window::Day => now - 86_400,
        Window::Week => now - 7 * 86_400,
        Window::All => 0,
    };
    let mut data = MapData::default();
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
                        Err(error) => data.warnings.push(error.to_string()),
                    }
                }
            }
            Err(error) => data.warnings.push(error.to_string()),
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
