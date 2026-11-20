use crate::{CandidateSettings, OptimizationError, PlanningSite};
use repeaternet_rf_core::{Point, RfError, Site, TerrainLayer, TerrainSource};
use std::collections::{BTreeMap, VecDeque};

/// Batch eligibility predicate for generated candidates (for example, road proximity).
pub type SiteFilter<'a> = dyn Fn(&[Point]) -> Result<Vec<bool>, OptimizationError> + 'a;

pub fn candidate_grid_spacing(terrain: &dyn TerrainSource, settings: &CandidateSettings) -> f64 {
    let spacing = if settings.refine_radius_m > 0.0 {
        settings.grid_spacing_m.min(settings.refine_radius_m)
    } else {
        settings.grid_spacing_m
    };
    spacing.max(terrain.resolution_m())
}

struct SearchArea {
    start: Point,
    direction: Point,
    length: f64,
    width: f64,
    unrestricted: bool,
    bounds: [f64; 4],
}

impl SearchArea {
    fn new(start: Point, end: Point, settings: &CandidateSettings) -> Self {
        let length = start.distance_to(end);
        let width = settings.corridor_width_m;
        let direction = if length == 0.0 {
            Point { x: 1.0, y: 0.0 }
        } else {
            Point {
                x: (end.x - start.x) / length,
                y: (end.y - start.y) / length,
            }
        };
        let padding = if settings.unrestricted_bounding_area {
            width
        } else {
            width * (direction.x.abs() + direction.y.abs())
        };
        Self {
            start,
            direction,
            length,
            width,
            unrestricted: settings.unrestricted_bounding_area,
            bounds: [
                start.x.min(end.x) - padding,
                start.y.min(end.y) - padding,
                start.x.max(end.x) + padding,
                start.y.max(end.y) + padding,
            ],
        }
    }

    fn along(&self, point: Point) -> f64 {
        (point.x - self.start.x) * self.direction.x + (point.y - self.start.y) * self.direction.y
    }

    fn includes(&self, point: Point) -> bool {
        let along = self.along(point);
        let across = (point.y - self.start.y) * self.direction.x
            - (point.x - self.start.x) * self.direction.y;
        self.unrestricted
            || (along >= -self.width
                && along <= self.length + self.width
                && across.abs() <= self.width)
    }
}

/// Returns only generated sites. Bounds are [min x, min y, max x, max y] in projected metres.
pub fn generate_candidates(
    terrain: &dyn TerrainSource,
    bounds: [f64; 4],
    anchors: [Point; 2],
    settings: &CandidateSettings,
    exclusions: &[Vec<Point>],
    filter: Option<&SiteFilter<'_>>,
    cancelled: &dyn Fn() -> bool,
) -> Result<Vec<PlanningSite>, OptimizationError> {
    generate(
        terrain, bounds, anchors, settings, exclusions, filter, cancelled,
    )
    .map(|(sites, _)| sites)
}

