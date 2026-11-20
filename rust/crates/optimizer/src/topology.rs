use crate::{
    compare_scores, CandidateSettings, OptimizationError, OptimizationPriority, PlanningSite,
    RoutingGraph, SiteOrigin,
};
use repeaternet_rf_core::LinkResult;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

const EXACT_LIMIT: usize = 100_000;
const BEAM_WIDTH: usize = 64;
const PATH_SEED_LIMIT: usize = 32;
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

    pub fn primary_distance_m(&self) -> f64 {
        self.primary_path_ids()
            .windows(2)
            .filter_map(|pair| {
                self.links.iter().find(|link| {
                    (link.source_id == pair[0] && link.target_id == pair[1])
                        || (link.source_id == pair[1] && link.target_id == pair[0])
                })
            })
            .map(|link| link.distance_m)
            .sum()
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
    if required.len() > settings.maximum_solution_routers {
        return Err(OptimizationError::InvalidInput(format!(
            "{} routers are required, but the solution limit is {}",
            required.len(),
            settings.maximum_solution_routers
        )));
    }
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
            (!s.is_client() && !s.is_required() && component[i] != usize::MAX).then_some(i)
        })
        .collect();
    let max_optional = (settings.maximum_solution_routers - required.len()).min(optional.len());
    let counts: Vec<_> = (0..=max_optional)
        .map(|k| combinations_count(optional.len(), k))
        .collect();
    let requested = settings.reliability_paths.max(1);
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
            seed_layers[seed.len()].insert(seed);
        }
        beam_layers(
            graph,
            &clients,
            &required,
            &optional,
            &protected,
            max_optional,
            cancelled,
        )
    } else {
        Vec::new()
    };

    let mut alternatives = Vec::new();
    for (depth, &count) in counts.iter().enumerate() {
        if cancelled() {
            return Ok(Vec::new());
        }
        let mut best: Option<(Vec<f64>, Vec<usize>, NetworkSolution)> = None;
        let mut evaluate = |selection: &[usize]| {
            let mut routers = required.clone();
            routers.extend_from_slice(selection);
            routers.sort_unstable();
            let mut nodes = clients.clone();
            nodes.extend(&routers);
            let subgraph = graph.induced(&nodes);
            if subgraph.bfs(0, |_, _, _| true).1.len() == nodes.len() {
                let reliability = (settings.priority == OptimizationPriority::MaximumReliability)
                    .then(|| {
                        representative_paths(&subgraph, clients.len(), requested, settings.priority)
                    });
                let score = topology_score(
                    &subgraph,
                    clients.len(),
                    reliability.as_ref().map_or(0, |(_, count)| *count),
                    settings.priority,
                );
                if best.as_ref().is_none_or(|(current, key, _)| {
                    compare_scores(&score, current)
                        .then_with(|| key.cmp(&routers))
                        .is_gt()
                }) {
                    let (paths, achieved) = reliability.unwrap_or_else(|| {
                        representative_paths(&subgraph, clients.len(), requested, settings.priority)
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
        if let Some((_, _, mut solution)) = best {
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

fn topology_score(
    graph: &RoutingGraph<'_>,
    clients: usize,
    achieved: usize,
    priority: OptimizationPriority,
) -> Vec<f64> {
    let mut margin = f64::INFINITY;
    for source in 0..clients - 1 {
        let widest = graph.widest_margins(source);
        for &value in &widest[source + 1..clients] {
            margin = margin.min(value);
        }
    }
    let routers = &graph.sites[clients..];
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
        return vec![leading, margin, degree as f64, mean, quality, -distance];
    }
    let fresnel = graph
        .edges
        .values()
        .map(|l| l.minimum_fresnel_clearance_ratio)
        .fold(f64::INFINITY, f64::min);
    let mut score = vec![margin, fresnel, -height, quality, -distance];
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
    let score = |solution: &NetworkSolution| {
        let height: f64 = solution
            .sites
            .iter()
            .filter(|s| !s.is_client())
            .map(|s| s.rf.antenna_height_m)
            .sum();
        let count = solution.router_count() as f64;
        let margin = solution.minimum_margin_db();
        match priority {
            OptimizationPriority::MinimumRouters => vec![count],
            OptimizationPriority::MinimumInfrastructure => vec![count * 100.0 + height, -margin],
            OptimizationPriority::MaximumReliability => vec![
                -(solution.achieved_path_count.min(requested) as f64),
                count,
                -margin,
            ],
            OptimizationPriority::FewestNewInstallations => vec![
                solution.proposed_router_count() as f64,
                count,
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

fn representative_paths(
    graph: &RoutingGraph<'_>,
    clients: usize,
    cutoff: usize,
    priority: OptimizationPriority,
) -> (ClientPaths, usize) {
    let mut by_pair = BTreeMap::new();
    let mut achieved = cutoff;
    for left in 0..clients {
        for right in left + 1..clients {
            let paths = if priority == OptimizationPriority::FewestNewInstallations {
                let preferred = graph.path(left, right, priority).expect("connected subset");
                let blocked = preferred[1..preferred.len() - 1].iter().copied().collect();
                let mut paths = disjoint_paths(
                    graph,
                    left,
                    right,
                    cutoff - 1,
                    &blocked,
                    preferred.len() == 2,
                );
                paths.insert(0, preferred);
                paths
            } else {
                let mut paths = disjoint_paths(graph, left, right, cutoff, &BTreeSet::new(), false);
                paths.sort_by(|a, b| {
                    a.len()
                        .cmp(&b.len())
                        .then_with(|| graph.bottleneck(b).total_cmp(&graph.bottleneck(a)))
                        .then_with(|| {
                            a.iter()
                                .map(|&i| &graph.sites[i].rf.id)
                                .cmp(b.iter().map(|&i| &graph.sites[i].rf.id))
                        })
                });
                paths
            };
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
    clients: &[usize],
    required: &[usize],
    optional: &[usize],
    protected: &BTreeSet<usize>,
    maximum: usize,
    cancelled: &dyn Fn() -> bool,
) -> Vec<Vec<Vec<usize>>> {
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
    let mut layers = vec![vec![Vec::new()]];
    for _ in 0..maximum {
        let mut expanded = BTreeSet::new();
        for selection in layers.last().expect("base beam layer") {
            for &node in &pool {
                if cancelled() {
                    return Vec::new();
                }
                if let Err(index) = selection.binary_search(&node) {
                    let mut trial = selection.clone();
                    trial.insert(index, node);
                    expanded.insert(trial);
                }
            }
        }
        let mut ranked = Vec::with_capacity(expanded.len());
        for selection in expanded {
            if cancelled() {
                return Vec::new();
            }
            let nodes: Vec<_> = clients
                .iter()
                .chain(required)
                .chain(&selection)
                .copied()
                .collect();
            let score = partial_score(graph, &nodes, clients.len());
            ranked.push((selection, score));
        }
        ranked.sort_unstable_by(|a, b| compare_scores(&b.1, &a.1).then_with(|| a.0.cmp(&b.0)));
        if ranked.is_empty() {
            break;
        }
        layers.push(
            ranked
                .into_iter()
                .take(BEAM_WIDTH)
                .map(|(selection, _)| selection)
                .collect(),
        );
    }
    layers
}

fn partial_score(graph: &RoutingGraph<'_>, nodes: &[usize], clients: usize) -> [f64; 6] {
    let induced = graph.induced(nodes);
    let mut seen = vec![false; nodes.len()];
    let mut connected_pairs = 0;
    let mut largest_client_component = 0;
    for node in 0..clients {
        if seen[node] {
            continue;
        }
        let (_, component) = induced.bfs(node, |_, _, _| true);
        let count = component.iter().filter(|&&i| i < clients).count();
        for i in component {
            seen[i] = true;
        }
        connected_pairs += count * (count - 1) / 2;
        largest_client_component = largest_client_component.max(count);
    }
    let edges = induced.edges.len();
    let boundary = nodes
        .iter()
        .map(|&i| graph.neighbors[i].len())
        .sum::<usize>()
        - 2 * edges;
    [
        connected_pairs as f64,
        largest_client_component as f64,
        edges as f64,
        boundary as f64,
        induced.edges.values().map(|l| l.worst_margin_db()).sum(),
        nodes[clients..]
            .iter()
            .map(|&i| graph.sites[i].site_quality)
            .sum(),
    ]
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
