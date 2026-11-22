use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Condvar, Mutex, OnceLock, Weak,
    },
    time::{Duration, Instant},
};

use geo::{Area, BooleanOps, BoundingRect, Contains, MultiPolygon, Validation};
use repeaternet_terrain::{
    write_elevation_tiff, Bounds, GeoTiff, LonLat, Point, SourceRaster, TerrainCrs, Transformer,
};
use reqwest::{
    blocking::{Client, RequestBuilder, Response},
    header::{HeaderMap, HeaderName, HeaderValue, CONTENT_TYPE},
    redirect::Policy,
};
use serde::{Deserialize, Serialize};

use crate::{
    invalid, response_error,
    tile_grid::{intersects_tile, TileGrid},
    transport, ProviderError, Result,
};

use super::{
    atomic_write, hash, range,
    schema::{expand_header, valid_url},
    tile_footprint, wcs, Coverage, DownloadArea, DownloadPlan, DownloadTile, MissingTile,
    PlanOptions, Product, ProductDefinition, ProviderRegistry, SourceChoice, SourceInfo,
    TerrainProvider, TileLimits,
};

pub(super) const PROVIDER: &str = "Terrain source";
const MIB: f64 = 1024.0 * 1024.0;
const SAMPLE_BATCH: usize = 32_768;
const DOM_UNAVAILABLE: &str = "No surface terrain source covers this area";

pub type DownloadCancellation = Arc<dyn Fn() -> bool + Send + Sync>;

#[derive(Clone)]
pub(super) struct FetchContext {
    cancelled: DownloadCancellation,
    deadline: Option<Instant>,
    bytes: Arc<AtomicU64>,
}

impl FetchContext {
    #[cfg(test)]
    pub(super) fn test_context() -> Self {
        Self {
            cancelled: Arc::new(|| false),
            deadline: None,
            bytes: Arc::default(),
        }
    }

    fn expired(&self) -> bool {
        self.deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
    }

    pub fn check(&self) -> Result<()> {
        transport::check(self.cancelled.as_ref())?;
        if self.expired() {
            return Err(response_error(PROVIDER, "source probe exceeded 30 seconds"));
        }
        Ok(())
    }

    pub fn send(&self, request: RequestBuilder) -> Result<Response> {
        self.check()?;
        let request = if let Some(deadline) = self.deadline {
            request.timeout(deadline.saturating_duration_since(Instant::now()))
        } else {
            request
        };
        request.send().map_err(|source| ProviderError::Transport {
            provider: PROVIDER,
            source: source.without_url(),
        })
    }

    pub fn stream(&self, response: Response, output: &mut impl Write, limit: u64) -> Result<()> {
        let mut last = 0;
        transport::stream(
            PROVIDER,
            response,
            output,
            limit,
            &|| (self.cancelled)() || self.expired(),
            |done, _| {
                self.bytes.fetch_add(done - last, Ordering::Relaxed);
                last = done;
            },
        )
    }
}

pub(super) fn check_tiff_type(response: &Response) -> Result<()> {
    if response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            let v = v.to_ascii_lowercase();
            v.contains("html") || v.contains("xml")
        })
    {
        return Err(response_error(
            PROVIDER,
            "expected a GeoTIFF, received HTML or XML",
        ));
    }
    Ok(())
}

pub struct TerrainDownloader {
    registry: Arc<ProviderRegistry>,
    cache: PathBuf,
    grid: TileGrid,
    client: Client,
}

#[derive(Debug)]
pub struct SourcePlan {
    pub dtm: Option<DownloadPlan>,
    pub dom: Option<DownloadPlan>,
    common: Option<DownloadPlan>,
    choice: SourceChoice,
    requested_resolution_m: f64,
    missing_area_km2: Option<f64>,
    dom_unavailable_reason: Option<&'static str>,
}

#[derive(Debug, Serialize)]
pub struct SourceEstimate {
    pub crs: String,
    pub summary: String,
    pub tile_count: usize,
    pub pixel_count: u64,
    pub download_area_km2: f64,
    pub requested_resolution_m: f64,
    pub effective_resolution_m: f64,
    pub estimated_raw_mib: f64,
    /// Raw Float32 size of the composed tiles absent from the cache.
    pub estimated_uncached_raw_mib: f64,
    pub estimated_disk_mib: f64,
    pub available_disk_mib: f64,
    pub cache_hits: usize,
    pub cache_total: usize,
    pub include_dom: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub missing_area_km2: Option<f64>,
    pub sources: Vec<EstimateSource>,
    pub dom_unavailable_reason: Option<&'static str>,
    pub uncovered_area_km2: f64,
    pub warnings: Vec<String>,
    pub registry_fingerprint: String,
}

