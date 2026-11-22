//! Exact two-terminal route planning on a screened RF graph.
//!
//! For every router count the planner finds the path with the widest bottleneck
//! (lowest link margin) using a hop-layered dynamic programme. That is exact for
//! the bottleneck. Among the paths that reach the bottleneck it then minimizes an
//! additive weakness cost, so the other hops are as strong as possible too.
//! Required routers are via points: the route visits all of them, in the order
//! that gives the best route.

use repeaternet_rf_core::{LinkResult, Point};

/// Waypoint orders are tried exhaustively up to this many required routers.
const MAX_PERMUTED_WAYPOINTS: usize = 6;

#[derive(Clone, Debug)]
pub(crate) struct PlannedPath {
    /// Site indices from the first terminal to the second.
    pub nodes: Vec<usize>,
    pub clear: bool,
    pub bottleneck_db: f64,
    pub cost: f64,
}

impl PlannedPath {
    pub(crate) fn router_count(&self) -> usize {
        self.nodes.len() - 2
    }

    /// True when `self` is a strictly better route than `other`.
    fn better_than(&self, other: &Self) -> bool {
        (self.clear, self.bottleneck_db, -self.cost)
            > (other.clear, other.bottleneck_db, -other.cost)
    }
}

/// Additive weakness of one hop once the bottleneck is fixed. Strong links cost
/// little; links near zero margin or without clearance cost much more.
fn hop_cost(hop: &Hop) -> f64 {
    let margin = hop.margin_db.max(0.0);
    let mut cost = 0.1 + 10.0 * (-margin / 6.0).exp();
    if !hop.los_clear {
        cost += 4.0;
    } else if !hop.fresnel_clear {
        cost += 1.5;
    }
    cost
}

/// One direction of a valid link in the planner's adjacency.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Hop {
    pub to: usize,
    pub margin_db: f64,
    pub los_clear: bool,
    pub fresnel_clear: bool,
}

/// Scalar summary of a valid link, small enough to keep for every screened pair.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Screened {
    pub margin_db: f64,
    pub los_clear: bool,
    pub fresnel_clear: bool,
}

impl From<&LinkResult> for Screened {
    fn from(link: &LinkResult) -> Self {
        Self {
            margin_db: link.worst_margin_db(),
            los_clear: link.los_clear,
            fresnel_clear: link.fresnel_clear,
        }
    }
}

impl Hop {
    fn clear(&self) -> bool {
        self.los_clear && self.fresnel_clear
    }
}

/// Compact adjacency over site indices that can be updated in place as links
/// are certified, so repeated planning does not rebuild a full routing graph.
pub(crate) struct LinkGraph {
    pub neighbors: Vec<Vec<Hop>>,
    pub positions: Vec<Point>,
}

impl LinkGraph {
    /// Build from valid links. Each pair must appear at most once.
    pub(crate) fn new(
        positions: Vec<Point>,
        links: impl IntoIterator<Item = ((usize, usize), Screened)>,
    ) -> Self {
        let mut graph = Self {
            neighbors: vec![Vec::new(); positions.len()],
            positions,
        };
        for ((left, right), link) in links {
            graph.neighbors[left].push(Self::hop(right, link));
            graph.neighbors[right].push(Self::hop(left, link));
        }
        graph
    }

    /// Insert, update or (with `None` or an invalid link) remove a link.
    pub(crate) fn set(&mut self, left: usize, right: usize, link: Option<&LinkResult>) {
        let link = link.filter(|link| link.valid);
        for (from, to) in [(left, right), (right, left)] {
            let hops = &mut self.neighbors[from];
            let existing = hops.iter().position(|hop| hop.to == to);
            match (existing, link) {
                (Some(index), Some(link)) => hops[index] = Self::hop(to, link.into()),
                (None, Some(link)) => hops.push(Self::hop(to, link.into())),
                (Some(index), None) => {
                    hops.swap_remove(index);
                }
                (None, None) => {}
            }
        }
    }

    fn hop(to: usize, link: Screened) -> Hop {
        Hop {
            to,
            margin_db: link.margin_db,
            los_clear: link.los_clear,
            fresnel_clear: link.fresnel_clear,
        }
    }

