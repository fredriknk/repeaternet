use crate::graph::OrderedFloat;
use crate::paths::{margin_band, plan_routes, LinkGraph, Screened};
use crate::{
    compare_scores, link_is_clear, pair, CandidateSettings, OptimizationError,
    OptimizationPriority, PlanningSite, RoutingGraph, SiteOrigin,
};
use repeaternet_rf_core::LinkResult;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashSet, VecDeque};

const EXACT_LIMIT: usize = 100_000;
const BEAM_WIDTH: usize = 64;
const PATH_SEED_LIMIT: usize = 32;
const QUALITY_SEED_HOP_LIMIT: usize = 16;
const PAIRED_SEED_ROUTERS: usize = 4;
pub type ClientPaths = BTreeMap<(String, String), Vec<Vec<String>>>;

#[derive(Clone, Debug)]
pub struct NetworkSolution {
    pub name: String,
    pub sites: Vec<PlanningSite>,
    pub links: Vec<LinkResult>,
    pub client_ids: Vec<String>,
    pub router_ids: Vec<String>,
    pub client_paths: ClientPaths,
    pub requested_path_count: usize,
    pub achieved_path_count: usize,
    pub diagnostics: Vec<String>,
}

impl NetworkSolution {
    pub fn router_count(&self) -> usize {
        self.router_ids.len()
    }

    pub fn new_router_count(&self) -> usize {
        self.sites
            .iter()
            .filter(|site| site.needs_new_router())
            .count()
    }

    pub fn existing_router_count(&self) -> usize {
        self.sites
            .iter()
            .filter(|s| s.origin == SiteOrigin::Known && self.router_ids.contains(&s.rf.id))
            .count()
    }

    pub fn proposed_router_count(&self) -> usize {
        self.router_count() - self.existing_router_count()
    }

    pub fn resilient(&self) -> bool {
        self.achieved_path_count >= self.requested_path_count
    }

    pub fn minimum_margin_db(&self) -> f64 {
        self.links
            .iter()
            .map(LinkResult::worst_margin_db)
            .reduce(f64::min)
            .unwrap_or(f64::NEG_INFINITY)
    }

    pub fn primary_path_ids(&self) -> &[String] {
        self.client_paths
            .values()
            .next()
            .and_then(|paths| paths.first())
            .map_or(&[], Vec::as_slice)
    }

    pub fn primary_route(&self) -> Vec<&PlanningSite> {
        self.primary_path_ids()
            .iter()
            .filter_map(|id| self.sites.iter().find(|site| site.rf.id == *id))
            .collect()
    }

    pub fn total_link_distance_m(&self) -> f64 {
        self.links.iter().map(|link| link.distance_m).sum()
    }

    pub fn primary_links(&self) -> impl Iterator<Item = &LinkResult> {
        self.primary_path_ids().windows(2).filter_map(|pair| {
            self.links.iter().find(|link| {
                (link.source_id == pair[0] && link.target_id == pair[1])
                    || (link.source_id == pair[1] && link.target_id == pair[0])
            })
        })
    }

    pub fn primary_distance_m(&self) -> f64 {
        self.primary_links().map(|link| link.distance_m).sum()
    }
}

pub fn solve_topologies(
    sites: &[PlanningSite],
    links: &[LinkResult],
    settings: &CandidateSettings,
    cancelled: &dyn Fn() -> bool,
    seeds: &[Vec<String>],
) -> Result<Vec<NetworkSolution>, OptimizationError> {
    if cancelled() {
        return Ok(Vec::new());
    }
    solve_graph(
        &RoutingGraph::new(sites, links)?,
        settings,
        cancelled,
        seeds,
    )
}

