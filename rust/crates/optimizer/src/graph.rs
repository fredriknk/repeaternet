use crate::{
    compare_scores, link_is_clear, pair, OptimizationError, OptimizationPriority, PlanningSite,
    SiteOrigin,
};
use repeaternet_rf_core::LinkResult;
use std::cmp::{Ordering, Reverse};
use std::collections::{BTreeMap, BinaryHeap};

/// Undirected graph of enabled sites and valid bidirectional RF links.
#[derive(Debug)]
pub struct RoutingGraph<'a> {
    pub(crate) sites: Vec<&'a PlanningSite>,
    pub(crate) neighbors: Vec<Vec<(usize, &'a LinkResult)>>,
    pub(crate) edges: BTreeMap<(usize, usize), &'a LinkResult>,
}

impl<'a> RoutingGraph<'a> {
    pub fn new(
        sites: &'a [PlanningSite],
        links: &'a [LinkResult],
    ) -> Result<Self, OptimizationError> {
        Self::from_links(sites, links.iter())
    }

    pub(crate) fn from_links(
        sites: &'a [PlanningSite],
        links: impl IntoIterator<Item = &'a LinkResult>,
    ) -> Result<Self, OptimizationError> {
        if let Some(site) = sites
            .iter()
            .find(|site| site.is_required() && !site.enabled)
        {
            return Err(OptimizationError::InvalidInput(format!(
                "Required site {} cannot be disabled",
                site.rf.id
            )));
        }
        let mut enabled: Vec<_> = sites.iter().filter(|site| site.included()).collect();
        enabled.sort_unstable_by_key(|site| &site.rf.id);
        if enabled
            .windows(2)
            .any(|pair| pair[0].rf.id == pair[1].rf.id)
        {
            return Err(OptimizationError::InvalidInput(
                "Site IDs must be unique".into(),
            ));
        }
        let ids: BTreeMap<_, _> = enabled
            .iter()
            .enumerate()
            .map(|(i, site)| (site.rf.id.as_str(), i))
            .collect();
        let mut edges: BTreeMap<_, &LinkResult> = BTreeMap::new();
        for link in links.into_iter().filter(|link| link.valid) {
            let (Some(&left), Some(&right)) = (
                ids.get(link.source_id.as_str()),
                ids.get(link.target_id.as_str()),
            ) else {
                continue;
            };
            if left != right {
                let edge = edges.entry(pair(left, right)).or_insert(link);
                if link.worst_margin_db() > edge.worst_margin_db() {
                    *edge = link;
                }
            }
        }
        Ok(Self::with_edges(enabled, edges))
    }

    fn with_edges(
        sites: Vec<&'a PlanningSite>,
        edges: BTreeMap<(usize, usize), &'a LinkResult>,
    ) -> Self {
        let mut neighbors = vec![Vec::new(); sites.len()];
        for (&(left, right), &link) in &edges {
            neighbors[left].push((right, link));
            neighbors[right].push((left, link));
        }
        Self {
            sites,
            edges,
            neighbors,
        }
    }

    pub(crate) fn induced(&self, nodes: &[usize]) -> Self {
        let mut edges = BTreeMap::new();
        for (right, &original_right) in nodes.iter().enumerate() {
            for (left, &original_left) in nodes[..right].iter().enumerate() {
                if let Some(&link) = self.edges.get(&pair(original_left, original_right)) {
                    edges.insert((left, right), link);
                }
            }
        }
        Self::with_edges(nodes.iter().map(|&node| self.sites[node]).collect(), edges)
    }

    pub fn select_path(
        &self,
        source: &str,
        target: &str,
        priority: OptimizationPriority,
    ) -> Option<Vec<String>> {
        let source = self.sites.iter().position(|site| site.rf.id == source)?;
        let target = self.sites.iter().position(|site| site.rf.id == target)?;
        self.path(source, target, priority)
            .map(|path| self.path_ids(&path))
    }

    pub(crate) fn path_ids(&self, path: &[usize]) -> Vec<String> {
        path.iter().map(|&i| self.sites[i].rf.id.clone()).collect()
    }

    pub(crate) fn path(
        &self,
        source: usize,
        target: usize,
        priority: OptimizationPriority,
    ) -> Option<Vec<usize>> {
        if source == target {
            return Some(vec![source]);
        }
        match priority {
            OptimizationPriority::MinimumRouters => self.hop_path(source, target, |_, _, _| true),
            OptimizationPriority::MaximumReliability => {
                self.reliability_path(source, target, |_, _, _| true)
            }
            OptimizationPriority::FewestNewInstallations => {
                let forward = self.installation_distances(source, false);
                let optimum = forward[target]?;
                let backward = self.installation_distances(target, true);
                self.hop_path(source, target, |left, right, _| {
                    forward[left].zip(backward[right]).is_some_and(|(a, b)| {
                        let cost = self.installation_cost(right);
                        (a.0 + cost.0 + b.0, a.1 + cost.1 + b.1) == optimum
                    })
                })
            }
            OptimizationPriority::MinimumInfrastructure => self.infrastructure_path(source, target),
        }
    }

