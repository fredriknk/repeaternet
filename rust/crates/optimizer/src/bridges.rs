use super::{OptimizationError, Pair, RoutingGraph, Search, Stage};
use crate::{pair, PlanningSite};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

const SOURCES_PER_ROUND: usize = 12;
const MAX_ROUNDS: usize = 16;
const MAX_BRIDGE_PAIRS: usize = 64_000;

impl Search<'_, '_> {
    pub(super) fn search_bridges(
        &mut self,
        terminals: &BTreeSet<usize>,
    ) -> Result<bool, OptimizationError> {
        let mut expanded = BTreeMap::new();
        let mut remaining = MAX_BRIDGE_PAIRS;
        for _ in 0..MAX_ROUNDS {
            if self.hooks.cancelled() || remaining == 0 {
                break;
            }
            let pairs = {
                let graph = self.certified_graph()?;
                let terminals = self.bridge_terminals(&graph, terminals);
                let budget = self.settings.maximum_solution_routers;
                let required = terminals
                    .iter()
                    .filter(|&&i| graph.sites[i].is_required())
                    .count();
                let paths = TerminalPaths::new(&graph, &terminals);
                if let Some(seed) = paths.seed(&graph, budget) {
                    self.seeds.push(seed);
                    return Ok(true);
                }
                let indices: Vec<_> = graph
                    .sites
                    .iter()
                    .map(|site| self.by_id[&site.rf.id])
                    .collect();
                let context = BridgeContext {
                    graph: &graph,
                    indices: &indices,
                    checked: &self.final_checked,
                    distance_limit: self.settings.maximum_link_distance_m,
                    cancelled: &|| self.hooks.cancelled(),
                };
                paths.farthest_first().into_iter().find_map(|right| {
                    context.next_pairs(
                        &paths.anchor,
                        right,
                        budget - required,
                        &mut expanded,
                        remaining,
                    )
                })
            };
            let Some(pairs) = pairs else {
                break;
            };
            if pairs.is_empty() {
                continue;
            }
            let total = pairs.len();
            remaining -= total;
            self.hooks
                .notify(Stage::CheckingInteriorBridgeLinks, 0, total);
            let mut done = 0;
            self.certify_pairs(&pairs, |_, _, hooks| {
                done += 1;
                if done % 32 == 0 || done == total {
                    hooks.notify(Stage::CheckingInteriorBridgeLinks, done, total);
                }
            })?;
        }
        // The last batch can make a feasible route even when it exhausted the work budget.
        if !self.hooks.cancelled() && remaining != MAX_BRIDGE_PAIRS {
            let graph = self.certified_graph()?;
            let terminals = self.bridge_terminals(&graph, terminals);
            let paths = TerminalPaths::new(&graph, &terminals);
            if let Some(seed) = paths.seed(&graph, self.settings.maximum_solution_routers) {
                self.seeds.push(seed);
            }
            return Ok(true);
        }
        Ok(false)
    }

    fn certified_graph(&self) -> Result<RoutingGraph<'_>, OptimizationError> {
        RoutingGraph::from_links(
            &self.sites,
            self.links
                .iter()
                .filter(|(key, _)| self.final_checked.contains(key))
                .map(|(_, link)| link),
        )
    }

    fn bridge_terminals(
        &self,
        graph: &RoutingGraph<'_>,
        terminals: &BTreeSet<usize>,
    ) -> Vec<usize> {
        graph
            .sites
            .iter()
            .enumerate()
            .filter_map(|(i, site)| terminals.contains(&self.by_id[&site.rf.id]).then_some(i))
            .collect()
    }

    pub(super) fn diagnose_bridge_failure(
        &mut self,
        terminals: &BTreeSet<usize>,
    ) -> Result<(), OptimizationError> {
        // Include sampled provisional links when describing search coverage. A
        // link that failed final certification has already been removed.
        let graph = RoutingGraph::from_links(&self.sites, self.links.values())?;
        let terminals = self.bridge_terminals(&graph, terminals);
        let isolated: Vec<_> = terminals
            .iter()
            .filter(|&&node| graph.neighbors[node].is_empty())
            .map(|&node| graph.sites[node].rf.id.as_str())
            .collect();
        let budget = self.settings.maximum_solution_routers;
        let message = if !isolated.is_empty() {
            format!(
                "Isolated endpoints or required routers have no usable links in the search graph: {}.",
                isolated.join(", ")
            )
        } else {
            let paths = TerminalPaths::new(&graph, &terminals);
            if paths.disconnected() {
                "The search graph has disconnected components containing endpoints or required routers.".into()
            } else {
                let required = terminals
                    .iter()
                    .filter(|&&i| graph.sites[i].is_required())
                    .count();
                let lower_bound = required + paths.maximum_optional_routers();
                if lower_bound > budget {
                    format!(
                        "The terminals are connected in the search graph but over budget: at least {lower_bound} routers are needed with a limit of {budget}."
                    )
                } else {
                    format!(
                        "The search graph connects the endpoints and required routers, but no certified mesh was found within the {budget}-router limit."
                    )
                }
            }
        };
        self.diagnostics.push(message);
        Ok(())
    }
}

struct TerminalPaths {
    anchor: Reach,
    targets: Vec<usize>,
}

impl TerminalPaths {
    fn new(graph: &RoutingGraph<'_>, terminals: &[usize]) -> Self {
        let (root, targets) = terminals.split_first().expect("at least two terminals");
        Self {
            anchor: Reach::new(graph, *root),
            targets: targets.to_vec(),
        }
    }