pub(crate) fn solve_graph(
    graph: &RoutingGraph<'_>,
    settings: &CandidateSettings,
    cancelled: &dyn Fn() -> bool,
    seeds: &[Vec<String>],
) -> Result<Vec<NetworkSolution>, OptimizationError> {
    let clients: Vec<_> = graph
        .sites
        .iter()
        .enumerate()
        .filter_map(|(i, s)| s.is_client().then_some(i))
        .collect();
    if clients.len() < 2 || cancelled() {
        return Ok(Vec::new());
    }
    let required: Vec<_> = graph
        .sites
        .iter()
        .enumerate()
        .filter_map(|(i, s)| s.is_required().then_some(i))
        .collect();
    let (component, _) = graph.bfs(clients[0], |_, _, _| true);
    if clients
        .iter()
        .chain(&required)
        .any(|&node| component[node] == usize::MAX)
    {
        return Ok(Vec::new());
    }
    let optional: Vec<_> = graph
        .sites
        .iter()
        .enumerate()
        .filter_map(|(i, s)| {
            (!s.is_client()
                && !s.is_required()
                && component[i] != usize::MAX
                && (settings.maximum_new_routers > 0 || !s.needs_new_router()))
            .then_some(i)
        })
        .collect();
    let available = optional
        .iter()
        .filter(|&&i| !graph.sites[i].needs_new_router())
        .count();
    let requested = settings.reliability_paths.max(1);
    let max_optional = settings
        .maximum_new_routers
        .saturating_add(available)
        .min(optional.len())
        .min(
            settings
                .maximum_hops
                .saturating_sub(1)
                .saturating_mul(requested)
                .saturating_mul(clients.len() * (clients.len() - 1) / 2),
        );
    let within_new_budget = |nodes: &[usize]| {
        nodes
            .iter()
            .filter(|&&node| graph.sites[node].needs_new_router())
            .count()
            <= settings.maximum_new_routers
    };
    let counts: Vec<_> = (0..=max_optional)
        .map(|k| combinations_count(optional.len(), k))
        .collect();
    let heuristic = counts.iter().any(|&count| count > EXACT_LIMIT);
    let mut seed_layers = vec![BTreeSet::new(); max_optional + 1];
    let optional_ids: BTreeMap<_, _> = optional
        .iter()
        .map(|&i| (graph.sites[i].rf.id.as_str(), i))
        .collect();
    let required_ids: BTreeSet<_> = required
        .iter()
        .map(|&i| graph.sites[i].rf.id.as_str())
        .collect();
    for seed in seeds {
        let ids: BTreeSet<_> = seed.iter().map(String::as_str).collect();
        if required_ids.is_subset(&ids) {
            if let Some(nodes) = ids
                .difference(&required_ids)
                .map(|id| optional_ids.get(id).copied())
                .collect::<Option<Vec<_>>>()
            {
                if nodes.len() <= max_optional {
                    seed_layers[nodes.len()].insert(nodes);
                }
            }
        }
    }
    let beam = if heuristic {
        let terminals: Vec<_> = clients.iter().chain(&required).copied().collect();
        let (protected, path_seeds) = path_seeds(
            graph,
            &terminals,
            &optional,
            max_optional,
            requested,
            cancelled,
        );
        for seed in path_seeds {
            if within_new_budget(&seed) {
                seed_layers[seed.len()].insert(seed);
            }
        }
        beam_layers(
            graph,
            &terminals,
            &optional,
            &protected,
            max_optional,
            &within_new_budget,
            cancelled,
        )
    } else {
        Vec::new()
    };
    let paired_seeds = if clients.len() == 2
        && required.is_empty()
        && requested == 2
        && settings.priority == OptimizationPriority::MaximumReliability
        && counts
            .get(PAIRED_SEED_ROUTERS)
            .is_some_and(|&count| count > EXACT_LIMIT)
    {
        let Some(seeds) = paired_path_seeds(graph, clients[0], clients[1], cancelled) else {
            return Ok(Vec::new());
        };
        seeds
    } else {
        BTreeSet::new()
    };

    let mut alternatives = Vec::new();
    for (depth, &count) in counts.iter().enumerate() {
        if cancelled() {
            return Ok(Vec::new());
        }
        let mut best: Option<(Vec<f64>, Vec<usize>, NetworkSolution)> = None;
        let mut evaluate = |selection: &[usize]| {
            if !within_new_budget(selection) {
                return;
            }
            let mut routers = required.clone();
            routers.extend_from_slice(selection);
            routers.sort_unstable();
            let mut nodes = clients.clone();
            nodes.extend(&routers);
            let subgraph = graph.induced(&nodes);
            if subgraph.bfs(0, |_, _, _| true).1.len() == nodes.len() {
                let must_check_paths = settings.maximum_hops < nodes.len() - 1
                    || (clients.len() == 2 && !required.is_empty());
                let paths = must_check_paths
                    .then(|| representative_paths(&subgraph, clients.len(), requested, settings));
                if paths.as_ref().is_some_and(|(_, achieved)| *achieved == 0) {
                    return;
                }
                let achieved = if settings.priority == OptimizationPriority::MaximumReliability {
                    paths.as_ref().map_or_else(
                        || independent_path_count(&subgraph, clients.len(), requested),
                        |(_, achieved)| *achieved,
                    )
                } else {
                    0
                };
                let score = topology_score(
                    &subgraph,
                    clients.len(),
                    achieved,
                    settings.priority,
                    paths.as_ref().map(|(paths, _)| paths),
                );
                if best.as_ref().is_none_or(|(current, key, _)| {
                    compare_scores(&score, current)
                        .then_with(|| key.cmp(&routers))
                        .is_gt()
                }) {
                    let (paths, achieved) = paths.unwrap_or_else(|| {
                        representative_paths(&subgraph, clients.len(), requested, settings)
                    });
                    best = Some((
                        score,
                        routers,
                        make_solution(&subgraph, clients.len(), requested, achieved, paths),
                    ));
                }
            }
        };
        if count <= EXACT_LIMIT {
            for_each_combination(&optional, depth, cancelled, &mut evaluate);
        } else if let Some(layer) = beam.get(depth) {
            for selection in layer {
                if cancelled() {
                    return Ok(Vec::new());
                }
                evaluate(selection);
            }
        }
        for selection in &seed_layers[depth] {
            if cancelled() {
                return Ok(Vec::new());
            }
            evaluate(selection);
        }
        let mut solution = best.map(|(_, _, solution)| solution);
        if depth == PAIRED_SEED_ROUTERS && !paired_seeds.is_empty() {
            let replacement = improve_with_paired_seeds(
                graph,
                &clients,
                &paired_seeds,
                solution.as_ref(),
                settings,
                cancelled,
            );
            if cancelled() {
                return Ok(Vec::new());
            }
            if let Some(replacement) = replacement {
                solution = Some(replacement);
            }
        }
        if let Some(mut solution) = solution {
            solution.diagnostics.push(if count <= EXACT_LIMIT {
                format!("Exhaustive subset search within screened graph ({count} subsets).")
            } else {
                format!("Heuristic subset search within screened graph (beam width {BEAM_WIDTH}); global optimum not guaranteed.")
            });
            alternatives.push(solution);
        }
    }
    if cancelled() {
        return Ok(Vec::new());
    }
    if settings.priority == OptimizationPriority::MaximumReliability {
        if let Some(index) = select_active_solution_index(&alternatives, settings.priority) {
            let active = &mut alternatives[index];
            if !active.resilient() {
                active.diagnostics.push(format!(
                    "No topology provides {requested} independent client paths; best available provides {}.",
                    active.achieved_path_count
                ));
            }
        }
    }
    Ok(alternatives)
}

fn combinations_count(n: usize, k: usize) -> usize {
    let mut count = 1u128;
    for i in 1..=k.min(n - k) {
        count = count * (n - i + 1) as u128 / i as u128;
        if count > EXACT_LIMIT as u128 {
            return EXACT_LIMIT + 1;
        }
    }
    count as usize
}

fn for_each_combination(
    pool: &[usize],
    size: usize,
    cancelled: &dyn Fn() -> bool,
    visit: &mut impl FnMut(&[usize]),
) {
    let mut positions: Vec<_> = (0..size).collect();
    let mut nodes = Vec::with_capacity(size);
    while !cancelled() {
        nodes.clear();
        nodes.extend(positions.iter().map(|&i| pool[i]));
        visit(&nodes);
        let Some(i) = (0..size)
            .rev()
            .find(|&i| positions[i] < pool.len() - size + i)
        else {
            break;
        };
        positions[i] += 1;
        for j in i + 1..size {
            positions[j] = positions[j - 1] + 1;
        }
    }
}

fn make_solution(
    graph: &RoutingGraph<'_>,
    clients: usize,
    requested: usize,
    achieved: usize,
    client_paths: ClientPaths,
) -> NetworkSolution {
    let count = graph.sites.len() - clients;
    let mut links: Vec<_> = graph.edges.values().map(|&link| link.clone()).collect();
    links.sort_by(|a, b| edge_id(a).cmp(&edge_id(b)));
    NetworkSolution {
        name: format!("{count} router{}", if count == 1 { "" } else { "s" }),
        sites: graph.sites.iter().map(|&site| site.clone()).collect(),
        links,
        client_ids: graph.sites[..clients]
            .iter()
            .map(|s| s.rf.id.clone())
            .collect(),
        router_ids: graph.sites[clients..]
            .iter()
            .map(|s| s.rf.id.clone())
            .collect(),
        client_paths,
        requested_path_count: requested,
        achieved_path_count: achieved,
        diagnostics: Vec::new(),
    }
}

