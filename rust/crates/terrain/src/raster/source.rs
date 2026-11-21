//! Windowed sampling of upstream elevation rasters.

use std::{collections::BTreeMap, fmt, path::PathBuf, sync::Arc};

use tiff_reader::{constants::TAG_SUB_IFDS, Ifd, OpenOptions, RasterLayout, TiffFile, TiffSample};

use crate::{Bounds, LonLat, Point, TerrainCrs, TerrainError, Transformer};

use super::geotiff::{
    elevation_layout, find_mask, geotransform, invalid, nodata, subfile_type, RasterResult,
    SparseBlocks,
};
use super::{geokeys, TiffSource};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceCrs {
    Geographic(u16),
    Projected(TerrainCrs),
}

impl SourceCrs {
    pub fn from_epsg(code: u16) -> Result<Self, TerrainError> {
        match code {
            4326 | 4258 | 4269 | 4283 | 4619 | 4167 => Ok(Self::Geographic(code)),
            _ => TerrainCrs::from_epsg(code).map(Self::Projected),
        }
    }

    pub fn transformer(self) -> Result<Option<Transformer>, TerrainError> {
        match self {
            Self::Geographic(_) => Ok(None),
            Self::Projected(crs) => Transformer::new(crs).map(Some),
        }
    }
}

impl fmt::Display for SourceCrs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Projected(crs) => crs.fmt(f),
            Self::Geographic(code) => write!(f, "EPSG:{code}"),
        }
    }
}

pub struct SourceRaster {
    tiff: TiffFile,
    image: Ifd,
    mask: Option<Ifd>,
    layout: RasterLayout,
    sparse: Option<SparseBlocks>,
    crs: SourceCrs,
    transform: [f64; 6],
    nodata: Option<f64>,
    resolution_m: f64,
}

fn raster_error(source: Box<dyn std::error::Error + Send + Sync>) -> TerrainError {
    TerrainError::Raster {
        path: PathBuf::from("upstream GeoTIFF"),
        source,
    }
}

fn ground_resolution(crs: SourceCrs, [_, a, b, _, d, e]: [f64; 6], latitude: f64) -> f64 {
    let (x, y) = match crs {
        SourceCrs::Projected(_) => (1.0, 1.0),
        SourceCrs::Geographic(_) => (111_320.0 * latitude.to_radians().cos(), 111_132.0),
    };
    (a * x).hypot(d * y).max((b * x).hypot(e * y))
}

fn raster_crs(image: &Ifd) -> RasterResult<(SourceCrs, bool)> {
    if let Some((code, point)) = geokeys::geographic_crs(image)? {
        Ok((SourceCrs::Geographic(code), point))
    } else {
        let (crs, point) = geokeys::projected_crs(image)?;
        Ok((SourceCrs::Projected(crs), point))
    }
}

impl SourceRaster {
    pub fn open(
        source: Arc<dyn TiffSource>,
        desired_resolution_m: f64,
        latitude: f64,
    ) -> Result<Self, TerrainError> {
        Self::open_inner(source, desired_resolution_m, latitude).map_err(raster_error)
    }

