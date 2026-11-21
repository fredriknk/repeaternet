//! Offline terrain primitives in projected metre coordinates.

pub mod coordinates;
pub mod corridor;
pub mod profile;
mod raster;

pub use coordinates::{norway_utm_epsg, LonLat, TerrainCrs, Transformer};
pub use raster::{
    write_elevation_tiff, ArrayTerrain, BytesSource, FileSource, GeoTiff, RasterMetadata,
    RasterTerrain, SourceCrs, SourceRaster, TiffError, TiffSource,
};
pub use repeaternet_rf_core::{Point, TerrainLayer, TerrainSource};

use std::{error::Error, fmt, path::PathBuf};

#[derive(Debug)]
pub enum TerrainError {
    InvalidInput(&'static str),
    UnsupportedCrs(u16),
    Projection(proj4rs::errors::Error),
    Raster {
        path: PathBuf,
        source: Box<dyn Error + Send + Sync>,
    },
    MismatchedCrs {
        path: PathBuf,
        expected: TerrainCrs,
        actual: Box<TerrainCrs>,
    },
}

impl fmt::Display for TerrainError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInput(message) => f.write_str(message),
            Self::UnsupportedCrs(epsg) => write!(f, "Unsupported terrain CRS EPSG:{epsg}"),
            Self::Projection(error) => write!(f, "Coordinate transformation failed: {error}"),
            Self::Raster { path, source } => write!(f, "{}: {source}", path.display()),
            Self::MismatchedCrs {
                path,
                expected,
                actual,
            } => {
                write!(f, "{} uses {actual}, expected {expected}", path.display())
            }
        }
    }
}

impl Error for TerrainError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Projection(error) => Some(error),
            Self::Raster { source, .. } => Some(source.as_ref()),
            _ => None,
        }
    }
}

/// Axis-aligned projected bounds, in metres.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bounds {
    pub min_x: f64,
    pub min_y: f64,
    pub max_x: f64,
    pub max_y: f64,
}

impl Bounds {
    pub fn as_array(self) -> [f64; 4] {
        [self.min_x, self.min_y, self.max_x, self.max_y]
    }

    pub fn contains(self, point: Point) -> bool {
        point.x >= self.min_x
            && point.x <= self.max_x
            && point.y >= self.min_y
            && point.y <= self.max_y
    }

    fn union(self, other: Self) -> Self {
        Self {
            min_x: self.min_x.min(other.min_x),
            min_y: self.min_y.min(other.min_y),
            max_x: self.max_x.max(other.max_x),
            max_y: self.max_y.max(other.max_y),
        }
    }
}