fn edge_id(link: &LinkResult) -> (&str, &str) {
    let a = link.source_id.as_str();
    let b = link.target_id.as_str();
    (a.min(b), a.max(b))
}

/// Rebuild the paths of a selected installation after final-resolution RF checks.
pub(crate) fn certify_installation(
    graph: &RoutingGraph<'_>,
    settings: &CandidateSettings,
) -> Option<NetworkSolution> {
    let (mut terminals, routers): (Vec<_>, Vec<_>) =
        (0..graph.sites.len()).partition(|&node| graph.sites[node].is_client());
    let clients = terminals.len();
    terminals.extend(routers);
    let subgraph = graph.induced(&terminals);
    if subgraph.bfs(0, |_, _, _| true).1.len() != terminals.len() {
        return None;
    }
    let requested = settings.reliability_paths.max(1);
    if subgraph
        .sites
        .iter()
        .filter(|site| site.needs_new_router())
        .count()
        > settings.maximum_new_routers
    {
        return None;
    }
    let (paths, achieved) = representative_paths(&subgraph, clients, requested, settings);
    if achieved == 0 {
        return None;
    }
    Some(make_solution(
        &subgraph, clients, requested, achieved, paths,
    ))
}

/// Build an installation around a planned route. `path` holds graph indices from
/// the first client to the second; the route stays the primary path even when the
/// induced mesh offers a stronger one that skips a required router.
pub(crate) fn path_installation(
    graph: &RoutingGraph<'_>,
    path: &[usize],
    settings: &CandidateSettings,
) -> Option<NetworkSolution> {
    let (&first, &last) = (path.first()?, path.last()?);
    let mut nodes = vec![first, last];
    nodes.extend_from_slice(&path[1..path.len() - 1]);
    let subgraph = graph.induced(&nodes);
    let local: Vec<usize> = std::iter::once(0)
        .chain(2..nodes.len())
        .chain(std::iter::once(1))
        .collect();
    if local
        .windows(2)
        .any(|hop| !subgraph.edges.contains_key(&pair(hop[0], hop[1])))
    {
        return None;
    }
    let requested = settings.reliability_paths.max(1);
    let blocked: BTreeSet<_> = local[1..local.len() - 1].iter().copied().collect();
    let skip_direct = local.len() == 2;
    let mut paths = vec![local];
    paths.extend(disjoint_paths(
        &subgraph,
        0,
        1,
        requested - 1,
        &blocked,
        skip_direct,
    ));
    paths.retain(|path| path.len() - 1 <= settings.maximum_hops);
    let achieved = paths.len().min(requested);
    let key = (
        subgraph.sites[0].rf.id.clone(),
        subgraph.sites[1].rf.id.clone(),
    );
    let ids = paths.iter().map(|path| subgraph.path_ids(path)).collect();
    Some(make_solution(
        &subgraph,
        2,
        requested,
        achieved,
        BTreeMap::from([(key, ids)]),
    ))
}

#[derive(Clone, Copy)]
struct PathQuality {
    los_blocked: usize,
    fresnel_impeded: usize,
    margin: f64,
}

impl PathQuality {
    const EMPTY: Self = Self {
        los_blocked: 0,
        fresnel_impeded: 0,
        margin: f64::INFINITY,
    };
    const MISSING: Self = Self {
        los_blocked: usize::MAX,
        fresnel_impeded: usize::MAX,
        margin: f64::NEG_INFINITY,
    };

    fn add_link(&mut self, link: &LinkResult) {
        self.los_blocked += usize::from(!link.los_clear);
        self.fresnel_impeded += usize::from(!link.fresnel_clear);
        self.margin = self.margin.min(link.worst_margin_db());
    }

    fn weakest(self, other: Self) -> Self {
        Self {
            los_blocked: self.los_blocked.max(other.los_blocked),
            fresnel_impeded: self.fresnel_impeded.max(other.fresnel_impeded),
            margin: self.margin.min(other.margin),
        }
    }

    fn no_worse_than(self, other: Self) -> bool {
        self.los_blocked <= other.los_blocked
            && self.fresnel_impeded <= other.fresnel_impeded
            && self.margin >= other.margin
    }

    fn cmp_clearance(self, other: Self) -> Ordering {
        other
            .los_blocked
            .cmp(&self.los_blocked)
            .then_with(|| other.fresnel_impeded.cmp(&self.fresnel_impeded))
    }

    fn cmp(self, other: Self) -> Ordering {
        self.cmp_clearance(other)
            .then_with(|| self.margin.total_cmp(&other.margin))
    }
}

fn reliability_quality(solution: &NetworkSolution, achieved: usize) -> [PathQuality; 2] {
    let links: BTreeMap<_, _> = solution
        .links
        .iter()
        .filter(|link| link.valid)
        .map(|link| (edge_id(link), link))
        .collect();
    let path_quality = |path: &[String]| {
        if path.len() < 2 {
            return PathQuality::MISSING;
        }
        let mut quality = PathQuality::EMPTY;
        for hop in path.windows(2) {
            let (a, b) = (hop[0].as_str(), hop[1].as_str());
            let Some(link) = links.get(&(a.min(b), a.max(b))) else {
                return PathQuality::MISSING;
            };
            quality.add_link(link);
        }
        quality
    };

    // The weakest primary and backup metrics across all client pairs determine
    // the network's quality. Only paths supported by the achieved count enter.
    if solution.client_paths.is_empty() {
        return [PathQuality::MISSING; 2];
    }
    let mut primary = PathQuality::EMPTY;
    let mut backup = PathQuality::EMPTY;
    for paths in solution.client_paths.values() {
        primary = primary.weakest(
            paths
                .first()
                .map_or(PathQuality::MISSING, |p| path_quality(p)),
        );
        if paths.len() < achieved {
            backup = PathQuality::MISSING;
        }
        for path in paths.iter().take(achieved).skip(1) {
            backup = backup.weakest(path_quality(path));
        }
    }
    [primary, backup]
}

fn compare_reliability_quality(a: [PathQuality; 2], b: [PathQuality; 2]) -> Ordering {
    a[0].cmp(b[0]).then_with(|| a[1].cmp(b[1]))
}

