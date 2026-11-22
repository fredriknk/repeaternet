//! Fetch shared terrain and sample one projected chord per undirected pair.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
    sync::Arc,
    time::Instant,
};

use anyhow::{Context, Result, bail, ensure};
use geo::{BoundingRect, Intersects, Line, Rect, unary_union};
use repeaternet_providers::sources::{
    DownloadArea, FetchedTiles, PlanOptions, Product, ProviderRegistry, SourceChoice, SourcePlan,
    TerrainDownloader, TileLimits,
};
use repeaternet_rf_core::calibration::TERMINAL_CLUTTER_RADIUS_M;
use repeaternet_terrain::{
    Bounds, GeoTiff, LonLat, Point, RasterTerrain, TerrainCrs, TerrainLayer, Transformer,
    corridor::corridor_geometry,
};
use sha2::{Digest, Sha256};

use crate::{
    dataset::{
        Endpoint, LinkRecord, PathFeatures, ProfileEnd, ProfileRecord, Region, SCHEMA_VERSION,
    },
    jsonl, paths,
};

const CORRIDOR_HALF_WIDTH_M: f64 = 300.0;
const MAX_MISSING_FRACTION: f64 = 0.02;
const MAX_GAP_SAMPLES: usize = 5;

struct Chord<'a> {
    id: String,
    links: Vec<&'a LinkRecord>,
    ends: [&'a Endpoint; 2],
    points: [Point; 2],
    distance: f64,
}

#[derive(Clone, Copy, Ord, PartialOrd, Eq, PartialEq)]
struct TerrainKey {
    epsg: u16,
    norway: bool,
}

impl TerrainKey {
    fn resolution(self) -> f64 {
        if self.norway { 10.0 } else { 30.0 }
    }

    fn terrain(self) -> &'static str {
        if self.norway { "kartverket" } else { "glo30" }
    }

    fn choice(self) -> SourceChoice {
        SourceChoice {
            source: if self.norway {
                "kartverket-nhm"
            } else {
                "copernicus-glo30"
            }
            .into(),
            fallback: false,
        }
    }
}

struct Batch<'a> {
    key: TerrainKey,
    chords: Vec<Chord<'a>>,
    plan: SourcePlan,
}

fn geographic(end: &Endpoint) -> LonLat {
    LonLat {
        longitude: end.lon,
        latitude: end.lat,
    }
}

fn ordered_ends(link: &LinkRecord) -> [&Endpoint; 2] {
    if link.tx.node < link.rx.node {
        [&link.tx, &link.rx]
    } else {
        [&link.rx, &link.tx]
    }
}

fn exclusion(link: &LinkRecord) -> Option<&'static str> {
    [
        "over_300km",
        "default_position",
        "self_link",
        "mqtt_suspect",
    ]
    .into_iter()
    .find(|flag| {
        link.flags
            .iter()
            .chain(&link.tx.flags)
            .chain(&link.rx.flags)
            .any(|s| s == flag)
    })
    .or_else(|| {
        if link.tx.node == link.rx.node {
            Some("self_link")
        } else if !(30.0..=150_000.0).contains(&link.distance_m) {
            Some("distance outside 30 m to 150 km")
        } else {
            None
        }
    })
}

