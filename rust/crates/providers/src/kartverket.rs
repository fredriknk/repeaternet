use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use geo::{Area, BooleanOps, BoundingRect, Intersects, MultiPolygon, Rect, Relate, Validation};
use repeaternet_terrain::{corridor::corridor_geometry, Bounds, GeoTiff, Point, TerrainCrs};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::{Builder, TempDir};

use crate::{invalid, response_error, transport, ProviderError, Result};

const PROVIDER: &str = "Kartverket";
const MIB: f64 = 1024.0 * 1024.0;
const BODY_LIMIT: u64 = 512 * 1024 * 1024;
const DEFAULT_SERVICES: &str = include_str!("kartverket_wcs.json");

#[derive(Clone, Debug, Deserialize)]
pub struct WcsService {
    pub dtm_endpoint: String,
    pub dom_endpoint: String,
    pub dtm_coverage: String,
    pub dom_coverage: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct WcsConfig {
    pub services: BTreeMap<String, WcsService>,
}

impl WcsConfig {
    pub fn from_json(json: &str) -> Result<Self> {
        serde_json::from_str(json)
            .map_err(|error| invalid(format!("Invalid WCS configuration: {error}")))
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        Self::from_json(&fs::read_to_string(path)?)
    }
}

impl Default for WcsConfig {
    fn default() -> Self {
        Self::from_json(DEFAULT_SERVICES).expect("embedded WCS configuration")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

#[derive(Debug, Serialize)]
pub struct TerrainEstimate {
    pub crs: String,
    pub summary: String,
    pub tile_count: usize,
    pub pixel_count: u64,
    pub download_area_km2: f64,
    pub requested_resolution_m: f64,
    pub effective_resolution_m: f64,
    pub estimated_raw_mib: f64,
    pub estimated_disk_mib: f64,
    pub available_disk_mib: f64,
    pub cache_hits: usize,
    pub cache_total: usize,
    pub include_dom: bool,
}

#[derive(Debug, Serialize)]
pub struct CoverageTerrainEstimate {
    pub crs: String,
    pub missing_area_km2: f64,
    pub download_area_km2: f64,
    pub tile_count: usize,
    pub requested_resolution_m: f64,
    pub effective_resolution_m: f64,
    pub estimated_raw_mib: f64,
    pub estimated_disk_mib: f64,
    pub cache_hits: usize,
    pub cache_total: usize,
    pub include_dom: bool,
}

#[derive(Debug)]
pub struct CoverageTerrainPlan {
    pub dtm: Option<DownloadPlan>,
    pub dom: Option<DownloadPlan>,
    pub estimate: CoverageTerrainEstimate,
}

#[derive(Clone, Copy, Debug)]
pub struct DownloadTiming {
    pub retry_base: Duration,
    pub retry_cap: Duration,
    pub inter_tile_pause: Duration,
}

impl Default for DownloadTiming {
    fn default() -> Self {
        Self {
            retry_base: Duration::from_secs(2),
            retry_cap: Duration::from_secs(60),
            inter_tile_pause: Duration::from_millis(500),
        }
    }
}

#[derive(Clone, Debug)]
pub struct WcsRequest<'a> {
    pub endpoint: &'a str,
    pub params: BTreeMap<&'static str, String>,
}

impl WcsRequest<'_> {
    fn cache_digest(&self) -> String {
        let entries = self
            .params
            .iter()
            .map(|(&k, v)| {
                let v = if matches!(k, "width" | "height") {
                    v.clone()
                } else {
                    transport::py_repr(v)
                };
                format!("({}, {v})", transport::py_repr(k))
            })
            .collect::<Vec<_>>()
            .join(", ");
        let serialized = format!("({}, [{entries}])", transport::py_repr(self.endpoint));
        crate::lower_hex(&Sha256::digest(serialized.as_bytes()))
    }
}

pub struct KartverketProvider {
    client: Client,
    services: WcsConfig,
    cache: PathBuf,
    limits: TileLimits,
    timing: DownloadTiming,
}

impl KartverketProvider {
    pub fn new(cache: impl Into<PathBuf>, services: WcsConfig, limits: TileLimits) -> Result<Self> {
        Self::with_client(
            cache,
            services,
            limits,
            transport::client(PROVIDER, 15, 300)?,
        )
    }

