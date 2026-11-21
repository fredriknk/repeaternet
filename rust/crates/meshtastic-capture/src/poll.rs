use std::{
    path::Path,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Result, ensure};
use chrono::{DateTime, SecondsFormat, Utc};
use reqwest::StatusCode;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use sondehub_import::{GB, database_bytes};

use crate::{
    Run, db,
    sources::{self, Api},
    upstream::{Http, HttpStatus, jitter, wait},
};

const HOUR: i64 = 3_600_000;
const NODE_INTERVAL: Duration = Duration::from_secs(6 * 3600);
const STATUS_INTERVAL: Duration = Duration::from_secs(60);
const PAGE_CAP: usize = 3;
const SEEN_PER_PORT: usize = 5;

pub struct Source {
    pub host: &'static str,
    api: Api,
    region_hint: Option<&'static str>,
}

impl Source {
    const fn new(host: &'static str, api: Api, region_hint: Option<&'static str>) -> Self {
        Self {
            host,
            api,
            region_hint,
        }
    }

    fn nodes_path(&self) -> &'static str {
        if self.api == Api::Meshview {
            "/api/nodes"
        } else {
            "/api/v1/nodes"
        }
    }
}

pub const SOURCES: &[Source] = &[
    Source::new("map.868.no", Api::Norway, None),
    Source::new("meshtastic.liamcottle.net", Api::Liam, None),
    Source::new("karta.meshat.se", Api::Liam, None),
    Source::new("meshtastic.valzzu.xyz", Api::Liam, None),
    Source::new("meshmap2.lucifernet.com", Api::Liam, None),
    Source::new("map.meshnet.si", Api::Liam, None),
    Source::new("meshview.format.ovh", Api::Meshview, None),
    Source::new("meshview.lsinfra.de", Api::Meshview, None),
    Source::new("meshview.bayme.sh", Api::Meshview, Some("US")),
    Source::new("meshview.socalmesh.org", Api::Meshview, Some("US")),
    Source::new("meshview.nyme.sh", Api::Meshview, Some("US")),
    Source::new("meshinfo.cvme.sh", Api::MeshInfo, Some("US")),
];

pub fn run(args: &Run, dir: &Path, stop: &(dyn Fn() -> bool + Sync)) -> Result<()> {
    let sources: Vec<_> = SOURCES
        .iter()
        .filter(|s| {
            (args.only.is_empty() || args.only.iter().any(|h| h == s.host))
                && !args.skip.iter().any(|h| h == s.host)
        })
        .collect();
    ensure!(!sources.is_empty(), "Enable at least one capture source");
    let failures = thread::scope(|scope| {
        let workers: Vec<_> = sources
            .into_iter()
            .map(|source| {
                (
                    source.host,
                    scope.spawn(move || host(source, args, dir, stop)),
                )
            })
            .collect();
        workers
            .into_iter()
            .map(|(host, worker)| match worker.join() {
                Ok(Ok(())) => 0,
                Ok(Err(err)) => {
                    eprintln!("{host} · stopped · {err:#}");
                    1
                }
                Err(_) => {
                    eprintln!("{host} · capture worker panicked");
                    1
                }
            })
            .sum::<usize>()
    });
    ensure!(failures == 0, "{failures} capture worker(s) failed");
    Ok(())
}

