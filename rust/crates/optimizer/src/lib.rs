//! Native candidate selection, RF certification and routing topology search.

mod cache;
mod candidates;
mod endpoint_access;
mod graph;
mod optimizer;
mod parallel;
mod paths;
mod screening;
mod topology;

pub use cache::{CacheStats, LinkMetricsCache};
pub use candidates::{
    candidate_grid_spacing, dense_corridor_half_width_m, generate_candidates, SiteFilter,
};
pub use graph::RoutingGraph;
pub use optimizer::{
    Cancellation, LinkEvaluation, OptimizationRequest, OptimizationResult, Progress,
    RouteOptimizer, SearchHooks, SearchStatus, Stage,
};
pub use screening::{backbone_site_indices, screening_pair_indices};
pub use topology::{select_active_solution_index, solve_topologies, ClientPaths, NetworkSolution};

use repeaternet_rf_core::{
    calibration::CERTIFICATION_MAX_STEP_M, LinkResult, RfError, Site, SiteKind,
};
use std::{error, fmt};

pub const ROUTE_MODEL_VERSION: &str = "route-rust-v2";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum OptimizationPriority {
    #[default]
    MinimumRouters,
    FewestNewInstallations,
    MinimumInfrastructure,
    MaximumReliability,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum InfrastructurePolicy {
    ProposedOnly,
    ExistingOnly,
    #[default]
    ExistingAndProposed,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum RouterPolicy {
    #[default]
    Optional,
    Required,
    Excluded,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SiteOrigin {
    Manual,
    Known,
    #[default]
    Optimized,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PlanningSite {
    pub rf: Site,
    pub origin: SiteOrigin,
    pub policy: RouterPolicy,
    pub enabled: bool,
    pub locked: bool,
    pub height_override: bool,
    pub terrain_slope: f64,
    pub site_quality: f64,
}

impl From<Site> for PlanningSite {
    fn from(rf: Site) -> Self {
        Self {
            rf,
            origin: SiteOrigin::Optimized,
            policy: RouterPolicy::Optional,
            enabled: true,
            locked: false,
            height_override: false,
            terrain_slope: 0.0,
            site_quality: 0.0,
        }
    }
}

impl PlanningSite {
    pub fn is_client(&self) -> bool {
        matches!(
            self.rf.kind,
            SiteKind::EndpointA | SiteKind::EndpointB | SiteKind::Client
        )
    }

    /// A generated site that consumes the route's new-router budget.
    pub fn needs_new_router(&self) -> bool {
        !self.is_client() && !self.is_required() && self.origin == SiteOrigin::Optimized
    }

    pub fn is_required(&self) -> bool {
        !self.is_client() && self.policy == RouterPolicy::Required
    }

    fn included(&self) -> bool {
        self.enabled && self.policy != RouterPolicy::Excluded
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum RoadAccessClasses {
    PublicRoads,
    #[default]
    AllDrivable,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SearchEffort {
    Quick,
    #[default]
    Balanced,
    Thorough,
}

impl SearchEffort {
    /// Apply the web planner's per-cell and neighbor caps, retaining all other
    /// settings. Every effort screens all candidate pairs within link range and
    /// scales the candidate area with route length; they differ in how densely
    /// candidate sites are placed in that area.
    pub fn resolve(self, requested: &CandidateSettings) -> CandidateSettings {
        let (per_cell, neighbors) = match self {
            Self::Quick => (2, 8),
            Self::Balanced => (3, 12),
            Self::Thorough => return requested.clone(),
        };
        CandidateSettings {
            candidates_per_cell: requested.candidates_per_cell.min(per_cell),
            maximum_neighbors_per_site: requested.maximum_neighbors_per_site.min(neighbors),
            ..requested.clone()
        }
    }
}

#[derive(Clone, Debug)]
pub struct CandidateSettings {
    pub infrastructure_policy: InfrastructurePolicy,
    pub corridor_width_m: f64,
    pub unrestricted_bounding_area: bool,
    pub grid_spacing_m: f64,
    pub cell_size_m: f64,
    /// Generated sites kept per cell near the route. The candidate count follows
    /// the route length: see [`dense_corridor_half_width_m`].
    pub candidates_per_cell: usize,
    pub maximum_neighbors_per_site: usize,
    /// Evaluate every candidate pair within link range instead of only nearby
    /// neighbours and a backbone. Two-terminal minimum-router searches then use
    /// the exact widest-path planner.
    pub exhaustive_screening: bool,
    /// Maximum generated router sites in an installation.
    pub maximum_new_routers: usize,
    /// Maximum links in each client-to-client path.
    pub maximum_hops: usize,
    pub reliability_paths: usize,
    pub parallel_workers: usize,
    pub coarse_sample_step_m: f64,
    pub medium_sample_step_m: f64,
    pub final_sample_step_m: f64,
    pub maximum_link_distance_m: Option<f64>,
    pub refine_radius_m: f64,
    pub refine_step_m: f64,
    pub optimize_heights: bool,
    pub minimum_router_height_m: f64,
    pub maximum_router_height_m: f64,
    pub router_height_step_m: f64,
    pub priority: OptimizationPriority,
    pub road_access_enabled: bool,
    pub maximum_road_distance_m: f64,
    pub road_access_classes: RoadAccessClasses,
}

impl Default for CandidateSettings {
    fn default() -> Self {
        Self {
            infrastructure_policy: InfrastructurePolicy::default(),
            corridor_width_m: 15_000.0,
            unrestricted_bounding_area: false,
            grid_spacing_m: 1_000.0,
            cell_size_m: 2_000.0,
            candidates_per_cell: 3,
            maximum_neighbors_per_site: 16,
            exhaustive_screening: true,
            maximum_new_routers: 6,
            maximum_hops: 7,
            reliability_paths: 2,
            parallel_workers: 0,
            coarse_sample_step_m: 200.0,
            medium_sample_step_m: 50.0,
            final_sample_step_m: CERTIFICATION_MAX_STEP_M,
            maximum_link_distance_m: Some(50_000.0),
            refine_radius_m: 250.0,
            refine_step_m: 50.0,
            optimize_heights: false,
            minimum_router_height_m: 2.0,
            maximum_router_height_m: 10.0,
            router_height_step_m: 1.0,
            priority: OptimizationPriority::default(),
            road_access_enabled: false,
            maximum_road_distance_m: 1_000.0,
            road_access_classes: RoadAccessClasses::default(),
        }
    }
}

#[derive(Debug)]
pub enum OptimizationError {
    InvalidInput(String),
    Rf(RfError),
    CertificationChanged { source: String, target: String },
}

impl fmt::Display for OptimizationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInput(message) => f.write_str(message),
            Self::Rf(error) => error.fmt(f),
            Self::CertificationChanged { source, target } => {
                write!(
                    f,
                    "Terrain or RF inputs changed while preparing profiles for {source}–{target}"
                )
            }
        }
    }
}

impl error::Error for OptimizationError {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            Self::Rf(error) => Some(error),
            _ => None,
        }
    }
}

impl From<RfError> for OptimizationError {
    fn from(error: RfError) -> Self {
        Self::Rf(error)
    }
}

fn compare_scores(left: &[f64], right: &[f64]) -> std::cmp::Ordering {
    left.iter()
        .zip(right)
        .map(|(a, b)| a.partial_cmp(b).unwrap_or_else(|| a.total_cmp(b)))
        .find(|order| order.is_ne())
        .unwrap_or_else(|| left.len().cmp(&right.len()))
}

fn pair(left: usize, right: usize) -> (usize, usize) {
    (left.min(right), left.max(right))
}

fn link_is_clear(link: &LinkResult) -> bool {
    link.los_clear && link.fresnel_clear
}
