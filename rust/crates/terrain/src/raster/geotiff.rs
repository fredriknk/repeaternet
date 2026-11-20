use std::{
    collections::{BTreeMap, HashSet},
    error::Error,
    io,
    path::{Path, PathBuf},
};

use tiff_reader::{
    constants::{TAG_COMPRESSION, TAG_NEW_SUBFILE_TYPE, TAG_SUB_IFDS},
    Ifd, OpenOptions, TagValue, TiffFile, TiffSample,
};

use crate::{Bounds, Point, TerrainCrs, TerrainError};

use super::geokeys;

const WINDOW_SIDE: usize = 256;
const MAX_BLOCK_BYTES: usize = 32 * 1024 * 1024;
const MODEL_PIXEL_SCALE: u16 = 33550;
const MODEL_TIEPOINT: u16 = 33922;
const MODEL_TRANSFORMATION: u16 = 34264;
const GDAL_NODATA: u16 = 42113;
const ORIENTATION: u16 = 274;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RasterMetadata {
    pub crs: TerrainCrs,
    pub width: usize,
    pub height: usize,
    pub bounds: Bounds,
    pub resolution_m: f64,
    /// GDAL order, with the top-left pixel corner as origin.
    pub geotransform: [f64; 6],
    pub nodata: Option<f64>,
    pub has_internal_mask: bool,
}

impl RasterMetadata {
    /// Four pixel-corner coordinates in raster order, without a closing vertex.
    pub fn footprint(self) -> [Point; 4] {
        footprint(self.geotransform, self.width, self.height)
    }
}

/// One local GeoTIFF elevation tile. Pixels are loaded only for touched windows.
pub struct GeoTiff {
    path: PathBuf,
    tiff: TiffFile,
    mask: Option<Ifd>,
    metadata: RasterMetadata,
    inverse_linear: [f64; 4],
    sample_format: u16,
    bits_per_sample: u16,
    sparse_blocks: Option<SparseBlocks>,
}

struct SparseBlocks {
    width: usize,
    height: usize,
    columns: usize,
    indices: HashSet<usize>,
}

pub(super) type RasterResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

impl GeoTiff {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, TerrainError> {
        let path = path.as_ref();
        Self::open_file(path).map_err(|source| TerrainError::Raster {
            path: path.into(),
            source,
        })
    }

