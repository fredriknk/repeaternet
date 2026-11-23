use std::path::Path;
use std::time::Duration;

use anyhow::{Result, ensure};
use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::Value;

use crate::{
    Bbox,
    model::{Packet, number, observer_key, public_key, snapshot_json, string},
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cursor {
    pub completed_until: Option<i64>,
    pub window_until: Option<i64>,
    pub next_offset: usize,
    pub last_packet_id: Option<i64>,
}

pub struct Run {
    pub id: i64,
    pub mode: &'static str,
    pub version: Option<String>,
    pub packet_pages: usize,
    pub observations: usize,
    pub nodes: usize,
    pub observers: usize,
    pub neighbors: usize,
    pub capped: bool,
    pub errors: Vec<String>,
}

pub enum Snapshot {
    Nodes,
    Observers,
    Neighbors,
}

pub fn now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

pub fn open(path: &Path, endpoint: &str) -> Result<Connection> {
    let mut db = Connection::open(path)?;
    db.busy_timeout(Duration::from_secs(5))?;
    db.pragma_update(None, "foreign_keys", true)?;
    let transaction = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let version: u32 = transaction.pragma_query_value(None, "user_version", |row| row.get(0))?;
    match version {
        0 => {
            let objects: i64 = transaction.query_row(
                "SELECT count(*) FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%'",
                [],
                |row| row.get(0),
            )?;
            ensure!(objects == 0, "Select an empty CoreScope capture database");
            transaction.execute_batch(include_str!("schema.sql"))?;
        }
        1 | 2 => {
            transaction.prepare("SELECT endpoint, window_until FROM packet_cursor")?;
        }
        _ => anyhow::bail!("Unsupported CoreScope capture schema version {version}"),
    }
    if version < 2 {
        transaction.execute_batch(include_str!("watch_schema.sql"))?;
    }
    bind_endpoint(&transaction, endpoint)?;
    transaction.execute_batch(include_str!("signal_schema.sql"))?;
    transaction.commit()?;
    db.pragma_update(None, "journal_mode", "WAL")?;
    Ok(db)
}

fn bind_endpoint(db: &Connection, endpoint: &str) -> Result<()> {
    // Keep the version 2 layout readable by earlier capture binaries.
    db.execute_batch(include_str!("instance_schema.sql"))?;
    let stored: Option<String> = db
        .query_row("SELECT base_url FROM capture_instance", [], |row| {
            row.get(0)
        })
        .optional()?;
    if let Some(stored) = stored {
        ensure!(
            stored == endpoint,
            "This capture database belongs to {stored}. Use a different --db for {endpoint}."
        );
        return Ok(());
    }

    // Check indexed endpoints from every legacy writer. Snapshot tables
    // inherit their endpoint from capture_runs via run_id.
    let (first, last): (Option<String>, Option<String>) = db.query_row(
        "WITH endpoints AS (
            SELECT (SELECT min(endpoint) FROM capture_runs) AS lo,
                   (SELECT max(endpoint) FROM capture_runs) AS hi
            UNION ALL SELECT (SELECT min(endpoint) FROM observations),
                             (SELECT max(endpoint) FROM observations)
            UNION ALL SELECT (SELECT min(endpoint) FROM packet_cursor),
                             (SELECT max(endpoint) FROM packet_cursor)
            UNION ALL SELECT (SELECT min(endpoint) FROM request_log),
                             (SELECT max(endpoint) FROM request_log)
            UNION ALL SELECT (SELECT min(endpoint) FROM watch_state),
                             (SELECT max(endpoint) FROM watch_state)
        ) SELECT min(lo), max(hi) FROM endpoints",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    ensure!(
        first == last,
        "This capture database already contains multiple CoreScope instances. Use a different --db."
    );
    if let Some(stored) = first {
        ensure!(
            stored == endpoint,
            "This capture database belongs to {stored}. Use a different --db for {endpoint}."
        );
    }
    db.execute("INSERT INTO capture_instance VALUES (1, ?)", [endpoint])?;
    Ok(())
}

impl Run {
    pub fn start(db: &Connection, endpoint: &str, mode: &'static str) -> Result<Self> {
        db.execute(
            "INSERT INTO capture_runs(endpoint, mode, started_at) VALUES (?, ?, ?)",
            params![endpoint, mode, now()],
        )?;
        Ok(Self {
            id: db.last_insert_rowid(),
            mode,
            version: None,
            packet_pages: 0,
            observations: 0,
            nodes: 0,
            observers: 0,
            neighbors: 0,
            capped: false,
            errors: Vec::new(),
        })
    }