    fn open_inner(
        source: Arc<dyn TiffSource>,
        desired_resolution_m: f64,
        latitude: f64,
    ) -> RasterResult<Self> {
        let tiff = TiffFile::from_source_with_options(
            source,
            OpenOptions {
                block_cache_bytes: 24 * 1024 * 1024,
                block_cache_slots: 64,
                decode_output_bytes: 32 * 1024 * 1024,
                ..OpenOptions::default()
            },
        )?;
        let base = tiff.ifd(0)?;
        if subfile_type(base)? != 0 {
            return invalid("Expected a full-resolution TIFF image");
        }
        let base_layout = elevation_layout(&tiff, base)?;
        let (crs, point) = raster_crs(base)?;
        let base_transform = geotransform(base, point)?;
        validate_transform(crs, base_transform)?;
        let mut transform = base_transform;
        let mut resolution = ground_resolution(crs, transform, latitude);
        let mut image = base.clone();
        let mut select = |overview: &Ifd| -> RasterResult<()> {
            if subfile_type(overview)? & 5 != 1
                || overview.width() == 0
                || overview.height() == 0
                || overview.width() >= base.width()
                || overview.height() >= base.height()
            {
                return Ok(());
            }
            let mut candidate = base_transform;
            let scale_x = f64::from(base.width()) / f64::from(overview.width());
            let scale_y = f64::from(base.height()) / f64::from(overview.height());
            for i in [1, 4] {
                candidate[i] *= scale_x;
            }
            for i in [2, 5] {
                candidate[i] *= scale_y;
            }
            if [33550, 33922, 34264]
                .into_iter()
                .any(|tag| overview.tag(tag).is_some())
            {
                let overview_point = if overview.tag(34735).is_some() {
                    let (overview_crs, overview_point) = raster_crs(overview)?;
                    if overview_crs != crs {
                        return invalid("TIFF overview uses a different CRS");
                    }
                    overview_point
                } else {
                    point
                };
                candidate = geotransform(overview, overview_point)?;
            }
            let candidate_resolution = ground_resolution(crs, candidate, latitude);
            if candidate_resolution > resolution && candidate_resolution <= desired_resolution_m {
                image = overview.clone();
                transform = candidate;
                resolution = candidate_resolution;
            }
            Ok(())
        };
        for overview in &tiff.ifds()[1..] {
            select(overview)?;
        }
        if base.tag(TAG_SUB_IFDS).is_some() {
            for offset in base
                .sub_ifd_offsets()
                .ok_or_else(|| super::geotiff::invalid_error("Invalid TIFF SubIFDs"))?
            {
                select(&tiff.read_ifd_at_offset(offset)?)?;
            }
        }
        validate_transform(crs, transform)?;
        if !resolution.is_finite() || resolution <= 0.0 {
            return invalid("Invalid GeoTIFF ground resolution");
        }
        let (layout, sparse) = if image.width() == base.width() {
            base_layout
        } else {
            elevation_layout(&tiff, &image)?
        };
        let sample_type = (layout.sample_format, layout.bits_per_sample);
        let nodata = nodata(&image, sample_type)?.or(nodata(base, sample_type)?);
        let mask = find_mask(&tiff, &image, true)?;
        Ok(Self {
            tiff,
            image,
            mask,
            layout,
            sparse,
            crs,
            transform,
            nodata,
            resolution_m: resolution,
        })
    }

    pub fn crs(&self) -> SourceCrs {
        self.crs
    }

    pub fn resolution_m(&self) -> f64 {
        self.resolution_m
    }

    pub fn matches_grid(
        &self,
        crs: TerrainCrs,
        bounds: Bounds,
        width: usize,
        height: usize,
    ) -> bool {
        let [west, dx, shear_x, north, shear_y, dy] = self.transform;
        self.crs == SourceCrs::Projected(crs)
            && self.layout.width == width
            && self.layout.height == height
            && dx > 0.0
            && dy < 0.0
            && shear_x == 0.0
            && shear_y == 0.0
            && [
                west,
                north + dy * height as f64,
                west + dx * width as f64,
                north,
            ]
            .into_iter()
            .zip(bounds.as_array())
            .all(|(actual, expected)| (actual - expected).abs() < 1e-6)
    }

    /// Read elevations at source-coordinate points using bilinear interpolation.
    /// Invalid pixels contribute no weight. Points outside the raster are NaN.
    pub fn sample(&self, points: &[Point]) -> Result<Vec<f32>, TerrainError> {
        self.sample_inner(points).map_err(raster_error)
    }

    fn sample_inner(&self, points: &[Point]) -> RasterResult<Vec<f32>> {
        const WINDOW: usize = 128;
        let [x, a, b, y, d, e] = self.transform;
        let det = a * e - b * d;
        let (width, height) = (self.layout.width, self.layout.height);
        let mut output = vec![f32::NAN; points.len()];
        let mut groups = BTreeMap::<_, Vec<_>>::new();
        for (index, point) in points.iter().enumerate() {
            let (dx, dy) = (point.x - x, point.y - y);
            let (col, row) = ((e * dx - b * dy) / det, (a * dy - d * dx) / det);
            if !(0.0..width as f64).contains(&col) || !(0.0..height as f64).contains(&row) {
                continue;
            }
            let col = (col - 0.5).clamp(0.0, (width - 1) as f64);
            let row = (row - 0.5).clamp(0.0, (height - 1) as f64);
            groups
                .entry((row as usize / WINDOW, col as usize / WINDOW))
                .or_default()
                .push((index, row, col));
        }
        for group in groups.values() {
            let (top, left, bottom, right) = group.iter().fold(
                (height, width, 0, 0),
                |(top, left, bottom, right), &(_, row, col)| {
                    (
                        top.min(row as usize),
                        left.min(col as usize),
                        bottom.max(row.ceil() as usize),
                        right.max(col.ceil() as usize),
                    )
                },
            );
            let cols = right - left + 1;
            let values = self.read_window_inner(top, left, bottom - top + 1, cols)?;
            for &(index, row, col) in group {
                let (r, c) = (row as usize, col as usize);
                let (dy, dx) = (row.fract(), col.fract());
                let mut sum = 0.0;
                let mut weight = 0.0;
                for (source_row, wy) in [(r, 1.0 - dy), (row.ceil() as usize, dy)] {
                    for (source_col, wx) in [(c, 1.0 - dx), (col.ceil() as usize, dx)] {
                        let value = values[(source_row - top) * cols + source_col - left];
                        if value.is_finite() {
                            sum += f64::from(value) * wx * wy;
                            weight += wx * wy;
                        }
                    }
                }
                if weight > 0.0 {
                    output[index] = (sum / weight) as f32;
                }
            }
        }
        Ok(output)
    }