    pub(crate) fn reliability_path(
        &self,
        source: usize,
        target: usize,
        eligible: impl Fn(usize, usize, &LinkResult) -> bool,
    ) -> Option<Vec<usize>> {
        let clear_margin = self.widest_margins_where(source, |a, b, link| {
            eligible(a, b, link) && link_is_clear(link)
        })[target];
        let require_clear = clear_margin > f64::NEG_INFINITY;
        let margin = if require_clear {
            clear_margin
        } else {
            self.widest_margins_where(source, &eligible)[target]
        };
        self.hop_path(source, target, |a, b, link| {
            eligible(a, b, link)
                && (!require_clear || link_is_clear(link))
                && link.worst_margin_db() >= margin
        })
    }

    fn hop_path(
        &self,
        source: usize,
        target: usize,
        eligible: impl Fn(usize, usize, &LinkResult) -> bool,
    ) -> Option<Vec<usize>> {
        let (forward, order) = self.bfs(source, |a, b, link| eligible(a, b, link));
        let hops = forward[target];
        if hops == usize::MAX {
            return None;
        }
        let (backward, _) = self.bfs(target, |a, b, link| eligible(b, a, link));
        let on_shortest = |left: usize, right: usize, link: &LinkResult| {
            forward[left] < hops
                && forward[right] == forward[left] + 1
                && backward[right] == hops - forward[right]
                && eligible(left, right, link)
        };
        // Optimize bottlenecks in separate passes. A weaker downstream edge
        // can erase a prefix's margin advantage without erasing its Fresnel loss.
        let widest = |metric: fn(&LinkResult) -> f64,
                      allowed: &dyn Fn(usize, usize, &LinkResult) -> bool| {
            let mut best = vec![f64::NEG_INFINITY; self.sites.len()];
            best[source] = f64::INFINITY;
            for &left in &order {
                for &(right, link) in &self.neighbors[left] {
                    if on_shortest(left, right, link) && allowed(left, right, link) {
                        best[right] = best[right].max(best[left].min(metric(link)));
                    }
                }
            }
            best[target]
        };
        let clear_margin = widest(LinkResult::worst_margin_db, &|_, _, link| {
            link_is_clear(link)
        });
        let require_clear = clear_margin > f64::NEG_INFINITY;
        let margin = if require_clear {
            clear_margin
        } else {
            widest(LinkResult::worst_margin_db, &|_, _, _| true)
        };
        let meets_margin = |link: &LinkResult| {
            (!require_clear || link_is_clear(link)) && link.worst_margin_db() >= margin
        };
        let fresnel = widest(
            |link| link.minimum_fresnel_clearance_ratio,
            &|_, _, link| meets_margin(link),
        );
        let mut best = vec![None; self.sites.len()];
        best[source] = Some(([0.0; 3], vec![source]));
        for left in order {
            let Some((score, path)) = best[left].clone() else {
                continue;
            };
            for &(right, link) in &self.neighbors[left] {
                if !on_shortest(left, right, link)
                    || !meets_margin(link)
                    || link.minimum_fresnel_clearance_ratio < fresnel
                {
                    continue;
                }
                let site = self.sites[right];
                let trial = [
                    score[0]
                        - if right == target {
                            0.0
                        } else {
                            site.rf.antenna_height_m
                        },
                    score[1] + site.site_quality,
                    score[2] - link.distance_m,
                ];
                let mut path = path.clone();
                path.push(right);
                if best[right].as_ref().is_none_or(|(current, current_path)| {
                    better_path(self, &trial, &path, current, current_path)
                }) {
                    best[right] = Some((trial, path));
                }
            }
        }
        best[target].take().map(|(_, path)| path)
    }

    pub(crate) fn bfs(
        &self,
        source: usize,
        allowed: impl Fn(usize, usize, &LinkResult) -> bool,
    ) -> (Vec<usize>, Vec<usize>) {
        let mut distance = vec![usize::MAX; self.sites.len()];
        let mut order = vec![source];
        distance[source] = 0;
        let mut cursor = 0;
        while let Some(&left) = order.get(cursor) {
            for &(right, link) in &self.neighbors[left] {
                if distance[right] == usize::MAX && allowed(left, right, link) {
                    distance[right] = distance[left] + 1;
                    order.push(right);
                }
            }
            cursor += 1;
        }
        (distance, order)
    }