/// Compare fully certified networks at the same installation count across search passes.
pub(crate) fn improves_certified_pair(
    candidate: &NetworkSolution,
    incumbent: &NetworkSolution,
) -> bool {
    let achieved = candidate.achieved_path_count;
    if achieved != incumbent.achieved_path_count {
        return achieved > incumbent.achieved_path_count;
    }
    let next = reliability_quality(candidate, achieved);
    let current = reliability_quality(incumbent, achieved);
    next[0].no_worse_than(current[0])
        && next[1].no_worse_than(current[1])
        && compare_reliability_quality(next, current).is_gt()
}

fn improve_with_paired_seeds(
    graph: &RoutingGraph<'_>,
    clients: &[usize],
    seeds: &BTreeSet<[usize; PAIRED_SEED_ROUTERS]>,
    incumbent: Option<&NetworkSolution>,
    settings: &CandidateSettings,
    cancelled: &dyn Fn() -> bool,
) -> Option<NetworkSolution> {
    let incumbent_quality = incumbent
        .filter(|solution| solution.achieved_path_count == 2)
        .map(|solution| reliability_quality(solution, 2));
    let mut best = None;
    let mut best_quality = incumbent_quality;
    for routers in seeds {
        if cancelled() {
            return None;
        }
        let mut nodes = clients.to_vec();
        nodes.extend(routers);
        let subgraph = graph.induced(&nodes);
        if routers
            .iter()
            .filter(|&&node| graph.sites[node].needs_new_router())
            .count()
            > settings.maximum_new_routers
        {
            continue;
        }
        let (paths, achieved) = representative_paths(&subgraph, 2, 2, settings);
        if achieved != 2 {
            continue;
        }
        let candidate = make_solution(&subgraph, 2, 2, achieved, paths);
        let quality = reliability_quality(&candidate, 2);
        // A seed can restore redundancy or improve one path while preserving
        // every quality metric of both incumbent paths.
        if incumbent_quality.is_some_and(|current| {
            !quality[0].no_worse_than(current[0]) || !quality[1].no_worse_than(current[1])
        }) {
            continue;
        }
        if best_quality.is_none_or(|current| compare_reliability_quality(quality, current).is_gt())
        {
            best = Some(candidate);
            best_quality = Some(quality);
        }
    }
    best
}

fn topology_score(
    graph: &RoutingGraph<'_>,
    clients: usize,
    achieved: usize,
    priority: OptimizationPriority,
    constrained_paths: Option<&ClientPaths>,
) -> Vec<f64> {
    let mut clear = true;
    let mut margin = f64::INFINITY;
    if let Some(paths) = constrained_paths {
        let links = graph
            .edges
            .values()
            .map(|&link| (edge_id(link), link))
            .collect();
        (clear, margin) = connection_quality(paths, &links);
    } else {
        for source in 0..clients - 1 {
            let widest = graph.widest_margins(source, true);
            let targets = source + 1..clients;
            let fallback = targets
                .clone()
                .any(|target| widest[target] == f64::NEG_INFINITY)
                .then(|| graph.widest_margins(source, false));
            clear &= fallback.is_none();
            for target in targets {
                let value = if widest[target] == f64::NEG_INFINITY {
                    fallback.as_ref().expect("unrestricted margins")[target]
                } else {
                    widest[target]
                };
                margin = margin.min(value);
            }
        }
    }
    let routers = &graph.sites[clients..];
    let new_routers = routers
        .iter()
        .filter(|site| site.needs_new_router())
        .count() as f64;
    let height: f64 = routers.iter().map(|s| s.rf.antenna_height_m).sum();
    let quality: f64 = routers.iter().map(|s| s.site_quality).sum();
    let distance: f64 = graph.edges.values().map(|link| link.distance_m).sum();
    if matches!(
        priority,
        OptimizationPriority::MaximumReliability | OptimizationPriority::MinimumInfrastructure
    ) {
        let leading = if priority == OptimizationPriority::MaximumReliability {
            achieved as f64
        } else {
            -100.0 * routers.len() as f64 - height
        };
        let degree = graph.neighbors.iter().map(Vec::len).min().unwrap_or(0);
        let mean = graph
            .edges
            .values()
            .map(|link| link.worst_margin_db())
            .sum::<f64>()
            / graph.edges.len() as f64;
        return vec![
            leading,
            f64::from(clear),
            margin_band(margin),
            -new_routers,
            margin,
            degree as f64,
            mean,
            quality,
            -distance,
        ];
    }
    let fresnel = graph
        .edges
        .values()
        .map(|l| l.minimum_fresnel_clearance_ratio)
        .fold(f64::INFINITY, f64::min);
    let mut score = vec![
        f64::from(clear),
        margin_band(margin),
        -new_routers,
        margin,
        fresnel,
        -height,
        quality,
        -distance,
    ];
    if priority == OptimizationPriority::FewestNewInstallations {
        score.insert(
            0,
            -(routers
                .iter()
                .filter(|s| s.origin != SiteOrigin::Known)
                .count() as f64),
        );
    }
    score
}

