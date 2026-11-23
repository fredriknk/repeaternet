use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use geo::{Area, BooleanOps, BoundingRect, MultiPolygon, Validation};
use repeaternet_terrain::{Bounds, GeoTiff, Point, TerrainCrs};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::{Builder, TempDir};

use crate::{invalid, response_error, transport, ProviderError, Result};

const PROVIDER: &str = "Kartverket";
const MIB: f64 = 1024.0 * 1024.0;
const BODY_LIMIT: u64 = 512 * 1024 * 1024;

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
        crate::sources::legacy_kartverket_services()
    }
}

use crate::tile_grid::{intersects_tile, pixels_for_axis, positive, TileGrid};
pub use crate::tile_grid::{
    DownloadArea, DownloadPlan, DownloadTile, PlanOptions, Product, TileLimits,
};

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
        resolution: f64,
        area: DownloadArea<'_>,
        options: PlanOptions,
    ) -> Result<DownloadPlan> {
        TileGrid {
            limits: self.limits,
        }
        .plan_download(bounds, resolution, area, options)
    }

    pub fn request(
        &self,
        product: Product,
        crs: TerrainCrs,
        bounds: Bounds,
        resolution: f64,
    ) -> Result<WcsRequest<'_>> {
        let tile = TileGrid {
            limits: self.limits,
        }
        .checked_tile(bounds, resolution)?;
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

/// A generation of validated cache tiles placed in a private sibling directory.
pub struct StagedGeneration(TempDir);

impl StagedGeneration {
    /// Stage cache tiles for a project. Tiles are hard links into the shared
    /// cache, so a project takes no extra disk space and keeps its terrain if
    /// the cache entry is later replaced or cleared. Cache tiles are only ever
    /// replaced by atomic rename, never rewritten in place, so a link never
    /// changes under the project.
    pub fn link_from_cache(
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
                link_or_copy(source, &output.join(format!("tile-{i:04}.tif")), cancelled)?;
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

/// Hard-link a cache tile into a project, or copy it when linking is not
/// possible (for example when `RF_PLANNER_CACHE_DIR` is on another volume).
fn link_or_copy(source: &Path, target: &Path, cancelled: &dyn Fn() -> bool) -> Result<()> {
    // Only link a plain file. On Unix `hard_link` may link a symlink itself,
    // which the terrain reader and project deletion reject, so copy the bytes
    // the symlink points to instead.
    let plain = fs::symlink_metadata(source).is_ok_and(|meta| meta.file_type().is_file());
    if plain && fs::hard_link(source, target).is_ok() {
        return Ok(());
    }
    let mut source = fs::File::open(source)?;
    let mut destination = fs::File::create_new(target)?;
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
    Ok(())
}
