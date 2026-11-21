use anyhow::{Result, ensure};
use serde_json::Value;

use crate::model::*;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Api {
    Norway,
    Liam,
    Meshview,
    MeshInfo,
}

pub fn rows<'a>(body: &'a Value, key: &str) -> Result<&'a [Value]> {
    match body.get(key) {
        Some(Value::Null) => Ok(&[]),
        Some(Value::Array(rows)) => Ok(rows),
        _ => anyhow::bail!("Unexpected response shape for {key}"),
    }
}

pub fn nodes(api: Api, body: &Value, fetched: i64) -> Result<Batch> {
    let mut batch = Batch::default();
    if api == Api::MeshInfo
        && let Some(nodes) = body["nodes"].as_object()
    {
        for (id, value) in nodes {
            if let Some(id) = hex_node(&Value::String(id.clone())) {
                add_node(api, id, value, fetched, &mut batch);
            }
        }
        return Ok(batch);
    }
    for value in rows(body, "nodes")? {
        let id = match api {
            Api::Norway => node(first(value, &["num", "id"])),
            Api::Liam | Api::Meshview => node(&value["node_id"]),
            Api::MeshInfo => hex_node(&value["id"]),
        };
        if let Some(id) = id {
            add_node(api, id, value, fetched, &mut batch);
        }
    }
    Ok(batch)
}

fn add_node(api: Api, id: NodeId, v: &Value, fetched: i64, batch: &mut Batch) {
    let (p, lat, lon, time, scale, source) = match api {
        Api::Norway => {
            let p = &v["position"];
            let source = if p["source"] == "mapreport" {
                "mapreport"
            } else {
                "position"
            };
            (p, &p["lat"], &p["lon"], &p["observed_at"], 1.0, source)
        }
        Api::Liam => (
            v,
            &v["latitude"],
            &v["longitude"],
            &v["position_updated_at"],
            1e7,
            "map_snapshot",
        ),
        Api::Meshview => (
            v,
            &v["last_lat"],
            &v["last_long"],
            &v["position_time_us"],
            1e7,
            "node_snapshot",
        ),
        Api::MeshInfo => {
            let p = &v["position"];
            (
                p,
                &p["latitude_i"],
                &p["longitude_i"],
                &p["time"],
                1e7,
                "node_snapshot",
            )
        }
    };
    if let (Some(lat), Some(lon)) = (number(lat), number(lon)) {
        let position = Position {
            node: id,
            time: timestamp(time).unwrap_or(fetched),
            lat: lat / scale,
            lon: lon / scale,
            altitude: number(first(p, &["alt", "altitude", "last_altitude"])),
            bits: bits(first(p, &["precision_bits", "position_precision"])),
            radius: number(first(p, &["precision_radius_m", "position_accuracy_m"]))
                .filter(|r| *r >= 0.0),
            source,
        };
        if let Some(position) = position.validate() {
            batch.positions.push(position);
        }
    }
    let radio = if api == Api::Norway {
        &v["identity"]
    } else {
        v
    };
    let time = timestamp(first(
        radio,
        &["observed_at", "updated_at", "last_seen_us", "last_seen"],
    ))
    .unwrap_or(fetched);
    let radio = Radio::read(radio);
    batch.radios.push((id, time, radio));
    if api == Api::Liam
        && let Some(time) = timestamp(first(v, &["neighbours_updated_at", "neighbors_updated_at"]))
    {
        let packet = Packet {
            id: format!("neighbor:{id}:{time}"),
            logical_id: None,
            time,
            gateway: None,
            hop_start: None,
            hop_limit: None,
            relay_hint: None,
            relay_state: None,
            via_mqtt: None,
            region: None,
            provenance: "node_snapshot",
        };
        neighbors(
            &mut batch.measurements,
            &packet,
            id,
            first(v, &["neighbours", "neighbors"]),
        );
    }
}

fn neighbors(out: &mut Vec<Measurement>, packet: &Packet, rx: NodeId, values: &Value) {
    for value in values.as_array().into_iter().flatten() {
        if let (Some(tx), Some(snr)) = (
            node(first(value, &["node_id", "id"])),
            signal(&value["snr"], 1.0),
        ) && tx != rx
        {
            out.push(Measurement {
                packet: packet.clone(),
                tx: Some(tx),
                rx,
                evidence: Evidence::Neighbor,
                hop: None,
                snr: Some(snr),
                rssi: None,
            });
        }
    }
}