#[derive(Debug, Serialize)]
pub struct EstimateSource {
    #[serde(flatten)]
    pub source: SourceInfo,
    pub tiles: usize,
}

#[derive(Debug)]
pub struct TileProgress<'a> {
    pub index: usize,
    pub count: usize,
    pub cached: bool,
    pub provider: Option<&'a str>,
}

#[derive(Default, Debug)]
pub struct FetchedTiles {
    pub paths: Vec<PathBuf>,
    pub warnings: Vec<String>,
    pub providers_used: BTreeSet<String>,
    /// Terrain response body bytes read from upstream, including retries.
    pub bytes_fetched: u64,
}

#[derive(Debug, Default, Serialize)]
pub struct ProbeResult {
    pub ok: bool,
    pub crs: Option<String>,
    pub resolution_m: Option<f64>,
    pub valid_fraction: f64,
    pub min_m: Option<f32>,
    pub max_m: Option<f32>,
    pub mean_m: Option<f64>,
    pub elapsed_ms: u64,
    pub bytes_fetched: u64,
    pub message: String,
}

impl TerrainDownloader {
    pub fn new(
        registry: Arc<ProviderRegistry>,
        cache: impl Into<PathBuf>,
        limits: TileLimits,
    ) -> Result<Self> {
        let redirects = Policy::custom(|attempt| {
            if attempt.previous().len() >= 5 {
                return attempt.error("too many terrain source redirects");
            }
            if valid_url(attempt.url().as_str())
                && attempt
                    .previous()
                    .first()
                    .is_some_and(|start| start.origin() == attempt.url().origin())
            {
                attempt.follow()
            } else {
                attempt.error("terrain source redirected to a different origin")
            }
        });
        let client = transport::client_builder(10, 60)
            .redirect(redirects)
            .user_agent(transport::user_agent("terrain data"))
            .build()
            .map_err(|source| ProviderError::Transport {
                provider: PROVIDER,
                source,
            })?;
        Ok(Self {
            registry,
            cache: cache.into(),
            grid: TileGrid { limits },
            client,
        })
    }

    pub fn fingerprint(&self) -> &str {
        self.registry.fingerprint()
    }

    pub fn source_info(&self, product: Product, used: &BTreeSet<String>) -> Vec<SourceInfo> {
        self.registry
            .terrain()
            .iter()
            .filter(|provider| used.contains(&provider.id))
            .map(|provider| SourceInfo::new(provider, product))
            .collect()
    }

    #[allow(clippy::too_many_arguments)]
    pub fn plan_download(
        &self,
        crs: TerrainCrs,
        bounds: Bounds,
        requested: f64,
        area: DownloadArea<'_>,
        options: PlanOptions,
        include_dom: bool,
        choice: SourceChoice,
    ) -> Result<SourcePlan> {
        self.registry.check_choice(&choice)?;
        require_registered(crs)?;
        let transform = Transformer::new(crs)?;
        let (common, dtm_covers) =
            self.select_grid(&transform, bounds, requested, area, options, &choice)?;
        if !dtm_covers && choice.source != "auto" && !choice.fallback {
            return Err(invalid(format!(
                "Terrain source {} has no ground terrain in this area",
                choice.source
            )));
        }
        let (dom, dom_unavailable_reason) = if include_dom {
            let mut dom = common.clone();
            self.retain_covered(&transform, Product::Dom, &choice, &mut dom)?;
            if dom.tiles.is_empty() {
                (None, Some(DOM_UNAVAILABLE))
            } else {
                (Some(dom), None)
            }
        } else {
            (None, None)
        };
        Ok(SourcePlan {
            dtm: Some(common.clone()),
            dom,
            common: Some(common),
            choice,
            requested_resolution_m: requested,
            missing_area_km2: None,
            dom_unavailable_reason,
        })
    }

