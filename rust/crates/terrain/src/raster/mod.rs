mod array;
mod geokeys;
mod geotiff;

pub use array::ArrayTerrain;
pub use geotiff::{GeoTiff, RasterMetadata};

use std::path::Path;

use repeaternet_rf_core::RfError;

use crate::{Bounds, Point, TerrainCrs, TerrainError, TerrainLayer, TerrainSource};

/// Ordered DTM and DOM mosaics. The first unmasked pixel wins within its
/// product. Prepend route tiles to supplemental coverage tiles.
pub struct RasterTerrain {
    dtm: Vec<GeoTiff>,
    dom: Vec<GeoTiff>,
    crs: TerrainCrs,
    bounds: Bounds,
    resolution_m: f64,
}

impl RasterTerrain {
    pub fn open<P: AsRef<Path>>(dtm_paths: &[P], dom_paths: &[P]) -> Result<Self, TerrainError> {
        if dtm_paths.is_empty() {
            return Err(TerrainError::InvalidInput(
                "At least one DTM GeoTIFF is required",
            ));
        }
        let open_all = |paths: &[P]| {
            paths
                .iter()
                .map(GeoTiff::open)
                .collect::<Result<Vec<_>, _>>()
        };
        Self::from_tiles(open_all(dtm_paths)?, open_all(dom_paths)?)
    }

    pub fn from_tiles(dtm: Vec<GeoTiff>, dom: Vec<GeoTiff>) -> Result<Self, TerrainError> {
        let first = dtm
            .first()
            .ok_or(TerrainError::InvalidInput(
                "At least one DTM GeoTIFF is required",
            ))?
            .metadata();
        let crs = first.crs;
        let resolution_m = first.resolution_m;
        let mut bounds = first.bounds;
        for tile in dtm.iter().chain(&dom) {
            let actual = tile.metadata().crs;
            if actual != crs {
                return Err(TerrainError::MismatchedCrs {
                    path: tile.path().into(),
                    expected: crs,
                    actual: Box::new(actual),
                });
            }
        }
        for tile in &dtm[1..] {
            bounds = bounds.union(tile.metadata().bounds);
        }
        Ok(Self {
            dtm,
            dom,
            crs,
            bounds,
            resolution_m,
        })
    }

    pub fn crs(&self) -> TerrainCrs {
        self.crs
    }

    pub fn bounds(&self) -> Bounds {
        self.bounds
    }

    pub fn ground_tiles(&self) -> &[GeoTiff] {
        &self.dtm
    }

    pub fn surface_tiles(&self) -> &[GeoTiff] {
        &self.dom
    }

    pub fn sample(&self, points: &[Point], layer: TerrainLayer) -> Result<Vec<f64>, TerrainError> {
        let tiles = match layer {
            TerrainLayer::SurfaceDom if !self.dom.is_empty() => &self.dom,
            _ => &self.dtm,
        };
        let mut samples = vec![None; points.len()];
        for tile in tiles {
            tile.fill_samples(points, &mut samples)?;
            if samples.iter().all(Option::is_some) {
                break;
            }
        }
        Ok(samples
            .into_iter()
            .map(|sample| sample.unwrap_or(f64::NAN))
            .collect())
    }
}

impl TerrainSource for RasterTerrain {
    fn resolution_m(&self) -> f64 {
        self.resolution_m
    }

    fn has_surface(&self) -> bool {
        !self.dom.is_empty()
    }

    fn sample(&self, points: &[Point], layer: TerrainLayer) -> Result<Vec<f64>, RfError> {
        RasterTerrain::sample(self, points, layer)
            .map_err(|error| RfError::Terrain(Box::new(error)))
    }
}
