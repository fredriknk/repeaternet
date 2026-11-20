use repeaternet_rf_core::RfError;

use crate::{Bounds, Point, TerrainCrs, TerrainError, TerrainLayer, TerrainSource};

/// Regularly spaced, row-major elevations. Row zero is at the minimum Y,
/// matching the Python in-memory terrain.
pub struct ArrayTerrain {
    dtm: Vec<f64>,
    dom: Option<Vec<f64>>,
    columns: usize,
    rows: usize,
    resolution_m: f64,
    bounds: Bounds,
    crs: TerrainCrs,
}

impl ArrayTerrain {
    pub fn new(
        columns: usize,
        rows: usize,
        dtm: Vec<f64>,
        dom: Option<Vec<f64>>,
        origin: Point,
        resolution_m: f64,
        crs: TerrainCrs,
    ) -> Result<Self, TerrainError> {
        if columns == 0 || rows == 0 || columns.checked_mul(rows) != Some(dtm.len()) {
            return Err(TerrainError::InvalidInput(
                "DTM length must match its nonempty dimensions",
            ));
        }
        if dom.as_ref().is_some_and(|dom| dom.len() != dtm.len()) {
            return Err(TerrainError::InvalidInput(
                "DOM and DTM must have identical shapes",
            ));
        }
        if !resolution_m.is_finite() || resolution_m <= 0.0 {
            return Err(TerrainError::InvalidInput(
                "Array resolution must be finite and positive",
            ));
        }
        let bounds = Bounds {
            min_x: origin.x,
            min_y: origin.y,
            max_x: origin.x + (columns - 1) as f64 * resolution_m,
            max_y: origin.y + (rows - 1) as f64 * resolution_m,
        };
        if bounds.as_array().iter().any(|value| !value.is_finite()) {
            return Err(TerrainError::InvalidInput("Array bounds must be finite"));
        }
        Ok(Self {
            dtm,
            dom,
            rows,
            columns,
            resolution_m,
            bounds,
            crs,
        })
    }

    pub fn crs(&self) -> TerrainCrs {
        self.crs
    }

    pub fn bounds(&self) -> Bounds {
        self.bounds
    }

    fn interpolate(&self, point: Point, data: &[f64]) -> f64 {
        let column = (point.x - self.bounds.min_x) / self.resolution_m;
        let row = (point.y - self.bounds.min_y) / self.resolution_m;
        if !(0.0..=(self.columns - 1) as f64).contains(&column)
            || !(0.0..=(self.rows - 1) as f64).contains(&row)
        {
            return f64::NAN;
        }
        let c0 = column.floor() as usize;
        let r0 = row.floor() as usize;
        let c1 = (c0 + 1).min(self.columns - 1);
        let r1 = (r0 + 1).min(self.rows - 1);
        let dx = column - c0 as f64;
        let dy = row - r0 as f64;
        data[r0 * self.columns + c0] * (1.0 - dx) * (1.0 - dy)
            + data[r0 * self.columns + c1] * dx * (1.0 - dy)
            + data[r1 * self.columns + c0] * (1.0 - dx) * dy
            + data[r1 * self.columns + c1] * dx * dy
    }
}

impl TerrainSource for ArrayTerrain {
    fn resolution_m(&self) -> f64 {
        self.resolution_m
    }

    fn has_surface(&self) -> bool {
        self.dom.is_some()
    }

    fn sample(&self, points: &[Point], layer: TerrainLayer) -> Result<Vec<f64>, RfError> {
        let data = match layer {
            TerrainLayer::GroundDtm => &self.dtm,
            TerrainLayer::SurfaceDom => self.dom.as_ref().unwrap_or(&self.dtm),
        };
        Ok(points
            .iter()
            .map(|&point| self.interpolate(point, data))
            .collect())
    }
}
