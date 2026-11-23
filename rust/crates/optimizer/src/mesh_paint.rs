use crate::{
    mesh, CandidateSettings, NetworkSolution, OptimizationError, PlanningSite, RoutingGraph,
};
use repeaternet_rf_core::Point;
use std::cmp::{Ordering, Reverse};
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_PAINT_SITES: usize = 512;
const MAX_INSTALLATION_SITES: usize = 64;
const MAX_SEEDS: usize = 8;
const MAX_ADDITIONS: usize = 24;
const MAX_ALTERNATIVES: usize = 4;

type SiteCoverage<'a> = dyn Fn(&PlanningSite) -> Result<Vec<bool>, OptimizationError> + 'a;

pub struct PaintObjective<'a> {
    /// Projected cell centres, in the same order returned by the evaluator.
    pub cells: &'a [Point],
    pub target_percent: u8,
    pub evaluate: &'a SiteCoverage<'a>,
}

#[derive(Clone, Debug)]
pub struct PaintCoverage {
    pub cell_count: usize,
    pub uncovered: Vec<usize>,
    pub target_percent: u8,
}

impl PaintCoverage {
    pub fn target_met(&self) -> bool {
        (self.cell_count - self.uncovered.len()) * 100
            >= self.cell_count * usize::from(self.target_percent)
    }
}

pub(crate) type SiteMasks = BTreeMap<String, Vec<u64>>;

pub(crate) fn mask(cells: Vec<bool>) -> Vec<u64> {
    cells
        .chunks(64)
        .map(|chunk| {
            chunk.iter().enumerate().fold(0, |bits, (index, &covered)| {
                bits | (u64::from(covered) << index)
            })
        })
        .collect()
}

pub(crate) fn report(
    sites: &[PlanningSite],
    masks: &SiteMasks,
    cell_count: usize,
    target_percent: u8,
) -> PaintCoverage {
    let mut covered = vec![0u64; cell_count.div_ceil(64)];
    for site in sites {
        union(&mut covered, &masks[&site.rf.id]);
    }
    PaintCoverage {
        cell_count,
        target_percent,
        uncovered: (0..cell_count)
            .filter(|&i| covered[i / 64] & (1 << (i % 64)) == 0)
            .collect(),
    }
}

fn union(left: &mut [u64], right: &[u64]) {
    for (left, right) in left.iter_mut().zip(right) {
        *left |= right;
    }
}

fn covered_count(bits: &[u64]) -> usize {
    bits.iter().map(|word| word.count_ones() as usize).sum()
}

pub(crate) fn compare(a: &NetworkSolution, b: &NetworkSolution) -> Ordering {
    match (&a.paint_coverage, &b.paint_coverage) {
        (Some(left), Some(right)) => left.target_met().cmp(&right.target_met()).then_with(|| {
            if left.target_met() {
                mesh::compare(a, b)
            } else {
                right
                    .uncovered
                    .len()
                    .cmp(&left.uncovered.len())
                    .then_with(|| mesh::compare(a, b))
            }
        }),
        _ => mesh::compare(a, b),
    }
}