    pub fn plan_coverage_extension(
        &self,
        crs: TerrainCrs,
        missing_dtm: &MultiPolygon<f64>,
        missing_dom: Option<&MultiPolygon<f64>>,
        resolution: f64,
        choice: SourceChoice,
    ) -> Result<SourcePlan> {
        self.registry.check_choice(&choice)?;
        if !missing_dtm.is_valid() || missing_dom.is_some_and(|geometry| !geometry.is_valid()) {
            return Err(invalid("Missing terrain geometry must be valid"));
        }
        let missing = missing_dom.map_or_else(|| missing_dtm.clone(), |dom| missing_dtm.union(dom));
        let area = missing.unsigned_area();
        let mut result = SourcePlan {
            dtm: None,
            dom: None,
            common: None,
            choice,
            requested_resolution_m: resolution,
            missing_area_km2: Some(area / 1_000_000.0),
            dom_unavailable_reason: None,
        };
        if area <= 0.0 {
            return Ok(result);
        }
        require_registered(crs)?;
        let bounds = missing.bounding_rect().expect("positive area has bounds");
        let bounds = Bounds {
            min_x: bounds.min().x,
            min_y: bounds.min().y,
            max_x: bounds.max().x,
            max_y: bounds.max().y,
        };
        let area = DownloadArea::Geometry(&missing);
        let options = PlanOptions::default();
        let transform = Transformer::new(crs)?;
        let (common, _) = self.select_grid(
            &transform,
            bounds,
            resolution,
            area,
            options,
            &result.choice,
        )?;
        let mut dtm = common.clone();
        dtm.tiles
            .retain(|tile| intersects_tile(missing_dtm, tile.bounds, false));
        if !dtm.tiles.is_empty() {
            result.dtm = Some(dtm);
        }
        if let Some(missing_dom) = missing_dom {
            let mut dom = common.clone();
            dom.tiles
                .retain(|tile| intersects_tile(missing_dom, tile.bounds, false));
            let needed = !dom.tiles.is_empty();
            self.retain_covered(&transform, Product::Dom, &result.choice, &mut dom)?;
            if !dom.tiles.is_empty() {
                result.dom = Some(dom);
            } else if needed {
                result.dom_unavailable_reason = Some(DOM_UNAVAILABLE);
            }
        }
        result.common = Some(common);
        Ok(result)
    }

    fn finest(
        &self,
        transform: &Transformer,
        plan: &DownloadPlan,
        choice: &SourceChoice,
    ) -> Result<(f64, bool)> {
        let mut resolution = f64::INFINITY;
        for tile in &plan.tiles {
            let footprint = tile_footprint(transform, tile.bounds)?;
            if let Some(provider) = self.registry.rank(Product::Dtm, &footprint, choice).first() {
                resolution = resolution.min(provider.resolution_m);
            }
        }
        Ok(if resolution.is_finite() {
            (resolution, true)
        } else {
            (0.0, false)
        })
    }

    fn select_grid(
        &self,
        transform: &Transformer,
        bounds: Bounds,
        requested: f64,
        area: DownloadArea<'_>,
        options: PlanOptions,
        choice: &SourceChoice,
    ) -> Result<(DownloadPlan, bool)> {
        let initial = self.grid.plan_download(bounds, requested, area, options);
        let prescan;
        let reference = match &initial {
            Ok(grid) => grid,
            Err(_) => {
                // A fine request can exceed the tile limit before the provider's
                // coarser native resolution is applied. Rank an area-capped grid first.
                let limits = self.grid.limits;
                let area_side = (limits.maximum_tile_area_km2 * 1_000_000.0).sqrt();
                let pixel_side = (limits.maximum_pixels_per_tile as f64).sqrt().floor();
                let resolution = requested.max(area_side / pixel_side);
                prescan = self.grid.plan_download(bounds, resolution, area, options);
                match &prescan {
                    Ok(grid) => grid,
                    Err(_) => return initial.map(|grid| (grid, false)),
                }
            }
        };
        let (finest, dtm_covers) = self.finest(transform, reference, choice)?;
        let effective = requested.max(finest);
        let mut plan = match initial {
            Ok(grid) if effective <= grid.effective_resolution_m => grid,
            Err(error) if effective <= requested => return Err(error),
            _ => self.grid.plan_download(bounds, effective, area, options)?,
        };
        plan.requested_resolution_m = requested;
        Ok((plan, dtm_covers))
    }

    fn retain_covered(
        &self,
        transform: &Transformer,
        product: Product,
        choice: &SourceChoice,
        plan: &mut DownloadPlan,
    ) -> Result<()> {
        let mut selected = Vec::new();
        for tile in plan.tiles.iter().copied() {
            let footprint = tile_footprint(transform, tile.bounds)?;
            if !self.registry.rank(product, &footprint, choice).is_empty() {
                selected.push(tile);
            }
        }
        plan.tiles = selected;
        Ok(())
    }