fn host(source: &Source, args: &Run, dir: &Path, stop: &dyn Fn() -> bool) -> Result<()> {
    let ceiling = (args.total_budget_gb * GB) as u64;
    let budget_reached = || -> Result<bool> {
        let bytes = database_bytes(dir)?;
        if bytes < ceiling {
            return Ok(false);
        }
        println!(
            "{} · stopped at database budget · {bytes} bytes in use · {ceiling} bytes allowed",
            source.host
        );
        Ok(true)
    };
    if budget_reached()? {
        return Ok(());
    }
    let path = dir.join(format!("meshtastic-capture-{}.sqlite3", source.host));
    let (mut db, _lock) = db::open(&path, source.host)?;
    let mut http = Http::new(
        source.host,
        Duration::from_millis(args.min_interval_ms),
        args.daily_budget,
        stop,
    )?;
    println!("{} · {}", source.host, path.display());
    if let Err(error) = http.robots(&db) {
        eprintln!("{} · stopped at robots.txt · {error:#}", source.host);
        status(source, &db, &http, "stopped")?;
        return Ok(());
    }
    let now = Instant::now();
    let mut nodes_due = Some(now);
    let mut checked_nodes = false;
    let mut checked_packets = source.api == Api::Liam;
    let mut capabilities_due = (source.api == Api::Norway).then_some(now);
    let mut packets_due = (source.api != Api::Liam).then_some(now);
    let mut status_due = now + STATUS_INTERVAL;
    let packet_interval = Duration::from_secs(if source.api == Api::Norway { 900 } else { 1800 });
    let mut last_error = false;

    while !stop() && !http.disabled {
        if budget_reached()? {
            return Ok(());
        }
        let now = Instant::now();
        if now >= status_due {
            status(source, &db, &http, "running")?;
            status_due = now + STATUS_INTERVAL;
        }
        if http.resume_ms > Utc::now().timestamp_millis() {
            if args.once {
                break;
            }
            wait(Duration::from_secs(1), stop);
            continue;
        }
        let result = if nodes_due.is_some_and(|due| now >= due) {
            checked_nodes = true;
            let query = match source.api {
                Api::MeshInfo => vec![("days", "30".into()), ("slim", "1".into())],
                Api::Meshview => vec![("days_active", "30".into())],
                _ => vec![],
            };
            let result = http.json(&mut db, source.nodes_path(), &query, true, |db, value| {
                db::store(
                    db,
                    source.host,
                    sources::nodes(source.api, value, Utc::now().timestamp_millis())?,
                )
            });
            nodes_due = if !args.once {
                Some(Instant::now() + jitter(NODE_INTERVAL, NODE_INTERVAL))
            } else {
                None
            };
            result.map(|_| ())
        } else if capabilities_due.take().is_some() {
            http.json(&mut db, "/api/v1/capabilities", &[], true, |_, _| Ok(()))
                .map(|_| ())
        } else if packets_due.is_some_and(|due| now >= due) {
            checked_packets = true;
            let result = match source.api {
                Api::Norway => norway(source, &mut db, &mut http),
                Api::MeshInfo => meshinfo(source, &mut db, &mut http),
                Api::Meshview => meshview(source, &mut db, &mut http),
                Api::Liam => unreachable!(),
            };
            packets_due = if !args.once {
                Some(Instant::now() + jitter(packet_interval, packet_interval))
            } else {
                None
            };
            result
        } else {
            if args.once {
                break;
            }
            wait(Duration::from_secs(1), stop);
            continue;
        };
        last_error = result.is_err() && !stop();
        if let Err(error) = result {
            if last_error {
                if http.resume_ms <= Utc::now().timestamp_millis() {
                    http.backoff(Duration::ZERO);
                }
                eprintln!("{} · {error:#}", source.host);
            }
        } else {
            http.success();
            if checked_nodes && checked_packets && capabilities_due.is_none() && http.requests == 1
            {
                return status(source, &db, &http, "capture paths disallowed by robots.txt");
            }
        }
    }
    let state = if http.disabled {
        "stopped after three HTTP 403/404/5xx responses"
    } else if last_error {
        "cycle ended early"
    } else {
        "finished"
    };
    status(source, &db, &http, state)
}

fn status(source: &Source, db: &Connection, http: &Http, state: &str) -> Result<()> {
    let (measurements, links) = db::stats(db)?;
    let backoff = DateTime::from_timestamp_millis(http.resume_ms)
        .filter(|time| *time > Utc::now())
        .map_or_else(
            || "none".into(),
            |time| time.to_rfc3339_opts(SecondsFormat::Secs, true),
        );
    println!(
        "{} · {} requests · {measurements} measurements · {links} located links · backoff {backoff} · {state}",
        source.host, http.requests
    );
    Ok(())
}

#[derive(Deserialize, Serialize)]
struct NorwayWindow {
    start: i64,
    end: i64,
    cursor: Option<String>,
}