    pub fn finish(&self, db: &Connection, requests: usize, stopped: bool) -> Result<&'static str> {
        let status = if stopped {
            "stopped"
        } else if !self.errors.is_empty() {
            "failed"
        } else if self.capped {
            "capped"
        } else {
            "complete"
        };
        db.execute(
            "UPDATE capture_runs SET ended_at = ?, status = ?, server_version = ?,
             requests = ?, packet_pages = ?, observations_added = ?, nodes = ?, observers = ?,
             neighbor_responses = ?, capped = ?, errors_json = ? WHERE id = ?",
            params![
                now(),
                status,
                self.version,
                requests as i64,
                self.packet_pages as i64,
                self.observations as i64,
                self.nodes as i64,
                self.observers as i64,
                self.neighbors as i64,
                self.capped,
                serde_json::to_string(&self.errors)?,
                self.id
            ],
        )?;
        Ok(status)
    }
}

pub fn latest_topology(db: &Connection, endpoint: &str) -> Result<Option<i64>> {
    Ok(db
        .query_row(
            "SELECT unixepoch(ended_at) FROM capture_runs WHERE endpoint = ?
         AND mode IN ('watch-topology', 'snapshot', 'run') AND status = 'complete'
         ORDER BY id DESC LIMIT 1",
            [endpoint],
            |row| row.get(0),
        )
        .optional()?
        .flatten())
}

pub fn cursor(db: &Connection, endpoint: &str) -> Result<Cursor> {
    Ok(db
        .query_row(
            "SELECT completed_until, window_until, next_offset, last_packet_id
             FROM packet_cursor WHERE endpoint = ?",
            [endpoint],
            |row| {
                Ok(Cursor {
                    completed_until: row.get(0)?,
                    window_until: row.get(1)?,
                    next_offset: row.get::<_, i64>(2)? as usize,
                    last_packet_id: row.get(3)?,
                })
            },
        )
        .optional()?
        .unwrap_or_default())
}

pub fn save_page(
    db: &mut Connection,
    endpoint: &str,
    previous: Cursor,
    next: Cursor,
    packets: &[Packet],
) -> Result<usize> {
    let transaction = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    ensure!(
        cursor(&transaction, endpoint)? == previous,
        "Another capture advanced the packet cursor"
    );
    let mut added = 0;
    {
        let mut insert = transaction.prepare_cached(
            "INSERT INTO observations VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24
             ) ON CONFLICT DO NOTHING",
        )?;
        let mut enrich = transaction.prepare_cached(
            "UPDATE observations SET resolved_path = COALESCE(?4, resolved_path),
                observation_count = COALESCE(?5, observation_count),
                scope_name = COALESCE(?6, scope_name)
             WHERE endpoint = ?1 AND packet_id = ?2 AND observation_key = ?3",
        )?;
        let mut insert_trace = transaction.prepare_cached(
            "INSERT INTO trace_hops VALUES (?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT DO NOTHING",
        )?;
        let mut save_signal = transaction.prepare_cached(
            "INSERT INTO reception_signal VALUES (?, ?, ?, ?, ?)
             ON CONFLICT DO UPDATE SET radio = COALESCE(excluded.radio, radio),
                                       score = COALESCE(excluded.score, score)",
        )?;
        for packet in packets {
            let advert = packet.advert();
            let first_seen = packet
                .first_seen()?
                .to_rfc3339_opts(SecondsFormat::Millis, true);
            let receptions = packet.receptions();
            if let Some(trace) = packet.trace_hops() {
                insert_trace.execute(params![
                    endpoint,
                    packet.id,
                    packet.hash,
                    packet.trace_observer(),
                    first_seen,
                    trace.hash_size as i64,
                    serde_json::to_string(&trace.path_hashes)?,
                    serde_json::to_string(&trace.snr_values)?,
                ])?;
            }
            if receptions.iter().any(|reception| reception.id.is_some()) {
                transaction.execute(
                    "DELETE FROM observations WHERE endpoint = ? AND packet_id = ?
                     AND observation_id IS NULL",
                    params![endpoint, packet.id],
                )?;
            }
            for reception in receptions {
                let observer = reception.observer_id.as_deref().map(observer_key);
                let key = if let Some(id) = reception.id {
                    format!("id:{id}")
                } else {
                    let has_expanded: bool = transaction.query_row(
                        "SELECT EXISTS(SELECT 1 FROM observations
                         WHERE endpoint = ? AND packet_id = ? AND observation_id IS NOT NULL)",
                        params![endpoint, packet.id],
                        |row| row.get(0),
                    )?;
                    if has_expanded {
                        continue;
                    }
                    format!("observer:{}", observer.as_deref().unwrap_or_default())
                };
                let resolved = reception.resolved_path();
                let inserted = insert.execute(params![
                    endpoint,
                    packet.id,
                    key,
                    reception.id,
                    packet.hash,
                    observer,
                    reception.observer_name,
                    reception.observer_iata,
                    reception.timestamp.as_deref().unwrap_or(&first_seen),
                    first_seen,
                    reception.rssi,
                    reception.snr,
                    reception.path_prefixes(),
                    resolved,
                    packet.route_type,
                    packet.payload_type,
                    reception.direction,
                    packet.observation_count,
                    packet.scope_name,
                    advert.as_ref().map(|value| &value.key),
                    advert.as_ref().and_then(|value| value.lat),
                    advert.as_ref().and_then(|value| value.lon),
                    advert.as_ref().and_then(|value| value.flags),
                    advert.as_ref().and_then(|value| value.name.as_deref()),
                ])?;
                added += inserted;
                if reception.radio.is_some() || reception.score.is_some() {
                    save_signal.execute(params![
                        endpoint,
                        packet.id,
                        key,
                        reception.radio,
                        reception.score
                    ])?;
                }
                if inserted == 0 {
                    enrich.execute(params![
                        endpoint,
                        packet.id,
                        key,
                        resolved,
                        packet.observation_count,
                        packet.scope_name,
                    ])?;
                }
            }
        }
    }
    transaction.execute(
        "INSERT INTO packet_cursor VALUES (?, ?, ?, ?, ?)
         ON CONFLICT(endpoint) DO UPDATE SET completed_until = excluded.completed_until,
            window_until = excluded.window_until, next_offset = excluded.next_offset,
            last_packet_id = excluded.last_packet_id",
        params![
            endpoint,
            next.completed_until,
            next.window_until,
            i64::try_from(next.next_offset)?,
            next.last_packet_id
        ],
    )?;
    transaction.commit()?;
    Ok(added)
}