    pub fn estimate(
        &self,
        crs: TerrainCrs,
        plan: &SourcePlan,
        directory: &Path,
    ) -> Result<SourceEstimate> {
        let transform = Transformer::new(crs)?;
        let mut grouped = BTreeMap::<_, EstimateSource>::new();
        let mut cache_hits = 0;
        let mut cache_total = 0;
        let mut raw = 0.0;
        let mut uncached_raw = 0.0;
        let mut uncovered = 0.0;
        let mut warnings = BTreeSet::new();
        for (product, grid) in [(Product::Dtm, &plan.dtm), (Product::Dom, &plan.dom)] {
            let Some(grid) = grid else {
                continue;
            };
            raw += grid.estimated_raw_mib_per_product();
            for &tile in &grid.tiles {
                let footprint = tile_footprint(&transform, tile.bounds)?;
                let ranked = self.registry.rank(product, &footprint, &plan.choice);
                let prepared = prepare_sources(&ranked, product);
                let cache = self.composed_path(crs, product, tile, &prepared);
                cache_total += 1;
                if cache.is_file() {
                    cache_hits += 1;
                    warnings.extend(read_report(&cache).warnings);
                } else {
                    uncached_raw += tile.pixel_count() as f64 * 4.0 / MIB;
                }
                if let Some(first) = ranked.first() {
                    grouped
                        .entry((product, first.id.clone()))
                        .or_insert_with(|| EstimateSource {
                            source: SourceInfo::new(first, product),
                            tiles: 0,
                        })
                        .tiles += 1;
                }
                if product == Product::Dtm
                    && !ranked
                        .iter()
                        .any(|p| matches!(p.coverage, Coverage::Global))
                {
                    let area = footprint.unsigned_area();
                    if area > 0.0 {
                        let remainder = ranked.iter().fold(footprint, |remaining, p| {
                            remaining.difference(&p.coverage.geometry())
                        });
                        uncovered +=
                            tile.area_km2() * (remainder.unsigned_area() / area).clamp(0.0, 1.0);
                    }
                }
            }
        }
        let common = plan.common.as_ref();
        let mut sources: Vec<_> = grouped.into_values().collect();
        sources.sort_by(|a, b| {
            a.source
                .product
                .cmp(&b.source.product)
                .then(b.tiles.cmp(&a.tiles))
                .then(a.source.id.cmp(&b.source.id))
        });
        Ok(SourceEstimate {
            crs: crs.to_string(),
            summary: common.map_or_else(
                || "No additional terrain tiles needed".into(),
                |grid| {
                    grid.summary(usize::from(plan.dtm.is_some()) + usize::from(plan.dom.is_some()))
                },
            ),
            tile_count: common.map_or(0, |grid| grid.tiles.len()),
            pixel_count: common.map_or(0, DownloadPlan::pixel_count),
            download_area_km2: common.map_or(0.0, DownloadPlan::download_area_km2),
            requested_resolution_m: plan.requested_resolution_m,
            effective_resolution_m: common.map_or(plan.requested_resolution_m, |grid| {
                grid.effective_resolution_m
            }),
            estimated_raw_mib: raw,
            estimated_uncached_raw_mib: uncached_raw,
            estimated_disk_mib: raw * 2.0,
            available_disk_mib: fs2::available_space(directory)? as f64 / MIB,
            cache_hits,
            cache_total,
            include_dom: plan.dom.is_some(),
            missing_area_km2: plan.missing_area_km2,
            sources,
            dom_unavailable_reason: plan.dom_unavailable_reason,
            uncovered_area_km2: uncovered,
            warnings: warnings.into_iter().collect(),
            registry_fingerprint: self.fingerprint().into(),
        })
    }

