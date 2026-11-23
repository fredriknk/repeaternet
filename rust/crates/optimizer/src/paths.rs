//! Exact two-terminal route planning on a screened RF graph.
//!
//! For each hop and new-router budget the planner finds the widest bottleneck
//! (lowest link margin) using layered dynamic programming. Within a margin
//! band it prefers fewer new routers, then minimizes an
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
    new_routers: usize,
    pub bottleneck_db: f64,
    pub cost: f64,
}

impl PlannedPath {
    pub(crate) fn router_count(&self) -> usize {
        self.nodes.len() - 2
    }

    fn quality(&self) -> (bool, f64, std::cmp::Reverse<usize>, f64, f64) {
        (
            self.clear,
            margin_band(self.bottleneck_db),
            std::cmp::Reverse(self.new_routers),
            self.bottleneck_db,
            -self.cost,
        )
    }
}

/// Use fixed 1 dB bands to compare margins transitively. Inside a band,
/// reusing available sites is worth more than a fractional dB improvement.
pub(crate) fn margin_band(margin_db: f64) -> f64 {
    margin_db.floor()
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
    pub new_router: Vec<bool>,
}

impl LinkGraph {
    /// Build from valid links. Each pair must appear at most once.
    pub(crate) fn new(
        positions: Vec<Point>,
        new_router: Vec<bool>,
        links: impl IntoIterator<Item = ((usize, usize), Screened)>,
    ) -> Self {
        let mut graph = Self {
            neighbors: vec![Vec::new(); positions.len()],
            positions,
            new_router,
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

    /// Widest margin with at most h hops and n new routers, for each (h, n, node).
    fn widest(
        &self,
        source: usize,
        budget: Budget,
        clear_only: bool,
        blocked: &[bool],
    ) -> Vec<Vec<f64>> {
        self.layers(
            source,
            budget,
            blocked,
            |_, hop, margin| Self::edge_ok(hop, clear_only).then(|| margin.min(hop.margin_db)),
            f64::INFINITY,
        )
        .0
    }

    /// Each layer retains the route from the preceding hop or router budget,
    /// and extends routes from the preceding hop layer.
    fn layers(
        &self,
        source: usize,
        budget: Budget,
        blocked: &[bool],
        extend: impl Fn(usize, &Hop, f64) -> Option<f64>,
        initial: f64,
    ) -> (Vec<Vec<f64>>, Vec<Vec<Move>>) {
        let count = self.graph.positions.len();
        let stride = budget.new + 1;
        let mut scores = Vec::<Vec<f64>>::new();
        let mut moves = Vec::<Vec<Move>>::new();
        for hops in 0..=budget.hops {
            for new in 0..=budget.new {
                let index = hops * stride + new;
                let mut next = vec![f64::NEG_INFINITY; count];
                let mut previous = vec![Move::Start; count];
                if hops == 0 {
                    next[source] = initial;
                } else {
                    next.clone_from(&scores[index - stride]);
                    previous.fill(Move::Budget(index - stride));
                    for cost in 0..=usize::from(new > 0) {
                        let from = index - stride - cost;
                        for (left, &score) in scores[from].iter().enumerate() {
                            if score == f64::NEG_INFINITY || (left != source && !self.through[left])
                            {
                                continue;
                            }
                            for hop in &self.graph.neighbors[left] {
                                let right = hop.to;
                                if right == source
                                    || blocked[right]
                                    || usize::from(self.graph.new_router[right]) != cost
                                {
                                    continue;
                                }
                                if let Some(trial) = extend(left, hop, score) {
                                    if trial > next[right] {
                                        next[right] = trial;
                                        previous[right] = Move::Hop(from, left);
                                    }
                                }
                            }
                        }
                    }
                }
                if new > 0 {
                    for (node, &score) in scores[index - 1].iter().enumerate() {
                        if score > next[node] {
                            next[node] = score;
                            previous[node] = Move::Budget(index - 1);
                        }
                    }
                }
                scores.push(next);
                moves.push(previous);
            }
        }
        (scores, moves)
    }

    fn cheapest(
        &self,
        source: usize,
        target: usize,
        budget: Budget,
        threshold: f64,
        clear_only: bool,
        blocked: &[bool],
    ) -> Option<(Vec<usize>, f64)> {
        let (scores, moves) = self.layers(
            source,
            budget,
            blocked,
            |left, hop, score| {
                (left != target && hop.margin_db >= threshold && Self::edge_ok(hop, clear_only))
                    .then(|| score - hop_cost(hop))
            },
            0.0,
        );
        let mut layer = scores.len() - 1;
        let score = scores[layer][target];
        if !score.is_finite() {
            return None;
        }
        let mut nodes = vec![target];
        let mut node = target;
        loop {
            match moves[layer][node] {
                Move::Start => break,
                Move::Budget(from) => layer = from,
                Move::Hop(from, previous) => {
                    node = previous;
                    layer = from;
                    nodes.push(node);
                }
            }
        }
        nodes.reverse();
        Some((nodes, -score))
    }

    /// If segments compete for a relay, replan later segments around the
    /// sites already used, within the total budget.
    fn realize(
        &self,
        sequence: &[usize],
        allocation: &[Budget],
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
        if planned.is_some() || allocation.len() == 1 {
            return planned;
        }
        let total_hops: usize = allocation.iter().map(|b| b.hops).sum();
        let total_new: usize = allocation.iter().map(|b| b.new).sum();
        let mut used_hops = 0;
        let mut used_new = 0;
        join(&mut |s, blocked| {
            let later = sequence.len() - 2 - s;
            let budget = Budget {
                hops: total_hops.checked_sub(used_hops + later)?,
                new: total_new - used_new,
            };
            let target = sequence[s + 1];
            let widest = self.widest(sequence[s], budget, clear_only, blocked);
            let value = widest.last()?[target];
            if value == f64::NEG_INFINITY {
                return None;
            }
            let stride = budget.new + 1;
            let hops = (1..=budget.hops)
                .find(|&hops| widest[hops * stride + budget.new][target] >= value)?;
            let new = (0..=budget.new).find(|&new| widest[hops * stride + new][target] >= value)?;
            let segment = self.cheapest(
                sequence[s],
                target,
                Budget { hops, new },
                value.min(threshold),
                clear_only,
                blocked,
            )?;
            used_hops += segment.0.len() - 1;
            used_new += segment
                .0
                .iter()
                .filter(|&&node| self.graph.new_router[node])
                .count();
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

#[derive(Clone, Copy, Default)]
struct Budget {
    hops: usize,
    new: usize,
}

#[derive(Clone, Copy)]
enum Move {
    Start,
    Budget(usize),
    Hop(usize, usize),
}

/// Best route per hop count, keeping improvements in quality or new-site use.
/// Routes visit every waypoint.
pub(crate) fn plan_routes(
    graph: &LinkGraph,
    source: usize,
    target: usize,
    waypoints: &[usize],
    max_hops: usize,
    max_new: usize,
    cancelled: &dyn Fn() -> bool,
) -> Vec<PlannedPath> {
    let count = graph.positions.len();
    let limit = Budget {
        hops: max_hops.min(count.saturating_sub(1)),
        new: max_new
            .min(max_hops.saturating_sub(1))
            .min(graph.new_router.iter().filter(|&&new| new).count()),
    };
    if waypoints.len() >= limit.hops {
        return Vec::new();
    }
    let stride = limit.new + 1;
    let index = |hops, new| hops * stride + new;
    let size = index(limit.hops, limit.new) + 1;
    let mut through = vec![true; count];
    for &node in [source, target].iter().chain(waypoints) {
        through[node] = false;
    }
    let planner = Planner { graph, through };
    let orders: Vec<Vec<usize>> = if waypoints.len() <= MAX_PERMUTED_WAYPOINTS {
        permutations(waypoints)
    } else {
        let (a, b) = (graph.positions[source], graph.positions[target]);
        let along = |node: usize| {
            let p = graph.positions[node];
            (p.x - a.x) * (b.x - a.x) + (p.y - a.y) * (b.y - a.y)
        };
        let mut sorted = waypoints.to_vec();
        sorted.sort_by(|&l, &r| along(l).total_cmp(&along(r)));
        vec![sorted]
    };
    let mut best: Vec<Option<PlannedPath>> = vec![None; limit.hops];
    for clear_only in [true, false] {
        if cancelled() {
            return Vec::new();
        }
        let widest: std::collections::BTreeMap<_, _> = std::iter::once(source)
            .chain(waypoints.iter().copied())
            .map(|node| {
                (
                    node,
                    planner.widest(node, limit, clear_only, &vec![false; count]),
                )
            })
            .collect();
        for order in &orders {
            if cancelled() {
                return Vec::new();
            }
            let sequence: Vec<_> = std::iter::once(source)
                .chain(order.iter().copied())
                .chain(std::iter::once(target))
                .collect();
            let segments = sequence.len() - 1;
            let mut table = vec![vec![(f64::NEG_INFINITY, Budget::default()); size]; segments + 1];
            table[0].fill((f64::INFINITY, Budget::default()));
            for s in 1..=segments {
                let layers = &widest[&sequence[s - 1]];
                for hops in s..=limit.hops {
                    for new in 0..=limit.new.min(hops - s) {
                        let mut best = (f64::NEG_INFINITY, Budget::default());
                        for leg_hops in 1..=hops - (s - 1) {
                            for leg_new in 0..=new.min(leg_hops - 1) {
                                let value = table[s - 1][index(hops - leg_hops, new - leg_new)]
                                    .0
                                    .min(layers[index(leg_hops, leg_new)][sequence[s]]);
                                if value > best.0 {
                                    best = (
                                        value,
                                        Budget {
                                            hops: leg_hops,
                                            new: leg_new,
                                        },
                                    );
                                }
                            }
                        }
                        table[s][index(hops, new)] = best;
                    }
                }
            }
            for total in segments..=limit.hops {
                let mut budgets = 0..=limit.new.min(total - segments);
                if segments == 1 {
                    let best_new = budgets
                        .clone()
                        .max_by_key(|&new| {
                            (
                                crate::graph::OrderedFloat(margin_band(
                                    table[1][index(total, new)].0,
                                )),
                                std::cmp::Reverse(new),
                            )
                        })
                        .expect("at least the zero-router budget");
                    budgets = best_new..=best_new;
                }
                for new in budgets {
                    let threshold = table[segments][index(total, new)].0;
                    if threshold == f64::NEG_INFINITY {
                        continue;
                    }
                    let mut allocation = vec![Budget::default(); segments];
                    let (mut hops_left, mut new_left) = (total, new);
                    for s in (1..=segments).rev() {
                        let leg = table[s][index(hops_left, new_left)].1;
                        allocation[s - 1] = leg;
                        hops_left -= leg.hops;
                        new_left -= leg.new;
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
                        new_routers: nodes.iter().filter(|&&node| graph.new_router[node]).count(),
                        nodes,
                        cost,
                    };
                    let slot = &mut best[planned.router_count()];
                    if slot
                        .as_ref()
                        .is_none_or(|current| planned.quality() > current.quality())
                    {
                        *slot = Some(planned);
                    }
                }
            }
        }
    }
    let mut frontier: Vec<PlannedPath> = Vec::new();
    for planned in best.into_iter().flatten() {
        if frontier.iter().all(|kept| {
            (
                planned.clear,
                margin_band(planned.bottleneck_db),
                std::cmp::Reverse(planned.new_routers),
            ) > (
                kept.clear,
                margin_band(kept.bottleneck_db),
                std::cmp::Reverse(kept.new_routers),
            )
        }) {
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
            (0..count)
                .map(|node| node != 0 && node != count - 1)
                .collect(),
            links
                .iter()
                .map(|&(a, b, margin)| ((a.min(b), a.max(b)), link(margin))),
        )
    }

    fn plan(graph: &LinkGraph, waypoints: &[usize], max_routers: usize) -> Vec<(Vec<usize>, f64)> {
        let target = graph.positions.len() - 1;
        let graph = LinkGraph {
            neighbors: graph.neighbors.clone(),
            positions: graph.positions.clone(),
            new_router: graph
                .new_router
                .iter()
                .enumerate()
                .map(|(i, &new)| new && !waypoints.contains(&i))
                .collect(),
        };
        plan_routes(
            &graph,
            0,
            target,
            waypoints,
            max_routers + 1,
            max_routers,
            &|| false,
        )
        .into_iter()
        .map(|planned| (planned.nodes, planned.bottleneck_db))
        .collect()
    }

    #[test]
    fn reuses_an_optional_router_with_comparable_margin() {
        let mut graph = graph(
            5,
            &[
                (0, 1, 10.2),
                (1, 4, 10.2), // existing
                (0, 2, 10.8),
                (2, 4, 10.8), // one new site
                (2, 3, 10.9),
                (3, 4, 10.9), // two new sites
            ],
        );
        graph.new_router[1] = false;
        let plans = plan_routes(&graph, 0, 4, &[], 3, 2, &|| false);
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].nodes, [0, 1, 4]);
        assert_eq!(plans[0].new_routers, 0);
    }

    #[test]
    fn counts_new_sites_and_total_hops_independently() {
        let mut graph = graph(
            6,
            &[
                (0, 1, 12.0),
                (1, 2, 12.0),
                (2, 3, 12.0),
                (3, 4, 12.0),
                (4, 5, 12.0),
            ],
        );
        // Optional existing, required and proposed sites are already available.
        graph.new_router[1..=3].fill(false);
        let plans = plan_routes(&graph, 0, 5, &[2], 5, 1, &|| false);
        assert_eq!(plans[0].nodes, [0, 1, 2, 3, 4, 5]);
        assert_eq!(plans[0].new_routers, 1);
        assert!(plan_routes(&graph, 0, 5, &[2], 4, 1, &|| false).is_empty());
        assert!(plan_routes(&graph, 0, 5, &[2], 5, 0, &|| false).is_empty());
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
