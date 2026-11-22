//! Rust engines share RF core's terrain sampling and curved-Earth geometry.

pub(crate) mod clutter;
pub(crate) mod delta;
pub(crate) mod multi_edge;

use repeaternet_rf_core::{
    RfError, Site, TerrainLayer, TerrainProfile, TerrainSource, knife_edge_v,
};
use serde_json::{Value, json};

use crate::dataset::{ProfileEnd, ProfileRecord};

pub(crate) const RFCORE: &str = "rfcore_bullington";
pub(crate) const DELTA: &str = "p526_delta_bullington";
pub(crate) const DEYGOUT: &str = "deygout3";
pub(crate) const GIOVANELI: &str = "giovaneli";
pub(crate) const EPSTEIN_PETERSON: &str = "epstein_peterson";
pub(crate) const IDS: [&str; 5] = [RFCORE, DELTA, DEYGOUT, GIOVANELI, EPSTEIN_PETERSON];

/// The undirected profile is stored in a local Cartesian frame with a=(0,0),
/// b=(distance,0). RF core supplies sample points in either direction.
pub(crate) struct StoredTerrain<'a> {
    pub profile: &'a ProfileRecord,
    pub surface: bool,
}

impl TerrainSource for StoredTerrain<'_> {
    fn resolution_m(&self) -> f64 {
        self.profile.resolution_m
    }

    fn has_surface(&self) -> bool {
        self.surface && self.profile.dom.is_some()
    }

    fn sample(
        &self,
        points: &[repeaternet_rf_core::Point],
        layer: TerrainLayer,
    ) -> Result<Vec<f64>, RfError> {
        let ground = &self.profile.dtm;
        let heights = match layer {
            TerrainLayer::SurfaceDom if self.surface => self.profile.dom.as_ref().unwrap_or(ground),
            _ => ground,
        };
        let last = heights.len() - 1;
        Ok(points
            .iter()
            .map(|point| {
                let index = (point.x / self.profile.distance_m).clamp(0.0, 1.0) * last as f64;
                let lo = index.floor() as usize;
                let hi = (lo + 1).min(last);
                heights[lo] + index.fract() * (heights[hi] - heights[lo])
            })
            .collect())
    }
}

pub(crate) fn site(end: &ProfileEnd, at_m: f64, antenna_m: f64) -> Site {
    let mut site = Site::new(&end.node, at_m, 0.0);
    site.ground_elevation_m = end.ground_m;
    site.surface_elevation_m = end.surface_m;
    site.antenna_height_m = antenna_m;
    site
}

pub(crate) struct Clearance {
    pub direct_v: f64,
    pub los: bool,
}

impl Clearance {
    pub fn from_profile(profile: &TerrainProfile, freq: f64) -> Self {
        let x = &profile.distances_m;
        let last = x.len() - 1;
        let mut direct_v = f64::NEG_INFINITY;
        let mut los = true;
        for (i, &d) in x.iter().enumerate().take(last).skip(1) {
            let clearance = profile.clearance_m[i];
            direct_v = direct_v.max(knife_edge_v(-clearance, d, x[last] - d, freq));
            los &= clearance >= 0.0;
        }
        Self { direct_v, los }
    }
}

pub(crate) fn bullington_diag(obstacles: &[repeaternet_rf_core::DiffractionObstacle]) -> Value {
    json!({
        "edges": obstacles.iter().map(|edge| {
            json!({"distance_m": edge.distance_m, "v": edge.v})
        }).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests;