pub fn observer_report_batch<'a>(
    db: &Connection,
    endpoint: &str,
    observers: &'a [Value],
    limit: usize,
) -> Result<Vec<(&'a str, DateTime<Utc>)>> {
    let mut query = db.prepare(
        "SELECT received_at FROM observer_neighbor_checkpoints WHERE endpoint = ? AND observer_id = ?",
    )?;
    let mut batch = Vec::new();
    for observer in observers {
        let Some(id) = string(observer, "id") else {
            continue;
        };
        let Some(advertised) = string(observer, "last_neighbors_report_at")
            .or_else(|| string(observer, "neighbors_report_at"))
            .and_then(|time| time.parse::<DateTime<Utc>>().ok())
        else {
            continue;
        };
        let stored: Option<String> = query
            .query_row(params![endpoint, observer_key(id)], |row| row.get(0))
            .optional()?;
        if stored
            .and_then(|time| time.parse::<DateTime<Utc>>().ok())
            .is_none_or(|time| advertised > time)
        {
            batch.push((id, advertised));
        }
    }
    batch.sort_unstable_by_key(|&(id, advertised)| (advertised, id));
    batch.truncate(limit);
    Ok(batch)
}

pub fn save_observer_report(
    db: &mut Connection,
    endpoint: &str,
    observer: &str,
    advertised: DateTime<Utc>,
    detail: &Value,
) -> Result<()> {
    let observer = observer_key(observer);
    let transaction = db.transaction()?;
    let mut checkpoint = advertised;
    if let Some(report) = detail
        .get("neighbor_report")
        .filter(|report| !report.is_null())
    {
        let received_at = string(report, "received_at")
            .ok_or_else(|| anyhow::anyhow!("CoreScope neighbor report has no received time"))?;
        checkpoint = received_at.parse()?;
        let mut insert = transaction.prepare_cached(
            "INSERT INTO observer_neighbor_reports VALUES (?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT DO NOTHING",
        )?;
        for neighbor in crate::model::records(report, "neighbors")? {
            let Some(pubkey) = string(neighbor, "pubkey").and_then(public_key) else {
                continue;
            };
            insert.execute(params![
                endpoint,
                observer,
                received_at,
                string(report, "reported_at"),
                pubkey,
                number(neighbor, "snr"),
                neighbor["heard_secs_ago"].as_i64(),
                string(neighbor, "status"),
            ])?;
        }
    }
    transaction.execute(
        "INSERT INTO observer_neighbor_checkpoints VALUES (?, ?, ?)
         ON CONFLICT DO UPDATE SET received_at = max(received_at, excluded.received_at)",
        params![
            endpoint,
            observer,
            checkpoint.to_rfc3339_opts(SecondsFormat::Nanos, true)
        ],
    )?;
    transaction.commit()?;
    Ok(())
}

