//! Local refinement of generated router positions on planned routes.

use super::{Cancellation, OptimizationError, Search, Stage};
use crate::parallel::for_each_unordered;
use crate::{link_is_clear, pair, NetworkSolution, SiteOrigin};
use repeaternet_rf_core::{LinkResult, Point, RfError, Site, SiteKind, TerrainLayer};

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

fn include_link(score: Score, link: &LinkResult) -> Score {
    (
        score.0 && link_is_clear(link),
        score.1.min(link.worst_margin_db()),
        score.2.min(link.minimum_fresnel_clearance_ratio),
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
        neighbors: &[usize],
        trial_id: &str,
        positions: &[Position],
    ) -> Result<Option<(Score, Site)>, OptimizationError> {
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
            trial.sample_terminal_clutter(self.terrain)?;
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
            let mut score = (true, f64::INFINITY, f64::INFINITY);
            for &other in neighbors {
                let Some(result) = link(other)? else {
                    return Ok(None);
                };
                score = include_link(score, &result);
            }
            Ok::<_, OptimizationError>(Some((score, trial)))
        };
        let cancellation = self.hooks.cancellation;
        let cancelled = || cancellation.is_some_and(|poll| poll() != Cancellation::Continue);
        let slots: Vec<usize> = (0..positions.len()).collect();
        let mut best: Option<(Score, usize, Site)> = None;
        for_each_unordered(
            &slots,
            self.rf_workers(slots.len()),
            &cancelled,
            evaluate,
            |slot, result| {
                let Some((score, site)) = result else {
                    return;
                };
                // Ties go to the lowest slot so the outcome is independent of scheduling.
                if best.as_ref().is_none_or(|(incumbent, incumbent_slot, _)| {
                    score > *incumbent || (score == *incumbent && slot < *incumbent_slot)
                }) {
                    best = Some((score, slot, site));
                }
            },
        )?;
        Ok(best.map(|(score, _, site)| (score, site)))
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
                if let Some(&refined) = refined.get(&(before, index, after)) {
                    path[k] = refined;
                    moved += 1;
                    continue;
                }
                let positions = self.refinement_positions(self.sites[index].rf.position)?;
                let mut suffix = 1;
                let mut trial_id = format!("{}.{suffix}", self.sites[index].rf.id);
                while self.by_id.contains_key(&trial_id) {
                    suffix += 1;
                    trial_id = format!("{}.{suffix}", self.sites[index].rf.id);
                }
                let Some((score, rf)) =
                    self.best_position(index, &[before, after], &trial_id, &positions)?
                else {
                    continue;
                };
                if score <= current || self.hooks.cancelled() {
                    continue;
                }
                let mut site = self.sites[index].clone();
                site.rf = rf;
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

    /// Move each installed router as one site, checking all of its mesh links.
    pub(super) fn refine_mesh(
        &mut self,
        alternatives: &mut [NetworkSolution],
    ) -> Result<(), OptimizationError> {
        if self.settings.refine_radius_m <= 0.0 || !self.generated_candidates {
            return Ok(());
        }
        let anchors: Vec<_> = self
            .sites
            .iter()
            .filter(|site| site.is_client() || site.is_required())
            .map(|site| site.rf.position)
            .collect();
        let area = repeaternet_terrain::corridor::MeshArea::new(&anchors);
        for solution in alternatives {
            let total = solution.sites.len();
            for slot in 0..total {
                if self.hooks.cancelled() {
                    return Ok(());
                }
                let id = solution.sites[slot].rf.id.clone();
                let index = self.by_id[&id];
                if !self.movable(index) {
                    continue;
                }
                self.hooks
                    .notify(Stage::RefiningRouterPositions, slot + 1, total);
                let incident: Vec<_> = solution
                    .links
                    .iter()
                    .enumerate()
                    .filter_map(|(link_index, link)| {
                        let other = if link.source_id == id {
                            &link.target_id
                        } else if link.target_id == id {
                            &link.source_id
                        } else {
                            return None;
                        };
                        Some((link_index, self.by_id[other]))
                    })
                    .collect();
                let neighbors: Vec<_> = incident.iter().map(|&(_, node)| node).collect();
                let current = incident
                    .iter()
                    .fold((true, f64::INFINITY, f64::INFINITY), |score, &(edge, _)| {
                        include_link(score, &solution.links[edge])
                    });
                let mut positions = self.refinement_positions(self.sites[index].rf.position)?;
                positions.retain(|&(point, ..)| {
                    area.distance_to(point) <= self.settings.corridor_width_m
                        && !crate::candidates::excluded(point, &self.exclusions)
                });
                let trial_id = format!("{id}.{}", self.sites.len());
                let Some((score, rf)) =
                    self.best_position(index, &neighbors, &trial_id, &positions)?
                else {
                    continue;
                };
                if score <= current || score.1 < current.1 || self.hooks.cancelled() {
                    continue;
                }
                let mut site = self.sites[index].clone();
                site.rf = rf;
                let new_index = self.sites.len();
                self.by_id.insert(trial_id.clone(), new_index);
                self.sites.push(site.clone());
                let hops: Vec<_> = neighbors
                    .iter()
                    .map(|&node| pair(node, new_index))
                    .collect();
                self.certify_unordered(&hops, Stage::RefiningRouterPositions)?;
                let Some(links) = hops
                    .iter()
                    .map(|key| self.links.get(key))
                    .collect::<Option<Vec<_>>>()
                else {
                    continue;
                };
                let score = links
                    .iter()
                    .fold((true, f64::INFINITY, f64::INFINITY), |score, link| {
                        include_link(score, link)
                    });
                if score <= current || score.1 < current.1 {
                    continue;
                }
                for ((edge, _), link) in incident.into_iter().zip(links) {
                    solution.links[edge] = link.clone();
                }
                solution.sites[slot] = site;
                for node in solution
                    .router_ids
                    .iter_mut()
                    .chain(solution.client_paths.values_mut().flatten().flatten())
                {
                    if *node == id {
                        node.clone_from(&trial_id);
                    }
                }
            }
        }
        Ok(())
    }
}
