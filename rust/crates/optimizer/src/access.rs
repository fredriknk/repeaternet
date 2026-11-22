//! Dense access sweep around terminals.
//!
//! Endpoints and required routers often sit low, and their first hop is
//! usually the weakest link of a route. The regular candidate grid keeps only a
//! few sites per cell, so it can miss the one nearby high point that both sees
//! the terminal and the wider area. This sweep checks every point on a fine
//! grid around each terminal at final resolution and adds the best ones.

use super::{Cancellation, OptimizationError, Search};
use crate::parallel::for_each_unordered;
use crate::{link_is_clear, PlanningSite};
use repeaternet_rf_core::{Point, RfError, Site, TerrainLayer};
use std::collections::BTreeSet;

const ACCESS_RADIUS_M: f64 = 5_000.0;
const ACCESS_SPACING_M: f64 = 100.0;
/// Candidate budget reserved per terminal. Two thirds go to the highest sites,
/// which favour a wide onward view, and one third to the strongest terminal links.
pub(super) const ACCESS_PER_TERMINAL: usize = 24;
const ACCESS_SEPARATION_M: f64 = 300.0;

struct Probe {
    point: Point,
    ground: f64,
    surface: Option<f64>,
    clear: bool,
    margin_db: f64,
}

impl Search<'_, '_> {
    fn access_points(
        &self,
        center: Point,
    ) -> Result<Vec<(Point, f64, Option<f64>)>, OptimizationError> {
        let radius = ACCESS_RADIUS_M.min(
            self.settings
                .maximum_link_distance_m
                .unwrap_or(f64::INFINITY),
        );
        let spacing = ACCESS_SPACING_M.max(self.terrain.resolution_m());
        let steps = (radius / spacing).floor() as i64;
        let mut points = Vec::new();
        for i in -steps..=steps {
            for j in -steps..=steps {
                let (dx, dy) = (i as f64 * spacing, j as f64 * spacing);
                let distance = dx.hypot(dy);
                if distance >= spacing && distance <= radius {
                    let point = Point {
                        x: center.x + dx,
                        y: center.y + dy,
                    };
                    if !crate::candidates::excluded(point, &self.exclusions) {
                        points.push(point);
                    }
                }
            }
        }
        if points.is_empty() {
            return Ok(Vec::new());
        }
        let ground = crate::candidates::sample(self.terrain, &points, TerrainLayer::GroundDtm)?;
        let surface = if self.terrain.has_surface() {
            crate::candidates::sample(self.terrain, &points, TerrainLayer::SurfaceDom)?
        } else {
            vec![f64::NAN; points.len()]
        };
        let mut result: Vec<_> = points
            .into_iter()
            .zip(ground)
            .zip(surface)
            .filter(|((_, ground), _)| ground.is_finite())
            .map(|((point, ground), surface)| {
                (point, ground, surface.is_finite().then_some(surface))
            })
            .collect();
        if let Some(filter) = self.site_filter.filter(|_| !result.is_empty()) {
            let points: Vec<_> = result.iter().map(|(point, ..)| *point).collect();
            let keep = filter(&points)?;
            if keep.len() != result.len() {
                return Err(OptimizationError::InvalidInput(
                    "Site filter returned the wrong number of points".into(),
                ));
            }
            let mut keep = keep.into_iter();
            result.retain(|_| keep.next().unwrap_or(false));
        }
        Ok(result)
    }

