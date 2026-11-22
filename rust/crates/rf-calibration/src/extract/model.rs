use std::collections::{BTreeMap, BTreeSet};

use repeaternet_providers::sources::Coverage;
use repeaternet_terrain::LonLat;

use super::{
    quality::{self, PositionQuality},
    stats,
};
use crate::dataset::{
    Detection, Endpoint, Evidence, LinkRecord, NodeRecord, Radio, Region, SCHEMA_VERSION,
};

#[derive(Clone, Copy)]
pub(super) struct Position {
    pub point: LonLat,
    pub source: &'static str,
    pub precision: Option<f64>,
    pub bits: Option<u8>,
    pub altitude: Option<f64>,
}

pub(super) struct TimedPosition {
    pub ms: i64,
    pub position: Position,
}

#[derive(Default)]
pub(super) struct Metadata {
    pub role: Option<String>,
    pub radio: Radio,
    pub noise_floor: Option<f64>,
}

#[derive(Default)]
pub(super) struct Node {
    pub positions: Vec<TimedPosition>,
    pub metadata: Vec<(i64, Metadata)>,
    pub observer_metadata: Vec<(i64, Metadata)>,
    pub saw_default: bool,
    spread: Option<f64>,
    position_count: u32,
}

impl Node {
    pub fn prepare(&mut self) {
        self.positions.sort_by_key(|entry| entry.ms);
        self.metadata.sort_by_key(|entry| entry.0);
        self.observer_metadata.sort_by_key(|entry| entry.0);
        (self.position_count, self.spread) =
            quality::position_spread(self.positions.iter().map(|entry| entry.position.point));
    }

    pub fn position(&self, ms: i64, nearest: bool) -> Option<&Position> {
        let index = self.positions.partition_point(|entry| entry.ms <= ms);
        let before = index.checked_sub(1).and_then(|i| self.positions.get(i));
        let after = self.positions.get(index);
        let entry = match (before, after) {
            (Some(before), Some(after))
                if nearest && before.ms.abs_diff(ms) > after.ms.abs_diff(ms) =>
            {
                Some(after)
            }
            (Some(before), _) => Some(before),
            (None, after) => after,
        };
        entry.map(|entry| &entry.position)
    }

    pub fn metadata(&self, ms: i64) -> MetadataRef<'_> {
        fn at(entries: &[(i64, Metadata)], ms: i64) -> Option<&Metadata> {
            let index = entries.partition_point(|(time, _)| *time <= ms);
            entries.get(index.saturating_sub(1)).map(|(_, data)| data)
        }
        let observer = at(&self.observer_metadata, ms);
        let node = at(&self.metadata, ms);
        MetadataRef {
            role: observer
                .and_then(|data| data.role.as_deref())
                .or_else(|| node.and_then(|data| data.role.as_deref())),
            radio: observer.or(node).map(|data| &data.radio),
            noise_floor: observer.and_then(|data| data.noise_floor),
        }
    }

    fn endpoint(
        &self,
        id: &str,
        position: &Position,
        ms: i64,
        quality: &PositionQuality,
    ) -> Endpoint {
        Endpoint {
            node: id.to_owned(),
            lat: position.point.latitude,
            lon: position.point.longitude,
            pos_source: position.source.into(),
            pos_precision_m: position.precision,
            altitude_m: position.altitude,
            role: self.metadata(ms).role.map(str::to_owned),
            flags: quality.flags(Some(position), self.saw_default, self.spread),
        }
    }

    fn record(&self, source: &str, id: &str, quality: &PositionQuality) -> NodeRecord {
        let position = self.position(i64::MAX, false);
        let metadata = self.metadata(i64::MAX);
        NodeRecord {
            schema: SCHEMA_VERSION,
            node: id.to_owned(),
            source: source.to_owned(),
            lat: position.map(|p| p.point.latitude),
            lon: position.map(|p| p.point.longitude),
            role: metadata.role.map(str::to_owned),
            radio: metadata.radio.cloned().unwrap_or_default(),
            noise_floor_dbm: metadata.noise_floor,
            position_count: self.position_count,
            position_spread_m: self.spread,
            flags: quality.flags(position, self.saw_default, self.spread),
        }
    }
}

pub(super) struct MetadataRef<'a> {
    pub role: Option<&'a str>,
    pub radio: Option<&'a Radio>,
    pub noise_floor: Option<f64>,
}

pub(super) fn radio_basis(radio: &mut Radio, field: &str, basis: &str) {
    radio.field_basis.insert(field.into(), basis.into());
    if !radio.basis.iter().any(|item| item == basis) {
        radio.basis.push(basis.into());
        radio.basis.sort_unstable();
    }
}

fn link_radio(
    tx: Option<&Radio>,
    rx: Option<&Radio>,
    meshtastic: bool,
    region: Option<&str>,
) -> Radio {
    let mut combined = Radio::default();
    let (primary, secondary) = if meshtastic { (tx, rx) } else { (rx, tx) };
    macro_rules! select {
        ($field:ident, $first:expr, $second:expr) => {
            if let Some((value, basis)) = $first.into_iter().chain($second).find_map(|radio| {
                radio
                    .$field
                    .clone()
                    .zip(radio.field_basis.get(stringify!($field)))
            }) {
                combined.$field = Some(value);
                radio_basis(&mut combined, stringify!($field), basis);
            }
        };
    }
    select!(freq_mhz, primary, secondary);
    select!(bw_khz, primary, secondary);
    select!(sf, primary, secondary);
    select!(cr, primary, secondary);
    select!(modem_preset, primary, secondary);
    select!(tx_power_dbm, tx, None::<&Radio>);
    if let Some(region) = region {
        combined.lora_region = Some(region.into());
        radio_basis(&mut combined, "lora_region", "measurement");
    } else {
        select!(lora_region, primary, secondary);
    }
    combined
}

