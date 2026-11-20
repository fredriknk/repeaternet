use std::collections::BTreeSet;

use anyhow::{Context, Result, ensure};
use chrono::{DateTime, SecondsFormat, Utc};
use repeaternet_providers::{
    ProviderError,
    corescope::capture::{CaptureClient, Endpoint},
};
use rusqlite::Connection;
use serde_json::Value;

use crate::{
    Bbox, Mode,
    db::{self, Cursor, Run, Snapshot},
    model::{number, observer_key, packet_page, public_key, records, string},
};

const NODE_LIMIT: usize = 500;
const PACKET_LIMIT: &str = "250";
const OVERLAP_SECONDS: i64 = 15 * 60;

pub fn run_once(
    db: &mut Connection,
    client: &mut CaptureClient,
    mode: Mode,
    bbox: Bbox,
    max_pages: usize,
    stopped: &dyn Fn() -> bool,
) -> Result<bool> {
    let request_start = client.requests();
    let mut run = Run::start(db, client.base_url(), mode.as_str())?;

    let result = client
        .get(Endpoint::Stats, &[], stopped)
        .map_err(Into::into)
        .and_then(|value| {
            run.version = Some(
                string(&value, "version")
                    .context("CoreScope did not report a version")?
                    .to_owned(),
            );
            Ok(())
        });
    record_error(&mut run, "/api/stats", result);

    if mode != Mode::Snapshot && !stopped() {
        let result = packets(db, client, max_pages, &mut run, stopped);
        record_error(&mut run, "/api/packets", result);
    }
    if mode != Mode::Packets && !stopped() {
        snapshot(db, client, bbox, max_pages, &mut run, stopped);
    }

    let status = run.finish(db, client.requests() - request_start, stopped())?;
    for error in &run.errors {
        eprintln!("{error}");
    }
    println!(
        "Capture {} {status}. {} observations added, {} nodes, {} observers, {} neighbor responses.",
        run.id, run.observations, run.nodes, run.observers, run.neighbors
    );
    Ok(run.errors.is_empty())
}

fn record_error(run: &mut Run, endpoint: &str, result: Result<()>) {
    if let Err(error) = result
        && !matches!(error.downcast_ref(), Some(ProviderError::Cancelled))
    {
        run.errors.push(format!("{endpoint} — {error:#}"));
    }
}

pub fn packets(
    db: &mut Connection,
    client: &mut CaptureClient,
    max_pages: usize,
    run: &mut Run,
    stopped: &dyn Fn() -> bool,
) -> Result<()> {
    let endpoint = client.base_url().to_owned();
    let mut cursor = db::cursor(db, &endpoint)?;
    let until = cursor
        .window_until
        .unwrap_or_else(|| Utc::now().timestamp());
    let until_text = api_time(until)?;
    let since_text = cursor
        .completed_until
        .map(|time| api_time(time - OVERLAP_SECONDS))
        .transpose()?;

    // CoreScope sorts by ingest ID but filters by first-seen time. Freeze the
    // filter window across capped runs, and verify an overlapping packet ID
    // before advancing an offset against a changing server-side list.
    for _ in 0..max_pages {
        let offset = cursor.next_offset.saturating_sub(1);
        let offset_text = offset.to_string();
        let mut query = vec![
            ("order", "desc"),
            ("groupByHash", "false"),
            ("expand", "observations"),
            ("limit", PACKET_LIMIT),
            ("offset", &offset_text),
            ("until", &until_text),
        ];
        if let Some(since) = &since_text {
            query.push(("since", since));
        }
        let response = client.get(Endpoint::Packets, &query, stopped)?;
        run.packet_pages += 1;
        let page = packet_page(response, offset)?;
        let mut next = Cursor {
            window_until: Some(until),
            ..cursor
        };
        let start = match cursor.last_packet_id {
            None => 0,
            Some(id) => match page.packets.iter().position(|packet| packet.id == id) {
                Some(index) => index,
                None => {
                    // If the anchor moved past the returned slice or expired,
                    // replay the bounded window. Existing rows are idempotent.
                    next.next_offset = 0;
                    next.last_packet_id = None;
                    db::save_page(db, &endpoint, cursor, next, &[])?;
                    cursor = next;
                    continue;
                }
            },
        };
        let consumed = offset + page.packets.len();
        let caught_up = consumed >= page.total || page.packets.is_empty();
        if caught_up {
            next = Cursor {
                completed_until: Some(until),
                ..Cursor::default()
            };
        } else {
            next.next_offset = consumed;
            next.last_packet_id = page.packets.last().map(|packet| packet.id);
        }
        run.observations += db::save_page(db, &endpoint, cursor, next, &page.packets[start..])?;
        if caught_up {
            return Ok(());
        }
        cursor = next;
    }
    run.capped = true;
    Ok(())
}

