use crate::candidates::{cell_key, AccessPool};
use crate::{CandidateSettings, OptimizationError, PlanningSite};
use repeaternet_rf_core::{Point, Site};
use std::collections::{BTreeMap, BTreeSet};

const CHECKS_PER_CLIENT: usize = 4_096;
const TOTAL_CHECKS: usize = 16_384;
const RETAINED_ACCESS_TARGET: usize = 6;
const PROMOTIONS_PER_CLIENT: usize = 12;
const PROMOTION_LIMIT: usize = 32;

type Cell = (i64, i64);
type Position = (u64, u64);

/// Preserve RF-valid client access on the existing terrain grid. Fixed and supplied
/// sites are outside the generated pool, and only a completed pass changes the pool.
#[cfg(test)]
pub(crate) fn recover(
    generated: &mut Vec<PlanningSite>,
    dropped: AccessPool,
    fixed: &[PlanningSite],
    settings: &CandidateSettings,
    search_height_m: f64,
    valid_link: &dyn Fn(&Site, &Site) -> Result<bool, OptimizationError>,
    cancelled: &dyn Fn() -> bool,
) -> Result<usize, OptimizationError> {
    recover_with_refinement(
        generated,
        dropped,
        fixed,
        settings,
        search_height_m,
        valid_link,
        &mut |_| Ok(Vec::new()),
        cancelled,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn recover_with_refinement(
    generated: &mut Vec<PlanningSite>,
    dropped: AccessPool,
    fixed: &[PlanningSite],
    settings: &CandidateSettings,
    search_height_m: f64,
    valid_link: &dyn Fn(&Site, &Site) -> Result<bool, OptimizationError>,
    refine: &mut dyn FnMut(Point) -> Result<Vec<PlanningSite>, OptimizationError>,
    cancelled: &dyn Fn() -> bool,
) -> Result<usize, OptimizationError> {
    if settings.candidates_per_cell == 0 || settings.maximum_solution_routers == 0 || cancelled() {
        return Ok(0);
    }
    let clients = fixed.iter().filter(|site| site.is_client());
    let client_count = clients.clone().count();
    if client_count == 0 {
        return Ok(0);
    }
    let checks_per_client = CHECKS_PER_CLIENT.min(TOTAL_CHECKS / client_count);
    let promotion_limit = PROMOTION_LIMIT;
    let promotion_quota = PROMOTIONS_PER_CLIENT.min(promotion_limit.div_ceil(client_count));
    let fixed_positions: BTreeSet<_> = fixed
        .iter()
        .map(|site| position(site.rf.position))
        .collect();
    let cell = |point| -> Cell { cell_key(point, settings.cell_size_m) };
    let mut protected = BTreeSet::new();
    let mut options = Vec::new();
    for client in clients {
        let distance = |point| {
            let distance = client.rf.position.distance_to(point);
            (distance > 0.0
                && settings
                    .maximum_link_distance_m
                    .is_none_or(|limit| distance <= limit))
            .then_some(distance)
        };
        let mut checks = 0;
        let mut retained_access = 0;
        // Keep the recovery pass small when the retained pool already offers access.
        for (index, site) in generated.iter().enumerate() {
            if cancelled() {
                return Ok(0);
            }
            if retained_access == RETAINED_ACCESS_TARGET || checks == checks_per_client / 2 {
                break;
            }
            if distance(site.rf.position).is_none() {
                continue;
            }
            let mut router = site.rf.clone();
            router.antenna_height_m = search_height_m;
            checks += 1;
            if valid_link(&client.rf, &router)? {
                protected.insert(index);
                retained_access += 1;
            }
        }
        if retained_access == RETAINED_ACCESS_TARGET || checks == checks_per_client {
            continue;
        }

        let mut nearest = Vec::new();
        let mut higher = Vec::new();
        let mut previous_cell = None;
        let mut cell_samples = 0;
        for (index, site) in dropped.iter().enumerate() {
            if cancelled() {
                return Ok(0);
            }
            if let Some(distance) = distance(site.point) {
                if fixed_positions.contains(&position(site.point)) {
                    continue;
                }
                nearest.push((index, distance));
                // The pool is grouped by cell and already ranked by terrain quality.
                let current_cell = cell(site.point);
                if previous_cell != Some(current_cell) {
                    previous_cell = Some(current_cell);
                    cell_samples = 0;
                }
                if cell_samples < 4 {
                    higher.push((index, site.quality - distance / 50_000.0));
                    cell_samples += 1;
                }
            }
        }
        let remaining = checks_per_client - checks;
        let remaining = if retained_access == 0 {
            remaining.saturating_sub(remaining.min(289))
        } else {
            remaining
        };
        keep_best(&mut nearest, remaining.div_ceil(2), |left, right| {
            left.1.total_cmp(&right.1).then(left.0.cmp(&right.0))
        });
        let local: BTreeSet<_> = nearest.iter().map(|&(index, _)| index).collect();
        higher.retain(|&(index, _)| !local.contains(&index));
        keep_best(&mut higher, remaining - nearest.len(), |left, right| {
            right.1.total_cmp(&left.1).then(left.0.cmp(&right.0))
        });

        let mut visible = Vec::new();
        for (index, _) in nearest.into_iter().chain(higher) {
            if cancelled() {
                return Ok(0);
            }
            let mut site = dropped[index].planning_site(format!("C-access-{}", index + 1));
            site.rf.antenna_height_m = search_height_m;
            checks += 1;
            if valid_link(&client.rf, &site.rf)? {
                visible.push(site);
            }
        }
        if visible.is_empty() && retained_access == 0 && checks < checks_per_client && !cancelled()
        {
            for mut site in refine(client.rf.position)? {
                if cancelled() {
                    return Ok(0);
                }
                if checks >= checks_per_client {
                    break;
                }
                if distance(site.rf.position).is_none()
                    || fixed_positions.contains(&position(site.rf.position))
                {
                    continue;
                }
                site.rf.antenna_height_m = search_height_m;
                checks += 1;
                if valid_link(&client.rf, &site.rf)? {
                    visible.push(site);
                }
            }
        }
        visible.sort_unstable_by(|left, right| {
            right
                .site_quality
                .total_cmp(&left.site_quality)
                .then_with(|| left.rf.id.cmp(&right.rf.id))
        });
        let mut covered = BTreeSet::new();
        let (distinct, nearby): (Vec<_>, Vec<_>) = visible
            .into_iter()
            .partition(|site| covered.insert(cell(site.rf.position)));
        options.push(distinct.into_iter().chain(nearby).take(promotion_quota));
    }

    let mut planned = Vec::new();
    let mut occupied = fixed_positions;
    let mut cell_counts = BTreeMap::new();
    for site in generated.iter() {
        occupied.insert(position(site.rf.position));
        *cell_counts.entry(cell(site.rf.position)).or_insert(0) += 1;
    }
    let mut replaceable: Vec<_> = (0..generated.len())
        .filter(|index| !protected.contains(index))
        .collect();
    replaceable.sort_unstable_by(|&left, &right| {
        generated[left]
            .site_quality
            .total_cmp(&generated[right].site_quality)
            .then(left.cmp(&right))
    });
    for _ in 0..promotion_quota {
        for site in options.iter_mut().filter_map(Iterator::next) {
            if cancelled() {
                return Ok(0);
            }
            if planned.len() == promotion_limit {
                break;
            }
            let point = site.rf.position;
            if occupied.contains(&position(point)) {
                continue;
            }
            let key = cell(point);
            let cell_full =
                cell_counts.get(&key).copied().unwrap_or(0) >= settings.candidates_per_cell;
            // A full cell swaps its weakest unprotected site; otherwise the site is added.
            let replacement = if !cell_full {
                None
            } else {
                let Some(slot) = replaceable
                    .iter()
                    .position(|&index| cell(generated[index].rf.position) == key)
                else {
                    continue;
                };
                let index = replaceable.remove(slot);
                cell_counts
                    .entry(cell(generated[index].rf.position))
                    .and_modify(|count| *count -= 1);
                Some(index)
            };
            occupied.insert(position(point));
            *cell_counts.entry(key).or_insert(0) += 1;
            planned.push((replacement, site));
        }
    }

    if cancelled() {
        return Ok(0);
    }
    let promoted = planned.len();
    for (replacement, site) in planned {
        if let Some(index) = replacement {
            generated[index] = site;
        } else {
            generated.push(site);
        }
    }
    Ok(promoted)
}

fn position(point: Point) -> Position {
    ((point.x + 0.0).to_bits(), (point.y + 0.0).to_bits())
}

fn keep_best<T>(values: &mut Vec<T>, limit: usize, compare: impl Fn(&T, &T) -> std::cmp::Ordering) {
    if values.len() > limit {
        values.select_nth_unstable_by(limit, compare);
        values.truncate(limit);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::candidates::{generate_candidates, generate_with_access_pool};
    use repeaternet_rf_core::{RfError, SiteKind, TerrainLayer, TerrainSource};
    use std::cell::Cell as Counter;

    struct Terrain;

    impl TerrainSource for Terrain {
        fn resolution_m(&self) -> f64 {
            50.0
        }

        fn has_surface(&self) -> bool {
            true
        }

        fn sample(&self, points: &[Point], layer: TerrainLayer) -> Result<Vec<f64>, RfError> {
            let elevation = match layer {
                TerrainLayer::GroundDtm => 100.0,
                TerrainLayer::SurfaceDom => 104.0,
            };
            Ok(vec![elevation; points.len()])
        }
    }

    fn terminal(id: &str, x: f64) -> PlanningSite {
        let mut rf = Site::new(id, x, 0.0);
        rf.kind = SiteKind::Client;
        rf.antenna_height_m = 7.0;
        rf.into()
    }

    fn settings() -> CandidateSettings {
        CandidateSettings {
            corridor_width_m: 0.0,
            grid_spacing_m: 100.0,
            cell_size_m: 500.0,
            candidates_per_cell: 1,
            maximum_link_distance_m: Some(450.0),
            optimize_heights: true,
            ..CandidateSettings::default()
        }
    }

    #[test]
    fn promotes_eligible_access_with_search_height_and_preserves_existing_access() {
        let mut fixed_router: PlanningSite = Site::new("fixed", 100.0, 0.0).into();
        fixed_router.rf.antenna_height_m = 23.0;
        fixed_router.height_override = true;
        let fixed = [terminal("A", 0.0), terminal("B", 900.0), fixed_router];
        let anchors = [fixed[0].rf.position, fixed[1].rf.position];
        let bounds = [0.0, 0.0, 900.0, 0.0];
        let excluded = vec![
            Point { x: 390.0, y: -1.0 },
            Point { x: 410.0, y: -1.0 },
            Point { x: 410.0, y: 1.0 },
            Point { x: 390.0, y: 1.0 },
        ];
        let exclusions = [excluded];
        let filter = |points: &[Point]| Ok(points.iter().map(|p| p.x != 300.0).collect());
        let (mut generated, counts, dropped) = generate_with_access_pool(
            &Terrain,
            bounds,
            anchors,
            &settings(),
            &exclusions,
            Some(&filter),
            &|| false,
        )
        .unwrap();
        assert_eq!(counts, [8, 9]);
        assert_eq!(
            generated,
            generate_candidates(
                &Terrain,
                bounds,
                anchors,
                &settings(),
                &exclusions,
                Some(&filter),
                &|| false,
            )
            .unwrap()
        );
        assert_eq!(generated[0].rf.position.x, 0.0);
        let existing = generated[1].clone();
        let evaluate = |client: &Site, router: &Site| {
            assert_eq!(client.antenna_height_m, 7.0);
            assert_eq!(router.antenna_height_m, 10.0);
            assert_eq!(router.ground_elevation_m, 100.0);
            assert_eq!(router.surface_elevation_m, Some(104.0));
            assert!(client.distance_to(router) <= 450.0);
            let x = router.position.x;
            assert!(![100.0, 300.0, 400.0].contains(&x));
            Ok((client.id == "A" && x == 200.0) || (client.id == "B" && x == 500.0))
        };
        assert_eq!(
            recover(
                &mut generated,
                dropped,
                &fixed,
                &settings(),
                settings().maximum_router_height_m,
                &evaluate,
                &|| false,
            )
            .unwrap(),
            1
        );
        assert_eq!(generated.len(), 2);
        assert_eq!(generated[0].rf.position.x, 200.0);
        assert_eq!(generated[0].rf.antenna_height_m, 10.0);
        assert_eq!(generated[1], existing);
        assert_eq!(fixed[2].rf.antenna_height_m, 23.0);
    }

    #[test]
    fn isolated_client_can_use_a_finer_site_when_the_coarse_grid_is_exhausted() {
        let client = terminal("A", 0.0);
        let mut generated = vec![Site::new("C1", 100.0, 0.0).into()];
        let settings = settings();
        let promoted = recover_with_refinement(
            &mut generated,
            Vec::new(),
            &[client],
            &settings,
            10.0,
            &|_, router| Ok(router.position.x == 50.0 && router.antenna_height_m == 10.0),
            &mut |point| {
                assert_eq!(point.x, 0.0);
                Ok(vec![Site::new("fine", 50.0, 0.0).into()])
            },
            &|| false,
        )
        .unwrap();
        assert_eq!(promoted, 1);
        assert_eq!(generated.len(), 1);
        assert_eq!(generated[0].rf.position.x, 50.0);
    }

    #[test]
    fn cancellation_during_rf_checks_preserves_the_original_pool() {
        let fixed = [terminal("A", -100.0)];
        let (mut generated, _, dropped) = generate_with_access_pool(
            &Terrain,
            [0.0, 0.0, 400.0, 0.0],
            [Point { x: 0.0, y: 0.0 }, Point { x: 400.0, y: 0.0 }],
            &settings(),
            &[],
            None,
            &|| false,
        )
        .unwrap();
        let initial = generated.clone();
        let stopped = Counter::new(false);
        let evaluate = |_: &Site, router: &Site| {
            let visible = router.position.x > 0.0;
            stopped.set(visible);
            Ok(visible)
        };
        assert_eq!(
            recover(
                &mut generated,
                dropped,
                &fixed,
                &settings(),
                10.0,
                &evaluate,
                &|| stopped.get(),
            )
            .unwrap(),
            0
        );
        assert!(stopped.get());
        assert_eq!(generated, initial);
    }

    #[test]
    fn rf_work_stays_bounded_for_a_dense_grid() {
        let settings = CandidateSettings {
            grid_spacing_m: 50.0,
            maximum_link_distance_m: None,
            ..settings()
        };
        let fixed = [terminal("A", -50.0)];
        let points = [
            Point { x: 0.0, y: 0.0 },
            Point {
                x: 300_000.0,
                y: 0.0,
            },
        ];
        let (mut generated, _, dropped) = generate_with_access_pool(
            &Terrain,
            [0.0, 0.0, 300_000.0, 0.0],
            points,
            &settings,
            &[],
            None,
            &|| false,
        )
        .unwrap();
        assert!(dropped.len() > CHECKS_PER_CLIENT);
        let checks = Counter::new(0);
        let evaluate = |_: &Site, _: &Site| {
            checks.set(checks.get() + 1);
            Ok(false)
        };
        assert_eq!(
            recover(
                &mut generated,
                dropped,
                &fixed,
                &settings,
                10.0,
                &evaluate,
                &|| false,
            )
            .unwrap(),
            0
        );
        assert!((CHECKS_PER_CLIENT / 2..=CHECKS_PER_CLIENT).contains(&checks.get()));
    }
}
