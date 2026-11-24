use std::{
    collections::{BTreeMap, HashSet},
    io::Write,
};

use repeaternet_rf_core::{
    EvaluationOptions, HeightReference, LinkEvaluator, LinkResult, Point, RadioBudget, RfError,
    RfSettings, Site, SiteKind, TerrainLayer, TerrainSource,
};
use repeaternet_terrain::{
    profile::{bounded_profile_step, ProfileSampling},
    Transformer,
};
use serde::{Deserialize, Serialize};

use crate::{
    area_bounds_wgs84,
    cache::{check_cancelled, CacheContext, Metrics, RadioKey},
    grid::{is_endpoint, make_grid},
    invalid,
    parallel::{for_each_ordered, for_each_serial, worker_count},
    CoverageArea, CoverageCell, CoverageError, CoverageGrid, CoverageMetricsCache, CoverageMode,
    CoverageSettings, CoverageSourceResult, CoverageState, MapBounds, Rejection, MAX_SOURCES,
};

/// Preserve certified solution order, including mesh clients, and append optional A/B endpoints.
pub fn select_sources(
    active_sites: &[Site],
    candidates: &[Site],
    settings: &CoverageSettings,
) -> Result<Vec<Site>, CoverageError> {
    settings.validate()?;
    let mut sources: Vec<_> = active_sites
        .iter()
        .filter(|site| !matches!(site.kind, SiteKind::EndpointA | SiteKind::EndpointB))
        .collect();
    let mut ids: HashSet<_> = sources.iter().map(|site| site.id.as_str()).collect();
    if settings.source_ids.iter().any(|id| {
        !sources
            .iter()
            .any(|site| site.id == *id && site.kind != SiteKind::Client)
    }) {
        return Err(invalid(
            "Selected coverage sources are not in this route alternative; refresh the source list",
        ));
    }
    if settings.include_endpoints {
        for site in candidates {
            if matches!(site.kind, SiteKind::EndpointA | SiteKind::EndpointB)
                && ids.insert(&site.id)
            {
                sources.push(site);
            }
        }
    }
    if !settings.source_ids.is_empty() {
        sources.retain(|site| settings.source_ids.contains(&site.id) || is_endpoint(site.kind));
    }
    if sources.is_empty() {
        return Err(invalid(
            "This route has no radio sources; include endpoints to preview their coverage",
        ));
    }
    if sources.len() > settings.maximum_sources {
        return Err(invalid(format!(
            "Select no more than {} radio sources",
            settings.maximum_sources
        )));
    }
    Ok(sources.into_iter().cloned().collect())
}

/// Build an adjusted snapshot without altering the baseline sites or their height reference.
pub fn apply_height_overrides(
    sources: &[Site],
    overrides: &BTreeMap<String, f64>,
) -> Result<Vec<Site>, CoverageError> {
    if overrides.len() > MAX_SOURCES {
        return Err(invalid(
            "Height scenarios may change at most 64 selected router sources",
        ));
    }
    for (id, height) in overrides {
        if !sources
            .iter()
            .any(|site| site.id == *id && !is_endpoint(site.kind))
        {
            return Err(invalid(
                "Height scenarios may change only selected router sources",
            ));
        }
        if !(0.1..=500.0).contains(height) {
            return Err(invalid(
                "Scenario antenna height must be between 0.1 and 500 m AGL",
            ));
        }
    }
    let mut adjusted = sources.to_vec();
    for source in &mut adjusted {
        if let Some(&height) = overrides.get(&source.id) {
            source.antenna_height_m = height;
        }
    }
    Ok(adjusted)
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CoverageEstimate {
    pub source_count: usize,
    pub source_ids: Vec<String>,
    pub requested_cells: usize,
    pub requested_area_cells: usize,
    pub outside_area_cells: usize,
    pub effective_cell_size_m: f64,
    pub terrain_available_cells: usize,
    pub unknown_cells: usize,
    pub planned_evaluations: usize,
    pub mode: CoverageMode,
    pub surface_available: bool,
    pub area_bounds_wgs84: MapBounds,
}

pub type CellChunkSink<'a> = dyn FnMut(&[CoverageCell]) -> Result<(), CoverageError> + 'a;

pub struct RunOptions<'a> {
    pub cancelled: &'a dyn Fn() -> bool,
    pub progress: Option<&'a mut dyn FnMut(usize, usize)>,
    pub on_chunk: Option<&'a mut CellChunkSink<'a>>,
    pub chunk_size: usize,
}

