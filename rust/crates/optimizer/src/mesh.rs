use crate::graph::OrderedFloat;
use crate::{pair, CandidateSettings, ClientPaths, NetworkSolution, RoutingGraph};
use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, VecDeque};

/// Connect terminals on a screened RF graph, charging a new router once
/// regardless of how many paths use it. Each budget gets its best installation
/// found by the search. Candidate sampling and RF certification are separate.
pub fn connect_mesh(
    graph: &RoutingGraph<'_>,
    settings: &CandidateSettings,
    cancelled: &dyn Fn() -> bool,
) -> Vec<NetworkSolution> {
    let (clients, routers): (Vec<_>, Vec<_>) =
        (0..graph.sites.len()).partition(|&node| graph.sites[node].is_client());
    if clients.len() < 2 || cancelled() {
        return Vec::new();
    }
    let (required, optional): (Vec<_>, Vec<_>) = routers
        .into_iter()
        .partition(|&node| graph.sites[node].is_required());
    let mut roots = optional.clone();
    roots.sort_unstable_by_key(|&node| {
        (
            Reverse(
                graph.neighbors[node]
                    .iter()
                    .filter(|(to, _)| graph.sites[*to].is_client())
                    .count(),
            ),
            graph.sites[node].needs_new_router(),
            Reverse(graph.neighbors[node].len()),
            node,
        )
    });
    roots.truncate(24);
    roots.extend(clients.iter().chain(&required).take(8).copied());
    let terminals: BTreeSet<_> = clients.iter().chain(&required).copied().collect();
    let mut selections = BTreeSet::from([Vec::new()]);
    for &root in &roots {
        if cancelled() {
            return Vec::new();
        }
        if let Some(nodes) = grow(graph, &terminals, root, None, cancelled) {
            selections.insert(nodes);
        }
    }
    // Try each single-router star, even when it did not rank among the roots.
    for &node in &optional {
        if graph.neighbors[node]
            .iter()
            .filter(|(to, _)| graph.sites[*to].is_client())
            .count()
            == clients.len()
        {
            selections.insert(vec![node]);
        }
    }
    let mut solutions = BTreeMap::new();
    let assess = |nodes: &[usize]| {
        let count = nodes
            .iter()
            .filter(|&&node| graph.sites[node].needs_new_router())
            .count();
        if count > settings.maximum_new_routers || cancelled() {
            return None;
        }
        let all: Vec<_> = clients
            .iter()
            .chain(&required)
            .chain(nodes)
            .copied()
            .collect();
        mesh_installation(&graph.induced(&all), settings.maximum_hops, cancelled)
    };
    for nodes in &selections {
        if let Some(solution) = assess(nodes) {
            keep_best(&mut solutions, solution);
        }
    }
    // Direct links and client-specific routers can connect every pair even
    // when there is no single router backbone. Seed those installations too.
    if solutions.is_empty() {
        selections.extend(pairwise_seed(
            graph, &clients, &required, settings, cancelled,
        ));
    }
    // Explore alternate connections by forbidding a paid router from the best
    // installation. Other roots can still reuse the rest of its infrastructure.
    if let Some(best) = solutions.values().next() {
        let banned: Vec<_> = optional
            .iter()
            .copied()
            .filter(|&node| {
                let site = graph.sites[node];
                site.needs_new_router() && best.router_ids.contains(&site.rf.id)
            })
            .collect();
        for node in banned {
            for &root in roots.iter().take(8).chain(clients.first()) {
                if cancelled() {
                    return Vec::new();
                }
                if let Some(nodes) = grow(graph, &terminals, root, Some(node), cancelled) {
                    selections.insert(nodes);
                }
            }
        }
    }
    for mut nodes in selections {
        if cancelled() {
            return Vec::new();
        }
        let Some(mut best) = assess(&nodes) else {
            continue;
        };
        // Prune any optional router whose removal improves the objective.
        for index in (0..nodes.len()).rev() {
            let node = nodes.remove(index);
            if let Some(trial) = assess(&nodes).filter(|trial| compare(trial, &best).is_gt()) {
                best = trial;
            } else {
                nodes.insert(index, node);
            }
        }
        keep_best(&mut solutions, best);
    }
    solutions.into_values().collect()
}