pub(crate) fn connect_cover(
    graph: &RoutingGraph<'_>,
    settings: &CandidateSettings,
    cell_count: usize,
    target: u8,
    masks: &SiteMasks,
    cancelled: &dyn Fn() -> bool,
) -> Vec<NetworkSolution> {
    let terminals: BTreeSet<_> = (0..graph.sites.len())
        .filter(|&node| graph.sites[node].is_client() || graph.sites[node].is_required())
        .collect();
    let node_masks: Vec<_> = graph.sites.iter().map(|site| &masks[&site.rf.id]).collect();
    let assess = |nodes: &BTreeSet<usize>| {
        if cancelled()
            || nodes.len() > MAX_INSTALLATION_SITES
            || nodes
                .iter()
                .filter(|&&i| graph.sites[i].needs_new_router())
                .count()
                > settings.maximum_new_routers
        {
            return None;
        }
        let subset = graph.induced(&nodes.iter().copied().collect::<Vec<_>>());
        let mut solution = mesh::installation(&subset, settings.maximum_hops, true, cancelled)?;
        solution.paint_coverage = Some(report(&solution.sites, masks, cell_count, target));
        Some(solution)
    };
    let mut roots: Vec<_> = (0..graph.sites.len())
        .filter(|&i| !graph.sites[i].is_client())
        .collect();
    roots.sort_unstable_by_key(|&i| {
        (
            Reverse(covered_count(node_masks[i])),
            graph.sites[i].needs_new_router(),
            Reverse(graph.neighbors[i].len()),
            i,
        )
    });
    let mut seeds = BTreeSet::from([terminals.clone()]);
    for root in roots
        .iter()
        .copied()
        .take(MAX_SEEDS)
        .chain(
            roots
                .iter()
                .copied()
                .filter(|&i| graph.sites[i].is_required() || !graph.sites[i].needs_new_router())
                .take(MAX_SEEDS),
        )
        .collect::<BTreeSet<_>>()
    {
        if let Some(nodes) = mesh::grow(graph, &terminals, root, None, cancelled) {
            seeds.insert(terminals.iter().copied().chain(nodes).collect());
        }
    }
    // Preserve the ordinary mesh search's hop-aware fallback as a seed.
    for solution in mesh::connect_mesh(graph, settings, cancelled) {
        let ids: BTreeSet<_> = solution.sites.iter().map(|site| &site.rf.id).collect();
        seeds.insert(
            (0..graph.sites.len())
                .filter(|&i| ids.contains(&graph.sites[i].rf.id))
                .collect(),
        );
    }

    let mut best = BTreeMap::new();
    for seed in seeds {
        if cancelled() {
            return Vec::new();
        }
        let mut nodes = seed;
        let Some(mut solution) = assess(&nodes) else {
            continue;
        };
        for _ in 0..MAX_ADDITIONS {
            if solution.paint_coverage.as_ref().unwrap().target_met() || cancelled() {
                break;
            }
            let mut covered = vec![0; cell_count.div_ceil(64)];
            for &node in &nodes {
                union(&mut covered, node_masks[node]);
            }
            let original = covered_count(&covered);
            let router_count = solution.new_router_count();
            // Either client of a directly linked pair can attach the first router.
            let has_routers = nodes.iter().any(|&i| !graph.sites[i].is_client());
            let (_, parent) = mesh::steiner_paths(
                graph,
                nodes
                    .iter()
                    .copied()
                    .filter(|&i| !has_routers || !graph.sites[i].is_client()),
                &nodes,
                None,
                |_| false,
                cancelled,
            );
            let mut trials = Vec::new();
            for &node in &roots {
                if cancelled() {
                    return Vec::new();
                }
                if parent[node].is_none() || nodes.contains(&node) {
                    continue;
                }
                let mut path = Vec::new();
                let mut next = node;
                let mut gain = covered.clone();
                while !nodes.contains(&next) {
                    path.push(next);
                    union(&mut gain, node_masks[next]);
                    next = parent[next].expect("attachment reaches an installed site");
                }
                let gain = covered_count(&gain) - original;
                let cost = path
                    .iter()
                    .filter(|&&i| graph.sites[i].needs_new_router())
                    .count();
                if gain > 0 && router_count + cost <= settings.maximum_new_routers {
                    trials.push((path, gain, cost, node));
                }
            }
            trials.sort_unstable_by(|a, b| {
                (b.2 == 0)
                    .cmp(&(a.2 == 0))
                    .then_with(|| (b.1 * a.2.max(1)).cmp(&(a.1 * b.2.max(1))))
                    .then_with(|| b.1.cmp(&a.1))
                    .then_with(|| a.3.cmp(&b.3))
            });
            let Some((next, trial)) =
                trials
                    .into_iter()
                    .take(MAX_SEEDS)
                    .find_map(|(path, _, _, _)| {
                        let next = nodes.iter().copied().chain(path).collect();
                        assess(&next).map(|trial| (next, trial))
                    })
            else {
                break;
            };
            nodes = next;
            solution = trial;
        }
        // Remove dispensable routers, testing both objectives after each removal.
        let mut optional: Vec<_> = nodes.difference(&terminals).copied().collect();
        optional.sort_unstable_by_key(|&node| Reverse(graph.sites[node].needs_new_router()));
        for node in optional {
            nodes.remove(&node);
            if let Some(trial) = assess(&nodes).filter(|trial| compare(trial, &solution).is_gt()) {
                solution = trial;
            } else {
                nodes.insert(node);
            }
        }
        let incumbent = best
            .entry(solution.new_router_count())
            .or_insert_with(|| solution.clone());
        if compare(&solution, incumbent).is_gt() {
            *incumbent = solution;
        }
    }
    let mut alternatives: Vec<_> = best.into_values().collect();
    alternatives.sort_unstable_by(|a, b| compare(b, a));
    alternatives.truncate(MAX_ALTERNATIVES);
    alternatives
}

#[cfg(test)]
mod tests {
    use super::*;
    use repeaternet_rf_core::{EvaluationOptions, LinkEvaluator, Site, SiteKind};
    use repeaternet_terrain::{ArrayTerrain, TerrainCrs};