impl Default for RunOptions<'_> {
    fn default() -> Self {
        Self {
            cancelled: &|| false,
            progress: None,
            on_chunk: None,
            chunk_size: 128,
        }
    }
}

/// A reusable terrain, RF and projection context. The transform must match the terrain CRS.
pub struct CoverageEvaluator<'a, T: TerrainSource + ?Sized> {
    pub(crate) terrain: &'a T,
    pub(crate) rf: &'a RfSettings,
    pub(crate) transform: &'a Transformer,
    pub(crate) parallel_workers: usize,
    cache: Option<CacheContext<'a>>,
}

impl<'a, T: TerrainSource + ?Sized> CoverageEvaluator<'a, T> {
    pub fn new(terrain: &'a T, rf: &'a RfSettings, transform: &'a Transformer) -> Self {
        Self {
            terrain,
            rf,
            transform,
            parallel_workers: 0,
            cache: None,
        }
    }

    /// Limit workers for grid and road-sample evaluation. Zero selects automatically.
    /// One evaluates on the calling thread.
    pub fn with_parallel_workers(mut self, workers: usize) -> Self {
        self.parallel_workers = workers;
        self
    }

    /// Attach a cache for this project and terrain snapshot. Callers must
    /// supply a different revision when the project or terrain data changes.
    pub fn with_cache(
        mut self,
        cache: &'a CoverageMetricsCache,
        terrain_revision: [u8; 32],
    ) -> Self {
        self.cache = Some(CacheContext::new(
            cache,
            &terrain_revision,
            self.terrain,
            self.rf,
        ));
        self
    }