pub fn select_active_solution_index(
    alternatives: &[NetworkSolution],
    priority: OptimizationPriority,
) -> Option<usize> {
    let requested = alternatives
        .iter()
        .map(|solution| solution.requested_path_count)
        .max()?;
    if priority == OptimizationPriority::MaximumReliability {
        return alternatives
            .iter()
            .enumerate()
            .map(|(index, solution)| {
                let achieved = solution.achieved_path_count.min(requested);
                (
                    index,
                    solution,
                    achieved,
                    reliability_quality(solution, achieved),
                )
            })
            .max_by(
                |(a_index, a, a_count, a_quality), (b_index, b, b_count, b_quality)| {
                    a_count
                        .cmp(b_count)
                        .then_with(|| {
                            a_quality[0]
                                .cmp_clearance(b_quality[0])
                                .then_with(|| {
                                    margin_band(a_quality[0].margin)
                                        .total_cmp(&margin_band(b_quality[0].margin))
                                })
                                .then_with(|| b.new_router_count().cmp(&a.new_router_count()))
                        })
                        .then_with(|| compare_reliability_quality(*a_quality, *b_quality))
                        .then_with(|| b.router_count().cmp(&a.router_count()))
                        .then_with(|| b.router_ids.cmp(&a.router_ids))
                        .then_with(|| b_index.cmp(a_index))
                },
            )
            .map(|(index, ..)| index);
    }
    // Fewest total routers starts with the shortest route. A longer route
    // competes with it when it has the same clearance and margin band.
    let baseline = alternatives.iter().min_by(|a, b| {
        let (a_clear, a_margin) = primary_connection_quality(a);
        let (b_clear, b_margin) = primary_connection_quality(b);
        a.router_count()
            .cmp(&b.router_count())
            .then_with(|| b_clear.cmp(&a_clear))
            .then_with(|| margin_band(b_margin).total_cmp(&margin_band(a_margin)))
    })?;
    let baseline_quality = primary_connection_quality(baseline);
    let score = |solution: &NetworkSolution| {
        let height: f64 = solution
            .sites
            .iter()
            .filter(|s| !s.is_client())
            .map(|s| s.rf.antenna_height_m)
            .sum();
        let count = solution.router_count() as f64;
        let (clear, margin) = primary_connection_quality(solution);
        match priority {
            OptimizationPriority::MinimumRouters => {
                let comparable = clear == baseline_quality.0
                    && margin_band(margin) == margin_band(baseline_quality.1);
                vec![
                    f64::from(!comparable),
                    solution.new_router_count() as f64,
                    count,
                    -margin,
                ]
            }
            OptimizationPriority::MinimumInfrastructure => {
                vec![
                    count * 100.0 + height,
                    -f64::from(clear),
                    -margin_band(margin),
                    solution.new_router_count() as f64,
                    -margin,
                ]
            }
            OptimizationPriority::MaximumReliability => unreachable!(),
            OptimizationPriority::FewestNewInstallations => vec![
                solution.proposed_router_count() as f64,
                count,
                -f64::from(clear),
                -margin,
                -solution
                    .links
                    .iter()
                    .map(|link| link.minimum_fresnel_clearance_ratio)
                    .reduce(f64::min)
                    .unwrap_or(f64::NEG_INFINITY),
                height,
                -solution
                    .sites
                    .iter()
                    .filter(|s| !s.is_client())
                    .map(|s| s.site_quality)
                    .sum::<f64>(),
                solution.links.iter().map(|link| link.distance_m).sum(),
            ],
        }
    };
    alternatives
        .iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| {
            compare_scores(&score(a), &score(b)).then_with(|| {
                if priority == OptimizationPriority::MinimumRouters {
                    Ordering::Equal
                } else {
                    a.router_ids.cmp(&b.router_ids)
                }
            })
        })
        .map(|(index, _)| index)
}

fn primary_connection_quality(solution: &NetworkSolution) -> (bool, f64) {
    let links: BTreeMap<_, _> = solution
        .links
        .iter()
        .map(|link| (edge_id(link), link))
        .collect();
    connection_quality(&solution.client_paths, &links)
}

fn connection_quality(
    paths: &ClientPaths,
    links: &BTreeMap<(&str, &str), &LinkResult>,
) -> (bool, f64) {
    let mut clear = true;
    let mut weakest: Option<f64> = None;
    for path in paths.values().filter_map(|paths| paths.first()) {
        for hop in path.windows(2) {
            let (a, b) = (hop[0].as_str(), hop[1].as_str());
            let Some(link) = links.get(&(a.min(b), a.max(b))) else {
                return (false, f64::NEG_INFINITY);
            };
            clear &= link_is_clear(link);
            let margin = link.worst_margin_db();
            weakest = Some(weakest.map_or(margin, |current| current.min(margin)));
        }
    }
    weakest.map_or((false, f64::NEG_INFINITY), |margin| (clear, margin))
}

fn independent_path_count(graph: &RoutingGraph<'_>, clients: usize, cutoff: usize) -> usize {
    let mut achieved = cutoff;
    for left in 0..clients {
        for right in left + 1..clients {
            achieved = achieved
                .min(disjoint_paths(graph, left, right, achieved, &BTreeSet::new(), false).len());
            if achieved == 1 {
                return achieved;
            }
        }
    }
    achieved
}

fn representative_paths(
    graph: &RoutingGraph<'_>,
    clients: usize,
    cutoff: usize,
    settings: &CandidateSettings,
) -> (ClientPaths, usize) {
    let priority = settings.priority;
    let waypoints: Vec<_> = if clients == 2 {
        (clients..graph.sites.len())
            .filter(|&i| graph.sites[i].is_required())
            .collect()
    } else {
        Vec::new()
    };
    let mut by_pair = BTreeMap::new();
    let mut achieved = cutoff;
    for left in 0..clients {
        for right in left + 1..clients {
            let path_priority = if priority == OptimizationPriority::FewestNewInstallations {
                priority
            } else {
                OptimizationPriority::MaximumReliability
            };
            let Some(preferred) = graph.path(left, right, path_priority).and_then(|path| {
                if waypoints.is_empty() && path.len() - 1 <= settings.maximum_hops {
                    Some(path)
                } else {
                    let planner = LinkGraph::new(
                        graph.sites.iter().map(|site| site.rf.position).collect(),
                        graph
                            .sites
                            .iter()
                            .map(|site| site.needs_new_router())
                            .collect(),
                        graph
                            .edges
                            .iter()
                            .map(|(&key, &link)| (key, Screened::from(link))),
                    );
                    plan_routes(
                        &planner,
                        left,
                        right,
                        &waypoints,
                        settings.maximum_hops,
                        settings.maximum_new_routers,
                        &|| false,
                    )
                    .pop()
                    .map(|path| path.nodes)
                }
            }) else {
                return (BTreeMap::new(), 0);
            };
            let blocked: BTreeSet<_> = preferred[1..preferred.len() - 1].iter().copied().collect();
            let skip_direct = preferred.len() == 2;
            let mut paths = if priority == OptimizationPriority::MaximumReliability && cutoff == 2 {
                graph
                    .reliability_path(left, right, |a, b, _| {
                        !blocked.contains(&a)
                            && !blocked.contains(&b)
                            && !(skip_direct && pair(a, b) == (left, right))
                    })
                    .into_iter()
                    .collect()
            } else {
                disjoint_paths(graph, left, right, cutoff - 1, &blocked, skip_direct)
            };
            paths.retain(|path| path.len() - 1 <= settings.maximum_hops);
            paths.insert(0, preferred);
            if paths.len() < cutoff
                && waypoints.is_empty()
                && priority != OptimizationPriority::FewestNewInstallations
            {
                // The strongest path can cross two independent paths. Use
                // residual flow to recover redundancy when that happens.
                let mut independent =
                    disjoint_paths(graph, left, right, cutoff, &BTreeSet::new(), false);
                independent.retain(|path| path.len() - 1 <= settings.maximum_hops);
                if independent.len() > paths.len() {
                    paths = independent;
                }
            }
            if waypoints.is_empty() && priority != OptimizationPriority::FewestNewInstallations {
                paths.sort_by(|a, b| {
                    graph
                        .path_is_clear(b)
                        .cmp(&graph.path_is_clear(a))
                        .then_with(|| graph.bottleneck(b).total_cmp(&graph.bottleneck(a)))
                        .then_with(|| a.len().cmp(&b.len()))
                        .then_with(|| {
                            a.iter()
                                .map(|&i| &graph.sites[i].rf.id)
                                .cmp(b.iter().map(|&i| &graph.sites[i].rf.id))
                        })
                });
            }
            achieved = achieved.min(paths.len());
            by_pair.insert(
                (
                    graph.sites[left].rf.id.clone(),
                    graph.sites[right].rf.id.clone(),
                ),
                paths.iter().map(|path| graph.path_ids(path)).collect(),
            );
        }
    }
    (by_pair, achieved)
}

