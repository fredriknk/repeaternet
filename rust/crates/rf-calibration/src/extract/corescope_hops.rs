use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use rusqlite::Connection;

use super::{
    corescope::MAX_DETECTION_M,
    model::{Sample, Source},
    quality::distance,
};
use crate::dataset::Evidence;

pub(super) type LocatedKeys = BTreeMap<String, String>;

fn matches_prefix<'a>(
    keys: &'a LocatedKeys,
    prefix: &str,
) -> impl Iterator<Item = (&'a str, &'a str)> + use<'a> {
    let prefix = prefix.to_owned();
    keys.range(prefix.clone()..)
        .take_while(move |(key, _)| key.starts_with(&prefix))
        .map(|(key, id)| (key.as_str(), id.as_str()))
}

fn hex(value: &str) -> bool {
    value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[derive(Default)]
pub(super) struct RelayHops {
    seen: BTreeMap<(String, String), BTreeSet<String>>,
    samples: Vec<(String, String, i64)>,
}

impl RelayHops {
    pub fn observe(
        &mut self,
        source: &mut Source,
        keys: &LocatedKeys,
        packet: (&str, i64),
        prefixes: &str,
        path: Option<&[Option<String>]>,
        sender: Option<&str>,
    ) {
        let (hash, ms) = packet;
        let Ok(prefixes) = serde_json::from_str::<Vec<Option<String>>>(prefixes) else {
            source.count("relay_unresolved");
            return;
        };
        if prefixes.len() < 2 && sender.is_none() {
            return;
        }
        let Some(path) = path.filter(|path| path.len() == prefixes.len()) else {
            source.count("relay_unresolved");
            return;
        };
        if hash.is_empty() {
            source.count("relay_missing_packet_hash");
            return;
        }
        let mut previous = sender;
        for (prefix, key) in prefixes.iter().zip(path) {
            let next = match (prefix.as_deref(), key.as_deref()) {
                (Some(prefix), _) if prefix.len() == 2 && hex(prefix) => {
                    source.count("relay_one_byte_prefix");
                    None
                }
                (Some(prefix), Some(key))
                    if prefix.len() >= 4 && prefix.len().is_multiple_of(2) && hex(prefix) =>
                {
                    let prefix = prefix.to_ascii_lowercase();
                    let mut candidates = matches_prefix(keys, &prefix);
                    let first = candidates.next();
                    if candidates.next().is_some() {
                        source.count("relay_ambiguous_prefix");
                        None
                    } else if let Some((_, id)) =
                        first.filter(|(raw, _)| raw.eq_ignore_ascii_case(key))
                    {
                        Some(id)
                    } else {
                        source.count("relay_unresolved");
                        None
                    }
                }
                _ => {
                    source.count("relay_unresolved");
                    None
                }
            };
            if let Some((tx, rx)) = previous.zip(next) {
                if tx == rx {
                    source.count("relay_self_link");
                } else if self
                    .seen
                    .entry((tx.to_owned(), rx.to_owned()))
                    .or_default()
                    .insert(hash.to_owned())
                {
                    self.samples.push((tx.to_owned(), rx.to_owned(), ms));
                }
            }
            previous = next;
        }
    }

    pub fn append(self, source: &mut Source) {
        for (tx, rx, ms) in self.samples {
            add(source, &tx, &rx, Evidence::CorescopeRelayHop, ms, None);
        }
    }
}

fn has_table(db: &Connection, table: &str) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = ?)",
        [table],
        |row| row.get(0),
    )?)
}

pub(super) fn read_reports(
    db: &Connection,
    source: &mut Source,
    keys: &LocatedKeys,
    identity: &impl Fn(&str) -> String,
) -> Result<()> {
    source.counts.insert("trace_rows", 0);
    source.counts.insert("neighbor_report_rows", 0);
    if has_table(db, "trace_hops")? {
        read_traces(db, source, keys)?;
    }
    if has_table(db, "observer_neighbor_reports")? {
        read_neighbors(db, source, identity)?;
    }
    Ok(())
}

