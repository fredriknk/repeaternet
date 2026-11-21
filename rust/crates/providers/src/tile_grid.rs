//! Shared planner grid and pixel limits.
use crate::{invalid, ProviderError, Result};
use geo::{Area, Intersects, MultiPolygon, Rect, Relate, Validation};
use repeaternet_terrain::{corridor::corridor_geometry, Bounds, Point};
use serde::{Deserialize, Serialize};

const MIB: f64 = 1024.0 * 1024.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Product {
    Dtm,
    Dom,
}

impl Product {
    pub fn name(self) -> &'static str {
        match self {
            Self::Dtm => "dtm",
            Self::Dom => "dom",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct TileLimits {
    pub maximum_tile_area_km2: f64,
    pub maximum_pixels_per_tile: u64,
    pub maximum_tiles: usize,
}

impl Default for TileLimits {
    fn default() -> Self {
        Self {
            maximum_tile_area_km2: 400.0,
            maximum_pixels_per_tile: 4_000_000,
            maximum_tiles: 256,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct PlanOptions {
    pub auto_resolution: bool,
    pub maximum_total_pixels: u64,
}

impl Default for PlanOptions {
    fn default() -> Self {
        Self {
            auto_resolution: true,
            maximum_total_pixels: 50_000_000,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum DownloadArea<'a> {
    Bounds,
    Corridor {
        points: &'a [Point],
        half_width_m: f64,
        extra_points: &'a [Point],
    },
    Geometry(&'a MultiPolygon<f64>),
}

#[derive(Clone, Copy, Debug)]
pub struct DownloadTile {
    pub bounds: Bounds,
    pub width_pixels: u64,
    pub height_pixels: u64,
}

impl DownloadTile {
    pub fn pixel_count(self) -> u64 {
        self.width_pixels * self.height_pixels
    }

    pub fn area_km2(self) -> f64 {
        area_m2(self.bounds) / 1_000_000.0
    }
}

#[derive(Clone, Debug)]
pub struct DownloadPlan {
    pub requested_bounds: Bounds,
    pub requested_resolution_m: f64,
    pub effective_resolution_m: f64,
    pub tiles: Vec<DownloadTile>,
    pub auto_resolution: bool,
    pub pixel_budget: u64,
}

impl DownloadPlan {
    pub fn pixel_count(&self) -> u64 {
        self.tiles.iter().map(|tile| tile.pixel_count()).sum()
    }

    pub fn download_area_km2(&self) -> f64 {
        self.tiles.iter().map(|tile| tile.area_km2()).sum()
    }

    pub fn estimated_raw_mib_per_product(&self) -> f64 {
        self.pixel_count() as f64 * 4.0 / MIB
    }

    pub fn summary(&self, product_count: usize) -> String {
        let mut resolution = format!("{} m", self.effective_resolution_m);
        if self.effective_resolution_m != self.requested_resolution_m {
            resolution.push_str(&format!(" (requested {} m)", self.requested_resolution_m));
        }
        format!(
            "{} tile(s), {} km² downloaded, {resolution}, {:.1} million pixels per product, approximately {} MiB raw",
            self.tiles.len(), grouped(self.download_area_km2()), self.pixel_count() as f64 / 1_000_000.0,
            grouped(self.estimated_raw_mib_per_product() * product_count as f64)
        )
    }
}

fn grouped(value: f64) -> String {
    let digits = format!("{value:.0}");
    let mut grouped = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(c);
    }
    grouped
}

pub(crate) struct TileGrid {
    pub limits: TileLimits,
}

impl TileGrid {
    pub fn plan_download(
        &self,
        bounds: Bounds,
        requested_resolution_m: f64,
        area: DownloadArea<'_>,
        options: PlanOptions,
    ) -> Result<DownloadPlan> {
        validate_bounds(bounds)?;
        if !positive(requested_resolution_m) || options.maximum_total_pixels == 0 {
            return Err(invalid(
                "Requested resolution and maximum total pixels must be positive",
            ));
        }

        let corridor;
        let (geometry, explicit) = match area {
            DownloadArea::Bounds => (None, false),
            DownloadArea::Corridor {
                points,
                half_width_m,
                extra_points,
            } => {
                if points.len() < 2 {
                    return Err(invalid("A route corridor requires at least two points"));
                }
                corridor = corridor_geometry(points, half_width_m, extra_points)?;
                (Some(&corridor), false)
            }
            DownloadArea::Geometry(geometry) => {
                let area = geometry.unsigned_area();
                if !positive(area) || !geometry.is_valid() {
                    return Err(invalid(
                        "Provide one valid download area with positive area",
                    ));
                }
                if area
                    > self.limits.maximum_tiles as f64
                        * self.limits.maximum_tile_area_km2
                        * 1_000_000.0
                {
                    return Err(invalid(
                        "The download exceeds the tile limit. Reduce the coverage area.",
                    ));
                }
                (Some(geometry), true)
            }
        };

        let target_area = geometry.map_or(area_m2(bounds), |geometry| {
            if explicit {
                geometry.unsigned_area()
            } else {
                area_m2(bounds).min(geometry.unsigned_area())
            }
        });
        let mut resolution = requested_resolution_m;
        if options.auto_resolution {
            resolution = nice_resolution(
                resolution.max((target_area / options.maximum_total_pixels as f64).sqrt()),
            );
        }

        for _ in 0..12 {
            let (tiles, count, pixels) =
                self.build_tiles(bounds, resolution, geometry, explicit)?;
            if options.auto_resolution && pixels > options.maximum_total_pixels {
                let next = nice_resolution(
                    resolution * (pixels as f64 / options.maximum_total_pixels as f64).sqrt(),
                );
                if next <= resolution {
                    break;
                }
                resolution = next;
                continue;
            }
            if count == 0 {
                return Err(invalid(
                    "No terrain tiles intersect the requested route corridor",
                ));
            }
            if count > self.limits.maximum_tiles {
                return Err(self.tile_count_error(count, explicit, options.auto_resolution));
            }
            return Ok(DownloadPlan {
                requested_bounds: bounds,
                requested_resolution_m,
                effective_resolution_m: resolution,
                tiles,
                auto_resolution: options.auto_resolution,
                pixel_budget: options.maximum_total_pixels,
            });
        }
        Err(invalid(
            "Could not create a download plan within the pixel budget",
        ))
    }

    fn tile_count_error(&self, count: usize, explicit: bool, auto: bool) -> ProviderError {
        let advice = if explicit {
            "Reduce the coverage area."
        } else if auto {
            "Increase the tile limit or reduce the corridor width."
        } else {
            "Enable automatic resolution or request a coarser raster."
        };
        invalid(format!(
            "The download requires {count} tiles; the configured limit is {}. {advice}",
            self.limits.maximum_tiles
        ))
    }

    fn build_tiles(
        &self,
        bounds: Bounds,
        resolution: f64,
        geometry: Option<&MultiPolygon<f64>>,
        explicit: bool,
    ) -> Result<(Vec<DownloadTile>, usize, u64)> {
        let area_side = (self.limits.maximum_tile_area_km2 * 1_000_000.0).sqrt();
        let pixel_side = (self.limits.maximum_pixels_per_tile as f64).sqrt().floor() * resolution;
        // Preserve the aligned grid for ordinary limits. A one-pixel request can cover an area
        // smaller than its requested resolution when the area cap is tighter.
        let side = area_side.min(pixel_side);
        if !positive(side) {
            return Err(invalid("Cannot align tiles at the requested resolution"));
        }
        let start_x = (bounds.min_x / side).floor() * side;
        let start_y = (bounds.min_y / side).floor() * side;
        let columns = ((bounds.max_x - start_x) / side).ceil() as usize;
        let rows = ((bounds.max_y - start_y) / side).ceil() as usize;
        if rows.saturating_mul(columns) > 5_000_000 {
            return Err(invalid(
                "The download grid is too large. Reduce the area or request a coarser raster.",
            ));
        }
        let mut tiles = Vec::new();
        let mut count = 0;
        let mut pixels = 0u64;
        for row in 0..rows {
            let y = start_y + row as f64 * side;
            for column in 0..columns {
                let x = start_x + column as f64 * side;
                if x >= bounds.max_x || y >= bounds.max_y {
                    continue;
                }
                let tile_bounds = Bounds {
                    min_x: x,
                    min_y: y,
                    max_x: x + side,
                    max_y: y + side,
                };
                if let Some(geometry) = geometry {
                    if !intersects_tile(geometry, tile_bounds, !explicit) {
                        continue;
                    }
                }
                let tile = self.checked_tile(tile_bounds, resolution)?;
                count += 1;
                pixels = pixels.saturating_add(tile.pixel_count());
                if count <= self.limits.maximum_tiles {
                    tiles.push(tile);
                }
            }
        }
        Ok((tiles, count, pixels))
    }

    pub(crate) fn checked_tile(&self, bounds: Bounds, resolution: f64) -> Result<DownloadTile> {
        validate_bounds(bounds)?;
        if !positive(resolution) {
            return Err(invalid("Resolution must be positive"));
        }
        let area = area_m2(bounds) / 1_000_000.0;
        if area > self.limits.maximum_tile_area_km2 + 1e-6 {
            return Err(invalid(format!(
                "Tile area is {area:.1} km²; configured per-tile maximum is {:.1} km²",
                self.limits.maximum_tile_area_km2
            )));
        }
        let width_pixels = pixels_for_axis(bounds.min_x, bounds.max_x, resolution);
        let height_pixels = pixels_for_axis(bounds.min_y, bounds.max_y, resolution);
        if width_pixels
            .checked_mul(height_pixels)
            .is_none_or(|pixels| pixels > self.limits.maximum_pixels_per_tile)
        {
            return Err(invalid("Tile exceeds the configured pixel limit"));
        }
        Ok(DownloadTile {
            bounds,
            width_pixels,
            height_pixels,
        })
    }
}

pub(crate) fn positive(value: f64) -> bool {
    value.is_finite() && value > 0.0
}

fn area_m2(bounds: Bounds) -> f64 {
    (bounds.max_x - bounds.min_x) * (bounds.max_y - bounds.min_y)
}

pub(crate) fn intersects_tile(
    geometry: &MultiPolygon<f64>,
    bounds: Bounds,
    include_boundary: bool,
) -> bool {
    let tile = Rect::new((bounds.min_x, bounds.min_y), (bounds.max_x, bounds.max_y)).to_polygon();
    geometry.intersects(&tile) && (include_boundary || !geometry.relate(&tile).is_touches())
}

fn validate_bounds(bounds: Bounds) -> Result<()> {
    if !bounds.as_array().into_iter().all(f64::is_finite)
        || !positive(area_m2(bounds))
        || bounds.min_x >= bounds.max_x
        || bounds.min_y >= bounds.max_y
    {
        return Err(invalid(
            "Download bounds must have finite positive width and height",
        ));
    }
    Ok(())
}

pub(crate) fn pixels_for_axis(min: f64, max: f64, resolution: f64) -> u64 {
    let cells = (max - min) / resolution;
    let nearest = cells.round();
    // Subtracting large UTM offsets can slightly inflate an integral span
    // on grids whose spacing is a fractional metre.
    let rounding_error = min.abs().max(max.abs()) * f64::EPSILON * 2.0 / resolution;
    let cells = if (cells - nearest).abs() <= rounding_error.min(1e-6) {
        nearest
    } else {
        cells.ceil()
    };
    cells.max(1.0) as u64
}

fn nice_resolution(minimum: f64) -> f64 {
    for choice in [
        1., 2., 5., 10., 15., 20., 25., 30., 50., 75., 100., 150., 200., 250., 500., 1000.,
    ] {
        if choice >= minimum - 1e-9 {
            return choice;
        }
    }
    let magnitude = 10f64.powf(minimum.log10().floor());
    (minimum / magnitude).ceil() * magnitude
}
