//! Local refinement of generated router positions on planned routes.

use super::{Cancellation, OptimizationError, RouteOptimizer, Search, SearchHooks, Stage};
use crate::parallel::for_each_unordered;
use crate::{link_is_clear, pair, NetworkSolution, SiteFilter, SiteOrigin};
use repeaternet_rf_core::{
    LinkEvaluator, LinkResult, Point, RfError, Site, SiteKind, TerrainLayer, TerrainSource,
};
use std::{cmp::Reverse, collections::BTreeSet};

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

pub struct PlacementSite {
    pub rf: Site,
    pub movable: bool,
}

pub struct PlacementRequest<'a> {
    pub sites: &'a [PlacementSite],
    /// Indices into sites. None selects all pairs initially within link range.
    pub links: Option<&'a [(usize, usize)]>,
    pub exclusions: &'a [Vec<Point>],
    pub radius_m: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlacementMargin {
    pub db: f64,
    pub valid: bool,
}

pub struct PlacementLink {
    pub source: usize,
    pub target: usize,
    pub before: Option<PlacementMargin>,
    pub after: Option<PlacementMargin>,
}

pub struct PlacementPreview {
    /// Same order and IDs as the input. Only position and terrain data can change.
    pub sites: Vec<Site>,
    pub links: Vec<PlacementLink>,
}

fn placement_score(
    margins: impl IntoIterator<Item = Option<PlacementMargin>>,
) -> (Reverse<usize>, f64, f64) {
    let (mut missing, mut weakest, mut sum) = (0, f64::INFINITY, 0.0);
    for margin in margins {
        if let Some(margin) = margin {
            weakest = weakest.min(margin.db);
            sum += margin.db;
        } else {
            missing += 1;
        }
    }
    (Reverse(missing), weakest, sum)
}

impl RouteOptimizer<'_> {
    /// Coordinate descent on a fixed graph. Cancellation returns no preview.
    pub fn optimize_placement(
        &self,
        request: PlacementRequest<'_>,
        mut hooks: SearchHooks<'_>,
    ) -> Result<Option<PlacementPreview>, OptimizationError> {
        const MAX_ROUNDS: usize = 4;
        let PlacementRequest {
            sites,
            links,
            exclusions,
            radius_m,
        } = request;
        if !(0.0 < radius_m && radius_m <= 2000.0) {
            return Err(OptimizationError::InvalidInput(
                "Move radius must be greater than 0 and at most 2,000 m".into(),
            ));
        }
        let mut current: Vec<_> = sites.iter().map(|site| site.rf.clone()).collect();
        let range = self
            .settings
            .maximum_link_distance_m
            .unwrap_or(f64::INFINITY);
        let links: BTreeSet<_> = match links {
            Some(links) => {
                if links
                    .iter()
                    .any(|&(a, b)| a == b || a >= sites.len() || b >= sites.len())
                {
                    return Err(OptimizationError::InvalidInput(
                        "Invalid placement link".into(),
                    ));
                }
                links.iter().map(|&(a, b)| pair(a, b)).collect()
            }
            None => current
                .iter()
                .enumerate()
                .flat_map(|(a, source)| {
                    current
                        .iter()
                        .enumerate()
                        .skip(a + 1)
                        .filter_map(move |(b, target)| {
                            (1.0..=range)
                                .contains(&source.distance_to(target))
                                .then_some((a, b))
                        })
                })
                .collect(),
        };
        let links: Vec<_> = links.into_iter().collect();
        let mut neighbors = vec![Vec::new(); sites.len()];
        for (edge, &(a, b)) in links.iter().enumerate() {
            neighbors[a].push((edge, b));
            neighbors[b].push((edge, a));
        }
        let movable: Vec<_> = sites
            .iter()
            .enumerate()
            .filter_map(|(i, site)| {
                (site.movable
                    && matches!(site.rf.kind, SiteKind::Router | SiteKind::Candidate)
                    && !neighbors[i].is_empty())
                .then_some(i)
            })
            .collect();
        if movable.is_empty() {
            return Err(OptimizationError::InvalidInput(
                "Select a movable router and another site linked to it".into(),
            ));
        }
        let native = LinkEvaluator::new(self.terrain, self.rf);
        let evaluator = self.evaluator.unwrap_or(&native);
        let measure = |a: &Site, b: &Site| -> Result<Option<PlacementMargin>, OptimizationError> {
            let (source, target) = if a.id < b.id { (a, b) } else { (b, a) };
            if source.distance_to(target) < 1.0 {
                return Ok(None);
            }
            match evaluator.evaluate(source, target, self.settings.final_sample_step_m, false) {
                Ok(link) => {
                    let db = link.worst_margin_db();
                    Ok(db.is_finite().then_some(PlacementMargin {
                        db,
                        valid: link.valid,
                    }))
                }
                Err(RfError::MissingTerrain) => Ok(None),
                Err(error) => Err(error.into()),
            }
        };
        let mut before = Vec::with_capacity(links.len());
        for &(a, b) in &links {
            if hooks.cancelled() {
                return Ok(None);
            }
            before.push(measure(&current[a], &current[b])?);
        }
        let mut margins = before.clone();
        let mut trials = vec![Vec::<Site>::new(); sites.len()];
        let total = MAX_ROUNDS * movable.len();
        for round in 0..MAX_ROUNDS {
            let mut moved = false;
            for (slot, &index) in movable.iter().enumerate() {
                if hooks.cancelled() {
                    return Ok(None);
                }
                hooks.notify(
                    Stage::RefiningRouterPositions,
                    round * movable.len() + slot,
                    total,
                );
                if round == 0 {
                    // Bound the disk grid even when a plan has a very small refine step.
                    let step = self
                        .settings
                        .refine_step_m
                        .min(radius_m)
                        .max(radius_m / 8.0);
                    for (point, ground, surface) in refinement_positions(
                        self.terrain,
                        sites[index].rf.position,
                        radius_m,
                        step,
                        exclusions,
                        self.site_filter,
                    )? {
                        if hooks.cancelled() {
                            return Ok(None);
                        }
                        let mut site = sites[index].rf.clone();
                        site.position = point;
                        site.ground_elevation_m = ground;
                        site.surface_elevation_m = surface;
                        site.sample_terminal_clutter(self.terrain)?;
                        trials[index].push(site);
                    }
                }
                let mut best_score = placement_score(margins.iter().copied());
                let mut best = None;
                let mut trial_margins = margins.clone();
                'positions: for trial in &trials[index] {
                    if hooks.cancelled() {
                        return Ok(None);
                    }
                    if current
                        .iter()
                        .enumerate()
                        .any(|(i, site)| i != index && trial.distance_to(site) < 1.0)
                    {
                        continue;
                    }
                    for &(edge, other) in &neighbors[index] {
                        if hooks.cancelled() {
                            return Ok(None);
                        }
                        if trial.distance_to(&current[other]) > range {
                            continue 'positions;
                        }
                        let margin = measure(trial, &current[other])?;
                        if margin.is_none() && margins[edge].is_some() {
                            continue 'positions;
                        }
                        trial_margins[edge] = margin;
                    }
                    let score = placement_score(trial_margins.iter().copied());
                    if score > best_score {
                        best_score = score;
                        best = Some((trial, trial_margins.clone()));
                    }
                }
                if let Some((trial, updated)) = best {
                    current[index] = trial.clone();
                    margins = updated;
                    moved = true;
                }
            }
            if !moved {
                break;
            }
        }
        if hooks.cancelled() {
            return Ok(None);
        }
        hooks.notify(Stage::RefiningRouterPositions, total, total);
        let links = links
            .into_iter()
            .zip(before)
            .zip(margins)
            .map(|(((source, target), before), after)| PlacementLink {
                source,
                target,
                before,
                after,
            })
            .collect();
        Ok(Some(PlacementPreview {
            sites: current,
            links,
        }))
    }
}