    fn margin(&self, left: usize, right: usize) -> f64 {
        self.neighbors[left]
            .iter()
            .find(|hop| hop.to == right)
            .map_or(f64::NEG_INFINITY, |hop| hop.margin_db)
    }
}

/// Produces the path and cost of segment `s` while avoiding blocked sites.
type SegmentFn<'f> = dyn FnMut(usize, &[bool]) -> Option<(Vec<usize>, f64)> + 'f;

struct Planner<'g> {
    graph: &'g LinkGraph,
    /// Nodes that may appear inside a segment (not terminals or via points).
    through: Vec<bool>,
}

impl Planner<'_> {
    fn edge_ok(hop: &Hop, clear_only: bool) -> bool {
        !clear_only || hop.clear()
    }

    /// `widest[h][v]`: best bottleneck from `source` to `v` with at most `h` hops,
    /// passing only through unblocked nodes.
    fn widest_by_hops(
        &self,
        source: usize,
        hops: usize,
        clear_only: bool,
        blocked: &[bool],
    ) -> Vec<Vec<f64>> {
        let count = self.graph.positions.len();
        let mut layers = vec![vec![f64::NEG_INFINITY; count]];
        layers[0][source] = f64::INFINITY;
        for _ in 0..hops {
            let previous = layers.last().expect("base layer");
            let mut next = previous.clone();
            for (left, &margin) in previous.iter().enumerate() {
                if margin == f64::NEG_INFINITY
                    || (left != source && (!self.through[left] || blocked[left]))
                {
                    continue;
                }
                for hop in &self.graph.neighbors[left] {
                    if Self::edge_ok(hop, clear_only) && !blocked[hop.to] {
                        let trial = margin.min(hop.margin_db);
                        if trial > next[hop.to] {
                            next[hop.to] = trial;
                        }
                    }
                }
            }
            layers.push(next);
        }
        layers
    }

    /// Cheapest simple path with at most `hops` hops using links at or above
    /// `threshold`, avoiding `blocked` nodes.
    fn cheapest(
        &self,
        source: usize,
        target: usize,
        hops: usize,
        threshold: f64,
        clear_only: bool,
        blocked: &[bool],
    ) -> Option<(Vec<usize>, f64)> {
        let count = self.graph.positions.len();
        let mut cost = vec![f64::INFINITY; count];
        cost[source] = 0.0;
        // moves[k][v]: predecessor when layer k improved v by a hop.
        let mut moves = Vec::with_capacity(hops);
        for _ in 0..hops {
            let mut next = cost.clone();
            let mut layer = vec![usize::MAX; count];
            for (left, &base) in cost.iter().enumerate() {
                if base == f64::INFINITY
                    || left == target
                    || (left != source && (!self.through[left] || blocked[left]))
                {
                    continue;
                }
                for hop in &self.graph.neighbors[left] {
                    let right = hop.to;
                    if right == source
                        || (right != target && (!self.through[right] || blocked[right]))
                        || hop.margin_db < threshold
                        || !Self::edge_ok(hop, clear_only)
                    {
                        continue;
                    }
                    let trial = base + hop_cost(hop);
                    if trial < next[right] {
                        next[right] = trial;
                        layer[right] = left;
                    }
                }
            }
            moves.push(layer);
            cost = next;
        }
        if cost[target] == f64::INFINITY {
            return None;
        }
        let mut path = vec![target];
        let mut node = target;
        for layer in moves.iter().rev() {
            if layer[node] != usize::MAX {
                node = layer[node];
                path.push(node);
            }
        }
        path.reverse();
        if path[0] != source {
            return None;
        }
        let mut seen = vec![false; count];
        for &node in &path {
            if std::mem::replace(&mut seen[node], true) {
                return None;
            }
        }
        Some((path, cost[target]))
    }

    /// Join segment paths through `sequence` without reusing a site. The planned
    /// allocation is tried first. If segments compete for the same relay, later
    /// segments are replanned around the sites already used, so the route can
    /// accept a weaker bottleneck instead of failing.
    fn realize(
        &self,
        sequence: &[usize],
        allocation: &[usize],
        threshold: f64,
        clear_only: bool,
    ) -> Option<(Vec<usize>, f64)> {
        let count = self.graph.positions.len();
        let join = |segments: &mut SegmentFn<'_>| {
            let mut blocked = vec![false; count];
            let mut nodes = vec![sequence[0]];
            let mut cost = 0.0;
            for s in 0..sequence.len() - 1 {
                let (segment, segment_cost) = segments(s, &blocked)?;
                for &node in &segment {
                    blocked[node] = true;
                }
                nodes.extend_from_slice(&segment[1..]);
                cost += segment_cost;
            }
            Some((nodes, cost))
        };
        let planned = join(&mut |s, blocked| {
            self.cheapest(
                sequence[s],
                sequence[s + 1],
                allocation[s],
                threshold,
                clear_only,
                blocked,
            )
        });
        if planned.is_some() {
            return planned;
        }
        let total: usize = allocation.iter().sum();
        let mut used = 0;
        join(&mut |s, blocked| {
            let later = sequence.len() - 2 - s;
            let budget = total
                .checked_sub(used + later)
                .filter(|&budget| budget > 0)?;
            let target = sequence[s + 1];
            let layers = self.widest_by_hops(sequence[s], budget, clear_only, blocked);
            let value = layers[budget][target];
            if value == f64::NEG_INFINITY {
                return None;
            }
            let hops = (1..=budget).find(|&hops| layers[hops][target] >= value)?;
            let segment = self.cheapest(
                sequence[s],
                target,
                hops,
                value.min(threshold),
                clear_only,
                blocked,
            )?;
            used += segment.0.len() - 1;
            Some(segment)
        })
    }
}