struct Trace {
    route: Vec<Option<NodeId>>,
    back: Vec<Option<NodeId>>,
    snr: Vec<Option<f64>>,
    back_snr: Vec<Option<f64>>,
}

impl Trace {
    fn read(
        fields: &Value,
        route: &Value,
        back: &Value,
        decode: fn(&Value) -> Option<NodeId>,
        scale: f64,
    ) -> Self {
        let ids = |values: &Value| {
            values
                .as_array()
                .into_iter()
                .flatten()
                .map(decode)
                .collect()
        };
        let signals = |name: &str| {
            fields[name]
                .as_array()
                .into_iter()
                .flatten()
                .map(|v| signal(v, scale))
                .collect()
        };
        Self {
            route: ids(route),
            back: ids(back),
            snr: signals("snr_towards"),
            back_snr: signals("snr_back"),
        }
    }

    fn is_reply(&self) -> bool {
        self.snr.len() == self.route.len() + 1
    }

    fn record(
        self,
        out: &mut Vec<Measurement>,
        packet: &Packet,
        sender: NodeId,
        destination: Option<NodeId>,
        reply: bool,
    ) {
        let (initiator, send_back_to) = if reply {
            (destination, destination)
        } else {
            (Some(sender), None)
        };
        let target = reply.then_some(sender);
        legs(
            out,
            packet,
            Evidence::Towards,
            initiator,
            self.route,
            target,
            &self.snr,
        );
        if reply {
            legs(
                out,
                packet,
                Evidence::Back,
                target,
                self.back,
                send_back_to,
                &self.back_snr,
            );
        }
    }
}

fn legs(
    out: &mut Vec<Measurement>,
    packet: &Packet,
    evidence: Evidence,
    start: Option<NodeId>,
    route: Vec<Option<NodeId>>,
    end: Option<NodeId>,
    snrs: &[Option<f64>],
) {
    // A route heard by a gateway may still be in flight. Only append its
    // destination when the SNR vector contains the corresponding final leg.
    let path: Vec<_> = [start]
        .into_iter()
        .chain(route)
        .chain(end.map(Some))
        .collect();
    for (hop, (pair, snr)) in path.windows(2).zip(snrs).enumerate() {
        if let ([Some(tx), Some(rx)], Some(snr)) = (pair, snr)
            && tx != rx
        {
            out.push(Measurement {
                packet: packet.clone(),
                tx: Some(*tx),
                rx: *rx,
                evidence,
                hop: Some(hop),
                snr: Some(*snr),
                rssi: None,
            });
        }
    }
}

fn reception(
    out: &mut Vec<Measurement>,
    packet: Packet,
    sender: NodeId,
    snr: Option<f64>,
    rssi: Option<f64>,
) {
    let Some(rx) = packet.gateway else { return };
    if rx == sender || (snr.is_none() && rssi.is_none()) {
        return;
    }
    let direct = matches!((packet.hop_start, packet.hop_limit), (Some(a), Some(b)) if a != 0 && a == b)
        && packet.relay_state != Some("relayed")
        && packet.via_mqtt != Some(true);
    out.push(Measurement {
        packet,
        tx: direct.then_some(sender),
        rx,
        evidence: Evidence::Gateway,
        hop: None,
        snr,
        rssi,
    });
}

