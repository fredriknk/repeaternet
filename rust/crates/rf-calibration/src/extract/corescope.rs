use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result};
use rusqlite::Connection;
use serde_json::Value;

use super::{
    model::{Metadata, Position, Sample, Source, TimedPosition, radio_basis},
    privacy::node_id,
    quality::{distance, point},
};
use crate::dataset::{Detection, Evidence, Radio};

const ACTIVE_MS: i64 = 120_000;
const MAX_DETECTION_M: f64 = 100_000.0;
const MIN_MISSES: u32 = 20;
const DETECTION_BASIS: &str = "distinct_packet_hashes_per_tx_with_fully_resolved_or_zero_hop_witness; rx reception within ±120 s of the first sighting; endpoints within 100 km at first sighting; unknown transmitter at rx excluded unless heard is confirmed";

pub(super) const NEIGHBOR_NOTE: &str = "CoreScope neighbors_snapshot omitted. The upstream graph averages observer SNR assigned to graph edges, including inferred origin-to-first-relay edges, and can pool both directions. It does not establish receiver SNR for a directed hop.";

pub(super) fn read(db: &Connection, salt: &str) -> Result<Source> {
    let base: String = db.query_row("SELECT base_url FROM capture_instance", [], |row| {
        row.get(0)
    })?;
    let url = url::Url::parse(&base).context("invalid CoreScope capture URL")?;
    let host = url.host_str().context("missing CoreScope capture host")?;
    let mut source = Source::new(format!("corescope:{}", host.to_ascii_lowercase()), false);
    let identity = |id: &str| node_id(salt, "corescope", id);
    read_snapshots(db, &mut source, &identity)?;
    for (counter, sql) in [
        (
            "neighbor_graph_snapshots",
            "SELECT count(*) FROM neighbors_snapshot WHERE source_pubkey = ''",
        ),
        (
            "neighbor_node_snapshots",
            "SELECT count(*) FROM neighbors_snapshot WHERE source_pubkey <> ''",
        ),
    ] {
        source.counts.insert(
            counter,
            db.query_row(sql, [], |row| row.get::<_, i64>(0))? as u64,
        );
    }

    let mut query = db.prepare(
        "SELECT
            CAST(round(unixepoch(timestamp, 'subsec') * 1000) AS INTEGER) AS seen_ms,
            CAST(round(unixepoch(first_seen, 'subsec') * 1000) AS INTEGER),
            packet_hash, observer_id, rssi, snr,
            CASE WHEN json_valid(path_prefixes) AND json_type(path_prefixes) = 'array'
                 THEN json_array_length(path_prefixes) END,
            resolved_path, sender_pubkey, advert_lat, advert_lon, direction,
            route_type,
            CASE WHEN json_valid(path_prefixes) AND json_type(path_prefixes) = 'array'
                 THEN length(json_extract(path_prefixes, '$[#-1]')) END
         FROM observations ORDER BY seen_ms, endpoint, packet_id, observation_key",
    )?;
    let mut rows = query.query([])?;
    let mut activity: BTreeMap<String, Vec<i64>> = BTreeMap::new();
    let mut packets: BTreeMap<String, Packet> = BTreeMap::new();
    let mut samples = Vec::new();
    let mut identities = BTreeMap::new();
    let mut id = |raw: String| {
        identities
            .entry(raw)
            .or_insert_with_key(|raw| identity(raw))
            .clone()
    };
    while let Some(row) = rows.next()? {
        source.count("observation_rows");
        let Some(ms) = row.get::<_, Option<i64>>(0)? else {
            source.count("observations_invalid_time");
            continue;
        };
        let sender = row
            .get::<_, Option<String>>(8)?
            .filter(|id| !id.is_empty())
            .map(&mut id);
        if let Some(sender) = &sender {
            let node = source.nodes.entry(sender.clone()).or_default();
            let lat: Option<f64> = row.get(9)?;
            let lon: Option<f64> = row.get(10)?;
            if let Some((lat, lon)) = lat.zip(lon) {
                node.saw_default |= lat == 0.0 && lon == 0.0;
                if let Some(point) = point(lat, lon) {
                    node.positions.push(TimedPosition {
                        ms,
                        position: position(point, "advert"),
                    });
                }
            }
        }
        if row.get::<_, Option<String>>(11)?.as_deref() != Some("rx") {
            continue;
        }
        let hash: String = row.get(2)?;
        let first_seen = row.get::<_, Option<i64>>(1)?.unwrap_or(ms);
        let packet = (!hash.is_empty()).then(|| {
            let packet = packets
                .entry(hash)
                .or_insert_with(|| Packet::new(first_seen));
            packet.first_ms = packet.first_ms.min(first_seen);
            packet
        });
        let Some(rx) = row
            .get::<_, Option<String>>(3)?
            .filter(|id| !id.is_empty())
            .map(&mut id)
        else {
            source.count("receptions_missing_observer");
            continue;
        };
        activity.entry(rx.clone()).or_default().push(ms);
        source.nodes.entry(rx.clone()).or_default();
        let rssi = row
            .get::<_, Option<f64>>(4)?
            .filter(|v| v.is_finite() && *v < 0.0);
        let snr = row.get::<_, Option<f64>>(5)?.filter(|v| v.is_finite());
        let valid_metrics = rssi.is_some() && snr.is_some();
        let prefix_count = row
            .get::<_, Option<i64>>(6)?
            .and_then(|n| usize::try_from(n).ok());
        let path: Option<Vec<Option<String>>> = row
            .get::<_, Option<String>>(7)?
            .and_then(|json| serde_json::from_str(&json).ok());
        // MeshCore flood packets (route types 0 and 1) accumulate the relays
        // that already forwarded them, so the final entry transmitted the
        // copy this observer heard. Direct packets carry the route ahead and
        // each relay strips itself, so their last relay is unknown. A
        // zero-hop advert is a direct packet with an empty path: its sender
        // is the transmitter whatever the route type.
        let route_type: Option<i64> = row.get(12)?;
        if prefix_count != Some(0) && !matches!(route_type, Some(0 | 1)) {
            if valid_metrics {
                source.count("receptions_direct_route_unattributed");
            }
            if let Some(packet) = packet {
                packet.unknown_tx.insert(rx);
            }
            continue;
        }
        let last_prefix_hex: Option<i64> = row.get(13)?;
        let zero_hop = prefix_count == Some(0);
        let (tx, evidence, complete) = if zero_hop {
            (sender, Evidence::CorescopeZeroHop, true)
        } else {
            let complete = path.as_ref().is_some_and(|p| {
                prefix_count == Some(p.len())
                    && p.iter()
                        .all(|key| key.as_ref().is_some_and(|key| !key.is_empty()))
            });
            let last = path
                .and_then(|mut p| p.pop().flatten())
                .filter(|id| !id.is_empty())
                .map(&mut id);
            (last, Evidence::CorescopeLastHop, complete)
        };
        let Some(tx) = tx else {
            if valid_metrics {
                source.count(if zero_hop {
                    "receptions_missing_sender"
                } else {
                    "receptions_unresolved_last_hop"
                });
            }
            if let Some(packet) = packet {
                packet.unknown_tx.insert(rx);
            }
            continue;
        };
        if tx == rx {
            if valid_metrics {
                source.count("receptions_self_link");
            }
            continue;
        }
        source.nodes.entry(tx.clone()).or_default();
        if let Some(packet) = packet {
            let heard = packet.transmitters.entry(tx.clone()).or_default();
            heard.receivers.insert(rx.clone());
            heard.complete_witness |= complete;
        }
        if valid_metrics {
            if !complete {
                source.count("receptions_resolved_last_hop_incomplete_path");
            }
            samples.push((
                tx,
                rx,
                evidence,
                Sample {
                    ms,
                    rssi,
                    snr,
                    region: None,
                },
                // A one-byte relay hash (two hex digits) is often ambiguous.
                !zero_hop && last_prefix_hex.is_some_and(|len| len <= 2),
            ));
        }
    }
    source.prepare_nodes();
    for (tx, rx, evidence, sample, short_prefix) in samples {
        let flags: &[&'static str] = if short_prefix {
            &["short_relay_prefix"]
        } else {
            &[]
        };
        source.add_sample(&tx, &rx, evidence, sample, flags);
    }
    attach_detection(&mut source, &activity, packets);
    Ok(source)
}