fn permutations(items: &[usize]) -> Vec<Vec<usize>> {
    if items.len() <= 1 {
        return vec![items.to_vec()];
    }
    let mut result = Vec::new();
    for (i, &first) in items.iter().enumerate() {
        let mut rest = items.to_vec();
        rest.remove(i);
        for mut tail in permutations(&rest) {
            tail.insert(0, first);
            result.push(tail);
        }
    }
    result
}

/// Best route for each router count from 0 through `max_routers`, keeping only
/// counts that improve on every smaller count. Routes visit every waypoint.
pub(crate) fn plan_routes(
    graph: &LinkGraph,
    source: usize,
    target: usize,
    waypoints: &[usize],
    max_routers: usize,
    cancelled: &dyn Fn() -> bool,
) -> Vec<PlannedPath> {
    let count = graph.positions.len();
    let mut through = vec![true; count];
    for &node in [source, target].iter().chain(waypoints) {
        through[node] = false;
    }
    let planner = Planner { graph, through };
    let max_hops = max_routers + 1;
    let orders: Vec<Vec<usize>> = if waypoints.len() <= MAX_PERMUTED_WAYPOINTS {
        permutations(waypoints)
    } else {
        // Order many via points along the endpoint axis.
        let (a, b) = (graph.positions[source], graph.positions[target]);
        let along = |node: usize| {
            let p = graph.positions[node];
            (p.x - a.x) * (b.x - a.x) + (p.y - a.y) * (b.y - a.y)
        };
        let mut sorted = waypoints.to_vec();
        sorted.sort_by(|&l, &r| along(l).total_cmp(&along(r)));
        vec![sorted]
    };
    let mut best: Vec<Option<PlannedPath>> = vec![None; max_routers + 1];
    for clear_only in [true, false] {
        if cancelled() {
            return Vec::new();
        }
        let sources: Vec<usize> = std::iter::once(source)
            .chain(waypoints.iter().copied())
            .collect();
        let widest: Vec<(usize, Vec<Vec<f64>>)> = sources
            .iter()
            .map(|&node| {
                (
                    node,
                    planner.widest_by_hops(node, max_hops, clear_only, &vec![false; count]),
                )
            })
            .collect();
        let bottleneck = |from: usize, to: usize, hops: usize| {
            widest
                .iter()
                .find(|(node, _)| *node == from)
                .map_or(f64::NEG_INFINITY, |(_, layers)| layers[hops][to])
        };
        for order in &orders {
            if cancelled() {
                return Vec::new();
            }
            let sequence: Vec<usize> = std::iter::once(source)
                .chain(order.iter().copied())
                .chain(std::iter::once(target))
                .collect();
            let segments = sequence.len() - 1;
            if segments > max_hops {
                continue;
            }
            // allocation[s][t]: best bottleneck for the first s segments using t hops.
            let mut table = vec![vec![(f64::NEG_INFINITY, 0usize); max_hops + 1]; segments + 1];
            table[0][0] = (f64::INFINITY, 0);
            for s in 1..=segments {
                for total in s..=max_hops {
                    for hops in 1..=total - (s - 1) {
                        let (prefix, _) = table[s - 1][total - hops];
                        if prefix == f64::NEG_INFINITY {
                            continue;
                        }
                        let value = prefix.min(bottleneck(sequence[s - 1], sequence[s], hops));
                        if value > table[s][total].0 {
                            table[s][total] = (value, hops);
                        }
                    }
                }
            }
            for total in segments..=max_hops {
                let threshold = table[segments][total].0;
                if threshold == f64::NEG_INFINITY {
                    continue;
                }
                let mut allocation = vec![0; segments];
                let mut remaining = total;
                for s in (1..=segments).rev() {
                    allocation[s - 1] = table[s][remaining].1;
                    remaining -= allocation[s - 1];
                }
                let Some((nodes, cost)) =
                    planner.realize(&sequence, &allocation, threshold, clear_only)
                else {
                    continue;
                };
                let planned = PlannedPath {
                    clear: clear_only,
                    bottleneck_db: nodes
                        .windows(2)
                        .map(|hop| graph.margin(hop[0], hop[1]))
                        .fold(f64::INFINITY, f64::min),
                    nodes,
                    cost,
                };
                let routers = planned.router_count();
                if routers <= max_routers
                    && best[routers]
                        .as_ref()
                        .is_none_or(|current| planned.better_than(current))
                {
                    best[routers] = Some(planned);
                }
            }
        }
    }
    // Only offer more routers when they buy a clearer or stronger route.
    let mut frontier: Vec<PlannedPath> = Vec::new();
    for planned in best.into_iter().flatten() {
        if frontier
            .iter()
            .all(|kept| (planned.clear, planned.bottleneck_db) > (kept.clear, kept.bottleneck_db))
        {
            frontier.push(planned);
        }
    }
    frontier
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(margin_db: f64) -> Screened {
        Screened {
            margin_db,
            los_clear: true,
            fresnel_clear: true,
        }
    }

    fn graph(count: usize, links: &[(usize, usize, f64)]) -> LinkGraph {
        LinkGraph::new(
            (0..count)
                .map(|i| Point {
                    x: i as f64 * 1000.0,
                    y: 0.0,
                })
                .collect(),
            links
                .iter()
                .map(|&(a, b, margin)| ((a.min(b), a.max(b)), link(margin))),
        )
    }

    fn plan(graph: &LinkGraph, waypoints: &[usize], max_routers: usize) -> Vec<(Vec<usize>, f64)> {
        let target = graph.positions.len() - 1;
        plan_routes(graph, 0, target, waypoints, max_routers, &|| false)
            .into_iter()
            .map(|planned| (planned.nodes, planned.bottleneck_db))
            .collect()
    }

    #[test]
    fn finds_the_widest_bottleneck_for_each_router_count() {
        // 0 = A, 5 = B. One weak relay, a stronger two-relay chain and an
        // even stronger three-relay chain.
        let graph = graph(
            6,
            &[
                (0, 1, 3.0),
                (1, 5, 30.0),
                (0, 2, 20.0),
                (2, 3, 12.0),
                (3, 5, 20.0),
                (2, 4, 25.0),
                (4, 3, 25.0),
            ],
        );
        assert_eq!(
            plan(&graph, &[], 6),
            [
                (vec![0, 1, 5], 3.0),
                (vec![0, 2, 3, 5], 12.0),
                (vec![0, 2, 4, 3, 5], 20.0),
            ]
        );
    }

    #[test]
    fn omits_router_counts_that_do_not_improve_the_route() {
        let graph = graph(5, &[(0, 1, 10.0), (1, 4, 10.0), (1, 2, 30.0), (2, 4, 10.0)]);
        assert_eq!(plan(&graph, &[], 6), [(vec![0, 1, 4], 10.0)]);
    }

    #[test]
    fn prefers_clear_hops_over_a_stronger_obstructed_bottleneck() {
        let mut graph = graph(4, &[(0, 1, 5.0), (1, 3, 5.0), (0, 2, 25.0), (2, 3, 25.0)]);
        graph.neighbors[2]
            .iter_mut()
            .for_each(|hop| hop.fresnel_clear = false);
        for hops in &mut graph.neighbors {
            for hop in hops.iter_mut().filter(|hop| hop.to == 2) {
                hop.fresnel_clear = false;
            }
        }
        assert_eq!(plan(&graph, &[], 6), [(vec![0, 1, 3], 5.0)]);
    }

    #[test]
    fn spends_spare_hops_on_the_weakest_part_of_the_route() {
        // Bottleneck ties at 10 dB; the additive cost then prefers strong other hops.
        let graph = graph(
            5,
            &[
                (0, 1, 10.0),
                (1, 4, 11.0),
                (0, 2, 10.0),
                (2, 4, 40.0),
                (0, 3, 2.0),
                (3, 4, 50.0),
            ],
        );
        assert_eq!(plan(&graph, &[], 6), [(vec![0, 2, 4], 10.0)]);
    }

    #[test]
    fn visits_every_waypoint_in_the_best_order() {
        // A = 0, B = 5, waypoints 3 and 1. The order 1 then 3 is strong; 3 then 1 is weak.
        let graph = graph(
            6,
            &[
                (0, 1, 20.0),
                (1, 3, 20.0),
                (3, 5, 20.0),
                (0, 3, 4.0),
                (3, 2, 15.0),
                (2, 1, 15.0),
                (1, 5, 4.0),
                (0, 5, 40.0),
            ],
        );
        let routes = plan(&graph, &[3, 1], 6);
        assert_eq!(routes[0], (vec![0, 1, 3, 5], 20.0));
        assert!(routes
            .iter()
            .all(|(path, _)| path.contains(&1) && path.contains(&3)));
    }

    #[test]
    fn a_waypoint_route_never_reuses_a_site() {
        // Node 2 is the only relay on both sides of waypoint 1.
        let graph = graph(4, &[(0, 2, 20.0), (2, 1, 20.0), (1, 3, 1.0), (2, 3, 20.0)]);
        let routes = plan(&graph, &[1], 6);
        assert_eq!(routes, [(vec![0, 2, 1, 3], 1.0)]);
    }

    #[test]
    fn segments_competing_for_one_relay_fall_back_to_a_weaker_detour() {
        // Both A->R and R->B prefer relay 1; the second segment must use 3.
        let graph = graph(
            5,
            &[
                (0, 1, 20.0),
                (1, 4, 20.0),
                (1, 2, 20.0),
                (2, 3, 15.0),
                (3, 4, 15.0),
            ],
        );
        assert_eq!(plan(&graph, &[2], 6), [(vec![0, 1, 2, 3, 4], 15.0)]);
    }

    #[test]
    fn unreachable_waypoints_give_no_route() {
        let graph = graph(4, &[(0, 1, 20.0), (1, 3, 20.0)]);
        assert!(plan(&graph, &[2], 6).is_empty());
    }

    #[test]
    fn removing_a_failed_link_updates_both_directions() {
        let mut graph = graph(3, &[(0, 1, 5.0), (1, 2, 5.0)]);
        assert_eq!(plan(&graph, &[], 6), [(vec![0, 1, 2], 5.0)]);
        graph.set(1, 0, None);
        assert_eq!(graph.margin(0, 1), f64::NEG_INFINITY);
        assert!(graph.neighbors[1].iter().all(|hop| hop.to != 0));
        assert!(plan(&graph, &[], 6).is_empty());
    }
}