    pub fn fetch_plan(
        &self,
        product: Product,
        crs: TerrainCrs,
        plan: &DownloadPlan,
        choice: &SourceChoice,
        cancelled: DownloadCancellation,
        mut progress: impl FnMut(TileProgress<'_>),
    ) -> Result<FetchedTiles> {
        let context = FetchContext {
            cancelled,
            deadline: None,
            bytes: Arc::default(),
        };
        let transform = Transformer::new(crs)?;
        let mut fetched = FetchedTiles::default();
        for (i, &tile) in plan.tiles.iter().enumerate() {
            context.check()?;
            let footprint = tile_footprint(&transform, tile.bounds)?;
            let ranked = self.registry.rank(product, &footprint, choice);
            let sources = prepare_sources(&ranked, product);
            let path = self.composed_path(crs, product, tile, &sources);
            let key = path.to_string_lossy();
            let _slot = gate(&key, 1, &context)?;
            let mut report_tile = |provider: Option<&str>, cached| {
                progress(TileProgress {
                    index: i + 1,
                    count: plan.tiles.len(),
                    provider,
                    cached,
                })
            };
            let report = if path.is_file() && validate_composed(&path, crs, tile, &context).is_ok()
            {
                report_tile(ranked.first().map(|p| p.name.as_str()), true);
                read_report(&path)
            } else {
                let (values, report) =
                    self.compose(&transform, tile, &sources, &context, |name| {
                        report_tile(Some(name), false)
                    })?;
                let parent = path.parent().expect("composed cache parent");
                fs::create_dir_all(parent)?;
                let mut staged = tempfile::NamedTempFile::new_in(parent)?;
                write_elevation_tiff(
                    &mut staged,
                    crs,
                    tile.bounds,
                    tile.width_pixels as u32,
                    tile.height_pixels as u32,
                    &values,
                )?;
                validate_composed(staged.path(), crs, tile, &context)?;
                atomic_write(
                    &path.with_extension("json"),
                    &serde_json::to_vec(&report).expect("report serializes"),
                )?;
                context.check()?;
                staged.persist(&path).map_err(|error| error.error)?;
                report
            };
            fetched.paths.push(path);
            fetched.warnings.extend(report.warnings);
            fetched.providers_used.extend(report.providers_used);
        }
        context.check()?;
        fetched.bytes_fetched = context.bytes.load(Ordering::Relaxed);
        Ok(fetched)
    }

    fn composed_path(
        &self,
        crs: TerrainCrs,
        product: Product,
        tile: DownloadTile,
        sources: &[Prepared<'_>],
    ) -> PathBuf {
        let bounds = tile.bounds.as_array().map(f64::to_bits);
        let providers: Vec<_> = sources.iter().map(|p| (&p.provider.id, &p.hash)).collect();
        let request = (
            1,
            crs.to_string(),
            bounds,
            tile.width_pixels,
            tile.height_pixels,
            product,
            providers,
        );
        let digest = hash(&serde_json::to_vec(&request).expect("tile key serializes"));
        self.cache
            .join("composed")
            .join(format!("{}-{}.tif", &digest[..24], product.name()))
    }

    fn compose(
        &self,
        transform: &Transformer,
        tile: DownloadTile,
        ranked: &[Prepared<'_>],
        context: &FetchContext,
        mut progress: impl FnMut(&str),
    ) -> Result<(Vec<f32>, TileReport)> {
        let locations = tile_points(transform, tile, context)?;
        let mut values = vec![f32::NAN; locations.len()];
        let mut report = TileReport::default();
        let mut failures = Vec::new();
        let mut storage = SampleStorage::default();
        for source in ranked {
            context.check()?;
            if values.iter().all(|value| value.is_finite()) {
                break;
            }
            let provider = source.provider;
            progress(&provider.name);
            let geometry = (!matches!(provider.coverage, Coverage::Global))
                .then(|| provider.coverage.geometry());
            let indexes: Vec<_> = values
                .iter()
                .zip(&locations)
                .enumerate()
                .filter_map(|(i, (&value, at))| {
                    (!value.is_finite()
                        && geometry.as_ref().is_none_or(|shape| {
                            shape.contains(&geo::Point::new(at.longitude, at.latitude))
                        }))
                    .then_some(i)
                })
                .collect();
            if indexes.is_empty() {
                continue;
            }
            if let Err(error) = self.sample_provider(
                source,
                transform,
                tile,
                &locations,
                &indexes,
                context,
                &mut storage,
            ) {
                context.check()?;
                failures.push((indexes, format!("{}: {error}", provider.name)));
                storage.clear();
                continue;
            }
            for (indexes, error) in storage.failures.drain(..) {
                failures.push((indexes, format!("{}: {error}", provider.name)));
            }
            let (valid, invalid) = filter_values(&mut storage.samples);
            if invalid > valid / 2 {
                report.warnings.push(format!(
                    "{} returned elevations outside the expected range",
                    provider.name
                ));
            } else {
                let mut used = false;
                for (index, value) in storage.samples.drain(..) {
                    if value.is_finite() {
                        values[index] = value;
                        used = true;
                    }
                }
                if used {
                    report.providers_used.push(provider.id.clone());
                }
            }
            storage.clear();
        }
        for (indexes, message) in failures {
            if indexes.iter().any(|&index| !values[index].is_finite()) {
                return Err(invalid(message));
            }
            report.warnings.push(message);
        }
        Ok((values, report))
    }

    #[allow(clippy::too_many_arguments)]
    fn sample_provider(
        &self,
        source: &Prepared<'_>,
        target: &Transformer,
        tile: DownloadTile,
        locations: &[LonLat],
        indexes: &[usize],
        context: &FetchContext,
        storage: &mut SampleStorage,
    ) -> Result<()> {
        let provider = source.provider;
        let _slot = gate(
            &format!("upstream:{}", provider.id),
            provider.max_concurrency,
            context,
        )?;
        let headers = source
            .headers
            .as_ref()
            .map_err(|e| invalid(e.to_string()))?;
        let root = self
            .cache
            .join("sources")
            .join(&provider.id)
            .join(&source.hash[..12]);
        let at = target.reverse(Point {
            x: (tile.bounds.min_x + tile.bounds.max_x) / 2.0,
            y: (tile.bounds.min_y + tile.bounds.max_y) / 2.0,
        })?;
        let resolution = tile_resolution(tile);
        match source.definition {
            ProductDefinition::Wcs(wcs) => {
                let raster = wcs::Fetch {
                    client: &self.client,
                    root: &root,
                    headers,
                    context,
                }
                .raster(wcs, provider, target, tile, at, resolution)?;
                let (width, height) = (tile.width_pixels as usize, tile.height_pixels as usize);
                if raster.matches_grid(target.crs(), tile.bounds, width, height) {
                    storage.read_grid(&raster, indexes, width, context)?;
                } else {
                    storage.sample(&raster, indexes, locations, context)?;
                }
            }
            ProductDefinition::Cog(cog) => {
                let source = range::open(&self.client, &root, &cog.url, headers, false, context)?
                    .expect("single COG is required");
                let raster = SourceRaster::open(source, resolution, at.latitude)?;
                storage.sample(&raster, indexes, locations, context)?;
            }
            ProductDefinition::CogTiles(cog) => {
                let mut cells = BTreeMap::<_, Vec<_>>::new();
                for &index in indexes {
                    cells
                        .entry(cog.cell(locations[index]))
                        .or_default()
                        .push(index);
                }
                for (cell, indexes) in cells {
                    context.check()?;
                    let count = storage.samples.len();
                    let result = (|| -> Result<()> {
                        let url = cog.url_for_cell(cell)?;
                        if !valid_url(&url) {
                            return Err(invalid("Invalid expanded COG URL"));
                        }
                        let source =
                            range::open(&self.client, &root, &url, headers, true, context)?;
                        if let Some(source) = source {
                            let raster = SourceRaster::open(source, resolution, at.latitude)?;
                            storage.sample(&raster, &indexes, locations, context)?;
                        } else if provider.missing_tile == Some(MissingTile::SeaLevel) {
                            storage
                                .samples
                                .extend(indexes.iter().map(|&index| (index, 0.0)));
                            storage.crs.get_or_insert_with(|| "EPSG:4326".into());
                            storage.resolution.get_or_insert(provider.resolution_m);
                        }
                        Ok(())
                    })();
                    if let Err(error) = result {
                        context.check()?;
                        storage.samples.truncate(count);
                        storage.failures.push((indexes, error.to_string()));
                    }
                }
            }
        }
        Ok(())
    }

    pub fn probe(&self, id: &str, at: LonLat) -> Result<ProbeResult> {
        self.probe_with_span(id, at, 1000.0)
    }

    pub(super) fn probe_with_span(&self, id: &str, at: LonLat, span: f64) -> Result<ProbeResult> {
        let provider = self.registry.provider(id)?;
        let product = if provider.products.dtm.is_some() {
            Product::Dtm
        } else {
            Product::Dom
        };
        let start = Instant::now();
        let context = FetchContext {
            cancelled: Arc::new(|| false),
            deadline: Some(start + Duration::from_secs(30)),
            bytes: Arc::default(),
        };
        let mut result = ProbeResult::default();
        let probe = (|| -> Result<()> {
            if !provider.coverage.contains(at) {
                return Err(invalid("The test point is outside this source’s coverage"));
            }
            let choice = SourceChoice {
                source: id.into(),
                fallback: false,
            };
            let area = MultiPolygon(vec![super::schema::rectangle([
                at.longitude - 1e-8,
                at.latitude - 1e-8,
                at.longitude + 1e-8,
                at.latitude + 1e-8,
            ])]);
            let transform = Transformer::new(self.registry.target_crs(&area, &choice)?)?;
            let point = transform.forward(at)?;
            let radius = span / 2.0;
            let bounds = Bounds {
                min_x: point.x - radius,
                min_y: point.y - radius,
                max_x: point.x + radius,
                max_y: point.y + radius,
            };
            let n = (span / provider.resolution_m.max(5.0)).ceil() as u64;
            let tile = DownloadTile {
                bounds,
                width_pixels: n,
                height_pixels: n,
            };
            let locations = tile_points(&transform, tile, &context)?;
            let indexes: Vec<_> = (0..locations.len()).collect();
            let sources = prepare_sources(&[provider], product);
            let mut storage = SampleStorage::default();
            self.sample_provider(
                &sources[0],
                &transform,
                tile,
                &locations,
                &indexes,
                &context,
                &mut storage,
            )?;
            if let Some((_, error)) = storage.failures.first() {
                return Err(invalid(error.clone()));
            }
            let (valid, invalid_count) = filter_values(&mut storage.samples);
            if invalid_count > valid / 2 {
                return Err(invalid(
                    "Source returned elevations outside the expected range",
                ));
            }
            let mut count = 0;
            let mut min = f32::INFINITY;
            let mut max = f32::NEG_INFINITY;
            let mut sum = 0.0;
            for (_, value) in storage
                .samples
                .into_iter()
                .filter(|(_, value)| value.is_finite())
            {
                count += 1;
                min = min.min(value);
                max = max.max(value);
                sum += f64::from(value);
            }
            result.crs = storage.crs;
            result.resolution_m = storage.resolution;
            result.valid_fraction = f64::from(count) / locations.len() as f64;
            result.min_m = (count > 0).then_some(min);
            result.max_m = (count > 0).then_some(max);
            result.mean_m = (count > 0).then(|| sum / f64::from(count));
            result.ok = count > 0;
            result.message = if result.ok {
                "Read elevation successfully"
            } else {
                "No valid elevations in the test window"
            }
            .into();
            Ok(())
        })();
        if let Err(error) = probe {
            result.message = if context.expired() {
                "Source probe exceeded 30 seconds".into()
            } else {
                error.to_string()
            };
        }
        result.elapsed_ms = start.elapsed().as_millis() as u64;
        result.bytes_fetched = context.bytes.load(Ordering::Relaxed);
        Ok(result)
    }
}

fn require_registered(crs: TerrainCrs) -> Result<()> {
    if crs.epsg_code().is_none() {
        return Err(invalid(
            "Downloading terrain requires a registered project CRS",
        ));
    }
    Ok(())
}

struct Prepared<'a> {
    provider: &'a TerrainProvider,
    definition: &'a ProductDefinition,
    headers: Result<HeaderMap>,
    hash: String,
}

fn prepare_sources<'a>(ranked: &[&'a TerrainProvider], product: Product) -> Vec<Prepared<'a>> {
    ranked
        .iter()
        .filter_map(|&provider| {
            let definition = provider.products.get(product)?;
            let mut expanded = BTreeMap::new();
            let headers = (|| {
                let mut headers = HeaderMap::new();
                for (name, value) in &provider.headers {
                    let name = HeaderName::try_from(name)
                        .map_err(|_| invalid("Invalid source header name"))?;
                    let value = expand_header(value, |env| {
                        std::env::var(env).map_err(|_| {
                            invalid(format!("Environment variable {env} is unavailable"))
                        })
                    })?;
                    let mut header = HeaderValue::from_str(&value)
                        .map_err(|_| invalid(format!("Invalid value for source header {name}")))?;
                    header.set_sensitive(true);
                    expanded.insert(name.as_str().to_owned(), hash(value.as_bytes()));
                    headers.insert(name, header);
                }
                Ok(headers)
            })();
            let fetch = (
                &provider.kind,
                definition,
                &provider.headers,
                expanded,
                provider.resolution_m,
                &provider.coverage,
                &provider.missing_tile,
            );
            let mut fingerprint = serde_json::to_vec(&fetch).expect("fetch fields serialize");
            fingerprint.push(u8::from(headers.is_ok()));
            Some(Prepared {
                provider,
                definition,
                headers,
                hash: hash(&fingerprint),
            })
        })
        .collect()
}