    fn open_file(path: &Path) -> RasterResult<Self> {
        for suffix in [".msk", ".aux.xml"] {
            let mut sidecar = path.as_os_str().to_os_string();
            sidecar.push(suffix);
            if Path::new(&sidecar).try_exists()? {
                return invalid(format!("Unsupported GeoTIFF sidecar {suffix}"));
            }
        }
        let tiff = TiffFile::open_with_options(
            path,
            OpenOptions {
                block_cache_bytes: 1024 * 1024,
                block_cache_slots: 16,
                decode_output_bytes: MAX_BLOCK_BYTES,
                ..OpenOptions::default()
            },
        )?;
        let image = tiff.ifd(0)?;
        if subfile_type(image)? != 0 {
            return invalid(
                "Expected a full-resolution elevation image in the first TIFF directory",
            );
        }
        let layout = image.raster_layout()?;
        if layout.samples_per_pixel != 1
            || !matches!(image.photometric_interpretation(), Some(0 | 1))
            || !image.extra_samples()?.is_empty()
        {
            return invalid("Expected a single-band grayscale elevation TIFF");
        }
        let sample_type = (layout.sample_format, layout.bits_per_sample);
        if !matches!(sample_type, (1 | 2, 8 | 16 | 32) | (3, 32 | 64)) {
            return invalid(format!(
                "Unsupported elevation sample format/depth {sample_type:?}"
            ));
        }
        let sparse_blocks = validate_storage(&tiff, image, layout.bytes_per_sample)?;
        let (crs, pixel_is_point) = geokeys::projected_crs(image)?;
        let transform = geotransform(image, pixel_is_point)?;
        let [_, a, b, _, d, e] = transform;
        let determinant = a * e - b * d;
        let inverse_linear = [
            e / determinant,
            -b / determinant,
            -d / determinant,
            a / determinant,
        ];
        let resolution_m = a.hypot(d).min(b.hypot(e));
        if inverse_linear.iter().any(|value| !value.is_finite())
            || !resolution_m.is_finite()
            || resolution_m <= 0.0
        {
            return invalid("Invalid or singular GeoTIFF affine transform");
        }
        let bounds = bounds_of(footprint(transform, layout.width, layout.height))?;
        let nodata = match image.tag(GDAL_NODATA) {
            None => None,
            Some(tag) => {
                let value = tag
                    .value
                    .as_str()
                    .ok_or_else(|| invalid_error("GDAL nodata must be numeric text"))?
                    .trim_matches(|c: char| c.is_whitespace() || c == '\0')
                    .parse::<f64>()
                    .map_err(|_| invalid_error("GDAL nodata must be numeric text"))?;
                // GDAL compares Float32 nodata after converting it to the band's type.
                Some(if sample_type == (3, 32) {
                    f64::from(value as f32)
                } else {
                    value
                })
            }
        };
        let mask = find_mask(&tiff, image)?;
        let metadata = RasterMetadata {
            crs,
            width: layout.width,
            height: layout.height,
            bounds,
            resolution_m,
            geotransform: transform,
            nodata,
            has_internal_mask: mask.is_some(),
        };
        let raster = Self {
            path: path.into(),
            tiff,
            mask,
            metadata,
            inverse_linear,
            sample_format: sample_type.0,
            bits_per_sample: sample_type.1,
            sparse_blocks,
        };
        raster.tiff.read_window_bytes(0, 0, 0, 1, 1)?;
        raster.read_mask(0, 0, 1, 1)?;
        Ok(raster)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn metadata(&self) -> &RasterMetadata {
        &self.metadata
    }

    /// Sample band 1 with pixel-floor semantics. Outside or masked pixels are NaN.
    pub fn sample(&self, points: &[Point]) -> Result<Vec<f64>, TerrainError> {
        let mut samples = vec![None; points.len()];
        self.fill_samples(points, &mut samples)?;
        Ok(samples
            .into_iter()
            .map(|sample| sample.unwrap_or(f64::NAN))
            .collect())
    }

    fn pixel(&self, point: Point) -> Option<(usize, usize)> {
        if !self.metadata.bounds.contains(point) {
            return None;
        }
        let [origin_x, pixel_width, shear_x, origin_y, shear_y, pixel_height] =
            self.metadata.geotransform;
        let [a, b, d, e] = self.inverse_linear;
        let x = point.x - origin_x;
        let y = point.y - origin_y;
        let (column, row) = if shear_x == 0.0 && shear_y == 0.0 {
            (x / pixel_width, y / pixel_height)
        } else {
            (a * x + b * y, d * x + e * y)
        };
        let (column, row) = (column.floor(), row.floor());
        if column >= 0.0
            && column < self.metadata.width as f64
            && row >= 0.0
            && row < self.metadata.height as f64
        {
            Some((row as usize, column as usize))
        } else {
            None
        }
    }

    pub(super) fn fill_samples(
        &self,
        points: &[Point],
        samples: &mut [Option<f64>],
    ) -> Result<(), TerrainError> {
        let mut windows = BTreeMap::<_, Vec<_>>::new();
        for (index, (&point, sample)) in points.iter().zip(samples.iter()).enumerate() {
            if sample.is_none() {
                if let Some((row, column)) = self.pixel(point) {
                    let key = (
                        row / WINDOW_SIDE * WINDOW_SIDE,
                        column / WINDOW_SIDE * WINDOW_SIDE,
                    );
                    windows.entry(key).or_default().push((index, row, column));
                }
            }
        }
        let read = match (self.sample_format, self.bits_per_sample) {
            (1, 8) => Self::fill_window::<u8>,
            (1, 16) => Self::fill_window::<u16>,
            (1, 32) => Self::fill_window::<u32>,
            (2, 8) => Self::fill_window::<i8>,
            (2, 16) => Self::fill_window::<i16>,
            (2, 32) => Self::fill_window::<i32>,
            (3, 32) => Self::fill_window::<f32>,
            (3, 64) => Self::fill_window::<f64>,
            _ => unreachable!("Validated sample type"),
        };
        for points in windows.into_values() {
            read(self, &points, samples).map_err(|source| TerrainError::Raster {
                path: self.path.clone(),
                source,
            })?;
        }
        Ok(())
    }

    fn fill_window<T: TiffSample + Copy + Into<f64>>(
        &self,
        points: &[(usize, usize, usize)],
        samples: &mut [Option<f64>],
    ) -> RasterResult<()> {
        // Clip each group to the requested pixels while keeping the 256 × 256
        // upper bound on temporary window size.
        let (_, row, col) = points[0];
        let (top, left, bottom, right) = points.iter().fold(
            (row, col, row, col),
            |(top, left, bottom, right), &(_, row, col)| {
                (top.min(row), left.min(col), bottom.max(row), right.max(col))
            },
        );
        let rows = bottom - top + 1;
        let cols = right - left + 1;
        // The image has one band. This API copies contiguous TIFF rows and
        // leaves samples in their source type until a point needs conversion.
        let values = self.tiff.read_window::<T>(0, top, left, rows, cols)?;
        let mask = self.read_mask(top, left, rows, cols)?;
        for &(index, row, col) in points {
            let sparse = self.sparse_blocks.as_ref().is_some_and(|blocks| {
                let block = row / blocks.height * blocks.columns + col / blocks.width;
                blocks.indices.contains(&block)
            });
            let value = if sparse {
                self.metadata.nodata.unwrap_or(0.0)
            } else {
                values[[row - top, col - left]].into()
            };
            let valid = match &mask {
                Some(mask) => mask[(row - top) * cols + col - left] != 0,
                None => !self
                    .metadata
                    .nodata
                    .is_some_and(|nodata| value == nodata || (value.is_nan() && nodata.is_nan())),
            };
            if valid {
                samples[index] = Some(value);
            }
        }
        Ok(())
    }

    fn read_mask(
        &self,
        row: usize,
        col: usize,
        rows: usize,
        cols: usize,
    ) -> RasterResult<Option<Vec<u8>>> {
        Ok(self
            .mask
            .as_ref()
            .map(|ifd| {
                self.tiff
                    .read_window_bytes_from_ifd(ifd, row, col, rows, cols)
            })
            .transpose()?)
    }
}

fn validate_storage(
    tiff: &TiffFile,
    image: &Ifd,
    bytes_per_sample: usize,
) -> RasterResult<Option<SparseBlocks>> {
    if image
        .tag(ORIENTATION)
        .is_some_and(|tag| tag.value.as_u16() != Some(1))
    {
        return invalid("Unsupported TIFF orientation");
    }
    if image
        .tag(TAG_COMPRESSION)
        .is_some_and(|tag| tag.value.as_u16().is_none())
        || !matches!(image.compression(), 1 | 5 | 8 | 32773 | 32946 | 50000)
    {
        return invalid(format!(
            "Unsupported TIFF compression {}",
            image.compression()
        ));
    }
    if !matches!(image.predictor(), 1..=3) || !matches!(image.fill_order(), 1 | 2) {
        return invalid("Unsupported TIFF predictor or fill order");
    }
    let (width, height, offsets, counts) = if image.is_tiled() {
        (
            image.tile_width().unwrap_or(0),
            image.tile_height().unwrap_or(0),
            image.tile_offsets(),
            image.tile_byte_counts(),
        )
    } else {
        (
            image.width(),
            image.rows_per_strip().min(image.height()),
            image.strip_offsets(),
            image.strip_byte_counts(),
        )
    };
    let (width, height) = (width as usize, height as usize);
    if width == 0
        || height == 0
        || width
            .checked_mul(height)
            .and_then(|size| size.checked_mul(bytes_per_sample))
            .is_none_or(|size| size > MAX_BLOCK_BYTES)
    {
        return invalid("Invalid TIFF storage block dimensions or block exceeds 32 MiB");
    }
    let columns = (image.width() as usize).div_ceil(width);
    let rows = (image.height() as usize).div_ceil(height);
    let (Some(offsets), Some(counts)) = (offsets, counts) else {
        return invalid("Missing TIFF storage block tables");
    };
    if columns.checked_mul(rows) != Some(offsets.len()) || offsets.len() != counts.len() {
        return invalid("TIFF storage block table size does not match the image");
    }
    let mut indices = HashSet::new();
    for (index, (offset, count)) in offsets.into_iter().zip(counts).enumerate() {
        if offset == 0 && count == 0 {
            indices.insert(index);
        } else if offset == 0
            || count == 0
            || offset
                .checked_add(count)
                .is_none_or(|end| end > tiff.source().len())
        {
            return invalid("Invalid TIFF storage block range");
        }
    }
    Ok((!indices.is_empty()).then_some(SparseBlocks {
        width,
        height,
        columns,
        indices,
    }))
}

fn find_mask(tiff: &TiffFile, image: &Ifd) -> RasterResult<Option<Ifd>> {
    let mut mask = None;
    let mut inspect = |ifd: &Ifd| -> RasterResult<()> {
        // Bit 1 marks a reduced-resolution image and bit 4 a transparency mask.
        if subfile_type(ifd)? & 5 != 4 {
            return Ok(());
        }
        if mask.is_some() {
            return invalid("Multiple full-resolution TIFF masks are unsupported");
        }
        let layout = ifd.raster_layout()?;
        if ifd.width() != image.width()
            || ifd.height() != image.height()
            || ifd.photometric_interpretation() != Some(4)
            || layout.samples_per_pixel != 1
            || layout.sample_format != 1
            || !matches!(layout.bits_per_sample, 1 | 8)
        {
            return invalid("Unsupported full-resolution TIFF mask layout");
        }
        validate_storage(tiff, ifd, layout.bytes_per_sample)?;
        mask = Some(ifd.clone());
        Ok(())
    };
    for ifd in &tiff.ifds()[1..] {
        inspect(ifd)?;
    }
    if image.tag(TAG_SUB_IFDS).is_some() {
        let offsets = image
            .sub_ifd_offsets()
            .ok_or_else(|| invalid_error("Invalid TIFF SubIFDs"))?;
        for offset in offsets {
            inspect(&tiff.read_ifd_at_offset(offset)?)?;
        }
    }
    Ok(mask)
}

fn subfile_type(ifd: &Ifd) -> RasterResult<u32> {
    match ifd.tag(TAG_NEW_SUBFILE_TYPE) {
        None => Ok(0),
        Some(tag) => tag
            .value
            .as_u32()
            .ok_or_else(|| invalid_error("Invalid TIFF subfile type").into()),
    }
}

pub(super) fn doubles(ifd: &Ifd, code: u16) -> RasterResult<Option<&[f64]>> {
    match ifd.tag(code).map(|tag| &tag.value) {
        None => Ok(None),
        Some(TagValue::Double(values)) if values.iter().all(|value| value.is_finite()) => {
            Ok(Some(values))
        }
        _ => invalid(format!("Invalid GeoTIFF DOUBLE tag {code}")),
    }
}

fn geotransform(ifd: &Ifd, pixel_is_point: bool) -> RasterResult<[f64; 6]> {
    let mut transform = match (
        doubles(ifd, MODEL_TRANSFORMATION)?,
        doubles(ifd, MODEL_PIXEL_SCALE)?,
        doubles(ifd, MODEL_TIEPOINT)?,
    ) {
        (Some([a, b, 0.0, x, d, e, 0.0, y, _, _, _, _, 0.0, 0.0, 0.0, 1.0]), None, None) => {
            [*x, *a, *b, *y, *d, *e]
        }
        (None, Some([a, scale_y, _]), Some([col, row, _, x, y, _]))
            if *a > 0.0 && *scale_y > 0.0 =>
        {
            [x - col * a, *a, 0.0, y + row * scale_y, 0.0, -scale_y]
        }
        _ => {
            return invalid(
                "Expected one affine matrix or a positive pixel scale and one tie point",
            )
        }
    };
    if pixel_is_point {
        transform[0] -= 0.5 * (transform[1] + transform[2]);
        transform[3] -= 0.5 * (transform[4] + transform[5]);
    }
    Ok(transform)
}

fn footprint([x, a, b, y, d, e]: [f64; 6], width: usize, height: usize) -> [Point; 4] {
    let (width, height) = (width as f64, height as f64);
    [(0.0, 0.0), (width, 0.0), (width, height), (0.0, height)].map(|(col, row)| Point {
        x: x + a * col + b * row,
        y: y + d * col + e * row,
    })
}

fn bounds_of(points: [Point; 4]) -> RasterResult<Bounds> {
    let first = points[0];
    let mut bounds = Bounds {
        min_x: first.x,
        max_x: first.x,
        min_y: first.y,
        max_y: first.y,
    };
    for point in points {
        if !point.x.is_finite() || !point.y.is_finite() {
            return invalid("GeoTIFF affine transform gives non-finite bounds");
        }
        bounds.min_x = bounds.min_x.min(point.x);
        bounds.min_y = bounds.min_y.min(point.y);
        bounds.max_x = bounds.max_x.max(point.x);
        bounds.max_y = bounds.max_y.max(point.y);
    }
    if bounds.min_x >= bounds.max_x || bounds.min_y >= bounds.max_y {
        return invalid("GeoTIFF affine transform gives degenerate bounds");
    }
    Ok(bounds)
}

pub(super) fn invalid_error(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

pub(super) fn invalid<T>(message: impl Into<String>) -> RasterResult<T> {
    Err(invalid_error(message).into())
}
