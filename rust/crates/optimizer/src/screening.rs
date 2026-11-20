use crate::{pair, PlanningSite, SiteOrigin};
use repeaternet_rf_core::Point;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap};

pub fn screening_pair_indices(
    sites: &[PlanningSite],
    neighbor_limit: usize,
    terminals: &BTreeSet<usize>,
) -> Vec<(usize, usize)> {
    screened_pairs(sites, neighbor_limit, terminals, &|| false)
        .into_iter()
        .collect()
}

pub(crate) fn screened_pairs(
    sites: &[PlanningSite],
    neighbor_limit: usize,
    terminals: &BTreeSet<usize>,
    cancelled: &dyn Fn() -> bool,
) -> BTreeSet<(usize, usize)> {
    let mut result = BTreeSet::new();
    if sites.len() < 2 || cancelled() {
        return result;
    }
    let default_terminals = BTreeSet::from([0, sites.len() - 1]);
    let terminals = if terminals.is_empty() {
        &default_terminals
    } else {
        terminals
    };
    for &terminal in terminals.iter().filter(|&&i| i < sites.len()) {
        for other in 0..sites.len() {
            if cancelled() {
                return BTreeSet::new();
            }
            if terminal != other {
                result.insert(pair(terminal, other));
            }
        }
    }
    let count = neighbor_limit.max(1).min(sites.len() - 1);
    let coordinates: Vec<_> = sites.iter().map(|site| site.rf.position).collect();
    let tree = if coordinates
        .iter()
        .all(|point| point.x.is_finite() && point.y.is_finite())
    {
        let mut indices: Vec<_> = (0..sites.len()).collect();
        KdNode::build(&coordinates, &mut indices, 0)
    } else {
        None
    };
    for (index, &point) in coordinates.iter().enumerate() {
        if cancelled() {
            return BTreeSet::new();
        }
        if terminals.contains(&index) {
            continue;
        }
        let mut nearest = BinaryHeap::new();
        if let Some(tree) = &tree {
            tree.nearest(&coordinates, index, count, &mut nearest);
        } else {
            for (neighbor, &other) in coordinates.iter().enumerate() {
                if index != neighbor {
                    retain(
                        &mut nearest,
                        count,
                        Neighbor {
                            distance: point.distance_to(other),
                            index: neighbor,
                        },
                    );
                }
            }
        }
        result.extend(
            nearest
                .into_iter()
                .map(|neighbor| pair(index, neighbor.index)),
        );
    }
    result
}

struct KdNode {
    index: usize,
    axis: usize,
    left: Option<Box<Self>>,
    right: Option<Box<Self>>,
}

fn coordinate(point: Point, axis: usize) -> f64 {
    if axis == 0 {
        point.x
    } else {
        point.y
    }
}

impl KdNode {
    fn build(points: &[Point], indices: &mut [usize], axis: usize) -> Option<Box<Self>> {
        if indices.is_empty() {
            return None;
        }
        let (left, index, right) = indices.select_nth_unstable_by(indices.len() / 2, |&a, &b| {
            coordinate(points[a], axis)
                .total_cmp(&coordinate(points[b], axis))
                .then_with(|| a.cmp(&b))
        });
        Some(Box::new(Self {
            index: *index,
            axis,
            left: Self::build(points, left, 1 - axis),
            right: Self::build(points, right, 1 - axis),
        }))
    }

    fn nearest(
        &self,
        points: &[Point],
        query: usize,
        limit: usize,
        nearest: &mut BinaryHeap<Neighbor>,
    ) {
        let point = points[query];
        let pivot = points[self.index];
        if query != self.index {
            retain(
                nearest,
                limit,
                Neighbor {
                    distance: point.distance_to(pivot),
                    index: self.index,
                },
            );
        }
        let offset = coordinate(point, self.axis) - coordinate(pivot, self.axis);
        let (near, far) = if offset <= 0.0 {
            (&self.left, &self.right)
        } else {
            (&self.right, &self.left)
        };
        if let Some(near) = near {
            near.nearest(points, query, limit, nearest);
        }
        if nearest.len() < limit
            || nearest
                .peek()
                .is_some_and(|edge| offset.abs() <= edge.distance)
        {
            if let Some(far) = far {
                far.nearest(points, query, limit, nearest);
            }
        }
    }
}