fn tile_resolution(tile: DownloadTile) -> f64 {
    ((tile.bounds.max_x - tile.bounds.min_x) / tile.width_pixels as f64)
        .max((tile.bounds.max_y - tile.bounds.min_y) / tile.height_pixels as f64)
}

fn tile_points(
    transform: &Transformer,
    tile: DownloadTile,
    context: &FetchContext,
) -> Result<Vec<LonLat>> {
    let mut points = Vec::with_capacity(tile.pixel_count() as usize);
    let [west, south, east, north] = tile.bounds.as_array();
    let dx = (east - west) / tile.width_pixels as f64;
    let dy = (north - south) / tile.height_pixels as f64;
    for row in 0..tile.height_pixels {
        context.check()?;
        for col in 0..tile.width_pixels {
            points.push(transform.reverse(Point {
                x: west + (col as f64 + 0.5) * dx,
                y: north - (row as f64 + 0.5) * dy,
            })?);
        }
    }
    Ok(points)
}

#[derive(Default)]
struct SampleStorage {
    samples: Vec<(usize, f32)>,
    failures: Vec<(Vec<usize>, String)>,
    crs: Option<String>,
    resolution: Option<f64>,
}

impl SampleStorage {
    fn clear(&mut self) {
        self.samples.clear();
        self.failures.clear();
        self.crs = None;
        self.resolution = None;
    }

