use anyhow::{Context, Result, ensure};
use chrono::{DateTime, SecondsFormat, Utc};
use repeaternet_providers::{
    ProviderError,
    corescope::capture::{CaptureClient, Endpoint},
};
use rusqlite::Connection;
use serde_json::Value;

use crate::{
    Bbox,
    db::{self, Cursor, Run, Snapshot},
    model::{observer_key, packet_page, public_key, records, string},
};

const NODE_LIMIT: usize = 500;
const PACKET_LIMIT: &str = "250";
const OVERLAP_SECONDS: i64 = 15 * 60;
const OBSERVER_REPORT_LIMIT: usize = 60;

pub fn get(
    db: &Connection,
    client: &mut CaptureClient,
    endpoint: Endpoint<'_>,
    query: &[(&str, &str)],
    stopped: &dyn Fn() -> bool,
) -> Result<Value> {
    let base = client.base_url().to_owned();
    Ok(client.get(endpoint, query, stopped, |neighbor| {
        db::log_request(db, &base, neighbor).map_err(|error| std::io::Error::other(error).into())
    })?)
}

pub fn topology(
    db: &mut Connection,
    client: &mut CaptureClient,
    max_pages: usize,
    run: &mut Run,
    stopped: &dyn Fn() -> bool,
) -> Result<()> {
    let stats = get(db, client, Endpoint::Stats, &[], stopped)?;
    run.version = string(&stats, "version").map(str::to_owned);

    let response = get(db, client, Endpoint::Observers, &[], stopped)?;
    let observers = records(&response, "observers")?;
    let keyed = observers
        .iter()
        .map(|observer| {
            let id = string(observer, "id").context("CoreScope observer has no ID")?;
            Ok((observer_key(id), observer))
        })
        .collect::<Result<Vec<_>>>()?;
    run.observers = db::save_snapshot(
        db,
        Snapshot::Observers,
        run.id,
        keyed.iter().map(|(key, value)| (key.as_str(), *value)),
    )?;

    let response = get(
        db,
        client,
        Endpoint::Graph,
        &[("min_count", "1"), ("min_score", "0")],
        stopped,
    )?;
    records(&response, "nodes")?;
    records(&response, "edges")?;
    run.neighbors = db::save_snapshot(db, Snapshot::Neighbors, run.id, [("", &response)])?;
    nodes(db, client, max_pages, run, stopped)?;
    let base = client.base_url().to_owned();
    for (id, advertised) in db::observer_report_batch(db, &base, observers, OBSERVER_REPORT_LIMIT)?
    {
        let detail = match get(db, client, Endpoint::Observer(id), &[], stopped) {
            Ok(detail) => detail,
            Err(error)
                if matches!(
                    error.downcast_ref(),
                    Some(ProviderError::Http { status: 404, .. })
                ) =>
            {
                Value::Null
            }
            Err(error) => {
                return Err(error).context("Fetching a CoreScope observer neighbor report");
            }
        };
        db::save_observer_report(db, &base, id, advertised, &detail)?;
    }
    Ok(())
}

pub fn neighbors(
    db: &mut Connection,
    client: &mut CaptureClient,
    bbox: Bbox,
    limit: usize,
    refresh_ms: i64,
    run: &mut Run,
    stopped: &dyn Fn() -> bool,
) -> Result<()> {
    let batch = db::neighbor_batch(
        db,
        client.base_url(),
        bbox,
        Utc::now().timestamp_millis(),
        limit,
        refresh_ms,
    )?;
    for key in batch {
        let response = match get(db, client, Endpoint::Neighbors(&key), &[], stopped) {
            Ok(response) => response,
            Err(error)
                if matches!(
                    error.downcast_ref(),
                    Some(ProviderError::Http { status: 404, .. })
                ) =>
            {
                continue;
            }
            Err(error) => return Err(error).with_context(|| format!("Neighbors for {key}")),
        };
        records(&response, "neighbors")?;
        run.neighbors +=
            db::save_snapshot(db, Snapshot::Neighbors, run.id, [(key.as_str(), &response)])?;
    }
    Ok(())
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
    let lower_bound = cursor.completed_until.or_else(|| {
        (cursor.window_until.is_none() && run.mode == "watch-packets").then_some(until)
    });
    let since_text = lower_bound
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
        let response = get(db, client, Endpoint::Packets, &query, stopped)?;
        run.packet_pages += 1;
        let page = packet_page(response, offset)?;
        let mut next = Cursor {
            completed_until: lower_bound,
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

fn nodes(
    db: &mut Connection,
    client: &mut CaptureClient,
    max_pages: usize,
    run: &mut Run,
    stopped: &dyn Fn() -> bool,
) -> Result<()> {
    let limit = NODE_LIMIT.to_string();
    let mut offset = 0;
    for _ in 0..max_pages {
        let response = get(
            db,
            client,
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