fn position(point: repeaternet_terrain::LonLat, source: &'static str) -> Position {
    Position {
        point,
        source,
        precision: None,
        bits: None,
        altitude: None,
    }
}

fn read_snapshots(
    db: &Connection,
    source: &mut Source,
    id: &impl Fn(&str) -> String,
) -> Result<()> {
    for (table, id_column, kind) in [
        ("nodes_snapshot", "pubkey", "node_snapshot"),
        ("observers_snapshot", "observer_id", "observer_snapshot"),
    ] {
        let mut query = db.prepare(&format!(
            "SELECT {id_column}, CAST(round(unixepoch(captured_at, 'subsec') * 1000) AS INTEGER),
                    lat, lon, raw_json FROM {table} ORDER BY captured_at, run_id, {id_column}"
        ))?;
        let mut rows = query.query([])?;
        while let Some(row) = rows.next()? {
            let raw: String = row.get(0)?;
            if raw.is_empty() {
                continue;
            }
            let Some(ms) = row.get::<_, Option<i64>>(1)? else {
                source.count("snapshots_invalid_time");
                continue;
            };
            let node = source.nodes.entry(id(&raw)).or_default();
            let lat: Option<f64> = row.get(2)?;
            let lon: Option<f64> = row.get(3)?;
            if let Some((lat, lon)) = lat.zip(lon) {
                node.saw_default |= lat == 0.0 && lon == 0.0;
                if let Some(point) = point(lat, lon) {
                    node.positions.push(TimedPosition {
                        ms,
                        position: position(point, kind),
                    });
                }
            }
            let raw_json: String = row.get(4)?;
            let json: Value =
                serde_json::from_str(&raw_json).context("invalid CoreScope snapshot metadata")?;
            let observer = kind == "observer_snapshot";
            let role = json[if observer { "nodeRole" } else { "role" }]
                .as_str()
                .map(str::to_ascii_lowercase)
                .filter(|r| {
                    matches!(
                        r.as_str(),
                        "companion" | "repeater" | "room" | "sensor" | "observer"
                    ) || r
                        .strip_prefix("type-")
                        .is_some_and(|n| n.parse::<u8>().is_ok())
                });
            let noise_floor = json["noise_floor"].as_f64().filter(|v| v.is_finite());
            let metadata = Metadata {
                role,
                radio: parse_radio(&json, kind),
                noise_floor,
            };
            if observer {
                node.observer_metadata.push((ms, metadata));
            } else {
                node.metadata.push((ms, metadata));
            }
        }
    }
    Ok(())
}