    fn read_grid(
        &mut self,
        raster: &SourceRaster,
        indexes: &[usize],
        width: usize,
        context: &FetchContext,
    ) -> Result<()> {
        self.crs.get_or_insert_with(|| raster.crs().to_string());
        self.resolution.get_or_insert(raster.resolution_m());
        let mut remaining = indexes;
        let batch_rows = (SAMPLE_BATCH / width).max(1);
        while let Some(&first) = remaining.first() {
            context.check()?;
            let top = first / width;
            let len = remaining.partition_point(|index| index / width < top + batch_rows);
            let last_row = remaining[len - 1] / width;
            let values = raster.read_window(top, 0, last_row - top + 1, width)?;
            self.samples.extend(
                remaining[..len]
                    .iter()
                    .map(|&index| (index, values[index - top * width])),
            );
            remaining = &remaining[len..];
        }
        Ok(())
    }

    fn sample(
        &mut self,
        raster: &SourceRaster,
        indexes: &[usize],
        points: &[LonLat],
        context: &FetchContext,
    ) -> Result<()> {
        self.crs.get_or_insert_with(|| raster.crs().to_string());
        self.resolution.get_or_insert(raster.resolution_m());
        let mut locations = Vec::with_capacity(indexes.len().min(SAMPLE_BATCH));
        for batch in indexes.chunks(SAMPLE_BATCH) {
            context.check()?;
            locations.clear();
            locations.extend(batch.iter().map(|&i| points[i]));
            let samples = raster.sample_wgs84(&locations)?;
            self.samples.extend(batch.iter().copied().zip(samples));
        }
        Ok(())
    }
}