fn trace_pair<'a>(
    source: &Source,
    keys: &'a LocatedKeys,
    from: &str,
    to: &str,
    ms: i64,
) -> Option<(&'a str, &'a str)> {
    let candidates = |prefix| {
        matches_prefix(keys, prefix)
            .filter_map(|(_, id)| Some((id, source.nodes[id].position(ms, false)?.point)))
    };
    let mut resolved = None;
    for (tx, a) in candidates(from) {
        for (rx, b) in candidates(to) {
            if tx != rx && distance(a, b) <= MAX_DETECTION_M {
                if resolved.is_some() {
                    return None;
                }
                resolved = Some((tx, rx));
            }
        }
    }
    resolved
}

fn read_traces(db: &Connection, source: &mut Source, keys: &LocatedKeys) -> Result<()> {
    let mut query = db.prepare(
        "SELECT CAST(round(unixepoch(timestamp, 'subsec') * 1000) AS INTEGER) AS ms,
                hash_size, path_hashes, snr_values
         FROM trace_hops ORDER BY ms, endpoint, packet_id",
    )?;
    let mut rows = query.query([])?;
    while let Some(row) = rows.next()? {
        source.count("trace_rows");
        let Some(ms) = row.get::<_, Option<i64>>(0)? else {
            source.count("trace_invalid_time");
            continue;
        };
        let size: i64 = row.get(1)?;
        let path = serde_json::from_str::<Vec<String>>(&row.get::<_, String>(2)?);
        let snr = serde_json::from_str::<Vec<Option<f64>>>(&row.get::<_, String>(3)?);
        let (Ok(path), Ok(snr)) = (path, snr) else {
            source.count("trace_invalid_route");
            continue;
        };
        if !(1..=32).contains(&size)
            || path.len() != snr.len()
            || path
                .iter()
                .any(|hash| hash.len() != size as usize * 2 || !hex(hash))
        {
            source.count("trace_invalid_route");
            continue;
        }
        for (pair, snr) in path.windows(2).zip(snr.into_iter().skip(1)) {
            let Some(snr) = snr.filter(|v| v.is_finite()) else {
                source.count("trace_invalid_snr");
                continue;
            };
            let from = pair[0].to_ascii_lowercase();
            let to = pair[1].to_ascii_lowercase();
            if let Some((tx, rx)) = trace_pair(source, keys, &from, &to, ms) {
                add(source, tx, rx, Evidence::CorescopeTraceHop, ms, Some(snr));
            } else {
                source.count("trace_unresolved");
            }
        }
    }
    Ok(())
}

fn read_neighbors(
    db: &Connection,
    source: &mut Source,
    identity: &impl Fn(&str) -> String,
) -> Result<()> {
    let mut query = db.prepare(
        "SELECT CAST(round((coalesce(unixepoch(reported_at, 'subsec'),
                                    unixepoch(received_at, 'subsec'))
                            - max(0, coalesce(heard_secs_ago, 0))) * 1000) AS INTEGER) AS ms,
                neighbor_pubkey, observer_id, snr
         FROM observer_neighbor_reports ORDER BY ms, endpoint, observer_id, received_at, neighbor_pubkey",
    )?;
    let mut rows = query.query([])?;
    while let Some(row) = rows.next()? {
        source.count("neighbor_report_rows");
        let (Some(ms), Some(snr)) = (
            row.get::<_, Option<i64>>(0)?,
            row.get::<_, Option<f64>>(3)?.filter(|v| v.is_finite()),
        ) else {
            source.count("neighbor_report_invalid_time_or_snr");
            continue;
        };
        let tx = identity(&row.get::<_, String>(1)?);
        let rx = identity(&row.get::<_, String>(2)?);
        if tx == rx {
            source.count("neighbor_report_self_link");
        } else {
            add(
                source,
                &tx,
                &rx,
                Evidence::CorescopeNeighborReport,
                ms,
                Some(snr),
            );
        }
    }
    Ok(())
}

fn add(source: &mut Source, tx: &str, rx: &str, evidence: Evidence, ms: i64, snr: Option<f64>) {
    if !source.nodes.contains_key(tx) || !source.nodes.contains_key(rx) {
        source.count("receptions_missing_position");
        return;
    }
    source.add_sample(
        tx,
        rx,
        evidence,
        Sample {
            ms,
            rssi: None,
            snr,
            region: None,
        },
        &[],
    );
}