pub fn norway(body: &Value) -> Result<Batch> {
    let mut batch = Batch::default();
    for v in rows(body, "rows")? {
        let (Some(sender), Some(id), Some(time)) =
            (node(&v["s"]), token(&v["o"]), timestamp(&v["t"]))
        else {
            continue;
        };
        if v["y"] == "invalid" || !v["iv"].is_null() {
            continue;
        }
        let relay_state = match v["rr"].as_str() {
            Some("own_packet") => Some("own_packet"),
            Some("relayed") => Some("relayed"),
            Some("direct") => Some("direct"),
            _ => None,
        };
        let packet = Packet {
            id,
            logical_id: token(&v["l"]).or_else(|| token(&v["p"])),
            time,
            gateway: node(&v["g"]),
            hop_start: unsigned(&v["hs"]),
            hop_limit: unsigned(&v["hl"]),
            relay_hint: unsigned(&v["r"]),
            relay_state,
            via_mqtt: v["via_mqtt"].as_bool(),
            region: v["tp"]
                .as_str()
                .and_then(|t| t.split('/').nth(1))
                .and_then(|r| region(&Value::String(r.into()))),
            provenance: "observations",
        };
        let position = first(v, &["po", "mr"]);
        if let (Some(lat), Some(lon)) = (number(&position["lat"]), number(&position["lon"]))
            && let Some(position) = (Position {
                node: sender,
                time,
                lat,
                lon,
                altitude: number(&position["alt"]),
                bits: bits(&position["precision_bits"]),
                radius: None,
                source: if v["po"].is_object() {
                    "position"
                } else {
                    "mapreport"
                },
            })
            .validate()
        {
            batch.positions.push(position);
        }
        if v["mr"].is_object() {
            batch.radios.push((sender, time, Radio::read(&v["mr"])));
        }
        neighbors(
            &mut batch.measurements,
            &packet,
            sender,
            &v["nb"]["neighbors"],
        );
        let trace = &v["tr"];
        if trace["valid"] == true {
            let decode = |hop: &Value| (hop["unknown"] != true).then(|| node(&hop["id"])).flatten();
            Trace::read(trace, &trace["route"], &trace["route_back"], decode, 1.0).record(
                &mut batch.measurements,
                &packet,
                sender,
                node(&v["d"]),
                trace["is_reply"] == true,
            );
        }
        if relay_state != Some("own_packet") {
            reception(
                &mut batch.measurements,
                packet,
                sender,
                signal(&v["sn"], 1.0),
                number(&v["rs"]).filter(|r| *r < 0.0),
            );
        }
    }
    Ok(batch)
}

pub fn meshinfo(body: &Value) -> Result<Batch> {
    let mut batch = Batch::default();
    for v in rows(body, "traceroutes")? {
        let (Some(sender), Some(id), Some(created)) = (
            hex_node(&v["from"]),
            token(&v["id"]),
            timestamp(&v["created_at"]),
        ) else {
            continue;
        };
        let fields = &v["payload"];
        let trace = Trace::read(fields, &v["route_ids"], &v["route_back_ids"], hex_node, 4.0);
        let reply = trace.is_reply();
        let destination = hex_node(&v["to"]);
        let logical = if reply {
            token(&v["packet_id"])
                .zip(destination)
                .map(|(id, node)| format!("{node}:{id}"))
        } else {
            Some(format!("{sender}:{id}"))
        };
        let packet = Packet {
            id: format!("{sender}:{id}:{created}"),
            logical_id: logical,
            time: timestamp(&v["timestamp"]).unwrap_or(created),
            gateway: hex_node(first(v, &["gateway_id", "gateway"])),
            hop_start: unsigned(&v["hop_start"]),
            hop_limit: unsigned(&v["hop_limit"]),
            relay_hint: unsigned(&v["relay_node"]),
            relay_state: None,
            via_mqtt: v["via_mqtt"].as_bool(),
            region: Some("US".into()),
            provenance: "traceroutes",
        };
        trace.record(&mut batch.measurements, &packet, sender, destination, reply);
    }
    Ok(batch)
}

pub struct MeshviewPacket {
    id: u64,
    logical_id: Option<String>,
    sender: NodeId,
    time: i64,
}

impl MeshviewPacket {
    pub fn id(&self) -> u64 {
        self.id
    }
    pub fn time(&self) -> i64 {
        self.time
    }
}