    pub fn read_window(
        &self,
        top: usize,
        left: usize,
        rows: usize,
        cols: usize,
    ) -> Result<Vec<f32>, TerrainError> {
        self.read_window_inner(top, left, rows, cols)
            .map_err(raster_error)
    }

    fn read_window_inner(
        &self,
        top: usize,
        left: usize,
        rows: usize,
        cols: usize,
    ) -> RasterResult<Vec<f32>> {
        let read = match (self.layout.sample_format, self.layout.bits_per_sample) {
            (1, 8) => Self::read_typed::<u8>,
            (1, 16) => Self::read_typed::<u16>,
            (1, 32) => Self::read_typed::<u32>,
            (2, 8) => Self::read_typed::<i8>,
            (2, 16) => Self::read_typed::<i16>,
            (2, 32) => Self::read_typed::<i32>,
            (3, 32) => Self::read_typed::<f32>,
            (3, 64) => Self::read_typed::<f64>,
            _ => unreachable!("validated sample format"),
        };
        read(self, top, left, rows, cols)
    }

    fn read_typed<T: TiffSample + Into<f64> + Copy>(
        &self,
        top: usize,
        left: usize,
        rows: usize,
        cols: usize,
    ) -> RasterResult<Vec<f32>> {
        let values = self
            .tiff
            .read_window_from_ifd::<T>(&self.image, top, left, rows, cols)?;
        let mask = self
            .mask
            .as_ref()
            .map(|mask| {
                self.tiff
                    .read_window_bytes_from_ifd(mask, top, left, rows, cols)
            })
            .transpose()?;
        Ok(values
            .iter()
            .enumerate()
            .map(|(i, &value)| {
                let row = top + i / cols;
                let col = left + i % cols;
                let value = if self
                    .sparse
                    .as_ref()
                    .is_some_and(|blocks| blocks.contains(row, col))
                {
                    self.nodata.unwrap_or(0.0)
                } else {
                    value.into()
                };
                let valid = mask
                    .as_ref()
                    .map_or(self.nodata != Some(value), |mask| mask[i] != 0);
                if valid && value.is_finite() {
                    value as f32
                } else {
                    f32::NAN
                }
            })
            .collect())
    }

    /// Transform WGS84 points once per raster before sampling.
    pub fn sample_wgs84(&self, points: &[LonLat]) -> Result<Vec<f32>, TerrainError> {
        let transform = self.crs.transformer()?;
        let center_x = self.transform[0] + self.transform[1] * self.layout.width as f64 / 2.0;
        let points: Vec<_> = points
            .iter()
            .map(|&location| match &transform {
                Some(transform) => transform.forward(location).unwrap_or(Point {
                    x: f64::NAN,
                    y: f64::NAN,
                }),
                None => Point {
                    x: center_x + (location.longitude - center_x + 180.0).rem_euclid(360.0) - 180.0,
                    y: location.latitude,
                },
            })
            .collect();
        self.sample(&points)
    }
}

fn validate_transform(crs: SourceCrs, [_, a, b, _, d, e]: [f64; 6]) -> RasterResult<()> {
    let determinant = a * e - b * d;
    if !determinant.is_finite()
        || determinant == 0.0
        || matches!(crs, SourceCrs::Geographic(_)) && (b != 0.0 || d != 0.0)
    {
        return invalid("Invalid affine transform or rotated geographic GeoTIFF");
    }
    Ok(())
}