pub(crate) fn largest_hop_count(paths: &ClientPaths) -> usize {
    paths
        .values()
        .flatten()
        .map(|path| path.len() - 1)
        .max()
        .unwrap_or(0)
}

pub(crate) fn compare(a: &NetworkSolution, b: &NetworkSolution) -> std::cmp::Ordering {
    b.new_router_count()
        .cmp(&a.new_router_count())
        .then_with(|| a.minimum_margin_db().total_cmp(&b.minimum_margin_db()))
        .then_with(|| largest_hop_count(&b.client_paths).cmp(&largest_hop_count(&a.client_paths)))
        .then_with(|| b.router_count().cmp(&a.router_count()))
        .then_with(|| b.router_ids.cmp(&a.router_ids))
}

fn keep_best(solutions: &mut BTreeMap<usize, NetworkSolution>, solution: NetworkSolution) {
    let count = solution.new_router_count();
    if solutions
        .get(&count)
        .is_none_or(|best| compare(&solution, best).is_gt())
    {
        solutions.insert(count, solution);
    }
}

fn pairwise_seed(
    graph: &RoutingGraph<'_>,
    clients: &[usize],
    required: &[usize],
    settings: &CandidateSettings,
    cancelled: &dyn Fn() -> bool,
) -> Option<Vec<usize>> {
    let mut used = BTreeSet::new();
    let mut unattached: BTreeSet<_> = required.iter().copied().collect();
    for (index, &source) in clients.iter().enumerate() {
        if cancelled() {
            return None;
        }
        let prev = mesh_tree(graph, source, settings.maximum_hops, |to, _| {
            settings.maximum_new_routers > 0 || !graph.sites[to].needs_new_router()
        });
        let mut add_path = |mut target: usize| {
            if prev[target].is_none() {
                return false;
            }
            while let Some(from) = prev[target] {
                let site = graph.sites[target];
                if !site.is_client() && !site.is_required() {
                    used.insert(target);
                }
                target = from;
            }
            true
        };
        if !clients[index + 1..].iter().copied().all(&mut add_path) {
            return None;
        }
        unattached.retain(|&target| !add_path(target));
    }
    unattached.is_empty().then(|| used.into_iter().collect())
}

fn mesh_tree(
    graph: &RoutingGraph<'_>,
    source: usize,
    max_hops: usize,
    eligible: impl Fn(usize, &repeaternet_rf_core::LinkResult) -> bool,
) -> Vec<Option<usize>> {
    let mut prev = vec![None; graph.sites.len()];
    let mut depth = vec![usize::MAX; graph.sites.len()];
    let mut queue = VecDeque::from([source]);
    depth[source] = 0;
    while let Some(node) = queue.pop_front() {
        if depth[node] == max_hops || (node != source && graph.sites[node].is_client()) {
            continue;
        }
        for &(to, link) in &graph.neighbors[node] {
            if depth[to] == usize::MAX && eligible(to, link) {
                prev[to] = Some(node);
                depth[to] = depth[node] + 1;
                queue.push_back(to);
            }
        }
    }
    prev
}

/// Grow a node-weighted Steiner subset using multi-source shortest paths from
/// the routers already in the installation. A client stops path expansion.
fn grow(
    graph: &RoutingGraph<'_>,
    terminals: &BTreeSet<usize>,
    root: usize,
    banned: Option<usize>,
    cancelled: &dyn Fn() -> bool,
) -> Option<Vec<usize>> {
    if banned == Some(root) {
        return None;
    }
    let mut installed = BTreeSet::from([root]);
    let mut remaining = terminals.clone();
    remaining.remove(&root);
    while !remaining.is_empty() {
        let sources = installed
            .iter()
            .copied()
            .filter(|&node| node == root || !graph.sites[node].is_client());
        let mut heap = BinaryHeap::new();
        let mut best = vec![None; graph.sites.len()];
        let mut predecessor = vec![None; graph.sites.len()];
        for node in sources {
            let score = (0, 0, OrderedFloat(f64::NEG_INFINITY));
            heap.push(Reverse((score, node)));
            best[node] = Some(score);
        }
        let target = loop {
            let Reverse((score, node)) = heap.pop()?;
            if cancelled() {
                return None;
            }
            if best[node] != Some(score) {
                continue;
            }
            if remaining.contains(&node) {
                break node;
            }
            if node != root && graph.sites[node].is_client() {
                continue;
            }
            for &(to, link) in &graph.neighbors[node] {
                if banned == Some(to) {
                    continue;
                }
                let new_router = graph.sites[to].needs_new_router() && !installed.contains(&to);
                let trial = (
                    score.0 + usize::from(new_router),
                    score.1 + 1,
                    score.2.max(OrderedFloat(-link.worst_margin_db())),
                );
                if best[to].is_none_or(|current| trial < current) {
                    best[to] = Some(trial);
                    predecessor[to] = Some(node);
                    heap.push(Reverse((trial, to)));
                }
            }
        };
        remaining.remove(&target);
        installed.insert(target);
        let mut node = target;
        while let Some(from) = predecessor[node] {
            installed.insert(from);
            node = from;
        }
    }
    Some(installed.difference(terminals).copied().collect())
}