    fn probe_terminal(&self, terminal: usize) -> Result<Vec<Probe>, OptimizationError> {
        let points = self.access_points(self.sites[terminal].rf.position)?;
        let (rf, step, height) = (
            &self.rf,
            self.settings.final_sample_step_m,
            self.router_height_m,
        );
        let terminal = &self.sites[terminal].rf;
        let evaluate = |slot: usize| -> Result<Option<Probe>, OptimizationError> {
            let (point, ground, surface) = points[slot];
            if terminal.position.distance_to(point) < 1.0 {
                return Ok(None);
            }
            let mut probe = Site::new("~access-probe", point.x, point.y);
            probe.ground_elevation_m = ground;
            probe.surface_elevation_m = surface;
            probe.antenna_height_m = height;
            let (source, target) = if terminal.id < probe.id {
                (terminal, &probe)
            } else {
                (&probe, terminal)
            };
            match rf.evaluator.evaluate(source, target, step, false) {
                Ok(link) if link.valid => Ok(Some(Probe {
                    point,
                    ground,
                    surface,
                    clear: link_is_clear(&link),
                    margin_db: link.worst_margin_db(),
                })),
                Ok(_) | Err(RfError::MissingTerrain) => Ok(None),
                Err(error) => Err(error.into()),
            }
        };
        let cancellation = self.hooks.cancellation;
        let cancelled = || cancellation.is_some_and(|poll| poll() != Cancellation::Continue);
        let slots: Vec<usize> = (0..points.len()).collect();
        let mut probes = Vec::new();
        for_each_unordered(
            &slots,
            self.rf_workers(slots.len()),
            &cancelled,
            evaluate,
            |_, probe| probes.extend(probe),
        )?;
        // Completion order varies with scheduling; sort for a deterministic choice.
        probes.sort_by(|a, b| {
            (a.point.x, a.point.y)
                .partial_cmp(&(b.point.x, b.point.y))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        Ok(probes)
    }

    fn choose_access(probes: Vec<Probe>, existing: &[Point], limit: usize) -> Vec<Probe> {
        let by_margin_limit = limit / 3;
        let by_height_limit = limit - by_margin_limit;
        let order = |key: &dyn Fn(&Probe) -> (bool, f64)| {
            let mut order: Vec<usize> = (0..probes.len()).collect();
            order.sort_by(|&a, &b| {
                let (a, b) = (key(&probes[a]), key(&probes[b]));
                b.0.cmp(&a.0).then(b.1.total_cmp(&a.1))
            });
            order
        };
        let by_height = order(&|probe| (probe.clear, probe.ground));
        let by_margin = order(&|probe| (probe.clear, probe.margin_db));
        let mut taken: Vec<Point> = existing.to_vec();
        let mut chosen = Vec::new();
        for (sorted, limit) in [(by_height, by_height_limit), (by_margin, by_margin_limit)] {
            let mut count = 0;
            for index in sorted {
                if count == limit {
                    break;
                }
                let point = probes[index].point;
                if taken
                    .iter()
                    .all(|other| other.distance_to(point) >= ACCESS_SEPARATION_M)
                {
                    taken.push(point);
                    chosen.push(index);
                    count += 1;
                }
            }
        }
        chosen.sort_unstable();
        let mut probes: Vec<Option<Probe>> = probes.into_iter().map(Some).collect();
        chosen
            .into_iter()
            .map(|index| probes[index].take().expect("chosen once"))
            .collect()
    }

    /// Add access sites around every terminal. Runs before any link is
    /// evaluated, so site indices can still be rearranged. Returns the count.
    pub(super) fn add_terminal_access_sites(
        &mut self,
        terminals: &BTreeSet<usize>,
    ) -> Result<usize, OptimizationError> {
        let per_terminal = self.access_budget / terminals.len().max(1);
        if per_terminal == 0 || !self.links.is_empty() || !self.final_checked.is_empty() {
            return Ok(0);
        }
        let existing: Vec<Point> = self.sites.iter().map(|site| site.rf.position).collect();
        let mut added = Vec::new();
        for &terminal in terminals {
            if self.hooks.cancelled() {
                return Ok(0);
            }
            let probes = self.probe_terminal(terminal)?;
            let taken: Vec<Point> = existing
                .iter()
                .copied()
                .chain(added.iter().map(|site: &PlanningSite| site.rf.position))
                .collect();
            for (k, probe) in Self::choose_access(probes, &taken, per_terminal)
                .into_iter()
                .enumerate()
            {
                let mut rf = Site::new(
                    format!("{}-access-{}", self.sites[terminal].rf.id, k + 1),
                    probe.point.x,
                    probe.point.y,
                );
                rf.ground_elevation_m = probe.ground;
                rf.surface_elevation_m = probe.surface;
                rf.antenna_height_m = self.router_height_m;
                added.push(PlanningSite::from(rf));
            }
        }
        if added.is_empty() {
            return Ok(0);
        }
        let count = added.len();
        // Keep the last client last for the two-terminal helpers that rely on it.
        let insert_at = self
            .sites
            .iter()
            .rposition(|site| site.is_client())
            .filter(|_| self.sites.iter().filter(|site| site.is_client()).count() == 2)
            .unwrap_or(self.sites.len());
        for site in &mut added {
            while self.by_id.contains_key(&site.rf.id) {
                site.rf.id.push('+');
            }
            self.by_id.insert(site.rf.id.clone(), usize::MAX);
        }
        self.sites.splice(insert_at..insert_at, added);
        self.by_id = self
            .sites
            .iter()
            .enumerate()
            .map(|(i, site)| (site.rf.id.clone(), i))
            .collect();
        Ok(count)
    }
}