fn parse_radio(json: &Value, basis: &str) -> Radio {
    let mut radio = Radio::default();
    if let Some(config) = json["radio"].as_str() {
        let parts: Vec<_> = config.split(',').map(str::trim).collect();
        if let [freq, bw, sf, cr] = parts[..] {
            radio.freq_mhz = freq
                .parse::<f64>()
                .ok()
                .filter(|v| v.is_finite() && *v > 0.0);
            radio.bw_khz = bw.parse::<f64>().ok().filter(|v| v.is_finite() && *v > 0.0);
            radio.sf = sf.parse().ok().filter(|sf| (5..=12).contains(sf));
            radio.cr = cr.parse().ok().filter(|cr| (5..=8).contains(cr));
            for (field, present) in [
                ("freq_mhz", radio.freq_mhz.is_some()),
                ("bw_khz", radio.bw_khz.is_some()),
                ("sf", radio.sf.is_some()),
                ("cr", radio.cr.is_some()),
            ] {
                if present {
                    radio_basis(&mut radio, field, basis);
                }
            }
        }
    }
    radio
}

struct Packet {
    first_ms: i64,
    transmitters: BTreeMap<String, Heard>,
    unknown_tx: BTreeSet<String>,
}

impl Packet {
    fn new(first_ms: i64) -> Self {
        Self {
            first_ms,
            transmitters: BTreeMap::new(),
            unknown_tx: BTreeSet::new(),
        }
    }
}