    pub fn estimate(
        &self,
        sources: &[Site],
        settings: &CoverageSettings,
        area: &CoverageArea,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<CoverageEstimate, CoverageError> {
        let (grid, cells) = make_grid(
            self.terrain,
            self.transform,
            area,
            settings,
            sources.len(),
            cancelled,
        )?;
        let terrain_cells = self
            .ground_samples(&cells, cancelled)?
            .into_iter()
            .filter(|value| value.is_finite())
            .count();
        Ok(CoverageEstimate {
            source_count: sources.len(),
            source_ids: sources.iter().map(|site| site.id.clone()).collect(),
            requested_cells: grid.requested_cells,
            requested_area_cells: grid.inside_area_cells,
            outside_area_cells: grid.outside_area_cells,
            effective_cell_size_m: grid.effective_cell_size_m,
            terrain_available_cells: terrain_cells,
            unknown_cells: grid.inside_area_cells - terrain_cells,
            planned_evaluations: grid.inside_area_cells * sources.len(),
            mode: settings.mode,
            surface_available: grid.surface_available,
            area_bounds_wgs84: area_bounds_wgs84(grid.bounds, self.transform)?,
        })
    }

    /// Cancellation during grid/terrain preparation returns Cancelled. Once evaluation
    /// begins, cancellation returns the completed prefix and flushes any pending full cells.
    /// Workers may finish out of order. Callbacks run in cell order on the calling thread.
    pub fn calculate(
        &self,
        sources: &[Site],
        settings: &CoverageSettings,
        area: &CoverageArea,
        options: RunOptions<'_>,
    ) -> Result<CoverageGrid, CoverageError>
    where
        T: Sync,
    {
        let (mut run, cells) = self.prepare_grid(sources.len(), settings, area, options)?;
        let workers = worker_count(
            self.parallel_workers,
            cells.len(),
            run.grid.terrain_available_cells * sources.len(),
        );
        for_each_ordered(
            cells,
            workers,
            run.options.cancelled,
            self.cell_evaluator(sources, settings),
            |cell| run.accept(cell),
        )?;
        run.finish()
    }

    /// Calculate coverage on the calling thread, including for non-`Sync` terrain.
    /// Uses the same chunk, progress and cancellation semantics as `calculate`.
    pub fn calculate_serial(
        &self,
        sources: &[Site],
        settings: &CoverageSettings,
        area: &CoverageArea,
        options: RunOptions<'_>,
    ) -> Result<CoverageGrid, CoverageError> {
        let (mut run, cells) = self.prepare_grid(sources.len(), settings, area, options)?;
        for_each_serial(
            cells,
            run.options.cancelled,
            self.cell_evaluator(sources, settings),
            |cell| run.accept(cell),
        )?;
        run.finish()
    }

    fn prepare_grid<'run>(
        &self,
        source_count: usize,
        settings: &CoverageSettings,
        area: &CoverageArea,
        options: RunOptions<'run>,
    ) -> Result<(GridRun<'run>, impl ExactSizeIterator<Item = CellInput>), CoverageError> {
        if options.chunk_size == 0 {
            return Err(invalid("Coverage chunk size must be positive"));
        }
        let (mut grid, cells) = make_grid(
            self.terrain,
            self.transform,
            area,
            settings,
            source_count,
            options.cancelled,
        )?;
        let samples = self.ground_samples(&cells, options.cancelled)?;
        grid.terrain_available_cells = samples.iter().filter(|value| value.is_finite()).count();
        let mut ground = samples.into_iter();
        let cells = cells.into_iter().map(move |cell| {
            let elevation = (cell.state != CoverageState::OutsideArea)
                .then(|| ground.next().expect("ground sample for every inside cell"));
            (cell, elevation)
        });
        let pending = Vec::with_capacity(options.chunk_size.min(grid.requested_cells));
        Ok((
            GridRun {
                grid,
                pending,
                options,
            },
            cells,
        ))
    }

