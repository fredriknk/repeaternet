use crate::cache::CacheKey;
use crate::candidates::{
    candidate_grid_spacing, dense_corridor_half_width_m, generate_with_access_pool, sample,
};
use crate::mesh_paint::{self, PaintCertification, SiteMasks, MAX_PAINT_SITES};
use crate::parallel::{for_each_batched, worker_count};
use crate::screening::screened_pairs;
use crate::topology::solve_graph;
use crate::PaintObjective;
use crate::{
    backbone_site_indices, link_is_clear, pair, select_active_solution_index, CandidateSettings,
    InfrastructurePolicy, LinkMetricsCache, NetworkSolution, OptimizationError,
    OptimizationPriority, PlanningSite, RoutingGraph, SiteFilter, SiteOrigin,
};
use repeaternet_rf_core::{
    calibration::CERTIFICATION_MAX_STEP_M, EvaluationOptions, LinkEvaluator, LinkResult, Point,
    RfError, RfSettings, Site, SiteKind, TerminalClutter, TerrainLayer, TerrainSource,
};
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

#[path = "access.rs"]
mod access;
#[path = "bridges.rs"]
mod bridges;
#[path = "path_search.rs"]
mod path_search;
#[path = "refine.rs"]
mod refine;
pub use refine::{
    PlacementLink, PlacementMargin, PlacementPreview, PlacementRequest, PlacementSite,
};

const BACKBONE_LIMIT: usize = 64;
const ROUTER_HEADROOM: usize = 2;
const MAX_ENDPOINT_FRONTIER: usize = 32;
type Pair = (usize, usize);
/// Prepared sites, optional road-filter counts and the access-site budget.
type PreparedSites = (Vec<PlanningSite>, Option<[usize; 2]>, usize);

/// RF evaluation seam for specialized adapters and deterministic tests. Independent
/// evaluations may run concurrently during coarse screening.
pub trait LinkEvaluation: Sync {
    fn optimistic_margin_db(&self, source: &Site, target: &Site) -> Result<f64, RfError>;
    fn evaluate(
        &self,
        source: &Site,
        target: &Site,
        step_m: f64,
        include_profile: bool,
    ) -> Result<LinkResult, RfError>;
}

impl<T: TerrainSource + Sync + ?Sized> LinkEvaluation for LinkEvaluator<'_, T> {
    fn optimistic_margin_db(&self, source: &Site, target: &Site) -> Result<f64, RfError> {
        LinkEvaluator::optimistic_margin_db(self, source, target)
    }

    fn evaluate(
        &self,
        source: &Site,
        target: &Site,
        step_m: f64,
        include_profile: bool,
    ) -> Result<LinkResult, RfError> {
        LinkEvaluator::evaluate(
            self,
            source,
            target,
            EvaluationOptions {
                sample_step_m: Some(step_m),
                include_profile,
                ..EvaluationOptions::default()
            },
        )
    }
}