fn filter_values(samples: &mut [(usize, f32)]) -> (usize, usize) {
    let mut valid = 0;
    let mut outside = 0;
    for (_, value) in samples {
        if value.is_finite() {
            valid += 1;
            if !(-500.0..=9000.0).contains(value) {
                outside += 1;
            }
        }
        if !(-12000.0..=9000.0).contains(value) {
            *value = f32::NAN;
        }
    }
    (valid, outside)
}

#[derive(Default, Serialize, Deserialize)]
struct TileReport {
    warnings: Vec<String>,
    providers_used: Vec<String>,
}

fn read_report(path: &Path) -> TileReport {
    fs::read(path.with_extension("json"))
        .ok()
        .and_then(|value| serde_json::from_slice(&value).ok())
        .unwrap_or_default()
}

fn validate_composed(
    path: &Path,
    crs: TerrainCrs,
    tile: DownloadTile,
    context: &FetchContext,
) -> Result<()> {
    context.check()?;
    let raster = GeoTiff::open(path)?;
    let meta = raster.metadata();
    if crs != meta.crs
        || meta.width as u64 != tile.width_pixels
        || meta.height as u64 != tile.height_pixels
        || meta
            .bounds
            .as_array()
            .into_iter()
            .zip(tile.bounds.as_array())
            .any(|(a, b)| (a - b).abs() > 1e-6)
    {
        return Err(response_error(
            PROVIDER,
            "composed GeoTIFF has an unexpected grid",
        ));
    }
    raster.validate_pixels(&|| (context.cancelled)() || context.expired())?;
    context.check()
}

#[derive(Default)]
struct Gate {
    active: Mutex<usize>,
    ready: Condvar,
}

struct Slot(Arc<Gate>);

impl Drop for Slot {
    fn drop(&mut self) {
        *self.0.active.lock().expect("terrain download slot lock") -= 1;
        self.0.ready.notify_all();
    }
}

fn gate(key: &str, limit: usize, context: &FetchContext) -> Result<Slot> {
    static GATES: OnceLock<Mutex<HashMap<String, Weak<Gate>>>> = OnceLock::new();
    let gate = {
        let mut gates = GATES
            .get_or_init(Mutex::default)
            .lock()
            .expect("terrain download gate lock");
        if let Some(gate) = gates.get(key).and_then(Weak::upgrade) {
            gate
        } else {
            gates.retain(|_, gate| gate.strong_count() > 0);
            let gate = Arc::new(Gate::default());
            gates.insert(key.to_owned(), Arc::downgrade(&gate));
            gate
        }
    };
    let mut active = gate.active.lock().expect("terrain download slot lock");
    while *active >= limit {
        context.check()?;
        (active, _) = gate
            .ready
            .wait_timeout(active, Duration::from_millis(50))
            .expect("terrain download slot wait");
    }
    context.check()?;
    *active += 1;
    drop(active);
    Ok(Slot(gate))
}