#[derive(Clone, Copy)]
struct Arc {
    to: usize,
    reverse: usize,
    residual: u8,
    original: bool,
}

fn flow_arc(network: &mut [Vec<Arc>], from: usize, to: usize) {
    let reverse = network[to].len();
    let forward = network[from].len();
    network[from].push(Arc {
        to,
        reverse,
        residual: 1,
        original: true,
    });
    network[to].push(Arc {
        to: from,
        reverse: forward,
        residual: 0,
        original: false,
    });
}

fn flow_path(
    network: &[Vec<Arc>],
    source: usize,
    target: usize,
    eligible: impl Fn(Arc) -> bool,
) -> Option<Vec<(usize, usize)>> {
    let mut predecessor = vec![None; network.len()];
    predecessor[source] = Some((source, 0));
    let mut queue = VecDeque::from([source]);
    while let Some(node) = queue.pop_front() {
        for (i, &arc) in network[node].iter().enumerate() {
            if predecessor[arc.to].is_none() && eligible(arc) {
                predecessor[arc.to] = Some((node, i));
                if arc.to == target {
                    let mut result = Vec::new();
                    let mut current = target;
                    while current != source {
                        let (from, edge) = predecessor[current]?;
                        result.push((from, edge));
                        current = from;
                    }
                    result.reverse();
                    return Some(result);
                }
                queue.push_back(arc.to);
            }
        }
    }
    None
}

// Split internal vertices into input/output nodes with unit capacity. Residual
// augmenting paths can reroute an earlier choice to recover real redundancy.
fn disjoint_paths(
    graph: &RoutingGraph<'_>,
    left: usize,
    right: usize,
    cutoff: usize,
    blocked: &BTreeSet<usize>,
    skip_direct: bool,
) -> Vec<Vec<usize>> {
    if cutoff == 0 {
        return Vec::new();
    }
    let mut network = vec![Vec::new(); 2 * graph.sites.len()];
    for (node, neighbors) in graph.neighbors.iter().enumerate() {
        if node != left && node != right && !blocked.contains(&node) {
            flow_arc(&mut network, 2 * node, 2 * node + 1);
        }
        if node == right || blocked.contains(&node) {
            continue;
        }
        for &(next, _) in neighbors {
            if next != left
                && !blocked.contains(&next)
                && !(skip_direct && node == left && next == right)
            {
                flow_arc(&mut network, 2 * node + 1, 2 * next);
            }
        }
    }
    let (source, target) = (2 * left + 1, 2 * right);
    let mut flow = 0;
    while flow < cutoff {
        let Some(path) = flow_path(&network, source, target, |arc| arc.residual > 0) else {
            break;
        };
        for (from, edge) in path {
            let arc = network[from][edge];
            network[from][edge].residual -= 1;
            network[arc.to][arc.reverse].residual += 1;
        }
        flow += 1;
    }
    let mut paths = Vec::new();
    for _ in 0..flow {
        let edges = flow_path(&network, source, target, |arc| {
            arc.original && arc.residual == 0
        })
        .expect("integral flow decomposes into paths");
        let mut path = vec![left];
        for (from, edge) in edges {
            let arc = &mut network[from][edge];
            arc.residual = 1;
            if arc.to % 2 == 0 {
                path.push(arc.to / 2);
            }
        }
        paths.push(path);
    }
    paths
}

fn beam_layers(
    graph: &RoutingGraph<'_>,
    terminals: &[usize],
    optional: &[usize],
    protected: &BTreeSet<usize>,
    maximum: usize,
    within_new_budget: &dyn Fn(&[usize]) -> bool,
    cancelled: &dyn Fn() -> bool,
) -> Vec<Vec<Vec<usize>>> {
    let (clients, required) = terminals.split_at(
        terminals
            .iter()
            .take_while(|&&node| graph.sites[node].is_client())
            .count(),
    );
    let mut ranked: Vec<_> = optional
        .iter()
        .map(|&node| {
            let neighbors = &graph.neighbors[node];
            let margins = neighbors.iter().map(|(_, link)| link.worst_margin_db());
            let score = [
                neighbors
                    .iter()
                    .filter(|(i, _)| graph.sites[*i].is_client())
                    .count() as f64,
                neighbors.len() as f64,
                margins
                    .clone()
                    .reduce(f64::max)
                    .unwrap_or(f64::NEG_INFINITY),
                margins.sum(),
                graph.sites[node].site_quality,
            ];
            (node, score)
        })
        .collect();
    ranked.sort_unstable_by(|a, b| compare_scores(&b.1, &a.1).then_with(|| a.0.cmp(&b.0)));
    let target = optional.len().min(
        64.max(maximum.saturating_mul(24))
            .max(clients.len().saturating_mul(16)),
    );
    let mut pool = protected.clone();
    for (node, _) in ranked {
        if pool.len() >= target {
            break;
        }
        pool.insert(node);
    }
    let mut scorer = BeamScorer::new(graph, clients, required, &pool);
    let mut layers = vec![vec![Vec::new()]];
    for _ in 0..maximum {
        let mut expanded = HashSet::new();
        let mut trial = Vec::new();
        for selection in layers.last().expect("base beam layer") {
            for &node in &pool {
                if cancelled() {
                    return Vec::new();
                }
                if let Err(index) = selection.binary_search(&node) {
                    trial.clear();
                    trial.extend_from_slice(&selection[..index]);
                    trial.push(node);
                    trial.extend_from_slice(&selection[index..]);
                    if within_new_budget(&trial) && !expanded.contains(trial.as_slice()) {
                        expanded.insert(trial.clone());
                    }
                }
            }
        }
        let mut ranked = Vec::with_capacity(expanded.len());
        for selection in expanded {
            if cancelled() {
                return Vec::new();
            }
            let score = scorer.score(&selection);
            ranked.push((selection, score));
        }
        if ranked.is_empty() {
            break;
        }
        let compare = |a: &(Vec<usize>, [f64; 6]), b: &(Vec<usize>, [f64; 6])| {
            compare_scores(&b.1, &a.1).then_with(|| a.0.cmp(&b.0))
        };
        if ranked.len() > BEAM_WIDTH {
            ranked.select_nth_unstable_by(BEAM_WIDTH, compare);
            ranked.truncate(BEAM_WIDTH);
        }
        ranked.sort_unstable_by(compare);
        layers.push(ranked.into_iter().map(|(selection, _)| selection).collect());
    }
    layers
}