    pub(crate) fn widest_margins(&self, source: usize, clear_only: bool) -> Vec<f64> {
        self.widest_margins_where(source, |_, _, link| !clear_only || link_is_clear(link))
    }

    fn widest_margins_where(
        &self,
        source: usize,
        eligible: impl Fn(usize, usize, &LinkResult) -> bool,
    ) -> Vec<f64> {
        let mut best = vec![f64::NEG_INFINITY; self.sites.len()];
        best[source] = f64::INFINITY;
        let mut queue = BinaryHeap::from([(OrderedFloat(best[source]), source)]);
        while let Some((OrderedFloat(margin), left)) = queue.pop() {
            if margin < best[left] {
                continue;
            }
            for &(right, link) in &self.neighbors[left] {
                if !eligible(left, right, link) {
                    continue;
                }
                let trial = margin.min(link.worst_margin_db());
                if trial > best[right] {
                    best[right] = trial;
                    queue.push((OrderedFloat(trial), right));
                }
            }
        }
        best
    }

    fn installation_cost(&self, node: usize) -> (usize, usize) {
        let site = self.sites[node];
        if site.is_client() {
            (0, 0)
        } else {
            (usize::from(site.origin != SiteOrigin::Known), 1)
        }
    }

    fn installation_distances(&self, start: usize, reverse: bool) -> Vec<Option<(usize, usize)>> {
        let mut best = vec![None; self.sites.len()];
        best[start] = Some((0, 0));
        let mut queue = BinaryHeap::from([Reverse(((0, 0), start))]);
        while let Some(Reverse((cost, left))) = queue.pop() {
            if Some(cost) != best[left] {
                continue;
            }
            for &(right, _) in &self.neighbors[left] {
                let step = self.installation_cost(if reverse { left } else { right });
                let trial = (cost.0 + step.0, cost.1 + step.1);
                if best[right].is_none_or(|score| trial < score) {
                    best[right] = Some(trial);
                    queue.push(Reverse((trial, right)));
                }
            }
        }
        best
    }

    fn infrastructure_path(&self, source: usize, target: usize) -> Option<Vec<usize>> {
        let mut best = vec![f64::INFINITY; self.sites.len()];
        let mut predecessor = vec![None; self.sites.len()];
        best[source] = 0.0;
        let mut queue = BinaryHeap::from([Reverse((OrderedFloat(0.0), source))]);
        while let Some(Reverse((OrderedFloat(cost), left))) = queue.pop() {
            if cost > best[left] {
                continue;
            }
            if left == target {
                let mut path = vec![target];
                while let Some(previous) = predecessor[*path.last()?] {
                    path.push(previous);
                }
                path.reverse();
                return Some(path);
            }
            for &(right, link) in &self.neighbors[left] {
                let mast = if right == target {
                    0.0
                } else {
                    100.0 + self.sites[right].rf.antenna_height_m
                };
                let trial = cost + mast + link.distance_m / 1_000_000.0;
                if trial < best[right] {
                    best[right] = trial;
                    predecessor[right] = Some(left);
                    queue.push(Reverse((OrderedFloat(trial), right)));
                }
            }
        }
        None
    }

    pub(crate) fn bottleneck(&self, path: &[usize]) -> f64 {
        path.windows(2)
            .map(|edge| self.edges[&pair(edge[0], edge[1])].worst_margin_db())
            .fold(f64::INFINITY, f64::min)
    }

    pub(crate) fn path_is_clear(&self, path: &[usize]) -> bool {
        path.windows(2)
            .all(|edge| link_is_clear(self.edges[&pair(edge[0], edge[1])]))
    }
}

fn better_path(
    graph: &RoutingGraph<'_>,
    score: &[f64],
    path: &[usize],
    current: &[f64],
    current_path: &[usize],
) -> bool {
    compare_scores(score, current)
        .then_with(|| {
            current_path
                .iter()
                .map(|&i| &graph.sites[i].rf.id)
                .cmp(path.iter().map(|&i| &graph.sites[i].rf.id))
        })
        .is_gt()
}

#[derive(Clone, Copy)]
pub(crate) struct OrderedFloat(pub(crate) f64);

impl PartialEq for OrderedFloat {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}

impl Eq for OrderedFloat {}

impl PartialOrd for OrderedFloat {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for OrderedFloat {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.total_cmp(&other.0)
    }
}
