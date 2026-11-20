use std::collections::{BTreeMap, BTreeSet};

use repeaternet_rf_core::{EvaluationOptions, LinkResult, Site, TerrainSource};
use repeaternet_terrain::profile::bounded_profile_step;
use serde::{Deserialize, Serialize};

use crate::{
    engine::profile_limit_detail, invalid, CoverageCell, CoverageError, CoverageEvaluator,
    CoverageSettings, CoverageState, Rejection,
};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NetworkLink {
    pub source_id: String,
    pub target_id: String,
    pub valid: Option<bool>,
    #[serde(default)]
    pub unresolved: bool,
    pub rejection: Option<Rejection>,
    pub rejection_detail: Option<String>,
}

impl From<&LinkResult> for NetworkLink {
    fn from(link: &LinkResult) -> Self {
        Self {
            source_id: link.source_id.clone(),
            target_id: link.target_id.clone(),
            valid: Some(link.valid),
            unresolved: false,
            rejection: None,
            rejection_detail: None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CoverageNetwork {
    #[serde(default)]
    pub router_ids: Vec<String>,
    #[serde(default)]
    pub source_ids: Vec<String>,
    #[serde(default)]
    pub network_links: Vec<NetworkLink>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BackboneRevalidation {
    pub network_links: Vec<NetworkLink>,
    pub backbone_revalidated_edges: usize,
    pub backbone_invalidated_edges: usize,
    pub backbone_unresolved_edges: usize,
}

impl<T: TerrainSource + ?Sized> CoverageEvaluator<'_, T> {
    /// Recheck only previously certified router-to-router edges incident to adjusted sites.
    pub fn revalidate_backbone(
        &self,
        network: &CoverageNetwork,
        active_sites: &[Site],
        adjusted_sources: &[Site],
        overrides: &BTreeMap<String, f64>,
        settings: &CoverageSettings,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<BackboneRevalidation, CoverageError> {
        settings.validate()?;
        let routers: BTreeSet<_> = network.router_ids.iter().map(String::as_str).collect();
        let affected = |link: &&NetworkLink| {
            link.valid == Some(true)
                && !link.unresolved
                && routers.contains(link.source_id.as_str())
                && routers.contains(link.target_id.as_str())
                && (overrides.contains_key(&link.source_id)
                    || overrides.contains_key(&link.target_id))
        };
        let affected_count = network.network_links.iter().filter(affected).count();
        if affected_count > 4096 {
            return Err(invalid(
                "Height scenario affects too many certified mesh links to revalidate safely",
            ));
        }
        let originals: BTreeMap<_, _> = active_sites
            .iter()
            .map(|site| (site.id.as_str(), site))
            .collect();
        let adjusted: BTreeMap<_, _> = adjusted_sources
            .iter()
            .map(|site| (site.id.as_str(), site))
            .collect();
        let find_site = |id: &str| {
            (if overrides.contains_key(id) {
                adjusted.get(id)
            } else {
                originals.get(id)
            })
            .copied()
            .ok_or_else(|| invalid(format!("Missing backbone site {id}")))
        };
        let mut result = BackboneRevalidation {
            network_links: Vec::with_capacity(network.network_links.len()),
            backbone_revalidated_edges: affected_count,
            backbone_invalidated_edges: 0,
            backbone_unresolved_edges: 0,
        };
        for edge in &network.network_links {
            if cancelled() {
                return Err(CoverageError::Cancelled);
            }
            let mut edge = edge.clone();
            if affected(&&edge) {
                let left = find_site(&edge.source_id)?;
                let right = find_site(&edge.target_id)?;
                let sampling = bounded_profile_step(
                    left.distance_to(right),
                    settings.profile_step_m,
                    self.terrain.resolution_m(),
                    settings.maximum_profile_samples,
                )?;
                if !sampling.fits() {
                    result.backbone_unresolved_edges += 1;
                    edge.valid = None;
                    edge.unresolved = true;
                    edge.rejection = Some(Rejection::ProfileSampleLimit);
                    edge.rejection_detail = Some(profile_limit_detail(sampling));
                } else {
                    let valid = self
                        .evaluate_metrics(
                            left,
                            right,
                            EvaluationOptions {
                                sample_step_m: Some(sampling.step_m),
                                include_profile: false,
                                ..EvaluationOptions::default()
                            },
                            cancelled,
                        )?
                        .is_some_and(|link| link.valid);
                    edge.valid = Some(valid);
                    edge.rejection = None;
                    edge.rejection_detail = None;
                    result.backbone_invalidated_edges += usize::from(!valid);
                }
            }
            result.network_links.push(edge);
        }
        if cancelled() {
            return Err(CoverageError::Cancelled);
        }
        Ok(result)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FailureCell {
    pub index: usize,
    pub state: CoverageState,
    pub local_source_count: usize,
    pub reference_source_count: usize,
    pub locally_covered: bool,
    pub reference_connected: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FailureReport {
    pub reference_id: String,
    pub reference_available: bool,
    pub baseline_reference_available: bool,
    pub failed_ids: Vec<String>,
    pub connected_source_ids: Vec<String>,
    pub disconnected_source_ids: Vec<String>,
    pub components: Vec<Vec<String>>,
    pub isolated_router_ids: Vec<String>,
    pub unresolved_links: usize,
    pub baseline_connected_source_ids: Vec<String>,
    pub unknown_cells: usize,
    pub unresolved_cells: usize,
    pub outside_area_cells: usize,
    pub lost_local_cells: usize,
    pub lost_reference_connected_cells: usize,
    pub newly_uncovered_cells: usize,
    pub cells: Vec<FailureCell>,
}

struct Components<'a> {
    members: Vec<BTreeSet<&'a str>>,
    isolated: Vec<String>,
}

impl<'a> Components<'a> {
    fn reference_component(&self, reference_id: &str) -> Option<&BTreeSet<&'a str>> {
        self.members
            .iter()
            .find(|component| component.contains(reference_id))
    }
}

fn components<'a>(
    routers: &BTreeSet<&'a str>,
    links: &[NetworkLink],
    disabled: &BTreeSet<&str>,
) -> Components<'a> {
    let mut graph: BTreeMap<&'a str, BTreeSet<&'a str>> = routers
        .iter()
        .filter(|id| !disabled.contains(**id))
        .map(|&id| (id, BTreeSet::new()))
        .collect();
    for link in links {
        if link.valid == Some(true) && !link.unresolved && link.source_id != link.target_id {
            if let (Some(&left), Some(&right)) = (
                routers.get(link.source_id.as_str()),
                routers.get(link.target_id.as_str()),
            ) {
                if graph.contains_key(left) && graph.contains_key(right) {
                    graph
                        .get_mut(left)
                        .expect("left router in graph")
                        .insert(right);
                    graph
                        .get_mut(right)
                        .expect("right router in graph")
                        .insert(left);
                }
            }
        }
    }
    let isolated = graph
        .iter()
        .filter(|(_, neighbors)| graph.len() > 1 && neighbors.is_empty())
        .map(|(&id, _)| id.to_owned())
        .collect();
    let mut remaining: BTreeSet<_> = graph.keys().copied().collect();
    let mut members = Vec::new();
    while let Some(root) = remaining.pop_first() {
        let mut component = BTreeSet::from([root]);
        let mut pending = vec![root];
        while let Some(current) = pending.pop() {
            for &neighbor in &graph[current] {
                if remaining.remove(neighbor) {
                    component.insert(neighbor);
                    pending.push(neighbor);
                }
            }
        }
        members.push(component);
    }
    Components { members, isolated }
}

pub(crate) fn usable_sources(cell: &CoverageCell) -> BTreeSet<&str> {
    cell.sources
        .iter()
        .filter(|row| row.valid_two_way)
        .map(|row| row.source_id.as_str())
        .collect()
}

/// Reclassify already evaluated cells using certified network reachability.
pub fn analyze_node_failures(
    cells: impl IntoIterator<Item = impl std::borrow::Borrow<CoverageCell>>,
    network: &CoverageNetwork,
    failed_ids: &[String],
    reference_id: &str,
) -> Result<FailureReport, CoverageError> {
    let routers: BTreeSet<_> = network.router_ids.iter().map(String::as_str).collect();
    let sources: BTreeSet<_> = network
        .source_ids
        .iter()
        .map(String::as_str)
        .filter(|id| routers.contains(id))
        .collect();
    let failed: BTreeSet<_> = failed_ids.iter().map(String::as_str).collect();
    if failed.len() != failed_ids.len() || !failed.is_subset(&sources) {
        return Err(invalid(
            "Failed router IDs must be unique selected coverage sources",
        ));
    }
    if !sources.contains(reference_id) {
        return Err(invalid(
            "Reference router must be an enabled coverage source",
        ));
    }
    let baseline = components(&routers, &network.network_links, &BTreeSet::new());
    let scenario = components(&routers, &network.network_links, &failed);
    let baseline_reference = baseline.reference_component(reference_id);
    let reference = scenario.reference_component(reference_id);
    let connected_sources: BTreeSet<_> = reference
        .into_iter()
        .flat_map(|members| members.intersection(&sources).copied())
        .collect();
    let baseline_sources: BTreeSet<_> = baseline_reference
        .into_iter()
        .flat_map(|members| members.intersection(&sources).copied())
        .collect();
    let mut report = FailureReport {
        reference_id: reference_id.to_owned(),
        reference_available: reference.is_some(),
        baseline_reference_available: baseline_reference.is_some(),
        failed_ids: failed.iter().map(|id| (*id).to_owned()).collect(),
        connected_source_ids: connected_sources
            .iter()
            .map(|id| (*id).to_owned())
            .collect(),
        disconnected_source_ids: sources
            .difference(&connected_sources)
            .filter(|id| !failed.contains(*id))
            .map(|id| (*id).to_owned())
            .collect(),
        components: scenario
            .members
            .iter()
            .map(|members| members.iter().map(|id| (*id).to_owned()).collect())
            .collect(),
        isolated_router_ids: scenario.isolated,
        unresolved_links: network
            .network_links
            .iter()
            .filter(|link| link.unresolved || link.valid.is_none())
            .count(),
        baseline_connected_source_ids: baseline_sources.iter().map(|id| (*id).to_owned()).collect(),
        ..FailureReport::default()
    };
    for cell in cells {
        let cell = cell.borrow();
        let usable = usable_sources(cell);
        let local: BTreeSet<_> = usable
            .intersection(&sources)
            .filter(|id| !failed.contains(*id))
            .copied()
            .collect();
        let reference_source_count = local.intersection(&connected_sources).count();
        match cell.state {
            CoverageState::OutsideArea => report.outside_area_cells += 1,
            CoverageState::Unresolved => report.unresolved_cells += 1,
            CoverageState::UnknownTerrain | CoverageState::NotEvaluated => {
                report.unknown_cells += 1
            }
            CoverageState::Covered | CoverageState::Uncovered => {
                report.lost_local_cells +=
                    usize::from(local.is_empty() && !usable.is_disjoint(&sources));
                report.lost_reference_connected_cells += usize::from(
                    reference_source_count == 0 && !usable.is_disjoint(&baseline_sources),
                );
            }
        }
        report.cells.push(FailureCell {
            index: cell.index,
            state: cell.state,
            local_source_count: local.len(),
            reference_source_count,
            locally_covered: !local.is_empty(),
            reference_connected: reference_source_count != 0,
        });
    }
    report.newly_uncovered_cells = report.lost_local_cells;
    Ok(report)
}
