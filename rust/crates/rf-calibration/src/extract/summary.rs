use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use super::{model::Source, stats::quantile};
use crate::dataset::{Evidence, LinkRecord, NodeRecord, Region, SCHEMA_VERSION};

#[derive(Serialize)]
pub(super) struct Summary<'a> {
    schema: u32,
    pub groups: Vec<Group>,
    sources: Vec<SourceSummary<'a>>,
    notes: Vec<&'a str>,
}

#[derive(Serialize)]
pub(super) struct Group {
    region: Region,
    source: String,
    evidence: Evidence,
    links: usize,
    links_n_ge_5: usize,
    receptions: u64,
    distinct_nodes: usize,
    flag_counts: BTreeMap<String, usize>,
    endpoint_flag_counts: BTreeMap<String, usize>,
    quality_drop_counts: BTreeMap<String, usize>,
    distance_m: DistanceQuantiles,
    detection_opportunities: u64,
    detection_heard: u64,
    non_detections: usize,
}

#[derive(Serialize)]
struct DistanceQuantiles {
    min: f64,
    p10: f64,
    p50: f64,
    p90: f64,
    max: f64,
}

#[derive(Serialize)]
struct SourceSummary<'a> {
    source: &'a str,
    nodes: usize,
    located_nodes: usize,
    node_flag_counts: BTreeMap<&'a str, usize>,
    input_counts: &'a BTreeMap<&'static str, u64>,
}

impl<'a> Summary<'a> {
    pub fn new(
        sources: &'a [Source],
        links: &[LinkRecord],
        nodes: &'a [NodeRecord],
        notes: Vec<&'a str>,
    ) -> Self {
        let mut groups = BTreeMap::<_, Vec<_>>::new();
        for link in links {
            groups
                .entry((link.region, &link.source, link.evidence))
                .or_default()
                .push(link);
        }
        Self {
            schema: SCHEMA_VERSION,
            groups: groups
                .into_iter()
                .map(|((region, source, evidence), links)| {
                    let mut nodes = BTreeSet::new();
                    let mut flag_counts = BTreeMap::new();
                    let mut endpoint_flag_counts = BTreeMap::new();
                    let mut quality_drop_counts = BTreeMap::new();
                    let mut distances = Vec::with_capacity(links.len());
                    let mut detection_opportunities = 0;
                    let mut detection_heard = 0;
                    for link in &links {
                        for endpoint in [&link.tx, &link.rx] {
                            nodes.insert(&endpoint.node);
                            for flag in &endpoint.flags {
                                *endpoint_flag_counts.entry(flag.clone()).or_default() += 1;
                            }
                        }
                        for flag in &link.flags {
                            *flag_counts.entry(flag.clone()).or_default() += 1;
                        }
                        // Match the fit's default flag filter and count each drop once.
                        let reason = link
                            .flags
                            .iter()
                            .find(|flag| {
                                matches!(
                                    flag.as_str(),
                                    "short_relay_prefix"
                                        | "over_300km"
                                        | "mqtt_suspect"
                                        | "via_mqtt"
                                        | "relayed"
                                        | "self_link"
                                        | "radio_mismatch"
                                        | "implausible_signal"
                                        | "implausible_distance"
                                )
                            })
                            .cloned()
                            .or_else(|| {
                                [("tx", &link.tx), ("rx", &link.rx)].into_iter().find_map(
                                    |(end, endpoint)| {
                                        endpoint
                                            .flags
                                            .iter()
                                            .find(|flag| {
                                                matches!(
                                                    flag.as_str(),
                                                    "default_position" | "moved"
                                                )
                                            })
                                            .map(|flag| format!("{end}_{flag}"))
                                    },
                                )
                            });
                        if let Some(reason) = reason {
                            *quality_drop_counts.entry(reason).or_default() += 1;
                        }
                        if let Some(detection) = &link.detection {
                            detection_opportunities += u64::from(detection.opportunities);
                            detection_heard += u64::from(detection.heard);
                        }
                        distances.push(link.distance_m);
                    }
                    distances.sort_by(f64::total_cmp);
                    Group {
                        region,
                        source: source.clone(),
                        evidence,
                        links: links.len(),
                        links_n_ge_5: links.iter().filter(|link| link.n >= 5).count(),
                        receptions: links.iter().map(|link| u64::from(link.n)).sum(),
                        distinct_nodes: nodes.len(),
                        flag_counts,
                        endpoint_flag_counts,
                        quality_drop_counts,
                        distance_m: DistanceQuantiles {
                            min: distances[0],
                            p10: quantile(&distances, 0.1),
                            p50: quantile(&distances, 0.5),
                            p90: quantile(&distances, 0.9),
                            max: distances[distances.len() - 1],
                        },
                        detection_opportunities,
                        detection_heard,
                        non_detections: if evidence == Evidence::CorescopeNonDetection {
                            links.len()
                        } else {
                            0
                        },
                    }
                })
                .collect(),
            sources: sources
                .iter()
                .map(|source| {
                    let mut count = 0;
                    let mut located_nodes = 0;
                    let mut node_flag_counts = BTreeMap::new();
                    for node in nodes.iter().filter(|node| node.source == source.id) {
                        count += 1;
                        located_nodes += usize::from(node.lat.is_some());
                        for flag in &node.flags {
                            *node_flag_counts.entry(flag.as_str()).or_default() += 1;
                        }
                    }
                    SourceSummary {
                        source: &source.id,
                        nodes: count,
                        located_nodes,
                        node_flag_counts,
                        input_counts: &source.counts,
                    }
                })
                .collect(),
            notes,
        }
    }

    pub fn print(&self) {
        println!(
            "{:<6}  {:<39}  {:<23} {:>6} {:>6} {:>8} {:>6} {:>9} {:>7} {:>6}",
            "Region",
            "Source",
            "Evidence",
            "Links",
            "n>=5",
            "RX",
            "Nodes",
            "Opp.",
            "Heard",
            "Misses"
        );
        for row in &self.groups {
            let evidence = row.evidence.as_str().split_once('_').unwrap().1;
            println!(
                "{:<6}  {:<39}  {:<23} {:>6} {:>6} {:>8} {:>6} {:>9} {:>7} {:>6}",
                match row.region {
                    Region::Norway => "norway",
                    Region::World => "world",
                },
                row.source,
                evidence,
                row.links,
                row.links_n_ge_5,
                row.receptions,
                row.distinct_nodes,
                row.detection_opportunities,
                row.detection_heard,
                row.non_detections
            );
        }
    }
}