struct BeamScorer<'g, 'a> {
    graph: &'g RoutingGraph<'a>,
    clients: usize,
    terminal_count: usize,
    nodes: Vec<usize>,
    parents: Vec<usize>,
    client_counts: Vec<usize>,
    // Cache only the beam pool and terminals. Large pools use the sparse graph.
    margins: Vec<Vec<Option<f64>>>,
    positions: Vec<usize>,
}

impl<'g, 'a> BeamScorer<'g, 'a> {
    fn new(
        graph: &'g RoutingGraph<'a>,
        clients: &[usize],
        required: &[usize],
        pool: &BTreeSet<usize>,
    ) -> Self {
        let nodes: Vec<_> = clients.iter().chain(required).copied().collect();
        let mut margins = Vec::new();
        let mut positions = Vec::new();
        let size = nodes.len() + pool.len();
        if size <= 512 {
            positions.resize(graph.sites.len(), usize::MAX);
            for (index, &node) in nodes.iter().chain(pool).enumerate() {
                positions[node] = index;
            }
            margins = vec![vec![None; size]; size];
            for (&(left, right), link) in &graph.edges {
                let (a, b) = (positions[left], positions[right]);
                if a != usize::MAX && b != usize::MAX {
                    let margin = Some(link.worst_margin_db());
                    margins[a][b] = margin;
                    margins[b][a] = margin;
                }
            }
        }
        Self {
            graph,
            clients: clients.len(),
            terminal_count: nodes.len(),
            nodes,
            parents: Vec::new(),
            client_counts: Vec::new(),
            margins,
            positions,
        }
    }

    fn score(&mut self, selection: &[usize]) -> [f64; 6] {
        self.nodes.truncate(self.terminal_count);
        self.nodes.extend_from_slice(selection);
        self.parents.clear();
        self.parents.extend(0..self.nodes.len());
        self.client_counts.clear();
        self.client_counts.resize(self.clients, 1);
        self.client_counts.resize(self.nodes.len(), 0);
        let mut connected_pairs = 0;
        let mut largest_client_component = 1;
        let mut edges = 0;
        let mut margin_sum = -0.0;
        for (left, &a) in self.nodes.iter().enumerate() {
            let row = (!self.margins.is_empty()).then(|| &self.margins[self.positions[a]]);
            // Match the (local left, local right) order used by the induced graph.
            for (offset, &b) in self.nodes[left + 1..].iter().enumerate() {
                let margin = match row {
                    Some(row) => row[self.positions[b]],
                    None => self
                        .graph
                        .edges
                        .get(&pair(a, b))
                        .map(|l| l.worst_margin_db()),
                };
                if let Some(margin) = margin {
                    edges += 1;
                    margin_sum += margin;
                    let root_a = component_root(&mut self.parents, left);
                    let root_b = component_root(&mut self.parents, left + 1 + offset);
                    if root_a != root_b {
                        self.parents[root_b] = root_a;
                        connected_pairs += self.client_counts[root_a] * self.client_counts[root_b];
                        self.client_counts[root_a] += self.client_counts[root_b];
                        largest_client_component =
                            largest_client_component.max(self.client_counts[root_a]);
                    }
                }
            }
        }
        let boundary = self
            .nodes
            .iter()
            .map(|&i| self.graph.neighbors[i].len())
            .sum::<usize>()
            - 2 * edges;
        [
            connected_pairs as f64,
            largest_client_component as f64,
            edges as f64,
            boundary as f64,
            margin_sum,
            self.nodes[self.clients..]
                .iter()
                .map(|&i| self.graph.sites[i].site_quality)
                .sum(),
        ]
    }
}

fn component_root(parents: &mut [usize], mut node: usize) -> usize {
    while parents[node] != node {
        parents[node] = parents[parents[node]];
        node = parents[node];
    }
    node
}

fn shortest_tree(
    graph: &RoutingGraph<'_>,
    start: usize,
    target: Option<usize>,
    allowed: impl Fn(usize, usize) -> bool,
) -> Vec<Option<usize>> {
    let mut predecessor = vec![None; graph.sites.len()];
    let mut pending = VecDeque::from([start]);
    predecessor[start] = Some(start);
    while let Some(left) = pending.pop_front() {
        if Some(left) == target {
            break;
        }
        for &(right, _) in &graph.neighbors[left] {
            if predecessor[right].is_none() && allowed(left, right) {
                predecessor[right] = Some(left);
                pending.push_back(right);
            }
        }
    }
    predecessor
}

fn tree_path(predecessor: &[Option<usize>], from: usize, to: usize) -> Option<Vec<usize>> {
    let mut path = vec![to];
    let mut node = to;
    while node != from {
        node = predecessor[node]?;
        path.push(node);
    }
    path.reverse();
    Some(path)
}

// A hop-bounded widest-path search adds seeds that a shortest-path enumeration
// can miss, especially when many weaker paths tie on hop count.
fn margin_path_seeds(
    graph: &RoutingGraph<'_>,
    source: usize,
    target: usize,
    max_hops: usize,
    clear_only: bool,
    cancelled: &dyn Fn() -> bool,
) -> Vec<Vec<usize>> {
    let mut margins = vec![f64::NEG_INFINITY; graph.sites.len()];
    margins[source] = f64::INFINITY;
    let mut predecessors = Vec::new();
    let mut seeds = Vec::new();
    for _ in 0..max_hops {
        let mut next = margins.clone();
        let mut previous: Vec<_> = (0..margins.len()).collect();
        for (left, &margin) in margins.iter().enumerate() {
            if cancelled() {
                return seeds;
            }
            if margin == f64::NEG_INFINITY {
                continue;
            }
            for &(right, link) in &graph.neighbors[left] {
                if clear_only && !link_is_clear(link) {
                    continue;
                }
                let trial = margin.min(link.worst_margin_db());
                if trial > next[right] {
                    next[right] = trial;
                    previous[right] = left;
                }
            }
        }
        if next == margins {
            break;
        }
        predecessors.push(previous);
        if next[target] > margins[target] {
            let mut node = target;
            let mut path = vec![target];
            for layer in predecessors.iter().rev() {
                let previous = layer[node];
                if previous != node {
                    path.push(previous);
                    node = previous;
                }
            }
            path.reverse();
            seeds.push(path);
        }
        margins = next;
    }
    seeds
}