pub fn meshview_packets(
    body: &Value,
    port: u32,
    band: Option<&str>,
) -> Result<(Batch, Vec<MeshviewPacket>)> {
    let mut batch = Batch::default();
    let mut packets = Vec::new();
    ensure!(matches!(port, 70 | 71), "Unexpected Meshview port");
    for v in rows(body, "packets")? {
        if unsigned(&v["portnum"]) != Some(port) {
            continue;
        }
        let id = first(v, &["storage_id", "id"]).as_u64();
        let (Some(id), Some(sender), Some(time)) = (
            id,
            node(&v["from_node_id"]),
            timestamp(&v["import_time_us"]),
        ) else {
            continue;
        };
        let logical_id = token(&v["reply_id"]).or_else(|| token(&v["packet_id"]));
        let packet = Packet {
            id: id.to_string(),
            logical_id: logical_id.clone(),
            time,
            gateway: None,
            hop_start: None,
            hop_limit: None,
            relay_hint: None,
            relay_state: None,
            via_mqtt: None,
            region: band.map(str::to_owned),
            provenance: "packets",
        };
        let fields = &v["payload"];
        let decoded;
        let fields = if let Some(text) = fields.as_str() {
            decoded = meshview_payload(text, port);
            &decoded
        } else {
            fields
        };
        if port == 70 {
            let trace = Trace::read(fields, &fields["route"], &fields["route_back"], node, 4.0);
            let reply = trace.is_reply();
            trace.record(
                &mut batch.measurements,
                &packet,
                sender,
                node(&v["to_node_id"]),
                reply,
            );
        } else {
            neighbors(
                &mut batch.measurements,
                &packet,
                sender,
                &fields["neighbors"],
            );
        }
        packets.push(MeshviewPacket {
            id,
            logical_id,
            sender,
            time,
        });
    }
    Ok((batch, packets))
}

// Meshview serializes the decoded protobuf as text. Read only numeric RF
// fields from ports 70 and 71, with null placeholders for unknown route hops.
fn meshview_payload(text: &str, port: u32) -> Value {
    use serde_json::{Map, json};
    let mut fields = Map::new();
    let mut neighbors = Vec::new();
    let mut neighbor = None;
    for line in text.lines().map(str::trim) {
        match (port, line) {
            (71, "neighbors {") => neighbor = Some(Map::new()),
            (71, "}") => {
                if let Some(entry) = neighbor.take() {
                    neighbors.push(Value::Object(entry));
                }
            }
            _ => {
                let Some((key, value)) = line.split_once(':') else {
                    continue;
                };
                let value = value.trim();
                if port == 70 && matches!(key, "route" | "route_back" | "snr_towards" | "snr_back")
                {
                    let Value::Array(items) = fields.entry(key).or_insert_with(|| json!([])) else {
                        unreachable!()
                    };
                    items.push(value.parse::<i64>().ok().map_or(Value::Null, Value::from));
                }
                if let Some(entry) = &mut neighbor {
                    let parsed = match key {
                        "node_id" => value.parse::<u32>().ok().map(Value::from),
                        "snr" => value
                            .parse::<f64>()
                            .ok()
                            .filter(|n| n.is_finite())
                            .map(Value::from),
                        _ => None,
                    };
                    if let Some(parsed) = parsed {
                        entry.insert(key.into(), parsed);
                    }
                }
            }
        }
    }
    fields.insert("neighbors".into(), Value::Array(neighbors));
    Value::Object(fields)
}

pub fn meshview_seen(body: &Value, base: &MeshviewPacket, band: Option<&str>) -> Result<Batch> {
    let mut batch = Batch::default();
    for v in rows(body, "seen")? {
        let Some(gateway) = node(&v["node_id"]) else {
            continue;
        };
        let recorded = v["import_time_us"]
            .as_i64()
            .or_else(|| v["rx_time"].as_i64());
        let Some(recorded) = recorded else { continue };
        let hop_start = unsigned(&v["hop_start"]);
        let hop_limit = unsigned(&v["hop_limit"]);
        let packet = Packet {
            id: format!("{}:{gateway}:{recorded}", base.id),
            logical_id: base.logical_id.clone(),
            time: timestamp(&v["rx_time"])
                .or_else(|| timestamp(&v["import_time_us"]))
                .unwrap_or(base.time),
            gateway: Some(gateway),
            hop_start,
            hop_limit,
            relay_hint: unsigned(&v["relay_node"]),
            relay_state: matches!((hop_start, hop_limit), (Some(a), Some(b)) if a > b)
                .then_some("relayed"),
            via_mqtt: v["via_mqtt"].as_bool(),
            region: v["topic"]
                .as_str()
                .and_then(|t| t.split('/').nth(1))
                .and_then(|r| region(&Value::String(r.into())))
                .or_else(|| band.map(str::to_owned)),
            provenance: "packets_seen",
        };
        reception(
            &mut batch.measurements,
            packet,
            base.sender,
            signal(&v["rx_snr"], 1.0),
            number(&v["rx_rssi"]).filter(|r| *r < 0.0),
        );
    }
    Ok(batch)
}
