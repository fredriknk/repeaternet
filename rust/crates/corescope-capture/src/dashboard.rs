use std::{
    collections::HashMap,
    fs,
    net::TcpListener,
    path::PathBuf,
    sync::{Arc, Mutex},
    thread::{self, JoinHandle},
    time::Duration,
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
use rusqlite::{
    Connection, OpenFlags, OptionalExtension,
    functions::{Aggregate, Context as SqlContext, FunctionFlags},
    params,
};
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

use crate::{Bbox, db};

#[cfg(test)]
#[path = "dashboard_tests.rs"]
mod tests;

const LEAFLET_JS: &str = include_str!("../../web-server/assets/leaflet/leaflet.js");
const LEAFLET_CSS: &str = include_str!("../../web-server/assets/leaflet/leaflet.css");

pub struct Dashboard {
    stop: Option<oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Dashboard {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub fn start(
    path: PathBuf,
    endpoint: &str,
    host: &str,
    bbox: Bbox,
    port: u16,
) -> Result<Dashboard> {
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).context(
        "Bind the capture dashboard. Choose another --dashboard-port or use --no-dashboard.",
    )?;
    listener.set_nonblocking(true)?;
    let address = listener.local_addr()?;
    let app = router(path, endpoint, host, bbox)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()?;
    let (stop, shutdown) = oneshot::channel();
    let thread = thread::Builder::new()
        .name("capture-dashboard".into())
        .spawn(move || {
            let result = runtime.block_on(async {
                axum::serve(tokio::net::TcpListener::from_std(listener)?, app)
                    .with_graceful_shutdown(async {
                        let _ = shutdown.await;
                    })
                    .await
            });
            if let Err(error) = result {
                eprintln!("Dashboard stopped · {error}");
            }
        })?;
    println!("Live map · http://{address}/");
    Ok(Dashboard {
        stop: Some(stop),
        thread: Some(thread),
    })
}

struct StateData {
    path: PathBuf,
    endpoint: String,
    bbox: Bbox,
    reader: Mutex<Reader>,
}

fn router(path: PathBuf, endpoint: &str, host: &str, bbox: Bbox) -> Result<Router> {
    let db = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    db.busy_timeout(Duration::from_secs(5))?;
    db.create_aggregate_function(
        "capture_median",
        1,
        FunctionFlags::SQLITE_DETERMINISTIC,
        Median,
    )?;
    let state = Arc::new(StateData {
        path,
        endpoint: endpoint.into(),
        bbox,
        reader: Mutex::new(Reader { db, cache: None }),
    });
    let host = host
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    let page = include_str!("dashboard.html").replace("{{host}}", &host);
    Ok(Router::new()
        .route("/", get(move || async move { Html(page) }))
        .route(
            "/leaflet.js",
            get(|| async { ([(header::CONTENT_TYPE, "text/javascript")], LEAFLET_JS) }),
        )
        .route(
            "/leaflet.css",
            get(|| async { ([(header::CONTENT_TYPE, "text/css")], LEAFLET_CSS) }),
        )
        .route("/api/map", get(api_map))
        .with_state(state))
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Window {
    #[default]
    Day,
    Week,
    All,
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Hops {
    #[default]
    Both,
    Zero,
    Relayed,
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
struct Filters {
    window: Window,
    hops: Hops,
    min_count: u32,
}

async fn api_map(
    State(state): State<Arc<StateData>>,
    Query(filters): Query<Filters>,
) -> Result<([(header::HeaderName, &'static str); 1], Json<MapData>), StatusCode> {
    let result = tokio::task::spawn_blocking(move || load(&state, filters)).await;
    match result {
        Ok(Ok(data)) => Ok(([(header::CACHE_CONTROL, "no-store")], Json(data))),
        Ok(Err(error)) => {
            eprintln!("Dashboard database read failed · {error:#}");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

#[derive(Serialize)]
struct MapData {
    bbox: Bbox,
    stats: Stats,
    map: Arc<MapLayers>,
}

#[derive(Serialize)]
struct Stats {
    trace_hops: i64,
    neighbor_report_rows: i64,
    receptions: i64,
    packets: i64,
    pairs: usize,
    located_pairs: usize,
    bbox_pairs: usize,
    observers: usize,
    located_observers: usize,
    nodes: usize,
    capture_started: Option<i64>,
    last_poll: Option<i64>,
    next_poll: Option<i64>,
    backoff_until: Option<i64>,
    backoff_reason: Option<String>,
    requests_24h: i64,
    db_bytes: u64,
}

#[derive(Default, Serialize)]
struct MapLayers {
    points: Vec<Point>,
    pairs: Vec<Link>,
}

// Wire format uses point indices so coordinates and labels appear once.
#[derive(Serialize)]
struct Point(String, f64, f64, bool); // name, lat, lon, is observer

#[derive(Serialize)]
struct Link(
    usize,
    usize,
    i64,
    Option<f64>,
    Option<f64>,
    f64,
    Option<i64>,
);
//             transmitter, observer, count, median SNR, median RSSI, km, last seen

struct Reader {
    db: Connection,
    cache: Option<MapCache>,
}

struct MapCache {
    revision: i64,
    cutoff: Option<i64>,
    filters: Filters,
    map: Arc<MapLayers>,
    totals: Totals,
}

#[derive(Clone, Copy, Default)]
struct Totals {
    trace_hops: i64,
    neighbor_report_rows: i64,
    receptions: i64,
    packets: i64,
    pairs: usize,
    located_pairs: usize,
    bbox_pairs: usize,
    observers: usize,
    located_observers: usize,
    nodes: usize,
}

fn load(state: &StateData, filters: Filters) -> Result<MapData> {
    let now = Utc::now().timestamp();
    let cutoff = match filters.window {
        Window::Day => Some(now / 60 * 60 - 86_400),
        Window::Week => Some(now / 60 * 60 - 7 * 86_400),
        Window::All => None,
    };
    let mut reader = state
        .reader
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let revision = reader
        .db
        .pragma_query_value(None, "data_version", |row| row.get(0))?;
    if !reader.cache.as_ref().is_some_and(|cache| {
        cache.revision == revision && cache.cutoff == cutoff && cache.filters == filters
    }) {
        let previous_totals = reader
            .cache
            .as_ref()
            .filter(|cache| cache.revision == revision)
            .map(|cache| cache.totals);
        let db = reader.db.transaction()?;
        let (map, totals) = layers(
            &db,
            &state.endpoint,
            state.bbox,
            filters,
            cutoff,
            previous_totals,
        )?;
        db.commit()?;
        reader.cache = Some(MapCache {
            revision,
            cutoff,
            filters,
            map: Arc::new(map),
            totals,
        });
    }
    let cache = reader.cache.as_ref().expect("map cache populated");
    let endpoint = &state.endpoint;
    let (last_poll, next_poll, backoff_until, backoff_reason) = reader.db.query_row(
        "SELECT last_poll, next_poll, backoff_until, backoff_reason FROM watch_state WHERE endpoint = ?",
        [endpoint],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    ).optional()?.unwrap_or_default();
    let capture_started = reader.db.query_row(
        "SELECT unixepoch(min(started_at)) FROM capture_runs WHERE endpoint = ?",
        [endpoint],
        |row| row.get(0),
    )?;
    let mut wal_path = state.path.as_os_str().to_owned();
    wal_path.push("-wal");
    let db_bytes =
        fs::metadata(&state.path)?.len() + fs::metadata(wal_path).map_or(0, |info| info.len());
    let totals = cache.totals;
    Ok(MapData {
        bbox: state.bbox,
        map: Arc::clone(&cache.map),
        stats: Stats {
            receptions: totals.receptions,
            trace_hops: totals.trace_hops,
            neighbor_report_rows: totals.neighbor_report_rows,
            packets: totals.packets,
            pairs: totals.pairs,
            located_pairs: totals.located_pairs,
            bbox_pairs: totals.bbox_pairs,
            observers: totals.observers,
            located_observers: totals.located_observers,
            nodes: totals.nodes,
            capture_started,
            last_poll,
            next_poll,
            backoff_until,
            backoff_reason,
            requests_24h: db::requests_today(&reader.db, endpoint, Utc::now().timestamp_millis())?,
            db_bytes,
        },
    })
}

fn location(lat: Option<f64>, lon: Option<f64>) -> Option<[f64; 2]> {
    let (lat, lon) = (lat?, lon?);
    ((lat != 0.0 || lon != 0.0) && (-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon))
        .then_some([lat, lon])
}

fn point_index(
    points: &mut Vec<Point>,
    name: Option<String>,
    coords: [f64; 2],
    observer: bool,
) -> usize {
    let index = points.len();
    let fallback = if observer {
        "Unnamed observer"
    } else {
        "Unnamed node"
    };
    points.push(Point(
        name.filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| fallback.into()),
        coords[0],
        coords[1],
        observer,
    ));
    index
}

fn layers(
    db: &Connection,
    endpoint: &str,
    bbox: Bbox,
    filters: Filters,
    cutoff: Option<i64>,
    previous_totals: Option<Totals>,
) -> Result<(MapLayers, Totals)> {
    let mut map = MapLayers::default();
    let mut nodes: HashMap<String, usize> = HashMap::new();
    let mut observers = HashMap::new();
    let mut inventory = db.prepare_cached(include_str!("dashboard_inventory.sql"))?;
    let inventory = inventory.query_map([endpoint], |row| {
        Ok((
            row.get::<_, bool>(0)?,
            row.get::<_, String>(1)?,
            row.get(2)?,
            location(row.get(3)?, row.get(4)?),
            row.get::<_, Option<String>>(5)?,
        ))
    })?;
    let mut totals = Totals::default();
    for row in inventory {
        let (is_observer, id, name, coords, public_key) = row?;
        if is_observer {
            totals.observers += 1;
            let coords = coords.or_else(|| {
                let &index = nodes.get(public_key.as_deref().unwrap_or(&id))?;
                let Point(_, lat, lon, _) = map.points[index];
                Some([lat, lon])
            });
            if let Some(coords) = coords {
                totals.located_observers += 1;
                observers.insert(id, point_index(&mut map.points, name, coords, true));
            }
        } else {
            totals.nodes += 1;
            if let Some(coords) = coords {
                nodes.insert(id, point_index(&mut map.points, name, coords, false));
            }
        }
    }
    let locate = |from: &str, to: &str| -> Option<(usize, usize)> {
        let from = nodes.get(from).or_else(|| observers.get(from))?;
        let to = observers.get(to)?;
        Some((*from, *to))
    };
    let (receptions, packets) = if let Some(previous) = previous_totals {
        (previous.receptions, previous.packets)
    } else {
        db.query_row(
            "SELECT count(*), count(DISTINCT packet_id) FROM observations WHERE endpoint = ?",
            [endpoint],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?
    };
    totals.receptions = receptions;
    totals.packets = packets;
    (totals.trace_hops, totals.neighbor_report_rows) = db.query_row(
        "SELECT
           (SELECT coalesce(sum(json_array_length(snr_values)), 0) FROM trace_hops WHERE endpoint = ?1),
           (SELECT count(*) FROM observer_neighbor_reports WHERE endpoint = ?1)",
        [endpoint],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if let Some(previous) = previous_totals {
        totals.pairs = previous.pairs;
        totals.located_pairs = previous.located_pairs;
        totals.bbox_pairs = previous.bbox_pairs;
    } else {
        let mut all = db.prepare_cached(
            "SELECT last_hop, observer_id FROM measured_hops WHERE endpoint = ?
             GROUP BY last_hop, observer_id",
        )?;
        let mut rows = all.query([endpoint])?;
        while let Some(row) = rows.next()? {
            totals.pairs += 1;
            if let Some((from, to)) = locate(row.get_ref(0)?.as_str()?, row.get_ref(1)?.as_str()?) {
                totals.located_pairs += 1;
                let inside = |index| {
                    let Point(_, lat, lon, _) = map.points[index];
                    (bbox.south..=bbox.north).contains(&lat)
                        && (bbox.west..=bbox.east).contains(&lon)
                };
                totals.bbox_pairs += usize::from(inside(from) && inside(to));
            }
        }
    }

    // Only the time predicate varies so SQLite can choose the appropriate index.
    let time_predicate = if cutoff.is_some() {
        "AND seen_unix >= ?2"
    } else {
        ""
    };
    let sql = format!(
        "SELECT last_hop, observer_id, count(*), capture_median(snr), capture_median(rssi),
                max(seen_unix)
         FROM measured_hops WHERE endpoint = ?1 {time_predicate}
         AND (?3 IS NULL OR zero_hop = ?3)
         GROUP BY last_hop, observer_id HAVING count(*) >= ?4"
    );
    let hops = match filters.hops {
        Hops::Both => None,
        Hops::Zero => Some(1),
        Hops::Relayed => Some(0),
    };
    let mut query = db.prepare_cached(&sql)?;
    let mut rows = query.query(params![endpoint, cutoff, hops, filters.min_count.max(1)])?;
    while let Some(row) = rows.next()? {
        let Some((from, to)) = locate(row.get_ref(0)?.as_str()?, row.get_ref(1)?.as_str()?) else {
            continue;
        };
        map.pairs.push(Link(
            from,
            to,
            row.get(2)?,
            row.get(3)?,
            row.get(4)?,
            distance_km(&map.points[from], &map.points[to]),
            row.get(5)?,
        ));
    }
    Ok((map, totals))
}

fn distance_km(from: &Point, to: &Point) -> f64 {
    let (lat1, lat2) = (from.1.to_radians(), to.1.to_radians());
    let angular = ((lat2 - lat1) / 2.0).sin().powi(2)
        + lat1.cos() * lat2.cos() * ((to.2 - from.2).to_radians() / 2.0).sin().powi(2);
    (2.0 * 6371.0088 * angular.clamp(0.0, 1.0).sqrt().asin() * 100.0).round() / 100.0
}

struct Median;

impl Aggregate<Vec<f64>, Option<f64>> for Median {
    fn init(&self, _: &mut SqlContext<'_>) -> rusqlite::Result<Vec<f64>> {
        Ok(Vec::new())
    }

    fn step(&self, ctx: &mut SqlContext<'_>, values: &mut Vec<f64>) -> rusqlite::Result<()> {
        if let Some(value) = ctx.get::<Option<f64>>(0)?.filter(|value| value.is_finite()) {
            values.push(value);
        }
        Ok(())
    }

    fn finalize(
        &self,
        _: &mut SqlContext<'_>,
        values: Option<Vec<f64>>,
    ) -> rusqlite::Result<Option<f64>> {
        let Some(mut values) = values.filter(|values| !values.is_empty()) else {
            return Ok(None);
        };
        let midpoint = values.len() / 2;
        let even = values.len().is_multiple_of(2);
        let (lower, &mut upper, _) = values.select_nth_unstable_by(midpoint, f64::total_cmp);
        if even {
            let low = lower
                .iter()
                .copied()
                .max_by(f64::total_cmp)
                .expect("even nonempty sample");
            Ok(Some(low.midpoint(upper)))
        } else {
            Ok(Some(upper))
        }
    }
}