fn retain(nearest: &mut BinaryHeap<Neighbor>, limit: usize, edge: Neighbor) {
    if nearest.len() < limit {
        nearest.push(edge);
    } else if nearest.peek().is_some_and(|current| edge < *current) {
        *nearest.peek_mut().expect("positive limit") = edge;
    }
}

struct Neighbor {
    distance: f64,
    index: usize,
}

impl Ord for Neighbor {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.distance
            .total_cmp(&other.distance)
            .then_with(|| self.index.cmp(&other.index))
    }
}

impl PartialOrd for Neighbor {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Neighbor {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}

impl Eq for Neighbor {}

pub fn backbone_site_indices(
    sites: &[PlanningSite],
    limit: usize,
    excluded: &BTreeSet<usize>,
) -> Vec<usize> {
    let mut pool: Vec<_> = sites
        .iter()
        .enumerate()
        .filter_map(|(i, s)| {
            (s.included()
                && !excluded.contains(&i)
                && s.rf.position.x.is_finite()
                && s.rf.position.y.is_finite())
            .then_some(i)
        })
        .collect();
    if limit == 0 {
        return Vec::new();
    }
    if pool.len() <= limit {
        return pool;
    }
    let finite = |value: f64| {
        if value.is_finite() {
            value
        } else {
            f64::NEG_INFINITY
        }
    };
    let rank = |&a: &usize, &b: &usize| {
        finite(sites[b].site_quality)
            .total_cmp(&finite(sites[a].site_quality))
            .then_with(|| {
                finite(sites[b].rf.antenna_absolute_elevation_m())
                    .total_cmp(&finite(sites[a].rf.antenna_absolute_elevation_m()))
            })
            .then_with(|| a.cmp(&b))
    };
    pool.sort_unstable_by(rank);
    let mut known_budget = (limit / 4).max(1);
    let mut known = Vec::new();
    pool.retain(|&i| {
        if sites[i].origin != SiteOrigin::Known {
            return true;
        }
        if known_budget == 0 {
            return false;
        }
        known.push(i);
        known_budget -= 1;
        true
    });
    // Rotate into the principal axes of the point distribution so the grid
    // covers a long diagonal corridor the same way as a horizontal one.
    let count = pool.len() as f64;
    let mean_x = pool.iter().map(|&i| sites[i].rf.position.x).sum::<f64>() / count;
    let mean_y = pool.iter().map(|&i| sites[i].rf.position.y).sum::<f64>() / count;
    let centered: Vec<_> = pool
        .iter()
        .map(|&i| {
            (
                sites[i].rf.position.x - mean_x,
                sites[i].rf.position.y - mean_y,
            )
        })
        .collect();
    let (xx, xy, yy) = centered
        .iter()
        .fold((0.0, 0.0, 0.0), |(xx, xy, yy), &(x, y)| {
            (xx + x * x, xy + x * y, yy + y * y)
        });
    let (sin, cos) = (0.5 * (2.0 * xy).atan2(xx - yy)).sin_cos();
    let rotated: Vec<_> = centered
        .iter()
        .map(|&(x, y)| (x * cos + y * sin, -x * sin + y * cos))
        .collect();
    let (lo_x, hi_x, lo_y, hi_y) = rotated.iter().fold(
        (
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
        ),
        |(lo_x, hi_x, lo_y, hi_y), &(x, y)| (lo_x.min(x), hi_x.max(x), lo_y.min(y), hi_y.max(y)),
    );
    let (width, height) = (hi_x - lo_x, hi_y - lo_y);
    let size = (width * height / limit as f64)
        .sqrt()
        .max(width.max(height) / limit as f64);
    let mut cells = BTreeMap::new();
    if size > 0.0 {
        for (&i, &(x, y)) in pool.iter().zip(&rotated) {
            let cell = (
                ((x - lo_x) / size).floor() as i64,
                ((y - lo_y) / size).floor() as i64,
            );
            cells.entry(cell).or_insert(i);
        }
    }
    let mut spread: Vec<_> = cells
        .into_values()
        .filter(|&i| sites[i].site_quality.is_finite())
        .collect();
    spread.sort_unstable_by(rank);
    let mut selected = BTreeSet::new();
    for i in known.into_iter().chain(spread).chain(pool) {
        if selected.len() == limit {
            break;
        }
        selected.insert(i);
    }
    selected.into_iter().collect()
}