    fn solve(
        definitions: &[(&str, SiteKind, [bool; 4])],
        edges: &[(usize, usize)],
        maximum_new_routers: usize,
    ) -> Vec<NetworkSolution> {
        let sites: Vec<_> = definitions
            .iter()
            .enumerate()
            .map(|(i, &(id, kind, _))| {
                let mut site = Site::new(id, 500.0 + i as f64 * 100.0, 500.0);
                site.kind = kind;
                site.antenna_height_m = 20.0;
                PlanningSite::from(site)
            })
            .collect();
        let terrain = ArrayTerrain::new(
            101,
            101,
            vec![0.0; 101 * 101],
            None,
            Point { x: 0.0, y: 0.0 },
            10.0,
            TerrainCrs::from_epsg(25833).unwrap(),
        )
        .unwrap();
        let rf = Default::default();
        let evaluator = LinkEvaluator::new(&terrain, &rf);
        let links: Vec<_> = edges
            .iter()
            .map(|&(a, b)| {
                let link = evaluator
                    .evaluate(&sites[a].rf, &sites[b].rf, EvaluationOptions::default())
                    .unwrap();
                assert!(link.valid);
                link
            })
            .collect();
        let masks = definitions
            .iter()
            .map(|&(id, _, cells)| (id.to_owned(), mask(cells.to_vec())))
            .collect();
        let settings = CandidateSettings {
            maximum_new_routers,
            ..Default::default()
        };
        connect_cover(
            &RoutingGraph::new(&sites, &links).unwrap(),
            &settings,
            4,
            100,
            &masks,
            &|| false,
        )
    }

    #[test]
    fn one_new_router_is_minimal_when_clients_cover_only_half() {
        let sites = [
            ("Alice", SiteKind::Client, [true, false, false, false]),
            ("Bob", SiteKind::Client, [false, true, false, false]),
            ("Shared", SiteKind::Candidate, [false, false, true, true]),
            ("Nearby", SiteKind::Candidate, [false, false, true, false]),
            ("Isolated", SiteKind::Candidate, [true, true, true, true]),
        ];
        let links = [(0, 1), (0, 2), (1, 2), (0, 3), (1, 3)];
        let alternatives = solve(&sites, &links, 2);
        let best = &alternatives[0];
        assert!(best.paint_coverage.as_ref().unwrap().target_met());
        assert_eq!(best.new_router_count(), 1);
        assert_eq!(best.router_ids, ["Shared"]);
        assert_eq!(best.client_paths.len(), 1);
        assert!(best
            .links
            .iter()
            .any(|edge| edge.source_id == "Shared" || edge.target_id == "Shared"));
        let no_installation = solve(&sites, &links, 0);
        assert_eq!(
            no_installation[0]
                .paint_coverage
                .as_ref()
                .unwrap()
                .uncovered,
            [2, 3]
        );
    }

    #[test]
    fn without_clients_a_bridge_is_counted_and_a_disconnected_site_cannot_cover() {
        let sites = [
            ("West", SiteKind::Candidate, [true, true, false, false]),
            ("Bridge", SiteKind::Candidate, [false; 4]),
            ("East", SiteKind::Candidate, [false, false, true, true]),
            ("Isolated", SiteKind::Candidate, [false, false, true, true]),
        ];
        let links = [(0, 1), (1, 2)];
        let full = solve(&sites, &links, 3);
        assert!(full[0].paint_coverage.as_ref().unwrap().target_met());
        assert_eq!(full[0].new_router_count(), 3);
        assert_eq!(full[0].links.len(), 2);
        assert_eq!(full[0].router_ids, ["Bridge", "East", "West"]);
        let partial = solve(&sites, &links, 2);
        assert!(!partial[0].paint_coverage.as_ref().unwrap().target_met());
        assert_eq!(partial[0].new_router_count(), 1);
        assert_eq!(
            partial[0].paint_coverage.as_ref().unwrap().uncovered.len(),
            2
        );
    }

    #[test]
    fn a_single_client_cannot_relay_between_covering_routers() {
        let sites = [
            ("Client", SiteKind::Client, [false; 4]),
            ("West", SiteKind::Candidate, [true, true, false, false]),
            ("East", SiteKind::Candidate, [false, false, true, true]),
        ];
        let alternatives = solve(&sites, &[(0, 1), (0, 2)], 2);
        assert!(!alternatives[0]
            .paint_coverage
            .as_ref()
            .unwrap()
            .target_met());
        assert_eq!(alternatives[0].client_ids, ["Client"]);
        assert_eq!(alternatives[0].new_router_count(), 1);
    }
}