fn norway(source: &Source, db: &mut Connection, http: &mut Http) -> Result<()> {
    let now = Utc::now().timestamp_millis();
    let mut window = db::state(db, "norway_window")?
        .map(|s| serde_json::from_str::<NorwayWindow>(&s))
        .transpose()?
        .filter(|w| w.cursor.is_some() && w.start >= now - 6 * HOUR)
        .unwrap_or(NorwayWindow {
            start: now - HOUR,
            end: now,
            cursor: None,
        });
    let date = |ms| -> Result<String> {
        Ok(DateTime::from_timestamp_millis(ms)
            .ok_or_else(|| anyhow::anyhow!("Invalid observation window"))?
            .to_rfc3339_opts(SecondsFormat::Millis, true))
    };
    for _ in 0..PAGE_CAP {
        let mut query = vec![
            ("start", date(window.start)?),
            ("end", date(window.end)?),
            ("since", date(window.start + 15 * 60_000)?),
            ("limit", "2000".into()),
        ];
        if let Some(cursor) = &window.cursor {
            query.push(("cursor", cursor.clone()));
        }
        let request = http.json(db, "/api/v1/observations", &query, false, |db, value| {
            let next = value["next_cursor"].as_str().filter(|s| !s.is_empty());
            let more = value["more"] == true;
            ensure!(
                !more || next.is_some_and(|cursor| Some(cursor) != window.cursor.as_deref()),
                "Observation cursor did not advance"
            );
            db::store(db, source.host, sources::norway(value)?)?;
            window.cursor = next.filter(|_| more).map(str::to_owned);
            db::set_state(db, "norway_window", &serde_json::to_string(&window)?)
        });
        if window.cursor.is_some()
            && request
                .as_ref()
                .err()
                .and_then(|error| error.downcast_ref::<HttpStatus>())
                .is_some_and(|status| {
                    matches!(
                        status.code,
                        StatusCode::BAD_REQUEST | StatusCode::CONFLICT | StatusCode::GONE
                    )
                })
        {
            window.cursor = None;
            db::set_state(db, "norway_window", &serde_json::to_string(&window)?)?;
        }
        let requested = request?;
        if !requested || window.cursor.is_none() {
            return Ok(());
        }
    }
    println!("{} · observation page cap reached", source.host);
    Ok(())
}

fn meshinfo(source: &Source, db: &mut Connection, http: &mut Http) -> Result<()> {
    let mut before = None::<String>;
    for _ in 0..PAGE_CAP {
        let mut query = vec![
            ("envelope", "1".into()),
            ("range", "24h".into()),
            ("slim", "1".into()),
            ("limit", "200".into()),
        ];
        if let Some(cursor) = &before {
            query.push(("before", cursor.clone()));
        }
        let requested = http.json(db, "/api/v1/traceroutes", &query, false, |db, value| {
            db::store(db, source.host, sources::meshinfo(value)?)?;
            let next = value["next_cursor"].as_str().filter(|s| !s.is_empty());
            ensure!(
                next.is_none() || next != before.as_deref(),
                "Traceroute cursor did not advance"
            );
            before = next.map(str::to_owned);
            Ok(())
        })?;
        if !requested || before.is_none() {
            return Ok(());
        }
    }
    println!("{} · traceroute page cap reached", source.host);
    Ok(())
}

fn meshview(source: &Source, db: &mut Connection, http: &mut Http) -> Result<()> {
    for port in [70, 71] {
        let now = Utc::now().timestamp_millis();
        let query = [
            ("portnum", port.to_string()),
            ("limit", "120".into()),
            ("since", ((now - HOUR) * 1000).to_string()),
        ];
        let mut packets = Vec::new();
        http.json(db, "/api/packets", &query, false, |db, value| {
            let (batch, listed) = sources::meshview_packets(value, port, source.region_hint)?;
            packets = listed;
            db::store(db, source.host, batch)
        })?;
        let mut seen = 0;
        for packet in &packets {
            if seen == SEEN_PER_PORT {
                break;
            }
            // Remember completed lookups through the overlap without unbounded state.
            let key = format!("seen:{}", packet.id());
            if db::state(db, &key)?.is_some() || packet.time() < now - HOUR {
                continue;
            }
            if !http.packet_seen(db, packet.id(), |db, value| {
                db::store(
                    db,
                    source.host,
                    sources::meshview_seen(value, packet, source.region_hint)?,
                )?;
                db::set_state(db, &key, &now.to_string())
            })? {
                break;
            }
            seen += 1;
        }
        db.execute(
            "DELETE FROM poll_state WHERE name LIKE 'seen:%' AND CAST(value AS INTEGER) < ?1",
            [now - 2 * HOUR],
        )?;
    }
    Ok(())
}
