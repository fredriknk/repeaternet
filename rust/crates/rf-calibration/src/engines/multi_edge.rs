//! Three-edge approximations using a common Deygout selection.

use repeaternet_rf_core::{knife_edge_loss_db, knife_edge_v};
use serde::Serialize;

#[derive(Serialize)]
pub struct Edge {
    pub distance_m: f64,
    pub v: f64,
    pub j_db: f64,
}

pub struct MultiEdge {
    pub deygout: f64,
    pub epstein_peterson: f64,
    pub giovaneli: f64,
    pub edges: Vec<Edge>,
}

#[derive(Clone, Copy)]
struct Candidate {
    v: f64,
    i: usize,
    a: usize,
    b: usize,
}

#[derive(Clone, Copy)]
struct Profile<'a> {
    x: &'a [f64],
    z: &'a [f64],
    freq: f64,
}

impl Profile<'_> {
    fn residual(self, a: usize, i: usize, b: usize, left: f64, right: f64) -> f64 {
        let d1 = self.x[i] - self.x[a];
        let d2 = self.x[b] - self.x[i];
        self.z[i] - (left * d2 + right * d1) / (d1 + d2)
    }

    fn v(self, a: usize, i: usize, b: usize, left: f64, right: f64) -> f64 {
        knife_edge_v(
            self.residual(a, i, b, left, right),
            self.x[i] - self.x[a],
            self.x[b] - self.x[i],
            self.freq,
        )
    }

    fn candidate(self, a: usize, b: usize) -> Option<Candidate> {
        (a + 1..b)
            .map(|i| Candidate {
                v: self.v(a, i, b, self.z[a], self.z[b]),
                i,
                a,
                b,
            })
            .max_by(|p, q| p.v.total_cmp(&q.v).then(q.i.cmp(&p.i)))
    }

    fn ep_edge(self, i: usize) -> f64 {
        knife_edge_loss_db(self.v(i - 1, i, i + 1, self.z[i - 1], self.z[i + 1]))
    }

    fn giovaneli(self, a: usize, b: usize) -> f64 {
        let residual = |i| self.residual(a, i, b, self.z[a], self.z[b]);
        let v = |i| self.v(a, i, b, self.z[a], self.z[b]);
        let Some(i) = (a + 1..b).max_by(|&p, &q| {
            residual(p)
                .total_cmp(&residual(q))
                .then_with(|| v(p).total_cmp(&v(q)))
                .then(q.cmp(&p))
        }) else {
            return 0.0;
        };
        if residual(i) <= 0.0 {
            return (a + 1..b).map(|i| self.ep_edge(i)).sum();
        }
        let slope = |q: usize| (self.z[q] - self.z[i]) / (self.x[q] - self.x[i]).abs();
        let support = |left: usize, right: usize| {
            (left..right)
                .max_by(|&p, &q| slope(p).total_cmp(&slope(q)).then(q.cmp(&p)))
                .unwrap()
        };
        let project = |q: usize, endpoint: usize| {
            self.z[i]
                + (self.z[q] - self.z[i]) * (self.x[endpoint] - self.x[i]) / (self.x[q] - self.x[i])
        };
        let left = project(support(a, i), a);
        let right = project(support(i + 1, b + 1), b);
        knife_edge_loss_db(self.v(a, i, b, left, right))
            + self.giovaneli(a, i)
            + self.giovaneli(i, b)
    }
}

pub fn evaluate(x: &[f64], z: &[f64], freq: f64) -> MultiEdge {
    let profile = Profile { x, z, freq };
    let last = x.len() - 1;
    let mut pending: Vec<_> = profile.candidate(0, last).into_iter().collect();
    let mut selected = Vec::with_capacity(3);
    while selected.len() < 3 {
        let Some((index, _)) = pending
            .iter()
            .enumerate()
            .max_by(|(_, p), (_, q)| p.v.total_cmp(&q.v).then(p.i.cmp(&q.i)))
        else {
            break;
        };
        let chosen = pending.swap_remove(index);
        if chosen.v <= -0.78 {
            break;
        }
        selected.push(chosen);
        pending.extend(profile.candidate(chosen.a, chosen.i));
        pending.extend(profile.candidate(chosen.i, chosen.b));
    }

    let mut indices: Vec<_> = selected.iter().map(|edge| edge.i).collect();
    indices.sort_unstable();
    let indices = [0].into_iter().chain(indices).chain([last]);
    let (x, z): (Vec<_>, Vec<_>) = indices
        .map(|i| (x[i], profile.residual(0, i, last, z[0], z[last])))
        .unzip();
    let reduced = Profile { x: &x, z: &z, freq };
    let edges: Vec<_> = selected
        .into_iter()
        .map(|edge| Edge {
            distance_m: profile.x[edge.i],
            v: edge.v,
            j_db: knife_edge_loss_db(edge.v),
        })
        .collect();
    MultiEdge {
        deygout: edges.iter().map(|edge| edge.j_db).sum(),
        epstein_peterson: (1..x.len() - 1).map(|i| reduced.ep_edge(i)).sum(),
        giovaneli: reduced.giovaneli(0, x.len() - 1),
        edges,
    }
}