    pub fn with_client(
        cache: impl Into<PathBuf>,
        services: WcsConfig,
        limits: TileLimits,
        client: Client,
    ) -> Result<Self> {
        if !positive(limits.maximum_tile_area_km2 * 1_000_000.0)
            || limits.maximum_pixels_per_tile == 0
            || limits.maximum_tiles == 0
        {
            return Err(invalid(
                "Tile area, pixel and count limits must be positive",
            ));
        }
        Ok(Self {
            client,
            services,
            cache: cache.into(),
            limits,
            timing: DownloadTiming::default(),
        })
    }

    pub fn with_timing(mut self, timing: DownloadTiming) -> Self {
        self.timing = timing;
        self
    }

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
                    "No WCS tiles intersect the requested route corridor",
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

    fn checked_tile(&self, bounds: Bounds, resolution: f64) -> Result<DownloadTile> {
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

    pub fn request(
        &self,
        product: Product,
        crs: TerrainCrs,
        bounds: Bounds,
        resolution: f64,
    ) -> Result<WcsRequest<'_>> {
        let tile = self.checked_tile(bounds, resolution)?;
        let crs = crs.to_string();
        let service =
            self.services.services.get(&crs).ok_or_else(|| {
                invalid(format!("No configured Kartverket WCS service for {crs}"))
            })?;
        let (endpoint, coverage) = match product {
            Product::Dtm => (&service.dtm_endpoint, &service.dtm_coverage),
            Product::Dom => (&service.dom_endpoint, &service.dom_coverage),
        };
        let bbox = bounds.as_array().map(transport::py_float).join(",");
        Ok(WcsRequest {
            endpoint,
            params: BTreeMap::from([
                ("service", "WCS".into()),
                ("version", "1.0.0".into()),
                ("request", "GetCoverage".into()),
                ("coverage", coverage.clone()),
                ("format", "GeoTIFF".into()),
                ("crs", crs),
                ("bbox", bbox),
                ("width", tile.width_pixels.to_string()),
                ("height", tile.height_pixels.to_string()),
            ]),
        })
    }

    fn destination(&self, product: Product, request: &WcsRequest<'_>) -> PathBuf {
        let digest = request.cache_digest();
        self.cache.join(format!(
            "kartverket_{}_{}.tif",
            product.name(),
            &digest[..20]
        ))
    }

    fn existing_cache_paths(&self, product: Product, request: &WcsRequest<'_>) -> Vec<PathBuf> {
        let canonical = self.destination(product, request);
        if !self.cache.is_dir() {
            return vec![];
        }
        let mut existing = Vec::new();
        if canonical.is_file() {
            existing.push(canonical);
        }
        // The Python planner emits float coordinates. Direct callers could also pass an
        // integer or a mixed tuple, which changes the legacy key without changing the raster.
        let coordinates: Vec<_> = request.params["bbox"].split(',').collect();
        let integral: Vec<_> = coordinates
            .iter()
            .enumerate()
            .filter_map(|(index, coordinate)| {
                coordinate
                    .strip_suffix(".0")
                    .map(|integer| (index, integer))
            })
            .collect();
        let mut legacy = request.clone();
        for mask in 1..(1 << integral.len()) {
            let mut bbox = coordinates.clone();
            for (bit, &(index, integer)) in integral.iter().enumerate() {
                if mask & (1 << bit) != 0 {
                    bbox[index] = integer;
                }
            }
            legacy.params.insert("bbox", bbox.join(","));
            let path = self.destination(product, &legacy);
            if path.is_file() {
                existing.push(path);
            }
        }
        existing
    }