    fn seed(&self, graph: &RoutingGraph<'_>, budget: usize) -> Option<Vec<String>> {
        let mut routers = BTreeSet::new();
        for &target in &self.targets {
            if self.anchor.routers[target] == usize::MAX {
                return None;
            }
            let mut node = Some(target);
            while let Some(current) = node {
                let site = graph.sites[current];
                if !site.is_client() {
                    routers.insert(site.rf.id.clone());
                }
                node = self.anchor.previous[current];
            }
        }
        (routers.len() <= budget).then(|| routers.into_iter().collect())
    }

    fn farthest_first(&self) -> Vec<usize> {
        let mut targets = self.targets.clone();
        targets.sort_unstable_by_key(|&node| (std::cmp::Reverse(self.anchor.routers[node]), node));
        targets
    }

    fn disconnected(&self) -> bool {
        self.maximum_optional_routers() == usize::MAX
    }

    fn maximum_optional_routers(&self) -> usize {
        self.targets
            .iter()
            .map(|&node| self.anchor.routers[node])
            .max()
            .unwrap_or(0)
    }
}

struct Reach {
    root: usize,
    routers: Vec<usize>,
    previous: Vec<Option<usize>>,
}

impl Reach {
    fn new(graph: &RoutingGraph<'_>, root: usize) -> Self {
        // Required routers already consume part of the budget, and clients cost zero.
        let mut routers = vec![usize::MAX; graph.sites.len()];
        let mut previous = vec![None; graph.sites.len()];
        let mut queue = VecDeque::from([root]);
        routers[root] = 0;
        while let Some(left) = queue.pop_front() {
            for &(right, _) in &graph.neighbors[left] {
                let cost = optional_router_cost(graph.sites[right]);
                let trial = routers[left] + cost;
                if trial < routers[right] {
                    routers[right] = trial;
                    previous[right] = Some(left);
                    if cost == 0 {
                        queue.push_front(right);
                    } else {
                        queue.push_back(right);
                    }
                }
            }
        }
        Self {
            root,
            routers,
            previous,
        }
    }
}

fn optional_router_cost(site: &PlanningSite) -> usize {
    usize::from(!site.is_client() && !site.is_required())
}

struct BridgeContext<'a, 'g> {
    graph: &'a RoutingGraph<'g>,
    indices: &'a [usize],
    checked: &'a BTreeSet<Pair>,
    distance_limit: Option<f64>,
    cancelled: &'a dyn Fn() -> bool,
}

impl BridgeContext<'_, '_> {
    fn next_pairs(
        &self,
        left: &Reach,
        target: usize,
        budget: usize,
        expanded: &mut BTreeMap<(usize, usize), usize>,
        remaining: usize,
    ) -> Option<Vec<Pair>> {
        let right = Reach::new(self.graph, target);
        let (left_depth, left_sources) = self.frontier(left, expanded, budget);
        let (right_depth, right_sources) = self.frontier(&right, expanded, budget);
        let (from, to, mut sources) =
            if (left_depth, left_sources.len()) <= (right_depth, right_sources.len()) {
                (left, &right, left_sources)
            } else {
                (&right, left, right_sources)
            };
        if sources.is_empty() {
            return None;
        }
        let sites = &self.graph.sites;
        let distance = |a: usize, b: usize| sites[a].rf.distance_to(&sites[b].rf);
        sources.sort_unstable_by(|&a, &b| {
            to.routers[a]
                .cmp(&to.routers[b])
                .then_with(|| distance(a, to.root).total_cmp(&distance(b, to.root)))
                .then_with(|| a.cmp(&b))
        });
        let mut targets: Vec<_> = (0..sites.len()).collect();
        targets.sort_unstable_by(|&a, &b| {
            to.routers[a]
                .cmp(&to.routers[b])
                .then_with(|| sites[b].site_quality.total_cmp(&sites[a].site_quality))
                .then_with(|| a.cmp(&b))
        });
        let mut unique = BTreeSet::new();
        let mut pairs = Vec::new();
        // A new edge must improve reach from this side. Try the closest
        // opposite-side frontiers first, then look for new intermediate sites.
        for source in sources.into_iter().take(SOURCES_PER_ROUND) {
            let depth = from.routers[source];
            expanded.insert((from.root, source), depth);
            for &target in &targets {
                if (self.cancelled)() {
                    return Some(Vec::new());
                }
                if pairs.len() == remaining {
                    return Some(pairs);
                }
                let key = pair(self.indices[source], self.indices[target]);
                let through_source = depth + optional_router_cost(sites[target]);
                if through_source < from.routers[target]
                    && through_source <= budget
                    && self
                        .distance_limit
                        .is_none_or(|limit| distance(source, target) <= limit)
                    && !self.checked.contains(&key)
                    && unique.insert(key)
                {
                    pairs.push(key);
                }
            }
        }
        Some(pairs)
    }

    fn frontier(
        &self,
        from: &Reach,
        expanded: &BTreeMap<(usize, usize), usize>,
        budget: usize,
    ) -> (usize, Vec<usize>) {
        let available = from.routers.iter().enumerate().filter(|&(node, &depth)| {
            depth <= budget
                && expanded
                    .get(&(from.root, node))
                    .is_none_or(|&previous| depth < previous)
        });
        let depth = available
            .clone()
            .map(|(_, &depth)| depth)
            .min()
            .unwrap_or(usize::MAX);
        let sources = available
            .filter_map(|(node, &cost)| (cost == depth).then_some(node))
            .collect();
        (depth, sources)
    }
}
