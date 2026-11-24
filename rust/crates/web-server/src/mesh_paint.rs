use repeaternet_coverage::{
    ClientRadioProfile,
    live::{self, LiveMode, SweepGrid},
};
use repeaternet_optimizer::{OptimizationError, PaintCoverage, PlanningSite};
use repeaternet_rf_core::{Point, RfSettings, Site, SiteKind};
use repeaternet_terrain::{LonLat, Transformer};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashSet;

use crate::{
    api::{ApiError, ApiResult},
    live_coverage_http::{default_floor, default_radius, default_resolution},
    live_terrain::LiveRaster,
    terrain_http::terrain_invalid,
};

const CELL_M: f64 = 250.0;
const EARTH_RADIUS_M: f64 = 6_378_137.0;
const MAX_INDEX: i32 = 80_149;
const MAX_CELLS: usize = 1_024;
const MAX_SPAN: u32 = 120;
const SWEEP_STEP_M: f64 = 200.0;
pub(crate) const PAINT_COVERAGE_MODEL: &str = "live-two-way-v1";

pub(crate) fn uses_paint(plan: &Value) -> bool {
    plan["mode"] == "mesh"
        && plan["mesh_paint"]["cells"]
            .as_array()
            .is_some_and(|cells| !cells.is_empty())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MeshPaint {
    cells: Vec<[i32; 2]>,
    target_percent: u8,
    minimum_margin_db: f64,
}

impl MeshPaint {
    pub fn read(plan: &Value) -> ApiResult<Option<Self>> {
        let Some(value) = plan.get("mesh_paint") else {
            return Ok(None);
        };
        let paint: Self = serde_json::from_value(value.clone()).map_err(|_| {
            ApiError::invalid("Paint needs grid cells, a target percentage and a handheld margin")
        })?;
        if !(1..=100).contains(&paint.target_percent)
            || !(0.0..=60.0).contains(&paint.minimum_margin_db)
        {
            return Err(ApiError::invalid(
                "Paint target must be 1 to 100% with a handheld margin of 0 to 60 dB",
            ));
        }
        if paint.cells.len() > MAX_CELLS {
            return Err(ApiError::invalid("Paint up to 1,024 coverage cells"));
        }
        let mut seen = HashSet::new();
        for &[x, y] in &paint.cells {
            if !(-MAX_INDEX - 1..=MAX_INDEX).contains(&x)
                || !(-MAX_INDEX - 1..=MAX_INDEX).contains(&y)
                || !seen.insert([x, y])
            {
                return Err(ApiError::invalid(
                    "Paint cells must be unique and within the map grid",
                ));
            }
        }
        for axis in [0, 1] {
            let mut indices = paint.cells.iter().map(|cell| cell[axis]);
            if let Some(first) = indices.next() {
                let (min, max) =
                    indices.fold((first, first), |(min, max), i| (min.min(i), max.max(i)));
                if min.abs_diff(max) >= MAX_SPAN {
                    return Err(ApiError::invalid(
                        "Keep the painted area within 120 grid cells across",
                    ));
                }
            }
        }
        Ok(Some(paint))
    }

    pub fn active(plan: &Value) -> ApiResult<Option<Self>> {
        if !crate::route_inputs::mesh_mode(plan)? {
            return Ok(None);
        }
        Ok(Self::read(plan)?.filter(|paint| !paint.cells.is_empty()))
    }

    fn centres(&self) -> impl Iterator<Item = Point> + '_ {
        self.cells.iter().map(|&[x, y]| Point {
            x: (f64::from(x) + 0.5) * CELL_M,
            y: (f64::from(y) + 0.5) * CELL_M,
        })
    }

    pub fn locations(&self) -> impl Iterator<Item = LonLat> + '_ {
        self.centres().map(|point| LonLat {
            longitude: (point.x / EARTH_RADIUS_M).to_degrees(),
            latitude: (point.y / EARTH_RADIUS_M).sinh().atan().to_degrees(),
        })
    }

    pub fn projected(&self, transform: &Transformer) -> ApiResult<Vec<Point>> {
        self.locations()
            .map(|site| transform.forward(site).map_err(terrain_invalid))
            .collect()
    }

    pub fn distance_from_area(&self, transform: &Transformer, point: Point) -> f64 {
        let Ok(location) = transform.reverse(point) else {
            return f64::INFINITY;
        };
        let latitude = location.latitude.to_radians();
        let x = EARTH_RADIUS_M * location.longitude.to_radians() / CELL_M;
        let y = EARTH_RADIUS_M * latitude.tan().asinh() / CELL_M;
        // Measure to the nearest painted square, including gaps between strokes.
        self.cells
            .iter()
            .map(|&[column, row]| {
                let wrapped_dx = (x - f64::from(column) - 0.5).abs();
                let world = std::f64::consts::TAU * EARTH_RADIUS_M / CELL_M;
                let dx = (wrapped_dx.min(world - wrapped_dx) - 0.5).max(0.0);
                let dy = ((y - f64::from(row) - 0.5).abs() - 0.5).max(0.0);
                dx.hypot(dy)
            })
            .fold(f64::INFINITY, f64::min)
            * CELL_M
            * latitude.cos()
    }

    pub fn handheld(plan: &Value) -> ApiResult<ClientRadioProfile> {
        let client = plan
            .pointer("/coverage/client")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let client: ClientRadioProfile = serde_json::from_value(client).map_err(terrain_invalid)?;
        client.validate().map_err(terrain_invalid)?;
        Ok(client)
    }

    pub fn target_percent(&self) -> u8 {
        self.target_percent
    }

    pub fn measure(
        &self,
        raster: &LiveRaster<'_>,
        site: &PlanningSite,
        rf: &RfSettings,
        client: &repeaternet_rf_core::RadioBudget,
        cells: &[Point],
    ) -> Result<Vec<bool>, OptimizationError> {
        let error = |error: crate::api::ApiError| {
            OptimizationError::InvalidInput(error.detail().to_owned())
        };
        let position = site.rf.position;
        let max_radius = default_radius();
        if cells
            .iter()
            .all(|&point| position.distance_to(point) > max_radius)
        {
            return Ok(vec![false; cells.len()]);
        }
        let radius = cells
            .iter()
            .map(|&point| position.distance_to(point))
            .fold(SWEEP_STEP_M, f64::max)
            .min(max_radius);
        let grid = SweepGrid::new(radius, SWEEP_STEP_M)
            .map_err(|error| OptimizationError::InvalidInput(error.to_string()))?;
        let margins = raster
            .sweep(&site.rf, rf, client, LiveMode::TwoWay, grid)
            .map_err(error)?;
        Ok(cells
            .iter()
            .map(|&point| margins.margin_at_point(point) >= self.minimum_margin_db)
            .collect())
    }

    pub fn certify(
        &self,
        terrain: &LiveRaster<'_>,
        transform: &Transformer,
        site: &PlanningSite,
        rf: &RfSettings,
        client: &repeaternet_rf_core::RadioBudget,
    ) -> Result<Vec<bool>, OptimizationError> {
        let error = |error: ApiError| OptimizationError::InvalidInput(error.detail().to_owned());
        let resolution = default_resolution();
        let location = transform
            .reverse(site.rf.position)
            .map_err(terrain_invalid)
            .map_err(error)?;
        let position = transform
            .forward(location)
            .map_err(terrain_invalid)
            .map_err(error)?;
        let live_site = Site {
            kind: match site.rf.kind {
                SiteKind::Candidate => SiteKind::Router,
                kind => kind,
            },
            antenna_height_m: site.rf.antenna_height_m,
            terminal_clutter: site.rf.terminal_clutter,
            ..Site::new(&site.rf.id, position.x, position.y)
        };
        let Some(live_site) = terrain
            .coverage_site(live_site, rf, resolution)
            .map_err(error)?
        else {
            return Ok(vec![false; self.cells.len()]);
        };
        let grid = SweepGrid::new(default_radius(), resolution)
            .map_err(terrain_invalid)
            .map_err(error)?;
        let polar = terrain
            .sweep(&live_site, rf, client, LiveMode::TwoWay, grid)
            .map_err(error)?;
        let floor = default_floor();
        let raster = live::resample(&polar, location, transform, floor)
            .map_err(terrain_invalid)
            .map_err(error)?;
        Ok(self
            .centres()
            .map(|point| raster.margin_at_point(point, floor) >= self.minimum_margin_db)
            .collect())
    }

    pub fn result(&self, coverage: Option<&PaintCoverage>) -> Value {
        let total = self.cells.len();
        let uncovered: Vec<_> = coverage.map_or_else(
            || self.cells.clone(),
            |coverage| coverage.uncovered.iter().map(|&i| self.cells[i]).collect(),
        );
        let covered = total - uncovered.len();
        json!({
            "cell_count": total,
            "covered_cells": covered,
            "covered_fraction": covered as f64 / total as f64,
            "target_percent": self.target_percent,
            "minimum_margin_db": self.minimum_margin_db,
            "target_met": coverage.is_some_and(PaintCoverage::target_met),
            "uncovered_cells": uncovered,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{dashboard_defaults, route_inputs_changed};

    #[test]
    fn a_valid_area_can_replace_clients_for_search() {
        let plan = json!({
            "mode": "mesh",
            "mesh_paint": {"cells": [[4748, 33571]], "target_percent": 90, "minimum_margin_db": 5.0}
        });
        let parsed = crate::route_inputs::parse(plan.clone(), &dashboard_defaults()).unwrap();
        assert_eq!(parsed.client_count, 0);
        let paint = parsed.paint.unwrap();
        let point = paint.locations().next().unwrap();
        assert!((59.0..61.0).contains(&point.latitude));
        assert!((10.0..11.0).contains(&point.longitude));
        assert_eq!(
            paint.result(None)["uncovered_cells"],
            plan["mesh_paint"]["cells"]
        );
        let mut malformed = plan;
        malformed["mesh_paint"]["cells"] = json!([[4748, 33571], [4748, 33571]]);
        assert!(MeshPaint::read(&malformed).is_err());
    }

    #[test]
    fn only_active_paint_and_the_handheld_budget_affect_the_route_identity() {
        let previous = json!({
            "mode": "mesh",
            "mesh_paint": {"cells": [[4748, 33571]], "target_percent": 90, "minimum_margin_db": 5.0},
            "coverage": {"mode": "two_way", "client": {"height_agl_m": 1.5, "name": "Handheld"}}
        });
        let mut edited = previous.clone();
        edited["coverage"]["mode"] = json!("downlink");
        edited["coverage"]["client"]["name"] = json!("Radio");
        assert!(!route_inputs_changed(&previous, &edited));
        edited["coverage"]["client"]["height_agl_m"] = json!(3.0);
        assert!(route_inputs_changed(&previous, &edited));
        edited["mode"] = json!("endpoints");
        let mut endpoints = edited.clone();
        endpoints.as_object_mut().unwrap().remove("mesh_paint");
        endpoints.as_object_mut().unwrap().remove("coverage");
        assert!(!route_inputs_changed(&endpoints, &edited));
    }
}