    fn cell_evaluator<'run>(
        &'run self,
        sources: &'run [Site],
        settings: &'run CoverageSettings,
    ) -> impl Fn(CellInput, &dyn Fn() -> bool) -> Result<CoverageCell, CoverageError> + 'run {
        let client = settings.client.as_radio_budget();
        move |(mut cell, elevation), cancelled| {
            if let Some(elevation) = elevation {
                if elevation.is_finite() {
                    let target = self.client_site(
                        format!("coverage-cell-{}", cell.index),
                        cell.position(),
                        elevation,
                        None,
                        &client,
                    )?;
                    self.evaluate_cell(&mut cell, sources, &target, settings, &client, cancelled)?;
                } else {
                    cell.state = CoverageState::UnknownTerrain;
                }
            }
            Ok(cell)
        }
    }

    fn ground_samples(
        &self,
        cells: &[CoverageCell],
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Vec<f64>, CoverageError> {
        let points: Vec<_> = cells
            .iter()
            .filter(|cell| cell.state != CoverageState::OutsideArea)
            .map(CoverageCell::position)
            .collect();
        let mut ground = Vec::with_capacity(points.len());
        for chunk in points.chunks(512) {
            if cancelled() {
                return Err(CoverageError::Cancelled);
            }
            ground.extend(self.sample(chunk, TerrainLayer::GroundDtm)?);
        }
        if cancelled() {
            return Err(CoverageError::Cancelled);
        }
        Ok(ground)
    }

    pub(crate) fn sample(
        &self,
        points: &[Point],
        layer: TerrainLayer,
    ) -> Result<Vec<f64>, CoverageError> {
        let samples = self.terrain.sample(points, layer)?;
        if samples.len() != points.len() {
            return Err(RfError::TerrainSampleCount {
                expected: points.len(),
                actual: samples.len(),
            }
            .into());
        }
        Ok(samples)
    }

    fn evaluate_cell(
        &self,
        cell: &mut CoverageCell,
        sources: &[Site],
        target: &Site,
        settings: &CoverageSettings,
        client: &RadioBudget,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(), CoverageError> {
        cell.sources.reserve(sources.len());
        for source in sources {
            if cancelled() {
                return Err(CoverageError::Cancelled);
            }
            let (result, _) = self.evaluate_source(
                source,
                target,
                EvaluationOptions {
                    sample_step_m: Some(settings.profile_step_m),
                    include_profile: false,
                    target_radio: Some(client),
                    ..EvaluationOptions::default()
                },
                settings.maximum_profile_samples,
                cancelled,
            )?;
            match result.rejection {
                Some(Rejection::UnknownTerrain) => cell.unknown_sources += 1,
                Some(Rejection::ProfileSampleLimit) => cell.unresolved_sources += 1,
                _ => {}
            }
            if let Some(margin) = result.usable_margin(settings.mode) {
                cell.source_count += 1;
                if cell.best_margin_db.is_none_or(|best| margin > best) {
                    cell.best_margin_db = Some(margin);
                    cell.best_source_id = Some(source.id.clone());
                }
            }
            cell.sources.push(result);
        }
        if cancelled() {
            return Err(CoverageError::Cancelled);
        }
        cell.state = if cell.source_count > 0 {
            CoverageState::Covered
        } else if cell.unresolved_sources > 0 {
            CoverageState::Unresolved
        } else if cell.unknown_sources > 0 {
            CoverageState::UnknownTerrain
        } else {
            CoverageState::Uncovered
        };
        Ok(())
    }

    pub(crate) fn evaluate_source(
        &self,
        source: &Site,
        target: &Site,
        mut options: EvaluationOptions<'_>,
        maximum_samples: usize,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(CoverageSourceResult, Option<LinkResult>), CoverageError> {
        let distance = source.distance_to(target);
        let sampling = bounded_profile_step(
            distance,
            options
                .sample_step_m
                .unwrap_or_else(|| self.terrain.resolution_m()),
            self.terrain.resolution_m(),
            maximum_samples,
        )?;
        let mut result = CoverageSourceResult {
            source_id: source.id.clone(),
            ..CoverageSourceResult::default()
        };
        if !sampling.fits() {
            result.rejection = Some(Rejection::ProfileSampleLimit);
            result.rejection_detail = Some(profile_limit_detail(sampling));
            return Ok((result, None));
        }
        if distance == 0.0 {
            // The Python evaluator rejects coincident source and client points as unknown.
            result.rejection = Some(Rejection::UnknownTerrain);
            return Ok((result, None));
        }
        options.sample_step_m = Some(sampling.step_m);
        let (metrics, profile) = if options.include_profile {
            let profile = self.evaluate_link(source, target, options, cancelled)?;
            let metrics = profile
                .as_ref()
                .map(|link| Metrics::new(link, self.rf.validation_mode));
            (metrics, profile)
        } else {
            (
                self.evaluate_metrics(source, target, options, cancelled)?,
                None,
            )
        };
        if let Some(metrics) = metrics {
            metrics.write_source(&mut result)?;
        } else {
            result.rejection = Some(Rejection::UnknownTerrain);
        }
        Ok((result, profile))
    }

    pub(crate) fn evaluate_metrics(
        &self,
        source: &Site,
        target: &Site,
        options: EvaluationOptions<'_>,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Option<Metrics>, CoverageError> {
        debug_assert!(!options.include_profile);
        let compute = || {
            self.evaluate_link(source, target, options, cancelled)
                .map(|link| link.map(|link| Metrics::new(&link, self.rf.validation_mode)))
        };
        if let (Some(context), Some(step), Some(radio), None) = (
            &self.cache,
            options.sample_step_m,
            RadioKey::new(options.target_radio),
            options.source_radio,
        ) {
            return context.cache.get_or_compute(
                context.key(source, target, step, radio),
                cancelled,
                compute,
            );
        }
        compute()
    }

    fn evaluate_link(
        &self,
        source: &Site,
        target: &Site,
        options: EvaluationOptions<'_>,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Option<LinkResult>, CoverageError> {
        check_cancelled(cancelled)?;
        let result = LinkEvaluator::new(self.terrain, self.rf).evaluate(
            source,
            target,
            EvaluationOptions {
                step_allowance: true,
                ..options
            },
        );
        check_cancelled(cancelled)?;
        match result {
            Ok(link) => Ok(Some(link)),
            Err(RfError::MissingTerrain) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }
    pub(crate) fn client_site(
        &self,
        id: String,
        position: Point,
        ground: f64,
        surface: Option<f64>,
        client: &RadioBudget,
    ) -> Result<Site, CoverageError> {
        let mut site = Site {
            id,
            position,
            kind: SiteKind::Client,
            ground_elevation_m: ground,
            surface_elevation_m: surface,
            antenna_height_m: client.antenna.height_agl_m,
            height_reference: HeightReference::GroundDtm,
            terminal_clutter: self.rf.antenna_settings(SiteKind::Client).terminal_clutter,
            clutter_height_m: None,
        };
        site.sample_terminal_clutter(self.terrain)?;
        Ok(site)
    }
}

pub(crate) fn profile_limit_detail(sampling: ProfileSampling) -> String {
    fn grouped(value: usize) -> String {
        let digits = value.to_string();
        let mut result = String::new();
        for (idx, digit) in digits.chars().enumerate() {
            if idx != 0 && (digits.len() - idx).is_multiple_of(3) {
                result.push(',');
            }
            result.push(digit);
        }
        result
    }
    format!(
        "This link requires {} terrain-profile samples at {} m spacing; the configured maximum is {}. Increase the profile step explicitly or raise the profile sample limit.",
        grouped(sampling.required_samples), sampling.step_m, grouped(sampling.maximum_samples)
    )
}

type CellInput = (CoverageCell, Option<f64>);

struct GridRun<'a> {
    grid: CoverageGrid,
    pending: Vec<CoverageCell>,
    options: RunOptions<'a>,
}

impl GridRun<'_> {
    fn accept(&mut self, cell: CoverageCell) -> Result<(), CoverageError> {
        match cell.state {
            CoverageState::Covered | CoverageState::Uncovered => self.grid.evaluated_cells += 1,
            CoverageState::UnknownTerrain => self.grid.unknown_cells += 1,
            CoverageState::Unresolved => self.grid.unresolved_cells += 1,
            _ => {}
        }
        self.grid.unresolved_source_evaluations += cell.unresolved_sources;
        self.grid.completed_cells += 1;
        self.pending.push(cell);
        if self.pending.len() == self.options.chunk_size {
            self.publish()?;
        }
        let done = self.grid.completed_cells;
        let total = self.grid.requested_cells;
        if done.is_multiple_of(20) || done == total {
            if let Some(report) = &mut self.options.progress {
                report(done, total);
            }
        }
        Ok(())
    }

    fn finish(mut self) -> Result<CoverageGrid, CoverageError> {
        self.publish()?;
        Ok(self.grid)
    }

    fn publish(&mut self) -> Result<(), CoverageError> {
        if self.pending.is_empty() {
            return Ok(());
        }
        if let Some(sink) = &mut self.options.on_chunk {
            sink(&self.pending)?;
            self.pending.clear();
        } else {
            self.grid.cells.append(&mut self.pending);
        }
        Ok(())
    }
}

/// Write the Python store's JSONL format, one array of completed cells per line.
pub fn write_cell_chunk(
    mut output: impl Write,
    cells: &[CoverageCell],
) -> Result<(), CoverageError> {
    serde_json::to_writer(&mut output, cells)?;
    output.write_all(b"\n")?;
    Ok(())
}