#[derive(Default)]
struct Heard {
    complete_witness: bool,
    receivers: BTreeSet<String>,
}

#[derive(Default)]
struct Tally {
    opportunities: u32,
    heard: u32,
    first_ms: Option<i64>,
}

fn attach_detection(
    source: &mut Source,
    activity: &BTreeMap<String, Vec<i64>>,
    packets: BTreeMap<String, Packet>,
) {
    let mut totals: BTreeMap<(&str, &str), Tally> = BTreeMap::new();
    let mut excluded_ambiguity = 0;
    let mut excluded_incomplete = 0;
    for packet in packets.values() {
        let ms = packet.first_ms;
        let receivers: Vec<_> = activity
            .iter()
            .filter_map(|(rx, times)| {
                let index = times.partition_point(|&time| time < ms.saturating_sub(ACTIVE_MS));
                let active = times
                    .get(index)
                    .is_some_and(|&time| time <= ms.saturating_add(ACTIVE_MS));
                let position = active
                    .then(|| source.nodes[rx].position(ms, false))
                    .flatten()?;
                Some((rx.as_str(), position.point))
            })
            .collect();
        for (tx, heard) in &packet.transmitters {
            if !heard.complete_witness {
                excluded_incomplete += 1;
                continue;
            }
            let Some(position) = source.nodes[tx].position(ms, false) else {
                continue;
            };
            for &(rx, rx_point) in &receivers {
                if tx == rx || distance(position.point, rx_point) > MAX_DETECTION_M {
                    continue;
                }
                let received = heard.receivers.contains(rx);
                if !received && packet.unknown_tx.contains(rx) {
                    excluded_ambiguity += 1;
                    continue;
                }
                let tally = totals.entry((tx, rx)).or_default();
                tally.opportunities += 1;
                tally.heard += u32::from(received);
                tally.first_ms = Some(tally.first_ms.map_or(ms, |first| first.min(ms)));
            }
        }
    }

    source.counts.insert(
        "detection_excluded_incomplete_witnesses",
        excluded_incomplete,
    );
    source.counts.insert(
        "detection_excluded_ambiguous_receiver_opportunities",
        excluded_ambiguity,
    );
    source
        .counts
        .insert("detection_packets", packets.len() as u64);
    source.counts.insert(
        "detection_eligible_opportunities",
        totals
            .values()
            .map(|tally| u64::from(tally.opportunities))
            .sum(),
    );
    source.counts.insert(
        "detection_eligible_heard",
        totals.values().map(|tally| u64::from(tally.heard)).sum(),
    );
    for ((tx, rx), tally) in totals {
        let detection = Detection {
            heard: tally.heard,
            opportunities: tally.opportunities,
            basis: DETECTION_BASIS.into(),
        };
        let last_hop = (tx.to_owned(), rx.to_owned(), Evidence::CorescopeLastHop);
        let zero_hop = (tx.to_owned(), rx.to_owned(), Evidence::CorescopeZeroHop);
        let measured = if source.links.contains_key(&last_hop) {
            last_hop
        } else {
            zero_hop
        };
        if tally.heard == 0 && tally.opportunities >= MIN_MISSES {
            let group = source
                .links
                .entry((
                    tx.to_owned(),
                    rx.to_owned(),
                    Evidence::CorescopeNonDetection,
                ))
                .or_default();
            group.detection = Some(detection);
            group.opportunity_ms = tally.first_ms;
        } else if let Some(group) = source.links.get_mut(&measured) {
            group.detection = Some(detection);
        }
    }
}