/// Positions within the refinement disk that have ground terrain and pass the
/// site filter, with their ground and optional surface elevations.
fn refinement_positions(
    terrain: &dyn TerrainSource,
    center: Point,
    radius: f64,
    step: f64,
    exclusions: &[Vec<Point>],
    site_filter: Option<&SiteFilter<'_>>,
) -> Result<Vec<Position>, OptimizationError> {
    let step = step.max(terrain.resolution_m()).max(1.0);
    let steps = (radius / step).floor() as i64;
    let mut points = Vec::new();
    for i in -steps..=steps {
        for j in -steps..=steps {
            let (dx, dy) = (i as f64 * step, j as f64 * step);
            if (dx != 0.0 || dy != 0.0) && dx * dx + dy * dy <= radius * radius {
                let point = Point {
                    x: center.x + dx,
                    y: center.y + dy,
                };
                if !crate::candidates::excluded(point, exclusions) {
                    points.push(point);
                }
            }
        }
    }
    if points.is_empty() {
        return Ok(Vec::new());
    }
    let ground = crate::candidates::sample(terrain, &points, TerrainLayer::GroundDtm)?;
    let surface = if terrain.has_surface() {
        crate::candidates::sample(terrain, &points, TerrainLayer::SurfaceDom)?
    } else {
        vec![f64::NAN; points.len()]
    };
    let mut positions: Vec<Position> = points
        .into_iter()
        .zip(ground)
        .zip(surface)
        .filter(|((_, ground), _)| ground.is_finite())
        .map(|((point, ground), surface)| (point, ground, surface.is_finite().then_some(surface)))
        .collect();
    if let Some(filter) = site_filter.filter(|_| !positions.is_empty()) {
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

impl Search<'_, '_> {
    fn refinement_positions(&self, center: Point) -> Result<Vec<Position>, OptimizationError> {
        refinement_positions(
            self.terrain,
            center,
            self.settings.refine_radius_m,
            self.settings.refine_step_m,
            &self.exclusions,
            self.site_filter,
        )
    }

    fn movable(&self, index: usize) -> bool {
        let site = &self.sites[index];
        self.generated_candidates
            && site.rf.kind == SiteKind::Candidate
            && site.origin == SiteOrigin::Optimized
            && !site.locked
            && !site.is_required()
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

#[cfg(test)]
mod placement_tests {
    use super::*;
    use crate::{CandidateSettings, Progress};
    use repeaternet_rf_core::RfSettings;
    use std::cell::Cell;

    struct FlatTerrain;

    impl TerrainSource for FlatTerrain {
        fn resolution_m(&self) -> f64 {
            10.0
        }

        fn has_surface(&self) -> bool {
            false
        }

        fn sample(&self, points: &[Point], _: TerrainLayer) -> Result<Vec<f64>, RfError> {
            Ok(vec![0.0; points.len()])
        }
    }

    fn site(id: &str, x: f64, kind: SiteKind, movable: bool) -> PlacementSite {
        let mut rf = Site::new(id, x, 0.0);
        rf.kind = kind;
        rf.antenna_height_m = 10.0;
        PlacementSite { rf, movable }
    }

    fn fixture() -> [PlacementSite; 5] {
        [
            site("A", 0.0, SiteKind::EndpointA, true),
            site("P1", 200.0, SiteKind::Router, true),
            site("R1", 700.0, SiteKind::Candidate, true),
            site("Client", 1600.0, SiteKind::Client, true),
            site("Existing", 1700.0, SiteKind::Router, false),
        ]
    }

    #[test]
    fn fixed_graph_improves_the_weakest_margin_without_moving_anchors() {
        let sites = fixture();
        let settings = CandidateSettings::default();
        let rf = RfSettings::default();
        let links = [(0, 1), (1, 2), (2, 3), (2, 4)];
        let preview = RouteOptimizer::new(&FlatTerrain, &rf, &settings)
            .optimize_placement(
                PlacementRequest {
                    sites: &sites,
                    links: Some(&links),
                    exclusions: &[],
                    radius_m: 250.0,
                },
                SearchHooks::default(),
            )
            .unwrap()
            .unwrap();
        for i in [0, 3, 4] {
            assert_eq!(preview.sites[i], sites[i].rf);
        }
        for i in [1, 2] {
            assert_ne!(preview.sites[i].position, sites[i].rf.position);
            assert!(preview.sites[i].distance_to(&sites[i].rf) <= 250.0);
            assert_eq!(
                preview.sites[i].antenna_height_m,
                sites[i].rf.antenna_height_m
            );
        }
        let before = preview
            .links
            .iter()
            .map(|link| link.before.unwrap().db)
            .reduce(f64::min)
            .unwrap();
        let after = preview
            .links
            .iter()
            .map(|link| link.after.unwrap().db)
            .reduce(f64::min)
            .unwrap();
        assert!(after > before);
        assert_eq!(preview.links.len(), links.len());
    }

    #[test]
    fn range_fallback_keeps_the_initial_edges_and_respects_both_site_filters() {
        let sites = [
            site("A", 0.0, SiteKind::EndpointA, false),
            site("P1", 200.0, SiteKind::Router, true),
            site("B", 500.0, SiteKind::EndpointB, false),
        ];
        let settings = CandidateSettings {
            maximum_link_distance_m: Some(300.0),
            ..Default::default()
        };
        let rf = RfSettings::default();
        let exclusions = [vec![
            Point {
                x: 240.0,
                y: -100.0,
            },
            Point {
                x: 260.0,
                y: -100.0,
            },
            Point { x: 260.0, y: 100.0 },
            Point { x: 240.0, y: 100.0 },
        ]];
        let roads = |points: &[Point]| {
            assert!(points
                .iter()
                .all(|p| !crate::candidates::excluded(*p, &exclusions)));
            Ok(points.iter().map(|p| p.y == 0.0).collect())
        };
        let preview = RouteOptimizer::new(&FlatTerrain, &rf, &settings)
            .with_site_filter(&roads)
            .optimize_placement(
                PlacementRequest {
                    sites: &sites,
                    links: None,
                    exclusions: &exclusions,
                    radius_m: 150.0,
                },
                SearchHooks::default(),
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            preview
                .links
                .iter()
                .map(|link| (link.source, link.target))
                .collect::<Vec<_>>(),
            [(0, 1), (1, 2)]
        );
        assert_eq!(preview.sites[1], sites[1].rf);
    }

    #[test]
    fn cancellation_during_refinement_returns_no_preview() {
        let sites = fixture();
        let rf = RfSettings::default();
        let cancelled = Cell::new(false);
        let cancellation = || {
            if cancelled.get() {
                Cancellation::Discard
            } else {
                Cancellation::Continue
            }
        };
        let mut progress = |progress: Progress| {
            assert_eq!(progress.stage, Stage::RefiningRouterPositions);
            cancelled.set(true);
        };
        let preview = RouteOptimizer::new(&FlatTerrain, &rf, &CandidateSettings::default())
            .optimize_placement(
                PlacementRequest {
                    sites: &sites,
                    links: None,
                    exclusions: &[],
                    radius_m: 250.0,
                },
                SearchHooks {
                    cancellation: Some(&cancellation),
                    progress: Some(&mut progress),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(cancelled.get());
        assert!(preview.is_none());
    }
}