fn group_links(links: &[LinkRecord]) -> Result<BTreeMap<TerrainKey, Vec<Chord<'_>>>> {
    let mut groups = BTreeMap::<_, Vec<_>>::new();
    let mut skipped = BTreeMap::<&str, usize>::new();
    for link in links {
        if let Some(reason) = exclusion(link) {
            *skipped.entry(reason).or_default() += 1;
            continue;
        }
        let [a, b] = ordered_ends(link);
        groups
            .entry(format!("{}~{}", a.node, b.node))
            .or_default()
            .push(link);
    }
    for (reason, count) in skipped {
        println!("Skipped {count} links ({reason})");
    }

    let mut batches = BTreeMap::<_, Vec<_>>::new();
    for (id, mut links) in groups {
        // Prefer one observed position pair over a non-detection if they differ.
        links.sort_unstable_by(|a, b| b.n.cmp(&a.n).then(a.link_id.cmp(&b.link_id)));
        let [a, b] = ordered_ends(links[0]);
        let norway = links[0].region == Region::Norway;
        let lon = a.lon + ((b.lon - a.lon + 180.0).rem_euclid(360.0) - 180.0) / 2.0;
        let lon = (lon + 180.0).rem_euclid(360.0) - 180.0;
        let crs = if norway {
            TerrainCrs::for_norway_longitude(lon)?
        } else {
            TerrainCrs::for_wgs84_location(LonLat {
                longitude: lon,
                latitude: (a.lat + b.lat) / 2.0,
            })?
        };
        let transform = Transformer::new(crs)?;
        let points = [
            transform.forward(geographic(a))?,
            transform.forward(geographic(b))?,
        ];
        let distance = points[0].distance_to(points[1]);
        if distance <= 0.0 {
            eprintln!("Dropped {id}: coincident projected endpoints");
            continue;
        }
        let key = TerrainKey {
            epsg: crs.epsg(),
            norway,
        };
        batches.entry(key).or_default().push(Chord {
            id,
            links,
            points,
            distance,
            ends: [a, b],
        });
    }
    Ok(batches)
}

fn plan_batch<'a>(
    downloader: &TerrainDownloader,
    key: TerrainKey,
    mut chords: Vec<Chord<'a>>,
    batches: &mut Vec<Batch<'a>>,
) -> Result<()> {
    let corridors = chords
        .iter()
        .map(|chord| corridor_geometry(&chord.points, CORRIDOR_HALF_WIDTH_M, &[]))
        .collect::<Result<Vec<_>, _>>()?;
    let area = unary_union(&corridors);
    let rect = area
        .bounding_rect()
        .context("empty terrain download area")?;
    let bounds = Bounds {
        min_x: rect.min().x,
        min_y: rect.min().y,
        max_x: rect.max().x,
        max_y: rect.max().y,
    };
    let crs = TerrainCrs::from_epsg(key.epsg)?;
    match downloader.plan_download(
        crs,
        bounds,
        key.resolution(),
        DownloadArea::Geometry(&area),
        PlanOptions {
            auto_resolution: false,
            ..PlanOptions::default()
        },
        key.norway,
        key.choice(),
    ) {
        Ok(plan) => batches.push(Batch { key, chords, plan }),
        Err(error) if chords.len() == 1 => {
            eprintln!("Dropped {}: terrain plan failed ({error})", chords[0].id);
        }
        Err(_) => {
            // Large or sparse regions can exceed the provider tile or grid limits.
            let axis = if rect.width() >= rect.height() {
                |p: Point| p.x
            } else {
                |p: Point| p.y
            };
            chords.sort_by(|a, b| {
                let midpoint = |chord: &Chord<'_>| axis(chord.points[0]) + axis(chord.points[1]);
                midpoint(a).total_cmp(&midpoint(b))
            });
            let rest = chords.split_off(chords.len() / 2);
            plan_batch(downloader, key, chords, batches)?;
            plan_batch(downloader, key, rest, batches)?;
        }
    }
    Ok(())
}

fn fetch(
    downloader: &TerrainDownloader,
    batch: &Batch<'_>,
    product: Product,
) -> Result<(FetchedTiles, usize)> {
    let grid = match product {
        Product::Dtm => &batch.plan.dtm,
        Product::Dom => &batch.plan.dom,
    };
    let mut hits = 0;
    let Some(grid) = grid else {
        return Ok((FetchedTiles::default(), hits));
    };
    let fetched = downloader.fetch_plan(
        product,
        TerrainCrs::from_epsg(batch.key.epsg)?,
        grid,
        &batch.key.choice(),
        Arc::new(|| false),
        |tile| {
            hits += usize::from(tile.cached);
            if !tile.cached {
                println!(
                    "Fetching EPSG:{} {} tile {}/{}",
                    batch.key.epsg,
                    product.name(),
                    tile.index,
                    tile.count
                );
            }
        },
    )?;
    for warning in fetched.warnings.iter().collect::<BTreeSet<_>>() {
        eprintln!("{warning}");
    }
    Ok((fetched, hits))
}

struct TileRevision {
    bounds: Rect,
    identity: Vec<u8>,
}

