//! Local refinement of generated router positions on planned routes.

use super::{Cancellation, OptimizationError, Search, Stage};
use crate::parallel::for_each_unordered;
use crate::{link_is_clear, pair, SiteOrigin};
use repeaternet_rf_core::{LinkResult, Point, RfError, SiteKind, TerrainLayer};

/// Ranking of a router position: clear hops first, then the weaker adjacent
/// margin, then the weaker Fresnel clearance.
type Score = (bool, f64, f64);

fn position_score(left: &LinkResult, right: &LinkResult) -> Score {
    (
        link_is_clear(left) && link_is_clear(right),
        left.worst_margin_db().min(right.worst_margin_db()),
        left.minimum_fresnel_clearance_ratio
            .min(right.minimum_fresnel_clearance_ratio),
    )
}

type Position = (Point, f64, Option<f64>);

impl Search<'_, '_> {
    fn movable(&self, index: usize) -> bool {
        let site = &self.sites[index];
        self.generated_candidates
            && site.rf.kind == SiteKind::Candidate
            && site.origin == SiteOrigin::Optimized
            && !site.locked
            && !site.is_required()
    }

    /// Positions within the refinement disk that have ground terrain and pass the
    /// site filter, with their ground and optional surface elevations.
    fn refinement_positions(&self, center: Point) -> Result<Vec<Position>, OptimizationError> {
        let radius = self.settings.refine_radius_m;
        let step = self
            .settings
            .refine_step_m
            .max(self.terrain.resolution_m())
            .max(1.0);
        let steps = (radius / step).floor() as i64;
        let mut points = Vec::new();
        for i in -steps..=steps {
            for j in -steps..=steps {
                let (dx, dy) = (i as f64 * step, j as f64 * step);
                if (dx != 0.0 || dy != 0.0) && dx * dx + dy * dy <= radius * radius {
                    points.push(Point {
                        x: center.x + dx,
                        y: center.y + dy,
                    });
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
        let mut positions: Vec<Position> = points
            .into_iter()
            .zip(ground)
            .zip(surface)
            .filter(|((_, ground), _)| ground.is_finite())
            .map(|((point, ground), surface)| {
                (point, ground, surface.is_finite().then_some(surface))
            })
            .collect();
        if let Some(filter) = self.site_filter.filter(|_| !positions.is_empty()) {
            let points: Vec<_> = positions.iter().map(|(point, ..)| *point).collect();
            let keep = filter(&points)?;
            if keep.len() != positions.len() {
                return Err(OptimizationError::InvalidInput(
                    "Site filter returned the wrong number of points".into(),
                ));
            }
            let mut keep = keep.into_iter();
            positions.retain(|_| keep.next().unwrap_or(false));
        }
        Ok(positions)
    }

    /// Best position for router `index` between `before` and `after`.
    fn best_position(
        &self,
        index: usize,
        before: usize,
        after: usize,
        trial_id: &str,
        positions: &[Position],
    ) -> Result<Option<(Score, usize)>, OptimizationError> {
        let (rf, sites) = (&self.rf, &self.sites);
        let step = self.settings.final_sample_step_m;
        let base = &sites[index].rf;
        let evaluate = |slot: usize| {
            let (point, ground, surface) = positions[slot];
            let mut trial = base.clone();
            trial.id = trial_id.to_owned();
            trial.position = point;
            trial.ground_elevation_m = ground;
            trial.surface_elevation_m = surface;
            let link = |other: usize| -> Result<Option<LinkResult>, OptimizationError> {
                let other = &sites[other].rf;
                if other.position.distance_to(trial.position) < 1.0 {
                    return Ok(None);
                }
                let (source, target) = if trial.id < other.id {
                    (&trial, other)
                } else {
                    (other, &trial)
                };
                match rf.evaluator.evaluate(source, target, step, false) {
                    Ok(link) => Ok(Some(link).filter(|link| link.valid)),
                    Err(RfError::MissingTerrain) => Ok(None),
                    Err(error) => Err(error.into()),
                }
            };
            Ok::<_, OptimizationError>(match (link(before)?, link(after)?) {
                (Some(left), Some(right)) => Some(position_score(&left, &right)),
                _ => None,
            })
        };
        let cancellation = self.hooks.cancellation;
        let cancelled = || cancellation.is_some_and(|poll| poll() != Cancellation::Continue);
        let slots: Vec<usize> = (0..positions.len()).collect();
        let mut best: Option<(Score, usize)> = None;
        for_each_unordered(
            &slots,
            self.rf_workers(slots.len()),
            &cancelled,
            evaluate,
            |slot, score| {
                let Some(score) = score else {
                    return;
                };
                // Ties go to the lowest slot so the outcome is independent of scheduling.
                if best.is_none_or(|(incumbent, incumbent_slot)| {
                    score > incumbent || (score == incumbent && slot < incumbent_slot)
                }) {
                    best = Some((score, slot));
                }
            },
        )?;
        Ok(best)
    }

    /// Move each generated router on each route to the best position within the
    /// refinement radius, evaluated at final resolution. A move is kept only when
    /// its certified hops beat the original ones. Returns the number of moves.
    pub(super) fn refine_routes(
        &mut self,
        plans: &mut [Vec<usize>],
    ) -> Result<usize, OptimizationError> {
        if self.settings.refine_radius_m <= 0.0 {
            return Ok(0);
        }
        let total: usize = plans.iter().map(|path| path.len().saturating_sub(2)).sum();
        let mut done = 0;
        let mut moved = 0;
        // Routes often share a hop context; reuse its refinement instead of adding a twin site.
        let mut refined = std::collections::BTreeMap::new();
        for path in plans.iter_mut() {
            for k in 1..path.len().saturating_sub(1) {
                if self.hooks.cancelled() {
                    return Ok(moved);
                }
                done += 1;
                self.hooks
                    .notify(Stage::RefiningRouterPositions, done, total);
                let (before, index, after) = (path[k - 1], path[k], path[k + 1]);
                if !self.movable(index) {
                    continue;
                }
                let (Some(left), Some(right)) = (
                    self.links.get(&pair(before, index)),
                    self.links.get(&pair(index, after)),
                ) else {
                    continue;
                };
                let current = position_score(left, right);
                let positions = self.refinement_positions(self.sites[index].rf.position)?;
                if let Some(&refined) = refined.get(&(before, index, after)) {
                    path[k] = refined;
                    moved += 1;
                    continue;
                }
                let mut suffix = 1;
                let mut trial_id = format!("{}.{suffix}", self.sites[index].rf.id);
                while self.by_id.contains_key(&trial_id) {
                    suffix += 1;
                    trial_id = format!("{}.{suffix}", self.sites[index].rf.id);
                }
                let Some((score, slot)) =
                    self.best_position(index, before, after, &trial_id, &positions)?
                else {
                    continue;
                };
                if score <= current || self.hooks.cancelled() {
                    continue;
                }
                let (point, ground, surface) = positions[slot];
                let mut site = self.sites[index].clone();
                site.rf.id = trial_id;
                site.rf.position = point;
                site.rf.ground_elevation_m = ground;
                site.rf.surface_elevation_m = surface;
                let new_index = self.sites.len();
                self.by_id.insert(site.rf.id.clone(), new_index);
                self.sites.push(site);
                let hops = [pair(before, new_index), pair(new_index, after)];
                self.certify_unordered(&hops, Stage::RefiningRouterPositions)?;
                if let (Some(left), Some(right)) =
                    (self.links.get(&hops[0]), self.links.get(&hops[1]))
                {
                    if position_score(left, right) > current {
                        refined.insert((before, index, after), new_index);
                        path[k] = new_index;
                        moved += 1;
                    }
                }
            }
        }
        Ok(moved)
    }
}