pub(super) struct Sample {
    pub ms: i64,
    pub rssi: Option<f64>,
    pub snr: Option<f64>,
    pub region: Option<String>,
}

#[derive(Default)]
pub(super) struct Group {
    pub samples: Vec<Sample>,
    pub flags: BTreeSet<&'static str>,
    pub detection: Option<Detection>,
    pub opportunity_ms: Option<i64>,
}

pub(super) struct Source {
    pub id: String,
    pub meshtastic: bool,
    pub nodes: BTreeMap<String, Node>,
    pub links: BTreeMap<(String, String, Evidence), Group>,
    pub counts: BTreeMap<&'static str, u64>,
}

impl Source {
    pub fn new(id: String, meshtastic: bool) -> Self {
        Self {
            id,
            meshtastic,
            nodes: BTreeMap::new(),
            links: BTreeMap::new(),
            counts: BTreeMap::new(),
        }
    }

    pub fn count(&mut self, reason: &'static str) {
        *self.counts.entry(reason).or_default() += 1;
    }

    pub fn prepare_nodes(&mut self) {
        self.nodes.values_mut().for_each(Node::prepare);
    }

    pub fn add_sample(
        &mut self,
        tx: &str,
        rx: &str,
        evidence: Evidence,
        sample: Sample,
        flags: &[&'static str],
    ) {
        debug_assert_ne!(tx, rx);
        let a = self.nodes.get(tx).expect("transmitter was registered");
        let b = self.nodes.get(rx).expect("receiver was registered");
        let (Some(tx_position), Some(rx_position)) = (
            a.position(sample.ms, self.meshtastic),
            b.position(sample.ms, self.meshtastic),
        ) else {
            self.count("receptions_missing_position");
            return;
        };
        let mismatch = a
            .metadata(sample.ms)
            .radio
            .zip(b.metadata(sample.ms).radio)
            .is_some_and(|(tx, rx)| quality::radio_mismatch(tx, rx));
        let group = self
            .links
            .entry((tx.to_owned(), rx.to_owned(), evidence))
            .or_default();
        group.flags.extend(flags);
        if mismatch {
            group.flags.insert("radio_mismatch");
        }
        if quality::distance(tx_position.point, rx_position.point) > 300_000.0 {
            group.flags.insert("over_300km");
        }
        group.samples.push(sample);
    }

    pub fn records(
        &self,
        coverage: &Coverage,
        quality: &PositionQuality,
    ) -> (Vec<LinkRecord>, Vec<NodeRecord>) {
        let links = self
            .links
            .iter()
            .map(|((tx, rx, evidence), group)| {
                let samples = &group.samples;
                let reference = samples.get(samples.len() / 2);
                let ms = reference
                    .map(|s| s.ms)
                    .or(group.opportunity_ms)
                    .expect("link reference time");
                let a = &self.nodes[tx];
                let b = &self.nodes[rx];
                let tx_position = a
                    .position(ms, self.meshtastic)
                    .expect("located transmitter");
                let rx_position = b.position(ms, self.meshtastic).expect("located receiver");
                let distance_m = quality::distance(tx_position.point, rx_position.point);
                let tx_radio = a.metadata(ms).radio;
                let rx_radio = b.metadata(ms).radio;
                let mut flags = group.flags.clone();
                if samples.len() < 5 {
                    flags.insert("few_samples");
                }
                if tx_radio
                    .zip(rx_radio)
                    .is_some_and(|(a, b)| quality::radio_mismatch(a, b))
                {
                    flags.insert("radio_mismatch");
                }
                let (signal, noise): (Vec<_>, Vec<_>) = samples
                    .iter()
                    .filter_map(|s| Some(stats::split_power(s.rssi?, s.snr?)))
                    .unzip();
                let signal = stats::summarize(signal);
                let freq_mhz = tx_radio
                    .or(rx_radio)
                    .and_then(|radio| radio.freq_mhz)
                    .unwrap_or(869.5);
                if signal
                    .as_ref()
                    .is_some_and(|s| quality::implausible_signal(s.median, distance_m, freq_mhz))
                {
                    flags.insert("implausible_signal");
                }
                LinkRecord {
                    schema: SCHEMA_VERSION,
                    link_id: format!("{}:{tx}>{rx}:{}", self.id, evidence.as_str()),
                    source: self.id.clone(),
                    evidence: *evidence,
                    region: if coverage.contains(tx_position.point)
                        && coverage.contains(rx_position.point)
                    {
                        Region::Norway
                    } else {
                        Region::World
                    },
                    tx: a.endpoint(tx, tx_position, ms, quality),
                    rx: b.endpoint(rx, rx_position, ms, quality),
                    distance_m,
                    radio: link_radio(
                        tx_radio,
                        rx_radio,
                        self.meshtastic,
                        reference.and_then(|s| s.region.as_deref()),
                    ),
                    n: samples.len() as u32,
                    first_ms: samples.first().map(|s| s.ms),
                    last_ms: samples.last().map(|s| s.ms),
                    rssi_dbm: stats::summarize(samples.iter().filter_map(|s| s.rssi).collect()),
                    snr_db: stats::summarize(samples.iter().filter_map(|s| s.snr).collect()),
                    signal_dbm: signal,
                    noise_dbm: stats::summarize(noise),
                    detection: group.detection.clone(),
                    flags: flags.into_iter().map(str::to_owned).collect(),
                }
            })
            .collect();
        let nodes = self
            .nodes
            .iter()
            .map(|(id, node)| node.record(&self.id, id, quality))
            .collect();
        (links, nodes)
    }
}