fn api_time(seconds: i64) -> Result<String> {
    Ok(DateTime::from_timestamp(seconds, 0)
        .context("Invalid packet cursor time")?
        .to_rfc3339_opts(SecondsFormat::Secs, true))
}

fn snapshot(
    db: &mut Connection,
    client: &mut CaptureClient,
    bbox: Bbox,
    max_pages: usize,
    run: &mut Run,
    stopped: &dyn Fn() -> bool,
) {
    let result = client
        .get(Endpoint::Observers, &[], stopped)
        .map_err(Into::into)
        .and_then(|response| {
            let observers = records(&response, "observers")?;
            let keyed = observers
                .iter()
                .map(|observer| {
                    let id = string(observer, "id").context("CoreScope observer has no ID")?;
                    Ok((observer_key(id), observer))
                })
                .collect::<Result<Vec<_>>>()?;
            run.observers += db::save_snapshot(
                db,
                Snapshot::Observers,
                run.id,
                keyed.iter().map(|(key, value)| (key.as_str(), *value)),
            )?;
            Ok(())
        });
    record_error(run, "/api/observers", result);

    let result = client
        .get(
            Endpoint::Graph,
            &[("min_count", "1"), ("min_score", "0")],
            stopped,
        )
        .map_err(Into::into)
        .and_then(|response| {
            records(&response, "nodes")?;
            records(&response, "edges")?;
            run.neighbors += db::save_snapshot(db, Snapshot::Neighbors, run.id, [("", &response)])?;
            Ok(())
        });
    record_error(run, "/api/analytics/neighbor-graph", result);

    let mut local_nodes = BTreeSet::new();
    let result = nodes(db, client, bbox, max_pages, run, &mut local_nodes, stopped);
    record_error(run, "/api/nodes", result);
    for key in local_nodes {
        let result: Result<()> = client
            .get(Endpoint::Neighbors(&key), &[], stopped)
            .map_err(Into::into)
            .and_then(|response| {
                records(&response, "neighbors")?;
                run.neighbors += db::save_snapshot(
                    db,
                    Snapshot::Neighbors,
                    run.id,
                    [(key.as_str(), &response)],
                )?;
                Ok(())
            });
        let keep_fetching = result.is_ok()
            || matches!(
                result.as_ref().err().and_then(|error| error.downcast_ref()),
                Some(ProviderError::Http { status: 404, .. })
            );
        if result.is_err() {
            record_error(run, &format!("/api/nodes/{key}/neighbors"), result);
        }
        if !keep_fetching {
            break;
        }
    }
}

fn nodes(
    db: &mut Connection,
    client: &mut CaptureClient,
    bbox: Bbox,
    max_pages: usize,
    run: &mut Run,
    local_nodes: &mut BTreeSet<String>,
    stopped: &dyn Fn() -> bool,
) -> Result<()> {
    let limit = NODE_LIMIT.to_string();
    let mut offset = 0;
    for _ in 0..max_pages {
        let response = client.get(
            Endpoint::Nodes,
            &[
                ("limit", &limit),
                ("offset", &offset.to_string()),
                ("sortBy", "name"),
            ],
            stopped,
        )?;
        let nodes = records(&response, "nodes")?;
        ensure!(
            nodes.len() <= NODE_LIMIT,
            "CoreScope ignored the node page limit"
        );
        let keyed = nodes
            .iter()
            .map(|node| {
                let key = string(node, "public_key")
                    .or_else(|| string(node, "pubkey"))
                    .and_then(public_key)
                    .context("CoreScope node has no valid public key")?;
                Ok((key, node))
            })
            .collect::<Result<Vec<(_, &Value)>>>()?;
        run.nodes += db::save_snapshot(
            db,
            Snapshot::Nodes,
            run.id,
            keyed.iter().map(|(key, node)| (key.as_str(), *node)),
        )?;
        local_nodes.extend(keyed.into_iter().filter_map(|(key, node)| {
            bbox.contains(number(node, "lat")?, number(node, "lon")?)
                .then_some(key)
        }));
        let more = response
            .get("has_more")
            .and_then(Value::as_bool)
            .unwrap_or_else(|| {
                response
                    .get("total")
                    .and_then(Value::as_u64)
                    .map_or(nodes.len() == NODE_LIMIT, |total| {
                        ((offset + nodes.len()) as u64) < total
                    })
            });
        if !more {
            return Ok(());
        }
        offset += NODE_LIMIT;
    }
    run.capped = true;
    Ok(())
}