fn tile_revisions(tiles: &[GeoTiff]) -> Result<Vec<TileRevision>> {
    tiles
        .iter()
        .map(|tile| {
            let path = tile.path();
            let meta = fs::metadata(path)?;
            let modified = meta.modified()?.duration_since(std::time::UNIX_EPOCH)?;
            let Bounds {
                min_x,
                min_y,
                max_x,
                max_y,
            } = tile.metadata().bounds;
            Ok(TileRevision {
                bounds: Rect::new((min_x, min_y), (max_x, max_y)),
                identity: serde_json::to_vec(&(path, meta.len(), modified.as_nanos()))?,
            })
        })
        .collect()
}

fn terrain_fingerprint(key: TerrainKey, chord: &Chord<'_>, tiles: &[TileRevision]) -> String {
    let mut hash = Sha256::new();
    hash.update(b"calibration-profile-v1");
    hash.update(key.epsg.to_le_bytes());
    hash.update(key.terrain());
    hash.update(key.resolution().to_le_bytes());
    let [a, b] = chord.points;
    let line = Line::new((a.x, a.y), (b.x, b.y));
    for tile in tiles.iter().filter(|tile| line.intersects(&tile.bounds)) {
        hash.update(&tile.identity);
        hash.update(b"\n");
    }
    hash.finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn sample_chord(
    raster: &RasterTerrain,
    key: TerrainKey,
    chord: &Chord<'_>,
    fingerprint: String,
) -> Result<ProfileRecord> {
    let intervals = ((chord.distance / key.resolution()).ceil() as usize).max(2);
    let step_m = chord.distance / intervals as f64;
    let [a, b] = chord.points;
    let points: Vec<_> = (0..=intervals)
        .map(|index| {
            let fraction = index as f64 / intervals as f64;
            Point {
                x: a.x + fraction * (b.x - a.x),
                y: a.y + fraction * (b.y - a.y),
            }
        })
        .collect();
    let mut dtm = raster.sample(&points, TerrainLayer::GroundDtm)?;
    let mut dom = if raster.surface_tiles().is_empty() {
        None
    } else {
        let surface = raster.sample(&points, TerrainLayer::SurfaceDom)?;
        surface
            .iter()
            .any(|sample| sample.is_finite())
            .then_some(surface)
    };
    let missing = dtm
        .iter()
        .enumerate()
        .filter(|&(index, value)| {
            !value.is_finite()
                || dom
                    .as_ref()
                    .is_some_and(|surface| !surface[index].is_finite())
        })
        .count();
    let missing_fraction = missing as f64 / points.len() as f64;
    ensure!(
        missing_fraction <= MAX_MISSING_FRACTION,
        "{:.2}% of terrain samples missing ({missing}/{})",
        100.0 * missing_fraction,
        points.len()
    );
    fill_gaps(&mut dtm).context("DTM")?;
    if let Some(surface) = &mut dom {
        fill_gaps(surface).context("DOM")?;
    }
    let endpoint = |end: &Endpoint, point: Point, index: usize| ProfileEnd {
        node: end.node.clone(),
        lat: end.lat,
        lon: end.lon,
        x: point.x,
        y: point.y,
        ground_m: dtm[index],
        surface_m: dom.as_ref().map(|values| values[index]),
    };
    Ok(ProfileRecord {
        schema: SCHEMA_VERSION,
        path_id: chord.id.clone(),
        link_ids: Vec::new(),
        terrain: key.terrain().into(),
        crs_epsg: key.epsg.into(),
        resolution_m: key.resolution(),
        step_m,
        distance_m: chord.distance,
        a: endpoint(chord.ends[0], a, 0),
        b: endpoint(chord.ends[1], b, intervals),
        dtm,
        dom,
        missing_fraction,
        terrain_fingerprint: fingerprint,
    })
}

fn fill_gaps(samples: &mut [f64]) -> Result<()> {
    let mut index = 0;
    while index < samples.len() {
        if samples[index].is_finite() {
            index += 1;
            continue;
        }
        let start = index;
        while index < samples.len() && !samples[index].is_finite() {
            index += 1;
        }
        let len = index - start;
        ensure!(
            len <= MAX_GAP_SAMPLES,
            "gap of {len} consecutive missing samples"
        );
        let left = start.checked_sub(1).map(|i| samples[i]);
        let right = samples.get(index).copied();
        let left = left.or(right).context("no finite elevations")?;
        let right = right.unwrap_or(left);
        for (i, value) in samples[start..index].iter_mut().enumerate() {
            *value = left + (right - left) * ((i + 1) as f64 / (len + 1) as f64);
        }
    }
    for value in samples {
        *value = (*value * 100.0).round() / 100.0;
    }
    Ok(())
}

fn quantile(sorted: &[f64], p: f64) -> f64 {
    let index = p * (sorted.len() - 1) as f64;
    let lo = sorted[index.floor() as usize];
    let hi = sorted[index.ceil() as usize];
    lo + (hi - lo) * index.fract()
}

fn interdecile(dtm: &[f64]) -> f64 {
    let intervals = dtm.len() - 1;
    let start = intervals.div_ceil(10);
    let end = intervals - start;
    let middle = &dtm[start..=end];
    let center = (middle.len() - 1) as f64 / 2.0;
    let centered_index = |index: usize| index as f64 - center;
    let (cov, variance) = middle
        .iter()
        .enumerate()
        .fold((0.0, 0.0), |(cov, var), (i, h)| {
            let x = centered_index(i);
            (cov + x * h, var + x * x)
        });
    let slope = if variance > 0.0 { cov / variance } else { 0.0 };
    let mut residuals: Vec<_> = middle
        .iter()
        .enumerate()
        .map(|(i, h)| h - slope * centered_index(i))
        .collect();
    residuals.sort_by(f64::total_cmp);
    quantile(&residuals, 0.9) - quantile(&residuals, 0.1)
}

fn path_features(profile: &ProfileRecord) -> PathFeatures {
    let mut result = PathFeatures {
        schema: SCHEMA_VERSION,
        path_id: profile.path_id.clone(),
        distance_m: profile.distance_m,
        delta_h_m: interdecile(&profile.dtm),
        a_clutter_height_m: None,
        b_clutter_height_m: None,
        clutter_fraction_gt_3m: None,
    };
    if let Some(dom) = &profile.dom {
        let excess: Vec<_> = dom.iter().zip(&profile.dtm).map(|(s, g)| s - g).collect();
        let count =
            ((TERMINAL_CLUTTER_RADIUS_M / profile.step_m).floor() as usize + 1).min(excess.len());
        let median = |slice: &[f64]| {
            let mut slice = slice.to_vec();
            slice.sort_by(f64::total_cmp);
            quantile(&slice, 0.5).max(0.0)
        };
        result.a_clutter_height_m = Some(median(&excess[..count]));
        result.b_clutter_height_m = Some(median(&excess[excess.len() - count..]));
        // Weight each interval by its fraction above the threshold.
        let above: f64 = excess
            .windows(2)
            .map(|edge| {
                let lo = edge[0].min(edge[1]);
                let hi = edge[0].max(edge[1]);
                if lo > 3.0 {
                    1.0
                } else if hi <= 3.0 {
                    0.0
                } else {
                    (hi - 3.0) / (hi - lo)
                }
            })
            .sum();
        result.clutter_fraction_gt_3m = Some(above / (excess.len() - 1) as f64);
    }
    result
}

pub fn run(links: &Path, work: &Path, max_download_gb: f64) -> Result<()> {
    let started = Instant::now();
    ensure!(
        max_download_gb.is_finite() && max_download_gb >= 0.0,
        "--max-download-gb must be finite and nonnegative"
    );
    paths::refuse_user_data(work)?;
    paths::refuse_user_data(links)?;
    let links = jsonl::read::<LinkRecord>(links)?;
    let groups = group_links(&links)?;
    let mut dropped = groups.values().map(Vec::len).sum::<usize>();
    let cache = paths::shared_cache_dir()?;
    fs::create_dir_all(&cache)?;
    let downloader = TerrainDownloader::new(
        Arc::new(ProviderRegistry::builtin()),
        &cache,
        TileLimits::default(),
    )?;
    let mut batches = Vec::new();
    for (key, chords) in groups {
        plan_batch(&downloader, key, chords, &mut batches)?;
    }

    let (mut estimated_bytes, mut estimated_hits, mut tile_count) = (0.0, 0, 0);
    for batch in &batches {
        let estimate =
            downloader.estimate(TerrainCrs::from_epsg(batch.key.epsg)?, &batch.plan, &cache)?;
        // The providers use twice the composed raw size for disk planning.
        estimated_bytes += 2.0 * estimate.estimated_uncached_raw_mib * 1024.0 * 1024.0;
        estimated_hits += estimate.cache_hits;
        tile_count += estimate.cache_total;
    }
    println!(
        "Terrain estimate: {estimated_bytes:.0} new bytes ({:.3} GB), {estimated_hits}/{tile_count} composed tile cache hits",
        estimated_bytes / 1e9
    );
    if estimated_bytes > max_download_gb * 1e9 {
        bail!("estimated terrain download exceeds {max_download_gb} GB");
    }

    let out = work.join("profiles.jsonl");
    paths::refuse_user_data(&out)?;
    let mut existing: BTreeMap<_, _> = jsonl::read_if_present::<ProfileRecord>(&out)?
        .into_iter()
        .map(|profile| (profile.path_id.clone(), profile))
        .collect();
    let mut profiles = Vec::new();
    let (mut reused, mut bytes, mut hits) = (0, 0, 0);
    for batch in batches {
        let (dtm, dtm_hits) = fetch(&downloader, &batch, Product::Dtm)?;
        let (dom, dom_hits) = fetch(&downloader, &batch, Product::Dom)?;
        bytes += dtm.bytes_fetched + dom.bytes_fetched;
        hits += dtm_hits + dom_hits;
        let raster = RasterTerrain::open(&dtm.paths, &dom.paths)?;
        let mut tiles = tile_revisions(raster.ground_tiles())?;
        tiles.extend(tile_revisions(raster.surface_tiles())?);
        for chord in batch.chords {
            let fingerprint = terrain_fingerprint(batch.key, &chord, &tiles);
            let previous = existing.remove(&chord.id).filter(|profile| {
                let matches = |end: &ProfileEnd, input: &Endpoint| {
                    end.lat == input.lat && end.lon == input.lon
                };
                profile.schema == SCHEMA_VERSION
                    && profile.terrain_fingerprint == fingerprint
                    && matches(&profile.a, chord.ends[0])
                    && matches(&profile.b, chord.ends[1])
            });
            let mut profile = if let Some(profile) = previous {
                reused += 1;
                profile
            } else {
                match sample_chord(&raster, batch.key, &chord, fingerprint) {
                    Ok(profile) => profile,
                    Err(error) => {
                        eprintln!("Dropped {}: {error:#}", chord.id);
                        continue;
                    }
                }
            };
            profile.link_ids = chord
                .links
                .iter()
                .map(|link| link.link_id.clone())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            profiles.push(profile);
            dropped -= 1;
        }
    }
    profiles.sort_unstable_by(|a, b| a.path_id.cmp(&b.path_id));
    let features: Vec<_> = profiles.iter().map(path_features).collect();
    jsonl::write(&work.join("path_features.jsonl"), &features)?;
    jsonl::write(&out, &profiles)?;
    println!("Read {bytes} upstream terrain bytes, {hits}/{tile_count} composed tile cache hits");
    println!(
        "Wrote {} profiles ({reused} reused, {dropped} dropped) in {:.2}s",
        profiles.len(),
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fills_short_gaps_and_rejects_long_ones() {
        let mut heights = [f64::NAN, 1.0, f64::NAN, f64::NAN, 2.0, f64::NAN];
        fill_gaps(&mut heights).unwrap();
        assert_eq!(heights, [1.0, 1.0, 1.33, 1.67, 2.0, 2.0]);
        assert!(fill_gaps(&mut [f64::NAN; 6]).is_err());
    }

    #[test]
    fn interdecile_removes_linear_grade() {
        assert_eq!(interdecile(&[2.0, 3.0, 4.0]), 0.0);
        assert_eq!(interdecile(&[0.0, 1.0, 2.0, 3.0, 4.0]), 0.0);
        assert!((interdecile(&[0.0, 2.0, 4.0, 2.0, 0.0]) - 1.6).abs() < 1e-12);
    }
}