pub fn log_request(db: &Connection, endpoint: &str, neighbor: Option<&str>) -> Result<()> {
    db.execute(
        "INSERT INTO request_log VALUES (?, ?, ?)",
        params![endpoint, Utc::now().timestamp_millis(), neighbor],
    )?;
    Ok(())
}

pub fn requests_today(db: &Connection, endpoint: &str, now_ms: i64) -> Result<i64> {
    Ok(db.query_row(
        "SELECT count(*) FROM request_log WHERE endpoint = ? AND sent_ms > ?",
        params![endpoint, now_ms - 86_400_000],
        |row| row.get(0),
    )?)
}

pub fn pair_count(db: &Connection, endpoint: &str) -> Result<i64> {
    Ok(db.query_row(
        "SELECT count(*) FROM (
            SELECT last_hop, observer_id FROM measured_hops WHERE endpoint = ?
            GROUP BY last_hop, observer_id
        )",
        [endpoint],
        |row| row.get(0),
    )?)
}

pub fn neighbor_batch(
    db: &Connection,
    endpoint: &str,
    bbox: Bbox,
    now_ms: i64,
    limit: usize,
    refresh_ms: i64,
) -> Result<Vec<String>> {
    const DAILY_NEIGHBOR_LIMIT: usize = 300;
    let threshold = now_ms - refresh_ms;
    let used: i64 = db.query_row(
        "SELECT count(*) FROM request_log WHERE endpoint = ? AND sent_ms > ? AND neighbor IS NOT NULL",
        params![endpoint, now_ms - 86_400_000],
        |row| row.get(0),
    )?;
    let limit = limit.min(DAILY_NEIGHBOR_LIMIT.saturating_sub(used as usize));
    if limit == 0 {
        return Ok(Vec::new());
    }
    let mut query = db.prepare(
        "SELECT n.pubkey, MAX(
             COALESCE((SELECT max(q.sent_ms) FROM request_log q
                       WHERE q.endpoint = :endpoint AND q.neighbor = n.pubkey
                         AND q.neighbor IS NOT NULL), 0),
             COALESCE((SELECT unixepoch(max(h.captured_at), 'subsec') * 1000
                       FROM neighbors_snapshot h JOIN capture_runs r ON r.id = h.run_id
                       WHERE r.endpoint = :endpoint AND h.source_pubkey = n.pubkey), 0)
         ) AS refreshed
         FROM nodes_snapshot n
         WHERE n.run_id = (
             SELECT id FROM capture_runs WHERE endpoint = :endpoint
               AND mode IN ('watch-topology', 'snapshot', 'run')
               AND status = 'complete' ORDER BY id DESC LIMIT 1
         ) AND n.lat BETWEEN :south AND :north AND n.lon BETWEEN :west AND :east
           AND (n.lat <> 0 OR n.lon <> 0)
           AND refreshed <= :threshold
         ORDER BY refreshed, n.pubkey LIMIT :limit",
    )?;
    let rows = query.query_map(
        rusqlite::named_params! {
            ":endpoint": endpoint, ":south": bbox.south, ":north": bbox.north,
            ":west": bbox.west, ":east": bbox.east,
            ":threshold": threshold, ":limit": limit as i64,
        },
        |row| row.get(0),
    )?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn save_snapshot<'a>(
    db: &mut Connection,
    table: Snapshot,
    run_id: i64,
    records: impl IntoIterator<Item = (&'a str, &'a Value)>,
) -> Result<usize> {
    let captured_at = now();
    let transaction = db.transaction()?;
    let mut count = 0;
    for (key, value) in records {
        let raw_json = snapshot_json(value);
        count += match table {
            Snapshot::Neighbors => transaction.execute(
                "INSERT OR IGNORE INTO neighbors_snapshot VALUES (?, ?, ?, ?)",
                params![run_id, captured_at, key, raw_json],
            )?,
            Snapshot::Nodes | Snapshot::Observers => {
                let sql = match table {
                    Snapshot::Nodes => {
                        "INSERT OR IGNORE INTO nodes_snapshot VALUES (?, ?, ?, ?, ?, ?)"
                    }
                    _ => "INSERT OR IGNORE INTO observers_snapshot VALUES (?, ?, ?, ?, ?, ?)",
                };
                transaction.execute(
                    sql,
                    params![
                        run_id,
                        captured_at,
                        key,
                        number(value, "lat"),
                        number(value, "lon"),
                        raw_json
                    ],
                )?
            }
        };
    }
    transaction.commit()?;
    Ok(count)
}