#[derive(Default)]
pub struct OptimizationRequest {
    pub clients: Vec<PlanningSite>,
    pub fixed_routers: Vec<PlanningSite>,
    /// Supplied candidate pool. None requests generation from the terrain.
    pub candidates: Option<Vec<PlanningSite>>,
    /// Projected [min x, min y, max x, max y]. Required for automatic generation.
    pub terrain_bounds: Option<[f64; 4]>,
    pub exclusions: Vec<Vec<Point>>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Cancellation {
    #[default]
    Continue,
    Discard,
    KeepCertified,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SearchStatus {
    Searching,
    Complete,
    Cancelled,
    Stopped,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Stage {
    GeneratingCandidates,
    ScreeningPossibleLinks,
    CheckingEndpointVisibleCandidates,
    CheckingTwoRepeaterSummitLinks,
    CheckingInteriorBridgeLinks,
    EvaluatingRfLinks,
    SearchingTopologies,
    MeasuringPaintCoverage,
    CheckingPaintCoverage,
    ValidatingSolutionAlternatives,
    RefiningRouterPositions,
    PreparingSolutionProfiles,
}

impl Stage {
    pub fn label(self) -> &'static str {
        match self {
            Self::GeneratingCandidates => "Generating candidates",
            Self::ScreeningPossibleLinks => "Screening possible links",
            Self::CheckingEndpointVisibleCandidates => "Checking endpoint-visible candidates",
            Self::CheckingTwoRepeaterSummitLinks => "Checking two-repeater summit links",
            Self::CheckingInteriorBridgeLinks => "Checking interior bridge links",
            Self::EvaluatingRfLinks => "Evaluating RF links",
            Self::MeasuringPaintCoverage => "Measuring handheld coverage",
            Self::CheckingPaintCoverage => "Checking selected sites' handheld coverage",
            Self::SearchingTopologies => "Searching topologies",
            Self::ValidatingSolutionAlternatives => "Validating solution alternatives",
            Self::RefiningRouterPositions => "Refining router positions",
            Self::PreparingSolutionProfiles => "Preparing solution profiles",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Progress {
    pub stage: Stage,
    pub done: usize,
    pub total: usize,
}

#[derive(Default)]
pub struct SearchHooks<'a> {
    pub cancellation: Option<&'a dyn Fn() -> Cancellation>,
    pub progress: Option<&'a mut dyn FnMut(Progress)>,
    /// Receives an owned, detached snapshot of a certified result.
    pub solution_progress: Option<&'a mut dyn FnMut(OptimizationResult)>,
}

impl SearchHooks<'_> {
    fn cancellation(&self) -> Cancellation {
        self.cancellation
            .map_or(Cancellation::Continue, |poll| poll())
    }

    fn cancelled(&self) -> bool {
        self.cancellation() != Cancellation::Continue
    }

    fn notify(&mut self, stage: Stage, done: usize, total: usize) {
        if let Some(progress) = self.progress.as_deref_mut() {
            progress(Progress { stage, done, total });
        }
    }
}

#[derive(Clone, Debug)]
pub struct OptimizationResult {
    pub candidates: Vec<PlanningSite>,
    /// Scalar valid links in the search graph; links outside selected alternatives may be provisional.
    pub search_links: Vec<LinkResult>,
    pub alternatives: Vec<NetworkSolution>,
    pub active_index: Option<usize>,
    pub diagnostics: Vec<String>,
    pub elapsed: Duration,
    pub search_complete: bool,
    pub status: SearchStatus,
}

impl OptimizationResult {
    pub fn found(&self) -> bool {
        self.active_solution().is_some()
    }
    pub fn router_count(&self) -> usize {
        self.active_solution()
            .map_or(0, NetworkSolution::router_count)
    }
    pub fn active_solution(&self) -> Option<&NetworkSolution> {
        self.alternatives.get(self.active_index?)
    }

    pub fn links(&self) -> &[LinkResult] {
        self.active_solution()
            .map_or(&[], |solution| &solution.links)
    }

    pub fn primary_route(&self) -> Vec<&PlanningSite> {
        self.active_solution()
            .map_or_else(Vec::new, NetworkSolution::primary_route)
    }

    pub fn select_solution(&mut self, index: usize) -> Option<&NetworkSolution> {
        let solution = self.alternatives.get(index)?;
        self.active_index = Some(index);
        Some(solution)
    }
}

pub struct RouteOptimizer<'a> {
    terrain: &'a (dyn TerrainSource + Sync),
    rf: &'a RfSettings,
    settings: &'a CandidateSettings,
    evaluator: Option<&'a dyn LinkEvaluation>,
    cache: Option<(&'a LinkMetricsCache, &'a str, &'a str)>,
    site_filter: Option<&'a SiteFilter<'a>>,
}

impl<'a> RouteOptimizer<'a> {
    pub fn new(
        terrain: &'a (dyn TerrainSource + Sync),
        rf: &'a RfSettings,
        settings: &'a CandidateSettings,
    ) -> Self {
        Self {
            terrain,
            rf,
            settings,
            evaluator: None,
            cache: None,
            site_filter: None,
        }
    }

    pub fn with_link_evaluation(mut self, evaluator: &'a dyn LinkEvaluation) -> Self {
        self.evaluator = Some(evaluator);
        self
    }

    /// `revision` must change whenever terrain, RF settings or evaluation implementation change.
    pub fn with_cache(
        mut self,
        cache: &'a LinkMetricsCache,
        namespace: &'a str,
        revision: &'a str,
    ) -> Self {
        self.cache = Some((cache, namespace, revision));
        self
    }

    pub fn with_site_filter(mut self, filter: &'a SiteFilter<'a>) -> Self {
        self.site_filter = Some(filter);
        self
    }

    /// Search the local disk around a movable generated site at medium resolution.
    /// This helper is opt-in; `optimize` does not move certified sites.
    pub fn refine_site(
        &self,
        previous: &PlanningSite,
        site: &PlanningSite,
        following: &PlanningSite,
    ) -> Result<PlanningSite, OptimizationError> {
        if site.rf.kind != SiteKind::Candidate
            || site.origin != SiteOrigin::Optimized
            || site.locked
            || site.is_required()
        {
            return Ok(site.clone());
        }
        let radius = self.settings.refine_radius_m;
        let step = self.settings.refine_step_m.max(self.terrain.resolution_m());
        let offsets = steps(-radius, radius + step * 0.5, step)?;
        let builtin = LinkEvaluator::new(self.terrain, self.rf);
        let rf = self.rf_evaluation(&builtin);
        let evaluate = |a: &Site, b: &Site| match rf.evaluate(
            a,
            b,
            self.settings.medium_sample_step_m,
            false,
            &|| false,
        ) {
            Err(RfError::InvalidInput(_)) => Ok(None),
            result => result,
        };
        let mut best = site.clone();
        let mut best_score = (f64::NEG_INFINITY, f64::NEG_INFINITY);
        for dx in offsets.clone() {
            let x = site.rf.position.x + dx;
            let points: Vec<_> = offsets
                .clone()
                .filter(|dy| dx * dx + dy * dy <= radius * radius)
                .map(|dy| Point {
                    x,
                    y: site.rf.position.y + dy,
                })
                .collect();
            let ground = sample(self.terrain, &points, TerrainLayer::GroundDtm)?;
            let (points, ground): (Vec<_>, Vec<_>) = points
                .into_iter()
                .zip(ground)
                .filter(|(_, elevation)| elevation.is_finite())
                .unzip();
            let keep = if let Some(filter) = self.site_filter.filter(|_| !points.is_empty()) {
                let keep = filter(&points)?;
                if keep.len() != points.len() {
                    return Err(OptimizationError::InvalidInput(
                        "Site filter returned the wrong number of points".into(),
                    ));
                }
                Some(keep)
            } else {
                None
            };
            for (index, (point, ground)) in points.into_iter().zip(ground).enumerate() {
                if keep.as_ref().is_some_and(|keep| !keep[index]) {
                    continue;
                }
                let mut trial = site.clone();
                trial.rf.position = point;
                trial.rf.ground_elevation_m = ground;
                trial.rf.surface_elevation_m = if self.terrain.has_surface() {
                    let surface = sample(self.terrain, &[point], TerrainLayer::SurfaceDom)?[0];
                    surface.is_finite().then_some(surface)
                } else {
                    None
                };
                trial.rf.terminal_clutter = self.rf.router.terminal_clutter;
                trial.rf.sample_terminal_clutter(self.terrain)?;
                let Some(left) = evaluate(&previous.rf, &trial.rf)? else {
                    continue;
                };
                let Some(right) = evaluate(&trial.rf, &following.rf)? else {
                    continue;
                };
                if !left.valid || !right.valid {
                    continue;
                }
                let score = (
                    left.worst_margin_db().min(right.worst_margin_db()),
                    left.minimum_fresnel_clearance_ratio
                        .min(right.minimum_fresnel_clearance_ratio),
                );
                if score > best_score {
                    best = trial;
                    best_score = score;
                }
            }
        }
        Ok(best)
    }

    /// Try the configured mast heights in ascending order for each interior route site.
    /// The caller supplies the route to adjust; `optimize` does not call this helper.
    pub fn reduce_mast_heights(&self, route: &mut [PlanningSite]) -> Result<(), OptimizationError> {
        let settings = self.settings;
        validate_final_step(settings.final_sample_step_m)?;
        let step = settings.router_height_step_m;
        let heights = steps(
            settings.minimum_router_height_m,
            settings.maximum_router_height_m + step * 0.5,
            step,
        )?;
        let builtin = LinkEvaluator::new(self.terrain, self.rf);
        let evaluator = self.evaluator.unwrap_or(&builtin);
        for index in 1..route.len().saturating_sub(1) {
            let mut trial = route[index].rf.clone();
            for height in heights.clone() {
                trial.antenna_height_m = height;
                let step = settings.final_sample_step_m;
                let left = evaluator.evaluate(&route[index - 1].rf, &trial, step, false)?;
                let right = evaluator.evaluate(&trial, &route[index + 1].rf, step, false)?;
                if left.valid && right.valid {
                    route[index].rf.antenna_height_m = height;
                    break;
                }
            }
        }
        Ok(())
    }

    pub fn optimize(
        &self,
        request: OptimizationRequest,
        hooks: SearchHooks<'_>,
    ) -> Result<OptimizationResult, OptimizationError> {
        self.optimize_mode(request, hooks, false, None)
    }

    pub fn optimize_mesh(
        &self,
        request: OptimizationRequest,
        hooks: SearchHooks<'_>,
    ) -> Result<OptimizationResult, OptimizationError> {
        self.optimize_mode(request, hooks, true, None)
    }

    pub fn optimize_painted_mesh(
        &self,
        request: OptimizationRequest,
        paint: PaintObjective<'_>,
        hooks: SearchHooks<'_>,
    ) -> Result<OptimizationResult, OptimizationError> {
        if paint.cells.is_empty() || !(1..=100).contains(&paint.target_percent) {
            return Err(OptimizationError::InvalidInput(
                "Add a painted coverage area and a target share".into(),
            ));
        }
        self.optimize_mode(request, hooks, true, Some(paint))
    }

    fn optimize_mode(
        &self,
        request: OptimizationRequest,
        hooks: SearchHooks<'_>,
        mesh: bool,
        paint: Option<PaintObjective<'_>>,
    ) -> Result<OptimizationResult, OptimizationError> {
        let builtin = LinkEvaluator::new(self.terrain, self.rf);
        let mut search = Search {
            settings: self.settings,
            terrain: self.terrain,
            site_filter: self.site_filter,
            exclusions: request.exclusions.clone(),
            access_budget: 0,
            generated_candidates: false,
            router_height_m: if self.settings.optimize_heights {
                self.settings.maximum_router_height_m
            } else {
                self.rf.router.height_agl_m
            },
            rf: self.rf_evaluation(&builtin),
            hooks,
            started: Instant::now(),
            sites: Vec::new(),
            by_id: BTreeMap::new(),
            links: BTreeMap::new(),
            coarse: Vec::new(),
            screened_links: 0,
            final_checked: BTreeSet::new(),
            snapshot: None,
            certified_by_count: BTreeMap::new(),
            diagnostics: Vec::new(),
            seeds: Vec::new(),
            mesh,
        };
        if search.hooks.cancelled() {
            return Ok(search.interrupted());
        }
        validate_search_settings(self.settings)?;
        search.hooks.notify(Stage::GeneratingCandidates, 0, 1);
        if search.hooks.cancelled() {
            return Ok(search.interrupted());
        }
        let automatic_candidates = request.candidates.is_none()
            && self.settings.infrastructure_policy != InfrastructurePolicy::ExistingOnly;
        let (sites, filter_counts, access_budget) = self.prepare_sites(
            request,
            &search.rf,
            &|| search.hooks.cancelled(),
            mesh,
            paint.as_ref().map_or(&[], |area| area.cells),
        )?;
        search.sites = sites;
        search.access_budget = access_budget;
        search.generated_candidates = automatic_candidates;
        if automatic_candidates
            && self.settings.candidates_per_cell > 0
            && self.settings.maximum_new_routers > 0
        {
            let spacing = candidate_grid_spacing(self.terrain, self.settings);
            search.diagnostics.push(format!(
                "Effective candidate grid spacing: {spacing:.1} m (requested {:.1} m).",
                self.settings.grid_spacing_m
            ));
            let generated = search
                .sites
                .iter()
                .filter(|site| site.origin == SiteOrigin::Optimized && !site.is_client())
                .count();
            let terminals: Vec<_> = search
                .sites
                .iter()
                .filter(|site| site.is_client() || site.is_required())
                .map(|site| site.rf.position)
                .collect();
            let paint_cells = paint.as_ref().map_or(&[][..], |paint| paint.cells);
            let anchors: Vec<_> = terminals
                .into_iter()
                .chain(paint_cells.iter().copied())
                .collect();
            let extent = anchors
                .iter()
                .flat_map(|a| anchors.iter().map(move |b| a.distance_to(*b)))
                .fold(0.0, f64::max);
            let subject = if paint.is_some() {
                "search area"
            } else {
                "route"
            };
            let corridor = self.settings.corridor_width_m;
            let dense = dense_corridor_half_width_m(extent, corridor);
            search.diagnostics.push(if dense < corridor {
                format!(
                    "Candidate sites: {generated} generated for a {:.1} km {subject}, dense within {:.1} km and sparse out to {:.1} km.",
                    extent / 1000.0,
                    dense / 1000.0,
                    corridor / 1000.0
                )
            } else {
                format!(
                    "Candidate sites: {generated} generated for a {:.1} km {subject}, within {:.1} km.",
                    extent / 1000.0,
                    corridor / 1000.0
                )
            });
        }
        if let Some([kept, checked]) = filter_counts {
            search.diagnostics.push(format!(
                "Road access filter kept {kept} of {checked} grid points."
            ));
        }
        search.hooks.notify(Stage::GeneratingCandidates, 1, 1);
        if search.hooks.cancelled() {
            return Ok(search.interrupted());
        }
        search.by_id = search
            .sites
            .iter()
            .enumerate()
            .map(|(i, site)| (site.rf.id.clone(), i))
            .collect();
        if let Some(paint) = paint {
            search.run_painted_mesh(paint)
        } else {
            search.run()
        }
    }

    fn rf_evaluation<'b>(&'b self, builtin: &'b dyn LinkEvaluation) -> RfEvaluation<'b> {
        RfEvaluation {
            evaluator: self.evaluator.unwrap_or(builtin),
            cache: self.cache,
            builtin: self.evaluator.is_none(),
        }
    }

    fn prepare_sites(
        &self,
        request: OptimizationRequest,
        evaluation: &RfEvaluation<'_>,
        cancelled: &dyn Fn() -> bool,
        mesh: bool,
        paint_cells: &[Point],
    ) -> Result<PreparedSites, OptimizationError> {
        let OptimizationRequest {
            clients,
            fixed_routers,
            candidates,
            terrain_bounds,
            exclusions,
        } = request;
        let policy = self.settings.infrastructure_policy;
        let permitted = |site: &PlanningSite| match policy {
            InfrastructurePolicy::ProposedOnly => site.origin != SiteOrigin::Known,
            InfrastructurePolicy::ExistingOnly => site.origin == SiteOrigin::Known,
            InfrastructurePolicy::ExistingAndProposed => true,
        };
        if (paint_cells.is_empty() && clients.len() < 2)
            || clients
                .iter()
                .any(|site| !site.is_client() || !site.included())
        {
            return Err(OptimizationError::InvalidInput(
                "At least two enabled clients are required".into(),
            ));
        }
        if clients.iter().enumerate().any(|(i, site)| {
            clients[..i]
                .iter()
                .any(|other| other.rf.position == site.rf.position)
        }) {
            return Err(OptimizationError::InvalidInput(
                "Client coordinates must be distinct".into(),
            ));
        }
        let mut sites = clients;
        let mut ids = BTreeSet::new();
        for site in &sites {
            if !ids.insert(site.rf.id.clone()) {
                return Err(duplicate_site());
            }
        }
        let mut fixed_ids = BTreeSet::new();
        for site in fixed_routers.into_iter().filter(permitted) {
            if site.is_client() {
                return Err(OptimizationError::InvalidInput(
                    "Fixed routers must have a router or candidate site kind".into(),
                ));
            }
            if ids.contains(&site.rf.id) || !fixed_ids.insert(site.rf.id.clone()) {
                return Err(duplicate_site());
            }
            if site.is_required() && !site.enabled {
                return Err(disabled_required(&site));
            }
            if !site.included() {
                continue;
            }
            if !ids.insert(site.rf.id.clone()) {
                return Err(duplicate_site());
            }
            sites.push(site);
        }
        let mut filter_counts = None;
        let mut access_budget = 0;
        if let Some(candidates) = candidates {
            let mut explicit_ids = BTreeSet::new();
            for site in candidates
                .into_iter()
                .filter(|site| site.is_client() || permitted(site))
            {
                if !explicit_ids.insert(site.rf.id.clone()) {
                    return Err(duplicate_site());
                }
                if fixed_ids.contains(&site.rf.id) && !ids.contains(&site.rf.id) {
                    continue;
                }
                if site.is_required() && !site.enabled {
                    return Err(disabled_required(&site));
                }
                if !site.included() {
                    continue;
                }
                if !ids.insert(site.rf.id.clone()) {
                    // Clients and fixed sites may also be included in a complete supplied pool.
                    if sites
                        .iter()
                        .any(|existing| existing.rf == site.rf && existing.origin == site.origin)
                    {
                        continue;
                    }
                    return Err(duplicate_site());
                }
                sites.push(site);
            }
        } else if policy != InfrastructurePolicy::ExistingOnly
            && self.settings.maximum_new_routers > 0
        {
            let mut settings = self.settings.clone();
            let terminals: Vec<_> = sites
                .iter()
                .filter(|s| s.is_client() || s.is_required())
                .collect();
            if settings.exhaustive_screening && settings.candidates_per_cell > 0 {
                access_budget = access::ACCESS_PER_TERMINAL * terminals.len();
            }
            let network = mesh || terminals.len() > 2;
            let anchors: Vec<_> = terminals
                .iter()
                .map(|site| site.rf.position)
                .chain(paint_cells.iter().copied())
                .collect();
            let mesh_area = mesh.then(|| repeaternet_terrain::corridor::MeshArea::new(&anchors));
            let (mut low, mut high) = (anchors[0], anchors.get(1).copied().unwrap_or(anchors[0]));
            if network {
                for point in anchors {
                    low = Point {
                        x: low.x.min(point.x),
                        y: low.y.min(point.y),
                    };
                    high = Point {
                        x: high.x.max(point.x),
                        y: high.y.max(point.y),
                    };
                }
                settings.unrestricted_bounding_area = true;
            }
            if self.site_filter.is_some() {
                filter_counts = Some([0; 2]);
            }
            if settings.candidates_per_cell > 0 {
                let bounds = terrain_bounds.ok_or_else(|| {
                    OptimizationError::InvalidInput(
                        "Terrain bounds are required for candidate generation".into(),
                    )
                })?;
                let (mut generated, mut counts, access_pool) = generate_with_access_pool(
                    self.terrain,
                    bounds,
                    [low, high],
                    &settings,
                    &exclusions,
                    self.site_filter,
                    cancelled,
                    mesh_area.as_ref(),
                )?;
                let mut clutter_heights = BTreeMap::new();
                let mut configure_clutter = |site: &mut Site| {
                    site.terminal_clutter = self.rf.router.terminal_clutter;
                    if site.terminal_clutter == TerminalClutter::P2108Dom
                        && self.terrain.has_surface()
                    {
                        let position = (site.position.x.to_bits(), site.position.y.to_bits());
                        site.clutter_height_m = match clutter_heights.entry(position) {
                            std::collections::btree_map::Entry::Occupied(entry) => *entry.get(),
                            std::collections::btree_map::Entry::Vacant(entry) => {
                                site.sample_terminal_clutter(self.terrain)?;
                                *entry.insert(site.clutter_height_m)
                            }
                        };
                    }
                    Ok::<_, OptimizationError>(())
                };
                for site in &mut generated {
                    if cancelled() {
                        break;
                    }
                    configure_clutter(&mut site.rf)?;
                }
                let search_height_m = if settings.optimize_heights {
                    settings.maximum_router_height_m
                } else {
                    self.rf.router.height_agl_m
                };
                let valid_access_link = |left: &Site, right: &Site| {
                    let (source, target) = if left.id <= right.id {
                        (left, right)
                    } else {
                        (right, left)
                    };
                    if evaluation.evaluator.optimistic_margin_db(source, target)? < 0.0 {
                        return Ok(false);
                    }
                    Ok(evaluation
                        .evaluate(
                            source,
                            target,
                            settings.final_sample_step_m,
                            false,
                            cancelled,
                        )?
                        .is_some_and(|link| link.valid))
                };
                let spacing = candidate_grid_spacing(self.terrain, &settings);
                let mut refine_access = |center: Point| {
                    let step = (spacing / 2.0).max(self.terrain.resolution_m());
                    if step >= spacing {
                        return Ok(Vec::new());
                    }
                    let radius = step * 8.0;
                    let local_bounds = [
                        bounds[0].max(center.x - radius),
                        bounds[1].max(center.y - radius),
                        bounds[2].min(center.x + radius),
                        bounds[3].min(center.y + radius),
                    ];
                    if local_bounds[0] > local_bounds[2] || local_bounds[1] > local_bounds[3] {
                        return Ok(Vec::new());
                    }
                    // At most 17 by 17 samples. The final pool still enforces both site caps.
                    let local_settings = CandidateSettings {
                        grid_spacing_m: step,
                        refine_radius_m: 0.0,
                        candidates_per_cell: 289,
                        ..settings.clone()
                    };
                    let (mut samples, checked) = crate::candidates::generate(
                        self.terrain,
                        local_bounds,
                        [low, high],
                        &local_settings,
                        &exclusions,
                        self.site_filter,
                        cancelled,
                    )?;
                    if let Some(area) = &mesh_area {
                        samples.retain(|site| {
                            area.distance_to(site.rf.position) <= settings.corridor_width_m
                        });
                    }
                    counts[0] += checked[0];
                    counts[1] += checked[1];
                    Ok(samples)
                };
                crate::endpoint_access::recover_with_refinement(
                    &mut generated,
                    access_pool,
                    &sites,
                    &settings,
                    search_height_m,
                    &valid_access_link,
                    &mut refine_access,
                    &mut configure_clutter,
                    cancelled,
                )?;
                if self.site_filter.is_some() {
                    filter_counts = Some(counts);
                }
                if !paint_cells.is_empty() && sites.len() + generated.len() > MAX_PAINT_SITES {
                    // Keep candidates near the paint and mesh terminals within the bounded pool.
                    let area = mesh_area
                        .as_ref()
                        .expect("paint uses mesh candidate generation");
                    let mut ranked: Vec<_> = generated
                        .into_iter()
                        .map(|site| (area.distance_to(site.rf.position), site))
                        .collect();
                    ranked.sort_by(|(a, left), (b, right)| {
                        a.total_cmp(b)
                            .then_with(|| right.site_quality.total_cmp(&left.site_quality))
                    });
                    generated = ranked
                        .into_iter()
                        .take(MAX_PAINT_SITES.saturating_sub(sites.len()))
                        .map(|(_, site)| site)
                        .collect();
                }
                for (i, mut site) in generated.into_iter().enumerate() {
                    if network {
                        site.rf.id = format!("N{}", i + 1);
                    }
                    while !ids.insert(site.rf.id.clone()) {
                        site.rf.id.insert(0, if network { 'N' } else { 'C' });
                    }
                    sites.push(site);
                }
            }
        }
        if cancelled() {
            return Ok((sites, filter_counts, access_budget));
        }
        for site in &mut sites {
            if !site.is_client() && !site.height_override {
                site.rf.antenna_height_m = if self.settings.optimize_heights {
                    self.settings.maximum_router_height_m
                } else {
                    self.rf.router.height_agl_m
                };
            }
            if !site.rf.position.x.is_finite()
                || !site.rf.position.y.is_finite()
                || !site.rf.ground_elevation_m.is_finite()
            {
                return Err(OptimizationError::InvalidInput(format!(
                    "Site {} has invalid coordinates or ground elevation",
                    site.rf.id
                )));
            }
        }
        // Keep the traditional A/candidate/B order for a two-terminal search.
        if sites.iter().filter(|site| site.is_client()).count() == 2 {
            let end = sites.remove(1);
            sites.push(end);
        }
        Ok((sites, filter_counts, access_budget))
    }
}

fn duplicate_site() -> OptimizationError {
    OptimizationError::InvalidInput(
        "Site IDs must be unique, including fixed and optional routers".into(),
    )
}

fn disabled_required(site: &PlanningSite) -> OptimizationError {
    OptimizationError::InvalidInput(format!("Required site {} cannot be disabled", site.rf.id))
}

fn steps(
    start: f64,
    stop: f64,
    step: f64,
) -> Result<impl Iterator<Item = f64> + Clone, OptimizationError> {
    let count = ((stop - start) / step).ceil();
    if !count.is_finite() || count >= usize::MAX as f64 || !step.is_finite() || step <= 0.0 {
        return Err(OptimizationError::InvalidInput(
            "Local search requires a finite range and positive step".into(),
        ));
    }
    Ok((0..count.max(0.0) as usize).map(move |i| start + i as f64 * step))
}

fn validate_search_settings(settings: &CandidateSettings) -> Result<(), OptimizationError> {
    validate_final_step(settings.final_sample_step_m)?;
    if std::iter::once(settings.coarse_sample_step_m)
        .chain(settings.maximum_link_distance_m)
        .any(|value| !value.is_finite() || value <= 0.0)
    {
        return Err(OptimizationError::InvalidInput(
            "RF sample steps and maximum distance must be positive and finite".into(),
        ));
    }
    Ok(())
}

fn validate_final_step(step_m: f64) -> Result<(), OptimizationError> {
    if !step_m.is_finite() || step_m <= 0.0 || step_m > CERTIFICATION_MAX_STEP_M {
        return Err(OptimizationError::InvalidInput(format!(
            "final_sample_step_m must be above zero and at most {CERTIFICATION_MAX_STEP_M} m"
        )));
    }
    Ok(())
}

struct RfEvaluation<'a> {
    evaluator: &'a dyn LinkEvaluation,
    cache: Option<(&'a LinkMetricsCache, &'a str, &'a str)>,
    builtin: bool,
}

impl RfEvaluation<'_> {
    fn plausible(
        &self,
        sites: &[PlanningSite],
        (a, b): Pair,
        max_distance: Option<f64>,
    ) -> Result<bool, OptimizationError> {
        let (left, right) = (&sites[a].rf, &sites[b].rf);
        let distance = left.distance_to(right);
        if distance == 0.0 || max_distance.is_some_and(|limit| distance > limit) {
            return Ok(false);
        }
        Ok(self.evaluator.optimistic_margin_db(left, right)? >= 0.0)
    }

    fn evaluate(
        &self,
        source: &Site,
        target: &Site,
        step: f64,
        profile: bool,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Option<LinkResult>, RfError> {
        let compute = || {
            if cancelled() {
                return Ok(None);
            }
            let mut link = match self.evaluator.evaluate(source, target, step, profile) {
                Ok(link) => link,
                Err(RfError::MissingTerrain) => return Ok(None),
                Err(error) => return Err(error),
            };
            if !profile {
                link.profile = None;
                link.dominant_obstacles.clear();
            }
            Ok(Some(link))
        };
        if let Some((cache, namespace, revision)) = self.cache.filter(|_| !profile) {
            cache.get_or_compute(
                CacheKey::new(namespace, revision, source, target, step),
                cancelled,
                compute,
            )
        } else {
            compute()
        }
    }

    /// Uncached coarse evaluation for exhaustive screening, which would only churn the cache.
    fn screen_pair(
        &self,
        sites: &[PlanningSite],
        (a, b): Pair,
        step: f64,
    ) -> Result<Option<LinkResult>, OptimizationError> {
        let (mut source, mut target) = (&sites[a].rf, &sites[b].rf);
        if source.id > target.id {
            std::mem::swap(&mut source, &mut target);
        }
        match self.evaluator.evaluate(source, target, step, false) {
            Ok(mut link) => {
                link.profile = None;
                link.dominant_obstacles = Vec::new();
                Ok(Some(link))
            }
            Err(RfError::MissingTerrain) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn search_pair(
        &self,
        sites: &[PlanningSite],
        (a, b): Pair,
        step: f64,
        profile: bool,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Option<LinkResult>, OptimizationError> {
        let (mut source, mut target) = (&sites[a].rf, &sites[b].rf);
        if source.id > target.id {
            std::mem::swap(&mut source, &mut target);
        }
        Ok(self.evaluate(source, target, step, profile, cancelled)?)
    }
}

struct Search<'a, 'b> {
    mesh: bool,
    settings: &'a CandidateSettings,
    terrain: &'a (dyn TerrainSource + Sync),
    site_filter: Option<&'a SiteFilter<'a>>,
    exclusions: Vec<Vec<Point>>,
    /// Part of the candidate budget reserved for terminal access sites.
    access_budget: usize,
    /// Candidates were generated from terrain, so their positions may be refined.
    generated_candidates: bool,
    /// Antenna height given to generated routers during the search.
    router_height_m: f64,
    rf: RfEvaluation<'a>,
    hooks: SearchHooks<'b>,
    started: Instant,
    sites: Vec<PlanningSite>,
    by_id: BTreeMap<String, usize>,
    links: BTreeMap<Pair, LinkResult>,
    /// Coarse screening summaries for the exact path search, which keeps full
    /// link results only for certified pairs.
    coarse: Vec<(Pair, crate::paths::Screened)>,
    /// Links that passed screening, for diagnostics after `coarse` is released.
    screened_links: usize,
    final_checked: BTreeSet<Pair>,
    snapshot: Option<OptimizationResult>,
    certified_by_count: BTreeMap<usize, NetworkSolution>,
    diagnostics: Vec<String>,
    seeds: Vec<Vec<String>>,
}

impl Search<'_, '_> {
    fn run(mut self) -> Result<OptimizationResult, OptimizationError> {
        let initial: BTreeSet<_> = self
            .sites
            .iter()
            .enumerate()
            .filter_map(|(i, site)| (site.is_client() || site.is_required()).then_some(i))
            .collect();
        if self.mesh {
            return self.run_mesh(&initial);
        }
        let access = self.add_terminal_access_sites(&initial)?;
        if self.hooks.cancelled() {
            return Ok(self.interrupted());
        }
        if access > 0 {
            self.diagnostics.push(format!(
                "Added {access} access sites from a fine sweep around the endpoints and required routers."
            ));
        }
        let terminals: BTreeSet<_> = self
            .sites
            .iter()
            .enumerate()
            .filter_map(|(i, site)| (site.is_client() || site.is_required()).then_some(i))
            .collect();
        let two_clients = self.sites.iter().filter(|site| site.is_client()).count() == 2;
        if two_clients
            && self.settings.exhaustive_screening
            && self.settings.priority == OptimizationPriority::MinimumRouters
        {
            return self.run_path_search(&terminals);
        }
        let low_hop_eligible =
            terminals.len() == 2 && self.sites.iter().all(|site| !site.is_required());
        let mut checked_low_hop =
            low_hop_eligible && self.settings.priority == OptimizationPriority::MinimumRouters;
        if checked_low_hop {
            self.low_hop()?;
            if self.hooks.cancelled() {
                return Ok(self.interrupted());
            }
        }
        self.screen(&terminals)?;
        let mut checked_bridges = false;
        let mut exploration = self.settings.clone();
        exploration.maximum_new_routers = exploration
            .maximum_new_routers
            .saturating_add(ROUTER_HEADROOM);
        loop {
            if self.hooks.cancelled() {
                return Ok(self.interrupted());
            }
            self.hooks.notify(Stage::SearchingTopologies, 0, 1);
            let explored = self.solve(&exploration)?;
            if self.hooks.cancelled() {
                return Ok(self.interrupted());
            }
            self.hooks.notify(Stage::SearchingTopologies, 1, 1);
            if self.hooks.cancelled() {
                return Ok(self.interrupted());
            }
            let pending = self.pending_pairs(&explored);
            if !pending.is_empty() {
                self.validate_pairs(pending)?;
                self.publish_certified(&explored, None)?;
                continue;
            }
            let mut alternatives = self.solve(self.settings)?;
            if self.hooks.cancelled() {
                return Ok(self.interrupted());
            }
            let pending = self.pending_pairs(&alternatives);
            if !pending.is_empty() {
                self.validate_pairs(pending)?;
                self.publish_certified(&alternatives, None)?;
                continue;
            }
            if alternatives.is_empty()
                && low_hop_eligible
                && !checked_low_hop
                && self.settings.priority == OptimizationPriority::MinimumInfrastructure
            {
                checked_low_hop = true;
                self.low_hop()?;
                continue;
            }
            if self.settings.priority == OptimizationPriority::MaximumReliability {
                for solution in alternatives.drain(..) {
                    self.retain_certified(solution);
                }
                alternatives = std::mem::take(&mut self.certified_by_count)
                    .into_values()
                    .collect();
            }
            if alternatives.is_empty() && !checked_bridges {
                checked_bridges = true;
                if self.search_bridges(&terminals)? {
                    continue;
                }
                if self.hooks.cancelled() {
                    return Ok(self.interrupted());
                }
            }
            if alternatives.is_empty() {
                self.diagnose_bridge_failure(&terminals)?;
            }
            self.restore_profiles(&mut alternatives)?;
            if self.hooks.cancelled() {
                return Ok(self.interrupted());
            }
            return Ok(self.finish(alternatives));
        }
    }

    fn finish(&mut self, alternatives: Vec<NetworkSolution>) -> OptimizationResult {
        let result = self.result(alternatives, true);
        if result.found() {
            if let Some(publish) = self.hooks.solution_progress.as_deref_mut() {
                publish(OptimizationResult {
                    search_links: Vec::new(),
                    candidates: result.candidates.clone(),
                    alternatives: result.alternatives.clone(),
                    diagnostics: result.diagnostics.clone(),
                    ..result.clone()
                });
            }
        }
        result
    }

    fn run_mesh(
        mut self,
        terminals: &BTreeSet<usize>,
    ) -> Result<OptimizationResult, OptimizationError> {
        self.screen(terminals)?;
        loop {
            if self.hooks.cancelled() {
                return Ok(self.interrupted());
            }
            self.hooks.notify(Stage::SearchingTopologies, 0, 1);
            let graph = RoutingGraph::from_links(&self.sites, self.links.values())?;
            let mut alternatives =
                crate::connect_mesh(&graph, self.settings, &|| self.hooks.cancelled());
            self.hooks.notify(Stage::SearchingTopologies, 1, 1);
            let pending = self.pending_pairs(&alternatives);
            if !pending.is_empty() {
                self.validate_pairs(pending)?;
                self.publish_certified(&alternatives, None)?;
                continue;
            }
            self.refine_mesh(&mut alternatives)?;
            self.restore_profiles(&mut alternatives)?;
            return Ok(if self.hooks.cancelled() {
                self.interrupted()
            } else {
                self.finish(alternatives)
            });
        }
    }

    fn run_painted_mesh(
        mut self,
        paint: PaintObjective<'_>,
    ) -> Result<OptimizationResult, OptimizationError> {
        if self.sites.len() > MAX_PAINT_SITES {
            return Err(OptimizationError::InvalidInput(format!(
                "Paint search supports up to {MAX_PAINT_SITES} candidate and fixed sites"
            )));
        }
        let terminals = self
            .sites
            .iter()
            .enumerate()
            .filter_map(|(i, site)| (site.is_client() || site.is_required()).then_some(i))
            .collect();
        self.screen(&terminals)?;
        let mut masks = SiteMasks::new();
        let count = self.sites.len();
        let cell_count = paint.cells.len();
        let target = paint.target_percent;
        self.hooks.notify(Stage::MeasuringPaintCoverage, 0, count);
        for (i, site) in self.sites.iter().enumerate() {
            if self.hooks.cancelled() {
                return Ok(self.interrupted());
            }
            masks.insert(
                site.rf.id.clone(),
                mesh_paint::evaluate_mask(paint.evaluate, site, cell_count)?,
            );
            self.hooks
                .notify(Stage::MeasuringPaintCoverage, i + 1, count);
        }
        // Certification can change the graph. Re-run the cover search on the
        // corrected links without recomputing the per-site handheld coverage.
        let mut certification = PaintCertification::new(paint);
        const ROUNDS: usize = 12;
        for round in 0..ROUNDS {
            if self.hooks.cancelled() {
                return Ok(self.interrupted());
            }
            self.hooks.notify(Stage::SearchingTopologies, round, ROUNDS);
            let graph = RoutingGraph::from_links(&self.sites, self.links.values())?;
            let mut alternatives = mesh_paint::connect_cover(
                &graph,
                self.settings,
                cell_count,
                target,
                &masks,
                &|| self.hooks.cancelled(),
            );
            let pending = self.pending_pairs(&alternatives);
            if !pending.is_empty() {
                self.validate_pairs(pending)?;
                self.publish_certified(&alternatives, Some(&mut certification))?;
                continue;
            }
            self.certify_paint(&mut alternatives, &mut certification)?;
            self.restore_profiles(&mut alternatives)?;
            return Ok(if self.hooks.cancelled() {
                self.interrupted()
            } else {
                self.finish(alternatives)
            });
        }
        if self.hooks.cancelled() {
            return Ok(self.interrupted());
        }
        let alternatives = self
            .snapshot
            .take()
            .map_or_else(Vec::new, |snapshot| snapshot.alternatives);
        self.diagnostics
            .push("Paint search reached its link certification limit.".into());
        Ok(self.finish(alternatives))
    }

    fn solve(
        &self,
        settings: &CandidateSettings,
    ) -> Result<Vec<NetworkSolution>, OptimizationError> {
        let graph = RoutingGraph::from_links(&self.sites, self.links.values())?;
        solve_graph(&graph, settings, &|| self.hooks.cancelled(), &self.seeds)
    }

    fn screen(&mut self, terminals: &BTreeSet<usize>) -> Result<(), OptimizationError> {
        if self.settings.exhaustive_screening {
            self.exhaustive_screen(terminals, false)?;
            if !self.mesh {
                self.seed_planned_routes();
            }
            return Ok(());
        }
        let mut pairs = screened_pairs(
            &self.sites,
            self.settings.maximum_neighbors_per_site,
            terminals,
            &|| self.hooks.cancelled(),
        );
        if self.hooks.cancelled() {
            return Ok(());
        }
        let backbone = backbone_site_indices(&self.sites, BACKBONE_LIMIT, terminals);
        for (i, &left) in backbone.iter().enumerate() {
            pairs.extend(backbone[i + 1..].iter().map(|&right| (left, right)));
        }
        let total = pairs.len();
        self.hooks.notify(Stage::ScreeningPossibleLinks, 0, total);
        let mut plausible = Vec::new();
        for (i, key) in pairs.into_iter().enumerate() {
            if self.hooks.cancelled() {
                return Ok(());
            }
            if !self.final_checked.contains(&key) && self.plausible(key)? {
                plausible.push(key);
            }
            if (i + 1) % 20 == 0 || i + 1 == total {
                self.hooks
                    .notify(Stage::ScreeningPossibleLinks, i + 1, total);
            }
        }
        let total = plausible.len();
        self.hooks.notify(Stage::EvaluatingRfLinks, 0, total);
        let mut retry = Vec::new();
        let mut done = 0;
        let cancellation = self.hooks.cancellation;
        let (rf, sites, links, hooks) = (&self.rf, &self.sites, &mut self.links, &mut self.hooks);
        let step = self.settings.coarse_sample_step_m;
        let cancelled = || cancellation.is_some_and(|poll| poll() != Cancellation::Continue);
        for_each_batched(
            &plausible,
            worker_count(self.settings.parallel_workers, total),
            (cancellation.is_some() || !rf.builtin).then_some(&cancelled),
            |key, cancelled| rf.search_pair(sites, key, step, false, cancelled),
            |key, link| {
                if let Some(link) = link.filter(|link| link.valid) {
                    links.insert(key, link);
                } else {
                    retry.push(key);
                }
                done += 1;
                hooks.notify(Stage::EvaluatingRfLinks, done, total);
            },
        )?;
        // Coarse/medium classifications cannot safely prove RF failure.
        self.certify_pairs(&retry, |_, _, _| {})?;

        if terminals.len() == 2 && self.settings.maximum_hops >= 3 {
            let a = *terminals.first().expect("two terminals");
            let b = *terminals.last().expect("two terminals");
            self.check_endpoint_frontier(a, b)?;
        }
        Ok(())
    }

    fn check_endpoint_frontier(&mut self, a: usize, b: usize) -> Result<(), OptimizationError> {
        if self.hooks.cancelled() {
            return Ok(());
        }
        // Favor routers with strong access links even if they are outside each
        // other's nearest neighbors and did not make the geographic backbone.
        let limit = self
            .settings
            .maximum_neighbors_per_site
            .clamp(1, MAX_ENDPOINT_FRONTIER);
        let frontier = |endpoint| {
            let mut links = Vec::new();
            for node in 0..self.sites.len() {
                if self.hooks.cancelled() {
                    break;
                }
                if node != a && node != b {
                    if let Some(link) = self.links.get(&pair(endpoint, node)) {
                        links.push((node, link));
                    }
                }
            }
            let rank = |(left_node, left): &(usize, &LinkResult),
                        (right_node, right): &(usize, &LinkResult)| {
                link_is_clear(right)
                    .cmp(&link_is_clear(left))
                    .then_with(|| right.worst_margin_db().total_cmp(&left.worst_margin_db()))
                    .then_with(|| {
                        self.sites[*left_node]
                            .rf
                            .id
                            .cmp(&self.sites[*right_node].rf.id)
                    })
            };
            if links.len() > limit {
                links.select_nth_unstable_by(limit, rank);
                links.truncate(limit);
            }
            links.into_iter().map(|(node, _)| node).collect::<Vec<_>>()
        };
        let left = frontier(a);
        let right = frontier(b);
        let mut pairs = BTreeSet::new();
        for left in left {
            for &right in &right {
                let key = pair(left, right);
                if left != right
                    && !self.links.contains_key(&key)
                    && !self.final_checked.contains(&key)
                {
                    pairs.insert(key);
                }
            }
        }
        if pairs.is_empty() || self.hooks.cancelled() {
            return Ok(());
        }
        let pairs: Vec<_> = pairs.into_iter().collect();
        let total = pairs.len();
        self.hooks
            .notify(Stage::CheckingTwoRepeaterSummitLinks, 0, total);
        let mut done = 0;
        self.certify_pairs(&pairs, |_, _, hooks| {
            done += 1;
            if done % 10 == 0 || done == total {
                hooks.notify(Stage::CheckingTwoRepeaterSummitLinks, done, total);
            }
        })
    }

    fn low_hop(&mut self) -> Result<(), OptimizationError> {
        if self.hooks.cancelled() {
            return Ok(());
        }
        let (a, b) = (0, self.sites.len() - 1);
        let maximum = self.settings.maximum_hops;
        if maximum == 0 {
            return Ok(());
        }
        self.certify((a, b), true)?;
        let direct = self.links.contains_key(&(a, b));
        if !direct && maximum >= 2 {
            let mut from_a = Vec::new();
            let mut to_b = Vec::new();
            let count = b - 1;
            self.hooks
                .notify(Stage::CheckingEndpointVisibleCandidates, 0, count);
            let pairs: Vec<_> = (1..b)
                .filter(|&i| {
                    self.settings.maximum_new_routers > 0 || !self.sites[i].needs_new_router()
                })
                .flat_map(|i| [(a, i), (i, b)])
                .collect();
            self.certify_pairs(&pairs, |(left, right), valid, hooks| {
                if left == a {
                    if valid {
                        from_a.push(right);
                    }
                    return;
                }
                if valid {
                    to_b.push(left);
                }
                if left % 10 == 0 || left == count {
                    hooks.notify(Stage::CheckingEndpointVisibleCandidates, left, count);
                }
            })?;
            if self.hooks.cancelled() {
                return Ok(());
            }
            if maximum >= 3 && !from_a.iter().any(|node| to_b.binary_search(node).is_ok()) {
                let total = from_a.len().saturating_mul(to_b.len());
                self.hooks
                    .notify(Stage::CheckingTwoRepeaterSummitLinks, 0, total);
                let mut done = 0;
                // Failed unscreened cross-pairs are streamed and never accumulated.
                for left in from_a {
                    for &right in &to_b {
                        if self.hooks.cancelled() {
                            return Ok(());
                        }
                        let new_routers = usize::from(self.sites[left].needs_new_router())
                            + usize::from(self.sites[right].needs_new_router());
                        if new_routers <= self.settings.maximum_new_routers {
                            self.certify(pair(left, right), false)?;
                        }
                        done += 1;
                        if done % 10 == 0 || done == total {
                            self.hooks
                                .notify(Stage::CheckingTwoRepeaterSummitLinks, done, total);
                        }
                    }
                }
            }
        }
        if self.hooks.cancelled() {
            return Ok(());
        }
        let path = {
            let graph = RoutingGraph::from_links(
                &self.sites,
                self.links
                    .iter()
                    .filter(|(key, _)| self.final_checked.contains(key))
                    .map(|(_, link)| link),
            )?;
            graph.select_path(
                &self.sites[a].rf.id,
                &self.sites[b].rf.id,
                self.settings.priority,
            )
        };
        let Some(path) = path.filter(|path| {
            path.len() - 1 <= maximum.min(3)
                && path
                    .iter()
                    .filter(|id| self.sites[self.by_id[*id]].needs_new_router())
                    .count()
                    <= self.settings.maximum_new_routers
        }) else {
            return Ok(());
        };
        let routers = path[1..path.len() - 1].to_vec();
        let nodes: Vec<_> = path.iter().map(|id| self.by_id[id]).collect();
        let mut pending = BTreeSet::new();
        for (i, &left) in nodes.iter().enumerate() {
            pending.extend(
                nodes[i + 1..]
                    .iter()
                    .map(|&right| pair(left, right))
                    .filter(|key| !self.final_checked.contains(key)),
            );
        }
        self.validate_pairs(pending)?;
        if self.hooks.cancelled() {
            return Ok(());
        }
        self.seeds.push(routers);
        let selected: Vec<_> = nodes.iter().map(|&i| self.sites[i].clone()).collect();
        let graph = RoutingGraph::from_links(&selected, self.links.values())?;
        let alternatives = solve_graph(
            &graph,
            self.settings,
            &|| self.hooks.cancelled(),
            &self.seeds,
        )?;
        self.publish_certified(&alternatives, None)
    }

    fn pending_pairs(&self, alternatives: &[NetworkSolution]) -> BTreeSet<Pair> {
        let mut pairs = BTreeSet::new();
        for solution in alternatives {
            for (i, site) in solution.sites.iter().enumerate() {
                let left = self.by_id[&site.rf.id];
                for other in &solution.sites[i + 1..] {
                    let key = pair(left, self.by_id[&other.rf.id]);
                    if !self.final_checked.contains(&key) {
                        pairs.insert(key);
                    }
                }
            }
        }
        pairs
    }

    fn validate_pairs(&mut self, pending: BTreeSet<Pair>) -> Result<(), OptimizationError> {
        if pending.is_empty() {
            return Ok(());
        }
        let count = pending.len();
        self.hooks
            .notify(Stage::ValidatingSolutionAlternatives, 0, count);
        for (i, key) in pending.into_iter().enumerate() {
            if self.hooks.cancelled() {
                return Ok(());
            }
            self.certify(key, true)?;
            self.hooks
                .notify(Stage::ValidatingSolutionAlternatives, i + 1, count);
        }
        Ok(())
    }

    fn certify(&mut self, key: Pair, remember_failure: bool) -> Result<bool, OptimizationError> {
        if self.final_checked.contains(&key) {
            return Ok(self.links.contains_key(&key));
        }
        let link = if self.plausible(key)? {
            let evaluated = self.evaluate(key, self.settings.final_sample_step_m, false);
            if self.hooks.cancelled() {
                return Ok(false);
            }
            evaluated?.filter(|link| link.valid)
        } else {
            None
        };
        Ok(Self::store_certification(
            &mut self.links,
            &mut self.final_checked,
            key,
            link,
            remember_failure,
        ))
    }

    /// Certify distinct pairs and apply their results in input order.
    fn certify_pairs(
        &mut self,
        pairs: &[Pair],
        mut accept: impl FnMut(Pair, bool, &mut SearchHooks<'_>),
    ) -> Result<(), OptimizationError> {
        let workers = worker_count(self.settings.parallel_workers, pairs.len());
        if workers <= 1 || !self.rf.builtin {
            for &key in pairs {
                if self.hooks.cancelled() {
                    break;
                }
                let valid = self.certify(key, true)?;
                if self.hooks.cancelled() {
                    break;
                }
                accept(key, valid, &mut self.hooks);
            }
            return Ok(());
        }

        let tasks: Vec<_> = pairs
            .iter()
            .map(|&key| (key, self.final_checked.contains(&key)))
            .collect();
        let cancellation = self.hooks.cancellation;
        let cancelled = || cancellation.is_some_and(|poll| poll() != Cancellation::Continue);
        let (rf, sites, hooks) = (&self.rf, &self.sites, &mut self.hooks);
        let (links, checked) = (&mut self.links, &mut self.final_checked);
        let max_distance = self.settings.maximum_link_distance_m;
        let step = self.settings.final_sample_step_m;
        for_each_batched(
            &tasks,
            workers,
            cancellation.is_some().then_some(&cancelled),
            |(key, already_checked), cancelled| {
                if already_checked || !rf.plausible(sites, key, max_distance)? {
                    return Ok(None);
                }
                Ok(rf
                    .search_pair(sites, key, step, false, cancelled)?
                    .filter(|link| link.valid))
            },
            |(key, already_checked), link| {
                let valid = if already_checked {
                    links.contains_key(&key)
                } else {
                    Self::store_certification(links, checked, key, link, true)
                };
                accept(key, valid, hooks);
            },
        )
    }

    fn store_certification(
        links: &mut BTreeMap<Pair, LinkResult>,
        checked: &mut BTreeSet<Pair>,
        key: Pair,
        link: Option<LinkResult>,
        remember_failure: bool,
    ) -> bool {
        if remember_failure || link.is_some() {
            checked.insert(key);
        }
        if let Some(link) = link {
            links.insert(key, link);
            true
        } else {
            links.remove(&key);
            false
        }
    }

    fn plausible(&self, key: Pair) -> Result<bool, OptimizationError> {
        self.rf
            .plausible(&self.sites, key, self.settings.maximum_link_distance_m)
    }

    fn evaluate(
        &self,
        key: Pair,
        step: f64,
        profile: bool,
    ) -> Result<Option<LinkResult>, OptimizationError> {
        self.rf
            .search_pair(&self.sites, key, step, profile, &|| self.hooks.cancelled())
    }

    fn retain_certified(&mut self, solution: NetworkSolution) {
        let count = solution.router_count();
        if self
            .certified_by_count
            .get(&count)
            .is_none_or(|incumbent| crate::topology::improves_certified_pair(&solution, incumbent))
        {
            self.certified_by_count.insert(count, solution);
        }
    }

    fn certify_paint(
        &mut self,
        alternatives: &mut [NetworkSolution],
        certification: &mut PaintCertification<'_>,
    ) -> Result<(), OptimizationError> {
        let count = alternatives.len();
        for (i, solution) in alternatives.iter_mut().enumerate() {
            if self.hooks.cancelled() {
                return Ok(());
            }
            self.hooks.notify(Stage::CheckingPaintCoverage, i, count);
            certification.certify(solution, &|| self.hooks.cancelled())?;
        }
        alternatives.sort_by(|a, b| mesh_paint::compare(b, a));
        Ok(())
    }

    fn publish_certified(
        &mut self,
        previous: &[NetworkSolution],
        paint: Option<&mut PaintCertification<'_>>,
    ) -> Result<(), OptimizationError> {
        let reliability = self.settings.priority == OptimizationPriority::MaximumReliability;
        if (self.snapshot.is_some() && !reliability)
            || previous.is_empty()
            || self.hooks.cancelled()
        {
            return Ok(());
        }
        let mut certified = Vec::new();
        for solution in previous
            .iter()
            .filter(|s| s.new_router_count() <= self.settings.maximum_new_routers)
        {
            if self.hooks.cancelled() {
                return Ok(());
            }
            if !self
                .pending_pairs(std::slice::from_ref(solution))
                .is_empty()
            {
                continue;
            }
            let graph = RoutingGraph::from_links(&solution.sites, self.links.values())?;
            let topology = if solution.paint_coverage.is_some() {
                // All selected sites were checked above. Keep the coverage
                // sites as terminals when rebuilding the certified topology.
                crate::mesh::installation(&graph, self.settings.maximum_hops, true, &|| {
                    self.hooks.cancelled()
                })
                .map(|mut topology| {
                    topology.paint_coverage = solution.paint_coverage.clone();
                    topology
                })
            } else if self.mesh {
                crate::mesh::mesh_installation(&graph, self.settings.maximum_hops, &|| {
                    self.hooks.cancelled()
                })
            } else {
                crate::topology::certify_installation(&graph, self.settings)
            };
            if let Some(mut topology) = topology {
                topology
                    .diagnostics
                    .push("Retained a route verified during final-resolution link checks.".into());
                certified.push(topology);
            }
        }
        if certified.is_empty() || self.hooks.cancelled() {
            return Ok(());
        }
        if let Some(paint) = paint {
            self.certify_paint(&mut certified, paint)?;
            if self.hooks.cancelled() {
                return Ok(());
            }
        }
        if reliability {
            for solution in &certified {
                self.retain_certified(solution.clone());
            }
        }
        if self.snapshot.is_some() {
            return Ok(());
        }
        for solution in &mut certified {
            solution
                .diagnostics
                .push("Certified route snapshot; broader search is still running.".into());
        }
        self.restore_profiles(&mut certified)?;
        if self.hooks.cancelled() {
            return Ok(());
        }
        let result = self.result(certified, false);
        self.snapshot = Some(result.clone());
        if let Some(publish) = self.hooks.solution_progress.as_deref_mut() {
            publish(result);
        }
        Ok(())
    }

    fn restore_profiles(
        &mut self,
        alternatives: &mut [NetworkSolution],
    ) -> Result<(), OptimizationError> {
        if alternatives.is_empty() {
            return Ok(());
        }
        let keys: BTreeSet<_> = alternatives
            .iter()
            .flat_map(|solution| &solution.links)
            .map(|link| pair(self.by_id[&link.source_id], self.by_id[&link.target_id]))
            .collect();
        let mut detailed = BTreeMap::new();
        let count = keys.len();
        self.hooks
            .notify(Stage::PreparingSolutionProfiles, 0, count);
        for (i, key) in keys.into_iter().enumerate() {
            if self.hooks.cancelled() {
                return Ok(());
            }
            let evaluated = self.evaluate(key, self.settings.final_sample_step_m, true);
            if self.hooks.cancelled() {
                return Ok(());
            }
            let link = evaluated?.filter(|link| link.valid).ok_or_else(|| {
                OptimizationError::CertificationChanged {
                    source: self.sites[key.0].rf.id.clone(),
                    target: self.sites[key.1].rf.id.clone(),
                }
            })?;
            detailed.insert(key, link);
            self.hooks
                .notify(Stage::PreparingSolutionProfiles, i + 1, count);
        }
        if self.hooks.cancelled() {
            return Ok(());
        }
        for link in alternatives
            .iter_mut()
            .flat_map(|solution| &mut solution.links)
        {
            *link =
                detailed[&pair(self.by_id[&link.source_id], self.by_id[&link.target_id])].clone();
        }
        Ok(())
    }

    fn result(
        &self,
        alternatives: Vec<NetworkSolution>,
        search_complete: bool,
    ) -> OptimizationResult {
        let active_index = if self.mesh {
            alternatives
                .iter()
                .enumerate()
                .max_by(|(_, a), (_, b)| mesh_paint::compare(a, b))
                .map(|(index, _)| index)
        } else {
            select_active_solution_index(&alternatives, self.settings.priority)
        };
        let mut diagnostics = if let Some(index) = active_index {
            alternatives[index].diagnostics.clone()
        } else {
            let mut diagnostics = Vec::new();
            let required: Vec<_> = self
                .sites
                .iter()
                .filter(|site| site.is_required())
                .map(|site| site.rf.id.as_str())
                .collect();
            if !required.is_empty() {
                diagnostics.push(format!("Required routers checked: {}", required.join(", ")));
            }
            diagnostics
        };
        diagnostics.extend(self.diagnostics.clone());
        OptimizationResult {
            candidates: self.sites.clone(),
            search_links: if search_complete {
                self.links.values().cloned().collect()
            } else {
                Vec::new()
            },
            alternatives,
            active_index,
            diagnostics,
            elapsed: self.started.elapsed(),
            search_complete,
            status: if search_complete {
                SearchStatus::Complete
            } else {
                SearchStatus::Searching
            },
        }
    }

    fn interrupted(&mut self) -> OptimizationResult {
        if self.hooks.cancellation() == Cancellation::KeepCertified {
            if let Some(mut snapshot) = self.snapshot.take() {
                snapshot.elapsed = self.started.elapsed();
                snapshot.status = SearchStatus::Stopped;
                return snapshot;
            }
        }
        OptimizationResult {
            candidates: std::mem::take(&mut self.sites),
            search_links: Vec::new(),
            alternatives: Vec::new(),
            active_index: None,
            diagnostics: vec!["Optimization cancelled".into()],
            elapsed: self.started.elapsed(),
            search_complete: false,
            status: SearchStatus::Cancelled,
        }
    }
}
