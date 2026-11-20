use std::path::Path;
use std::time::Duration;

use anyhow::{Result, ensure};
use chrono::{SecondsFormat, Utc};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::Value;

use crate::model::{Packet, number, observer_key, snapshot_json};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cursor {
    pub completed_until: Option<i64>,
    pub window_until: Option<i64>,
    pub next_offset: usize,
    pub last_packet_id: Option<i64>,
}

pub struct Run {
    pub id: i64,
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

pub fn open(path: &Path) -> Result<Connection> {
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
        1 => {
            transaction.prepare("SELECT endpoint, window_until FROM packet_cursor")?;
        }
        _ => anyhow::bail!("Unsupported CoreScope capture schema version {version}"),
    }
    transaction.commit()?;
    Ok(db)
}

impl Run {
    pub fn start(db: &Connection, endpoint: &str, mode: &str) -> Result<Self> {
        db.execute(
            "INSERT INTO capture_runs(endpoint, mode, started_at) VALUES (?, ?, ?)",
            params![endpoint, mode, now()],
        )?;
        Ok(Self {
            id: db.last_insert_rowid(),
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
        for packet in packets {
            let advert = packet.advert();
            let first_seen = packet
                .first_seen()?
                .to_rfc3339_opts(SecondsFormat::Millis, true);
            let receptions = packet.receptions();
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