pub(crate) fn mesh_installation(
    graph: &RoutingGraph<'_>,
    max_hops: usize,
    cancelled: &dyn Fn() -> bool,
) -> Option<NetworkSolution> {
    let clients: Vec<_> = (0..graph.sites.len())
        .filter(|&node| graph.sites[node].is_client())
        .collect();
    let required: Vec<_> = (0..graph.sites.len())
        .filter(|&node| graph.sites[node].is_required())
        .collect();
    if clients.len() < 2 {
        return None;
    }
    let mut margins: Vec<_> = graph
        .edges
        .values()
        .map(|link| link.worst_margin_db())
        .collect();
    margins.sort_unstable_by(f64::total_cmp);
    margins.dedup();
    let paths = |threshold: f64, collect: bool| {
        let mut paths = BTreeMap::new();
        let mut used = BTreeSet::new();
        let mut unattached: BTreeSet<_> = required.iter().copied().collect();
        for (index, &source) in clients.iter().enumerate() {
            if cancelled() {
                return None;
            }
            let prev = mesh_tree(graph, source, max_hops, |_, link| {
                link.worst_margin_db() >= threshold
            });
            let mut path_to = |mut node: usize| {
                let mut path = vec![node];
                while let Some(from) = prev[node] {
                    used.insert(pair(from, node));
                    path.push(from);
                    node = from;
                }
                path.reverse();
                path
            };
            for &target in &clients[index + 1..] {
                prev[target]?;
                if collect {
                    paths.insert(
                        (
                            graph.sites[source].rf.id.clone(),
                            graph.sites[target].rf.id.clone(),
                        ),
                        vec![graph.path_ids(&path_to(target))],
                    );
                }
            }
            unattached.retain(|&node| {
                if prev[node].is_none() {
                    return true;
                }
                if collect {
                    path_to(node);
                }
                false
            });
        }
        unattached.is_empty().then_some((paths, used))
    };
    // Feasibility is monotone in the margin threshold, even with a hop limit.
    let count = margins.partition_point(|&margin| paths(margin, false).is_some());
    let (client_paths, used) = paths(*margins.get(count.checked_sub(1)?)?, true)?;
    let nodes: BTreeSet<_> = used.iter().flat_map(|&(a, b)| [a, b]).collect();
    let sites: Vec<_> = nodes
        .iter()
        .map(|&node| graph.sites[node].clone())
        .collect();
    let router_ids: Vec<_> = sites
        .iter()
        .filter(|site| !site.is_client())
        .map(|site| site.rf.id.clone())
        .collect();
    let new_count = sites.iter().filter(|site| site.needs_new_router()).count();
    Some(NetworkSolution {
        name: format!(
            "{new_count} new router{}",
            if new_count == 1 { "" } else { "s" }
        ),
        sites,
        links: used
            .into_iter()
            .map(|key| graph.edges[&key].clone())
            .collect(),
        client_ids: clients
            .iter()
            .map(|&node| graph.sites[node].rf.id.clone())
            .collect(),
        router_ids,
        client_paths,
        requested_path_count: 1,
        achieved_path_count: 1,
        diagnostics: vec![
            "Mesh search compares sampled installations. A global minimum is not guaranteed."
                .into(),
        ],
    })
}