// Keep the shortlist bounded while scanning three-hop routes, then pair each
// retained route with its strongest internally disjoint partner.
fn paired_path_seeds(
    graph: &RoutingGraph<'_>,
    source: usize,
    target: usize,
    cancelled: &dyn Fn() -> bool,
) -> Option<BTreeSet<[usize; PAIRED_SEED_ROUTERS]>> {
    let mut routes = BinaryHeap::with_capacity(PATH_SEED_LIMIT);
    for &(x, ax) in &graph.neighbors[source] {
        if x == target {
            continue;
        }
        for &(y, xy) in &graph.neighbors[x] {
            if cancelled() {
                return None;
            }
            if y == source || y == target {
                continue;
            }
            let Some(&yb) = graph.edges.get(&pair(y, target)) else {
                continue;
            };
            let mut quality = PathQuality::EMPTY;
            for link in [ax, xy, yb] {
                quality.add_link(link);
            }
            let route = (
                OrderedFloat(-quality.margin),
                quality.los_blocked,
                quality.fresnel_impeded,
                x,
                y,
            );
            if routes.len() < PATH_SEED_LIMIT {
                routes.push(route);
            } else if routes.peek().is_some_and(|worst| route < *worst) {
                *routes.peek_mut().expect("full shortlist") = route;
            }
        }
    }
    let routes = routes.into_sorted_vec();
    let mut seeds = BTreeSet::new();
    for &(_, _, _, x, y) in &routes {
        if let Some(&(_, _, _, u, v)) = routes
            .iter()
            .find(|&&(_, _, _, u, v)| x != u && x != v && y != u && y != v)
        {
            let mut nodes = [x, y, u, v];
            nodes.sort_unstable();
            seeds.insert(nodes);
        }
    }
    Some(seeds)
}

fn path_seeds(
    graph: &RoutingGraph<'_>,
    terminals: &[usize],
    optional: &[usize],
    maximum: usize,
    requested: usize,
    cancelled: &dyn Fn() -> bool,
) -> (BTreeSet<usize>, BTreeSet<Vec<usize>>) {
    let optional: BTreeSet<_> = optional.iter().copied().collect();
    let mut protected = BTreeSet::new();
    let mut seeds = BTreeSet::new();
    let mut add = |nodes: Vec<usize>| {
        let nodes: BTreeSet<_> = nodes
            .into_iter()
            .filter(|node| optional.contains(node))
            .collect();
        if nodes.len() <= maximum {
            seeds.insert(nodes.into_iter().collect());
        }
    };
    let mut closure = Vec::new();
    for (i, &left) in terminals.iter().enumerate() {
        if cancelled() {
            return (protected, seeds);
        }
        let tree = shortest_tree(graph, left, None, |_, _| true);
        for (j, &right) in terminals.iter().enumerate().skip(i + 1) {
            if let Some(path) = tree_path(&tree, left, right) {
                protected.extend(path.iter().copied().filter(|node| optional.contains(node)));
                closure.push((path.len(), i, j, path));
            }
        }
    }
    if terminals.len() == 2 {
        let (source, target) = (terminals[0], terminals[1]);
        let max_hops = maximum.saturating_add(1).min(QUALITY_SEED_HOP_LIMIT);
        let has_obstructed_links = graph.edges.values().any(|link| !link_is_clear(link));
        for clear_only in [true, false]
            .into_iter()
            .take(1 + usize::from(has_obstructed_links))
        {
            for path in margin_path_seeds(graph, source, target, max_hops, clear_only, cancelled) {
                protected.extend(path.iter().copied().filter(|node| optional.contains(node)));
                add(path);
            }
        }
        if let Some((_, _, _, first)) = closure.pop() {
            let mut paths = vec![first];
            let mut pending = BTreeSet::new();
            // Yen's loopless k-shortest paths with unit RF hop weights.
            while paths.len() <= PATH_SEED_LIMIT && !cancelled() {
                let path = paths.last().expect("initial path");
                if path.len() - 2 > maximum {
                    break;
                }
                add(path.clone());
                if paths.len() == PATH_SEED_LIMIT {
                    break;
                }
                for i in 0..path.len() - 1 {
                    if cancelled() {
                        break;
                    }
                    let root = &path[..=i];
                    let spur = path[i];
                    let tree = shortest_tree(graph, spur, Some(target), |left, right| {
                        !root[..i].contains(&right)
                            && !(left == spur
                                && paths.iter().any(|old| {
                                    old.starts_with(root) && old.get(i + 1) == Some(&right)
                                }))
                    });
                    if let Some(spur_path) = tree_path(&tree, spur, target) {
                        let trial: Vec<_> = root[..i].iter().chain(&spur_path).copied().collect();
                        pending.insert((trial.len(), trial));
                    }
                }
                let Some((_, next)) = pending.pop_first() else {
                    break;
                };
                paths.push(next);
            }
            if requested > 1 && !cancelled() {
                add(
                    disjoint_paths(graph, source, target, requested, &BTreeSet::new(), false)
                        .into_iter()
                        .flatten()
                        .collect(),
                );
            }
        }
    } else {
        let mut hubs: Vec<_> = optional
            .iter()
            .copied()
            .chain(terminals.iter().copied())
            .collect();
        hubs.sort_unstable_by_key(|&node| (std::cmp::Reverse(graph.neighbors[node].len()), node));
        let roots: BTreeSet<_> = terminals
            .iter()
            .copied()
            .chain(hubs.into_iter().take(PATH_SEED_LIMIT))
            .collect();
        for root in roots {
            if cancelled() {
                return (protected, seeds);
            }
            let tree = shortest_tree(graph, root, None, |_, _| true);
            if let Some(paths) = terminals
                .iter()
                .map(|&terminal| tree_path(&tree, root, terminal))
                .collect::<Option<Vec<_>>>()
            {
                add(paths.into_iter().flatten().collect());
            }
        }
        // Expand a minimum spanning tree over the terminal metric closure
        // into paths on the RF graph for another Steiner subset seed.
        closure.sort_unstable();
        let mut groups: Vec<_> = (0..terminals.len()).collect();
        let mut tree_nodes = Vec::new();
        for (_, left, right, path) in closure {
            let (a, b) = (groups[left], groups[right]);
            if a != b {
                for group in &mut groups {
                    if *group == b {
                        *group = a;
                    }
                }
                tree_nodes.extend(path);
            }
        }
        add(tree_nodes);
    }
    (protected, seeds)
}