    fn validated_cache_path(
        &self,
        product: Product,
        request: &WcsRequest<'_>,
        raster: RasterRequest,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Option<PathBuf>> {
        for path in self.existing_cache_paths(product, request) {
            match validate_download(
                &path,
                raster.crs,
                raster.bounds,
                raster.resolution,
                cancelled,
            ) {
                Ok(()) => return Ok(Some(path)),
                Err(ProviderError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(ProviderError::InvalidResponse { .. }) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(None)
    }

    pub fn cache_path(
        &self,
        product: Product,
        crs: TerrainCrs,
        bounds: Bounds,
        resolution: f64,
    ) -> Result<PathBuf> {
        let request = self.request(product, crs, bounds, resolution)?;
        Ok(self.destination(product, &request))
    }

    /// Estimates count files present. Fetch validates each hit before using it.
    pub fn cached_tile_count(
        &self,
        product: Product,
        crs: TerrainCrs,
        plan: &DownloadPlan,
    ) -> Result<usize> {
        plan.tiles.iter().try_fold(0, |count, tile| {
            let request = self.request(product, crs, tile.bounds, plan.effective_resolution_m)?;
            Ok(count + usize::from(!self.existing_cache_paths(product, &request).is_empty()))
        })
    }

    pub fn estimate(
        &self,
        crs: TerrainCrs,
        plan: &DownloadPlan,
        include_dom: bool,
        disk_directory: &Path,
    ) -> Result<TerrainEstimate> {
        let product_count = 1 + usize::from(include_dom);
        let mut cache_hits = self.cached_tile_count(Product::Dtm, crs, plan)?;
        if include_dom {
            cache_hits += self.cached_tile_count(Product::Dom, crs, plan)?;
        }
        let raw = plan.estimated_raw_mib_per_product() * product_count as f64;
        Ok(TerrainEstimate {
            crs: crs.to_string(),
            summary: plan.summary(product_count),
            tile_count: plan.tiles.len(),
            pixel_count: plan.pixel_count(),
            download_area_km2: plan.download_area_km2(),
            requested_resolution_m: plan.requested_resolution_m,
            effective_resolution_m: plan.effective_resolution_m,
            estimated_raw_mib: raw,
            estimated_disk_mib: raw * 2.0,
            available_disk_mib: fs2::available_space(disk_directory)? as f64 / MIB,
            cache_hits,
            cache_total: plan.tiles.len() * product_count,
            include_dom,
        })
    }

    /// Plan on the union of the missing product footprints, then select tiles
    /// for each product independently. An empty polygon represents no missing
    /// area. A DOM geometry, including an empty one, enables include_dom.
    pub fn plan_coverage_extension(
        &self,
        crs: TerrainCrs,
        missing_dtm: &MultiPolygon<f64>,
        missing_dom: Option<&MultiPolygon<f64>>,
        resolution: f64,
    ) -> Result<CoverageTerrainPlan> {
        if !positive(resolution) {
            return Err(invalid("Resolution must be positive"));
        }
        if !missing_dtm.is_valid() || missing_dom.is_some_and(|geometry| !geometry.is_valid()) {
            return Err(invalid("Missing terrain geometry must be valid"));
        }
        let missing = missing_dom.map_or_else(|| missing_dtm.clone(), |dom| missing_dtm.union(dom));
        let missing_area = missing.unsigned_area();
        let mut estimate = CoverageTerrainEstimate {
            crs: crs.to_string(),
            missing_area_km2: missing_area / 1_000_000.0,
            download_area_km2: 0.0,
            tile_count: 0,
            requested_resolution_m: resolution,
            effective_resolution_m: resolution,
            estimated_raw_mib: 0.0,
            estimated_disk_mib: 0.0,
            cache_hits: 0,
            cache_total: 0,
            include_dom: missing_dom.is_some(),
        };
        let mut dtm = None;
        let mut dom = None;
        if let Some(bounds) = missing.bounding_rect().filter(|_| missing_area > 0.0) {
            let [min, max] = [bounds.min(), bounds.max()];
            let common = self.plan_download(
                Bounds {
                    min_x: min.x,
                    min_y: min.y,
                    max_x: max.x,
                    max_y: max.y,
                },
                resolution,
                DownloadArea::Geometry(&missing),
                PlanOptions::default(),
            )?;
            estimate.tile_count = common.tiles.len();
            estimate.download_area_km2 = common.download_area_km2();
            estimate.effective_resolution_m = common.effective_resolution_m;
            for (product, geometry, output) in [
                (Product::Dtm, Some(missing_dtm), &mut dtm),
                (Product::Dom, missing_dom, &mut dom),
            ] {
                let Some(geometry) = geometry else {
                    continue;
                };
                let mut plan = common.clone();
                plan.tiles
                    .retain(|tile| intersects_tile(geometry, tile.bounds, false));
                if !plan.tiles.is_empty() {
                    estimate.estimated_raw_mib += plan.estimated_raw_mib_per_product();
                    estimate.cache_hits += self.cached_tile_count(product, crs, &plan)?;
                    estimate.cache_total += plan.tiles.len();
                    *output = Some(plan);
                }
            }
            estimate.estimated_disk_mib = estimate.estimated_raw_mib * 2.0;
        }
        Ok(CoverageTerrainPlan { dtm, dom, estimate })
    }

    pub fn fetch_plan(
        &self,
        product: Product,
        crs: TerrainCrs,
        plan: &DownloadPlan,
        cancelled: &dyn Fn() -> bool,
        mut progress: impl FnMut(usize, usize, bool),
    ) -> Result<Vec<PathBuf>> {
        let mut paths = Vec::with_capacity(plan.tiles.len());
        let mut previous_download = false;
        for (index, tile) in plan.tiles.iter().enumerate() {
            transport::check(cancelled)?;
            let resolution = plan.effective_resolution_m;
            let request = self.request(product, crs, tile.bounds, resolution)?;
            let raster = RasterRequest {
                crs,
                bounds: tile.bounds,
                resolution,
            };
            let hit = self.validated_cache_path(product, &request, raster, cancelled)?;
            let cached = hit.is_some();
            if previous_download && !cached {
                transport::wait(self.timing.inter_tile_pause, cancelled)?;
            }
            progress(index + 1, plan.tiles.len(), cached);
            transport::check(cancelled)?;
            let path = if let Some(path) = hit {
                path
            } else {
                self.download(
                    &request,
                    self.destination(product, &request),
                    raster,
                    cancelled,
                    |_, _| {},
                )?
            };
            paths.push(path);
            previous_download = !cached;
        }
        Ok(paths)
    }

    pub fn fetch(
        &self,
        product: Product,
        crs: TerrainCrs,
        bounds: Bounds,
        resolution: f64,
        cancelled: &dyn Fn() -> bool,
        progress: impl FnMut(u64, u64),
    ) -> Result<PathBuf> {
        transport::check(cancelled)?;
        let request = self.request(product, crs, bounds, resolution)?;
        let path = self.destination(product, &request);
        let raster = RasterRequest {
            crs,
            bounds,
            resolution,
        };
        if let Some(hit) = self.validated_cache_path(product, &request, raster, cancelled)? {
            return Ok(hit);
        }
        self.download(&request, path, raster, cancelled, progress)
    }

    fn download(
        &self,
        request: &WcsRequest<'_>,
        path: PathBuf,
        raster: RasterRequest,
        cancelled: &dyn Fn() -> bool,
        mut progress: impl FnMut(u64, u64),
    ) -> Result<PathBuf> {
        fs::create_dir_all(&self.cache)?;
        for attempt in 0..5 {
            transport::check(cancelled)?;
            let mut retry_after = None;
            let result = (|| {
                let response = self
                    .client
                    .get(request.endpoint)
                    .query(&request.params)
                    .header(
                        "User-Agent",
                        transport::user_agent("Kartverket terrain client"),
                    )
                    .send()
                    .map_err(|source| ProviderError::Transport {
                        provider: PROVIDER,
                        source,
                    })?;
                if !response.status().is_success() {
                    retry_after = transport::retry_seconds(&response, false);
                    return Err(ProviderError::Http {
                        provider: PROVIDER,
                        status: response.status().as_u16(),
                    });
                }
                let mut temporary = Builder::new()
                    .prefix(".kartverket-")
                    .suffix(".part")
                    .tempfile_in(&self.cache)?;
                transport::stream(
                    PROVIDER,
                    response,
                    temporary.as_file_mut(),
                    BODY_LIMIT,
                    cancelled,
                    &mut progress,
                )?;
                validate_download(
                    temporary.path(),
                    raster.crs,
                    raster.bounds,
                    raster.resolution,
                    cancelled,
                )?;
                temporary.as_file().sync_all()?;
                transport::check(cancelled)?;
                temporary.persist(&path).map_err(|error| error.error)?;
                Ok(())
            })();
            transport::check(cancelled)?;
            match result {
                Ok(()) => return Ok(path),
                Err(error) => {
                    let retry = match &error {
                        ProviderError::Http { status, .. } => {
                            !(400..500).contains(status) || matches!(status, 408 | 429)
                        }
                        ProviderError::Transport { .. } | ProviderError::InvalidResponse { .. } => {
                            true
                        }
                        _ => false,
                    };
                    if !retry || attempt == 4 {
                        return Err(error);
                    }
                    let backoff = self
                        .timing
                        .retry_base
                        .saturating_mul(2u32.pow(attempt))
                        .as_secs_f64();
                    let seconds = backoff
                        .max(retry_after.unwrap_or(0.0))
                        .min(self.timing.retry_cap.as_secs_f64());
                    transport::wait(Duration::from_secs_f64(seconds), cancelled)?;
                }
            }
        }
        unreachable!("the final attempt returns")
    }
}

#[derive(Clone, Copy)]
struct RasterRequest {
    crs: TerrainCrs,
    bounds: Bounds,
    resolution: f64,
}

fn positive(value: f64) -> bool {
    value.is_finite() && value > 0.0
}

fn area_m2(bounds: Bounds) -> f64 {
    (bounds.max_x - bounds.min_x) * (bounds.max_y - bounds.min_y)
}

fn intersects_tile(geometry: &MultiPolygon<f64>, bounds: Bounds, include_boundary: bool) -> bool {
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

fn pixels_for_axis(min: f64, max: f64, resolution: f64) -> u64 {
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

fn validate_download(
    path: &Path,
    crs: TerrainCrs,
    bounds: Bounds,
    resolution: f64,
    cancelled: &dyn Fn() -> bool,
) -> Result<()> {
    transport::check(cancelled)?;
    if fs::metadata(path)?.len() > BODY_LIMIT {
        return Err(response_error(PROVIDER, "GeoTIFF exceeds the size limit"));
    }
    let raster =
        GeoTiff::open(path).map_err(|error| response_error(PROVIDER, error.to_string()))?;
    let metadata = raster.metadata();
    if metadata.crs != crs {
        return Err(response_error(
            PROVIDER,
            "downloaded terrain has an unexpected CRS",
        ));
    }
    let tolerance = (2.0 * resolution).max(1.0);
    if metadata
        .bounds
        .as_array()
        .into_iter()
        .zip(bounds.as_array())
        .any(|(a, b)| (a - b).abs() > tolerance)
    {
        return Err(response_error(
            PROVIDER,
            "downloaded terrain bounds do not match the requested tile",
        ));
    }
    let [x, a, b, y, d, e] = metadata.geotransform;
    let width = pixels_for_axis(bounds.min_x, bounds.max_x, resolution);
    let height = pixels_for_axis(bounds.min_y, bounds.max_y, resolution);
    let expected_x = (bounds.max_x - bounds.min_x) / width as f64;
    let expected_y = (bounds.max_y - bounds.min_y) / height as f64;
    if metadata.width as u64 != width
        || metadata.height as u64 != height
        || (a.hypot(d) - expected_x).abs() > expected_x * 0.01
        || (b.hypot(e) - expected_y).abs() > expected_y * 0.01
    {
        return Err(response_error(
            PROVIDER,
            "downloaded terrain does not match the requested resolution",
        ));
    }
    // Sample each bounded window to check decoding and nodata without allocating the full tile.
    let mut valid = false;
    let mut points = Vec::with_capacity(128 * 128);
    for row in (0..metadata.height).step_by(128) {
        for column in (0..metadata.width).step_by(128) {
            transport::check(cancelled)?;
            points.clear();
            for j in row..(row + 128).min(metadata.height) {
                for i in column..(column + 128).min(metadata.width) {
                    let i = i as f64 + 0.5;
                    let j = j as f64 + 0.5;
                    points.push(Point {
                        x: x + a * i + b * j,
                        y: y + d * i + e * j,
                    });
                }
            }
            let samples = raster
                .sample(&points)
                .map_err(|error| response_error(PROVIDER, error.to_string()))?;
            valid |= samples.into_iter().any(f64::is_finite);
        }
    }
    transport::check(cancelled)?;
    if !valid {
        return Err(response_error(
            PROVIDER,
            "downloaded terrain contains no valid elevation data",
        ));
    }
    Ok(())
}

/// A generation of validated cache tiles copied to a private sibling directory.
pub struct StagedGeneration(TempDir);

impl StagedGeneration {
    pub fn copy_from_cache(
        generation_root: &Path,
        dtm: &[PathBuf],
        dom: &[PathBuf],
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Self> {
        transport::check(cancelled)?;
        fs::create_dir_all(generation_root)?;
        let stage = Builder::new()
            .prefix(".staging-")
            .tempdir_in(generation_root)?;
        for (product, sources) in [(Product::Dtm, dtm), (Product::Dom, dom)] {
            if sources.is_empty() {
                continue;
            }
            let output = stage.path().join(product.name());
            fs::create_dir(&output)?;
            for (i, source) in sources.iter().enumerate() {
                transport::check(cancelled)?;
                let mut source = fs::File::open(source)?;
                let mut destination = fs::File::create(output.join(format!("tile-{i:04}.tif")))?;
                let mut buffer = [0; 64 * 1024];
                loop {
                    transport::check(cancelled)?;
                    let size = source.read(&mut buffer)?;
                    if size == 0 {
                        break;
                    }
                    destination.write_all(&buffer[..size])?;
                }
                destination.sync_all()?;
            }
        }
        transport::check(cancelled)?;
        Ok(Self(stage))
    }

    pub fn path(&self) -> &Path {
        self.0.path()
    }

    /// Rename both products together under an unused name in the generation root.
    /// The caller holds its project commit lock and updates the active generation afterwards.
    pub fn publish(self, name: &str, cancelled: &dyn Fn() -> bool) -> Result<PathBuf> {
        let name_path = Path::new(name);
        if name_path.file_name() != Some(name_path.as_os_str()) || name.contains(['/', '\\']) {
            return Err(invalid("Generation name must be a single path component"));
        }
        transport::check(cancelled)?;
        let destination = self.path().parent().expect("stage parent").join(name);
        match destination.symlink_metadata() {
            Ok(_) => return Err(invalid("Terrain generation already exists")),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        transport::check(cancelled)?;
        fs::rename(self.path(), &destination)?;
        Ok(destination)
    }
}