pub(crate) fn generate(
    terrain: &dyn TerrainSource,
    bounds: [f64; 4],
    anchors: [Point; 2],
    settings: &CandidateSettings,
    exclusions: &[Vec<Point>],
    filter: Option<&SiteFilter<'_>>,
    cancelled: &dyn Fn() -> bool,
) -> Result<(Vec<PlanningSite>, [usize; 2]), OptimizationError> {
    if settings.maximum_candidates == 0 || settings.candidates_per_cell == 0 || cancelled() {
        return Ok((Vec::new(), [0; 2]));
    }
    let area = SearchArea::new(anchors[0], anchors[1], settings);
    let spacing = candidate_grid_spacing(terrain, settings);
    if bounds
        .iter()
        .chain(&area.bounds)
        .any(|value| !value.is_finite())
        || !settings.corridor_width_m.is_finite()
        || settings.corridor_width_m < 0.0
        || [
            spacing,
            terrain.resolution_m(),
            settings.grid_spacing_m,
            settings.cell_size_m,
        ]
        .into_iter()
        .any(|value| !value.is_finite() || value <= 0.0)
    {
        return Err(OptimizationError::InvalidInput("Candidate grid requires finite bounds, nonnegative corridor width, and positive spacing and cell size".into()));
    }
    let left = bounds[0].max(area.bounds[0]);
    let bottom = bounds[1].max(area.bounds[1]);
    let right = bounds[2].min(area.bounds[2]);
    let top = bounds[3].min(area.bounds[3]);
    let axis_count =
        |low: f64, high: f64| ((high + spacing * 0.5 - low) / spacing).ceil().max(0.0) as usize;
    let (columns, rows) = (axis_count(left, right), axis_count(bottom, top));
    const BATCH: usize = 4096;
    let total = columns
        .checked_mul(rows)
        .ok_or_else(|| OptimizationError::InvalidInput("Candidate grid is too large".into()))?;
    let mut samples = Vec::new();
    let mut filter_counts = [0; 2];
    let delta = terrain.resolution_m().max(spacing / 4.0);
    for offset in (0..total).step_by(BATCH) {
        if cancelled() {
            return Ok((Vec::new(), filter_counts));
        }
        let points: Vec<_> = (offset..offset.saturating_add(BATCH).min(total))
            .map(|i| Point {
                x: left + (i % columns) as f64 * spacing,
                y: bottom + (i / columns) as f64 * spacing,
            })
            .filter(|&point| area.includes(point))
            .collect();
        if points.is_empty() {
            continue;
        }
        let ground = sample(terrain, &points, TerrainLayer::GroundDtm)?;
        let mut valid: Vec<_> = points
            .into_iter()
            .zip(ground)
            .filter(|(point, elevation)| {
                elevation.is_finite() && (filter.is_none() || !excluded(*point, exclusions))
            })
            .collect();
        let points: Vec<_> = valid.iter().map(|&(point, _)| point).collect();
        if let Some(allowed) = filter.filter(|_| !points.is_empty()) {
            if cancelled() {
                return Ok((Vec::new(), filter_counts));
            }
            let keep = allowed(&points)?;
            if keep.len() != points.len() {
                return Err(OptimizationError::InvalidInput(
                    "Site filter returned the wrong number of points".into(),
                ));
            }
            filter_counts[1] += keep.len();
            filter_counts[0] += keep.iter().filter(|&&value| value).count();
            valid = valid
                .into_iter()
                .zip(keep)
                .filter_map(|(item, keep)| keep.then_some(item))
                .collect();
        }
        if valid.is_empty() {
            continue;
        }
        if cancelled() {
            return Ok((Vec::new(), filter_counts));
        }
        let points: Vec<_> = valid.iter().map(|&(point, _)| point).collect();
        let surface = if terrain.has_surface() {
            sample(terrain, &points, TerrainLayer::SurfaceDom)?
        } else {
            vec![f64::NAN; points.len()]
        };
        let mut slope_points = Vec::with_capacity(4 * points.len());
        for point in points {
            slope_points.extend([
                Point {
                    x: point.x + delta,
                    y: point.y,
                },
                Point {
                    x: point.x - delta,
                    y: point.y,
                },
                Point {
                    x: point.x,
                    y: point.y + delta,
                },
                Point {
                    x: point.x,
                    y: point.y - delta,
                },
            ]);
        }
        let slopes = sample(terrain, &slope_points, TerrainLayer::GroundDtm)?;
        let (slopes, _) = slopes.as_chunks::<4>();
        for (((point, ground), surface), slope) in valid.into_iter().zip(surface).zip(slopes) {
            let slope = ((slope[0] - slope[1]) / (2.0 * delta))
                .hypot((slope[2] - slope[3]) / (2.0 * delta));
            samples.push(GridSite {
                point,
                ground,
                surface: surface.is_finite().then_some(surface),
                slope: if slope.is_finite() { slope } else { 1.0 },
                quality: 0.0,
            });
        }
    }
    let min = samples
        .iter()
        .map(|s| s.ground)
        .fold(f64::INFINITY, f64::min);
    let max = samples
        .iter()
        .map(|s| s.ground)
        .fold(f64::NEG_INFINITY, f64::max);
    let span = (max - min).max(1.0);
    let mut cell_positions = BTreeMap::new();
    let mut cells: Vec<Vec<usize>> = Vec::new();
    for (i, site) in samples.iter_mut().enumerate() {
        if cancelled() {
            return Ok((Vec::new(), filter_counts));
        }
        if filter.is_none() && excluded(site.point, exclusions) {
            continue;
        }
        let obstruction = (site.surface.unwrap_or(site.ground) - site.ground).max(0.0);
        site.quality = (site.ground - min) / span
            - site.slope.min(1.0) * 0.35
            - (obstruction / 20.0).min(1.0) * 0.25;
        let key = (
            (site.point.x / settings.cell_size_m).floor() as i64,
            (site.point.y / settings.cell_size_m).floor() as i64,
        );
        let cell = *cell_positions.entry(key).or_insert_with(|| {
            cells.push(Vec::new());
            cells.len() - 1
        });
        cells[cell].push(i);
    }
    let rank = |&a: &usize, &b: &usize| {
        samples[b]
            .quality
            .total_cmp(&samples[a].quality)
            .then_with(|| samples[b].ground.total_cmp(&samples[a].ground))
            .then_with(|| a.cmp(&b))
    };
    let mut retained = Vec::new();
    for mut cell in cells {
        if cancelled() {
            return Ok((Vec::new(), filter_counts));
        }
        cell.sort_unstable_by(rank);
        retained.extend(cell.into_iter().take(settings.candidates_per_cell));
    }
    let limit = settings.maximum_candidates;
    if retained.len() > limit {
        let (minimum, maximum) = retained
            .iter()
            .map(|&i| area.along(samples[i].point))
            .fold((f64::INFINITY, f64::NEG_INFINITY), |(min, max), value| {
                (min.min(value), max.max(value))
            });
        if area.length == 0.0 || minimum == maximum {
            retained.sort_unstable_by(rank);
            retained.truncate(limit);
        } else {
            let band_count = ((limit as f64).sqrt().ceil() as usize).min(limit);
            let mut bands = vec![Vec::new(); band_count];
            for i in retained.drain(..) {
                let progress = (area.along(samples[i].point) - minimum) / (maximum - minimum);
                bands[((progress * band_count as f64) as usize).min(band_count - 1)].push(i);
            }
            let mut queue: VecDeque<_> = bands
                .into_iter()
                .filter(|band| !band.is_empty())
                .map(|mut band| {
                    band.sort_unstable_by(rank);
                    band.into_iter()
                })
                .collect();
            while retained.len() < limit {
                let mut band = queue.pop_front().expect("more sites than requested");
                retained.push(band.next().expect("nonempty band"));
                if band.len() != 0 {
                    queue.push_back(band);
                }
            }
        }
    }
    let candidates = retained
        .into_iter()
        .enumerate()
        .map(|(index, i)| {
            let site = &samples[i];
            let mut rf = Site::new(format!("C{}", index + 1), site.point.x, site.point.y);
            rf.ground_elevation_m = site.ground;
            rf.surface_elevation_m = site.surface;
            if settings.optimize_heights {
                rf.antenna_height_m = settings.minimum_router_height_m;
            }
            PlanningSite {
                terrain_slope: site.slope,
                site_quality: site.quality,
                ..rf.into()
            }
        })
        .collect();
    Ok((candidates, filter_counts))
}

struct GridSite {
    point: Point,
    ground: f64,
    surface: Option<f64>,
    slope: f64,
    quality: f64,
}

fn excluded(point: Point, polygons: &[Vec<Point>]) -> bool {
    polygons.iter().any(|polygon| {
        let mut inside = false;
        for (&a, &b) in polygon.iter().zip(polygon.iter().cycle().skip(1)) {
            if (a.y > point.y) != (b.y > point.y)
                && point.x < (b.x - a.x) * (point.y - a.y) / (b.y - a.y) + a.x
            {
                inside = !inside;
            }
        }
        inside
    })
}

pub(crate) fn sample(
    terrain: &dyn TerrainSource,
    points: &[Point],
    layer: TerrainLayer,
) -> Result<Vec<f64>, RfError> {
    let elevations = terrain.sample(points, layer)?;
    if elevations.len() != points.len() {
        return Err(RfError::TerrainSampleCount {
            expected: points.len(),
            actual: elevations.len(),
        });
    }
    Ok(elevations)
}
