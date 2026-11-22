//! SondeHub receiver terrain and air-to-ground calibration inputs.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Read,
    path::Path,
    sync::Arc,
    thread,
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use flate2::read::ZlibDecoder;
use repeaternet_providers::sources::{
    Coverage, DownloadArea, PlanOptions, Product, ProviderRegistry, SourceChoice, SourcePlan,
    TerrainDownloader, TileLimits,
};
use repeaternet_rf_core::{EARTH_MEAN_RADIUS_M, Site, bullington_loss_db, sample_profile};
use repeaternet_terrain::{
    Bounds, LonLat, Point, RasterTerrain, TerrainCrs, TerrainLayer, Transformer,
};
use rusqlite::{Connection, OpenFlags, Row};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::{dataset::SCHEMA_VERSION, jsonl, paths};

const SNAPSHOT: &str = "sondehub-import.sqlite3";
const RADIUS: f64 = 10_000.0;
const BEARINGS: i32 = 720;
const BAD_GEOMETRY: i64 = 1 | 2 | 4 | 8;
const BASE_K: f64 = 4.0 / 3.0;
const WORLD_LIMIT: usize = 200;
const SHADOW_LIMIT: usize = 48;

#[derive(Clone, Copy, Debug, Serialize)]
struct Position {
    lat: f64,
    lon: f64,
}

impl Position {
    fn from_row(row: &Row<'_>, index: usize) -> rusqlite::Result<Self> {
        Ok(Self {
            lat: row.get(index)?,
            lon: row.get(index + 1)?,
        })
    }

    fn geographic(self) -> LonLat {
        LonLat {
            latitude: self.lat,
            longitude: self.lon,
        }
    }

    fn valid(self) -> bool {
        (-90.0..=90.0).contains(&self.lat)
            && (-180.0..=180.0).contains(&self.lon)
            && (self.lat != 0.0 || self.lon != 0.0)
    }

    fn cell(self) -> (i32, i32) {
        (
            (self.lat * 1e4).round() as i32,
            (self.lon * 1e4).round() as i32,
        )
    }

    fn geometry(self, other: Self) -> (f64, f64) {
        let a = self.lat.to_radians();
        let b = other.lat.to_radians();
        let delta = (other.lon - self.lon).to_radians();
        let hav = ((b - a) / 2.0).sin().powi(2) + a.cos() * b.cos() * (delta / 2.0).sin().powi(2);
        let distance = 2.0 * EARTH_MEAN_RADIUS_M * hav.clamp(0.0, 1.0).sqrt().asin();
        let bearing =
            (delta.sin() * b.cos()).atan2(a.cos() * b.sin() - a.sin() * b.cos() * delta.cos());
        (distance, bearing)
    }

    fn along(self, distance: f64, bearing: f64) -> Self {
        let a = self.lat.to_radians();
        let (sin_d, cos_d) = (distance / EARTH_MEAN_RADIUS_M).sin_cos();
        let b = (a.sin() * cos_d + a.cos() * sin_d * bearing.cos())
            .clamp(-1.0, 1.0)
            .asin();
        let delta = (bearing.sin() * sin_d * a.cos()).atan2(cos_d - a.sin() * b.sin());
        Self {
            lat: b.to_degrees(),
            lon: (self.lon + delta.to_degrees() + 180.0).rem_euclid(360.0) - 180.0,
        }
    }
}

fn pseudonym(salt: &str, namespace: &str, key: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(salt);
    digest.update(namespace);
    digest.update(key);
    format!(
        "{:016x}",
        u64::from_be_bytes(digest.finalize()[..8].try_into().unwrap())
    )
}

struct Geoid(Vec<i16>);

impl Geoid {
    fn load() -> Result<Self> {
        // Same embedded EGM96 grid and interpolation as the importer.
        let grid = include_bytes!("../../sondehub-import/src/egm96-1deg.i16.zlib");
        let mut data = Vec::new();
        ZlibDecoder::new(grid.as_slice()).read_to_end(&mut data)?;
        ensure!(data.len() == 360 * 181 * 2, "invalid EGM96 grid length");
        Ok(Self(
            data.as_chunks::<2>()
                .0
                .iter()
                .copied()
                .map(i16::from_le_bytes)
                .collect(),
        ))
    }

    fn msl(&self, point: Position, altitude: f64, datum: &str) -> Option<f64> {
        match datum {
            "MSL" => Some(altitude),
            "WGS84_ellipsoid" => {
                let row = (90.0 - point.lat).clamp(0.0, 180.0);
                let col = (point.lon + 180.0).rem_euclid(360.0);
                let (y, x) = (row as usize, col as usize);
                let at = |r: usize| {
                    let a = f64::from(self.0[360 * r + x]);
                    let b = f64::from(self.0[360 * r + (x + 1) % 360]);
                    a + col.fract() * (b - a)
                };
                let a = at(y);
                Some(altitude - (a + row.fract() * (at((y + 1).min(180)) - a)) / 100.0)
            }
            _ => None,
        }
    }
}

#[derive(Serialize)]
struct Receiver {
    schema: u32,
    receiver: String,
    region: &'static str,
    position: Position,
    reported_alt_m: Option<f64>,
    reported_alt_datum: String,
    ground_m: Option<f64>,
    antenna_msl_m: Option<f64>,
    height_basis: Option<&'static str>,
}

struct ReceiverPlan {
    receiver: Receiver,
    crs: TerrainCrs,
    center: Point,
    plan: SourcePlan,
}

fn inventories(db: &Connection, salt: &str, coverage: &Coverage) -> Result<Vec<(i64, Receiver)>> {
    // Keep the most frequently reported location per ID (about 11 m cells).
    let mut statement = db.prepare(
        "SELECT r.id, r.receiver_key, min(m.rx_lat), min(m.rx_lon),
                avg(m.rx_alt_m), min(m.rx_alt_datum), count(*)
         FROM measured_link m JOIN receiver r ON r.id=m.receiver_id
         WHERE (m.quality_flags & 15)=0 AND
               (m.rssi_kind='rdzTTGOsonde' OR m.snr_kind='radiosonde_auto_rx')
         GROUP BY r.id, round(m.rx_lat,4), round(m.rx_lon,4)
         ORDER BY r.id, count(*) DESC, round(m.rx_lat,4), round(m.rx_lon,4)",
    )?;
    let mut rows = statement.query([])?;
    let mut receivers = BTreeMap::new();
    while let Some(row) = rows.next()? {
        let id = row.get::<_, i64>(0)?;
        let position = Position::from_row(row, 2)?;
        if !position.valid() || receivers.contains_key(&id) {
            continue;
        }
        let key = row.get::<_, String>(1)?;
        receivers.insert(
            id,
            Receiver {
                schema: SCHEMA_VERSION,
                receiver: pseudonym(salt, "sondehub:", &key),
                region: if coverage.contains(position.geographic()) {
                    "norway"
                } else {
                    "world"
                },
                position,
                reported_alt_m: row.get::<_, Option<f64>>(4)?.filter(|h| h.is_finite()),
                reported_alt_datum: row.get(5)?,
                ground_m: None,
                antenna_msl_m: None,
                height_basis: None,
            },
        );
    }
    let (mut norway, mut world): (Vec<_>, Vec<_>) = receivers
        .into_iter()
        .partition(|(_, rx)| rx.region == "norway");
    let order = |a: &(i64, Receiver), b: &(i64, Receiver)| a.1.receiver.cmp(&b.1.receiver);
    norway.sort_unstable_by(order);
    world.sort_unstable_by(order);
    world.truncate(WORLD_LIMIT);
    norway.extend(world);
    Ok(norway)
}

fn choice(region: &str) -> SourceChoice {
    SourceChoice {
        source: if region == "norway" {
            "kartverket-nhm"
        } else {
            "copernicus-glo30"
        }
        .into(),
        fallback: false,
    }
}

fn resolution(region: &str) -> f64 {
    if region == "norway" { 10.0 } else { 30.0 }
}

fn bounds(a: Point, b: Point, padding: f64) -> Bounds {
    Bounds {
        min_x: a.x.min(b.x) - padding,
        min_y: a.y.min(b.y) - padding,
        max_x: a.x.max(b.x) + padding,
        max_y: a.y.max(b.y) + padding,
    }
}

fn plan(
    downloader: &TerrainDownloader,
    crs: TerrainCrs,
    region: &str,
    bounds: Bounds,
    area: DownloadArea<'_>,
) -> Result<SourcePlan> {
    Ok(downloader.plan_download(
        crs,
        bounds,
        resolution(region),
        area,
        PlanOptions {
            auto_resolution: false,
            ..PlanOptions::default()
        },
        false,
        choice(region),
    )?)
}

fn plan_receiver(downloader: &TerrainDownloader, receiver: Receiver) -> Result<ReceiverPlan> {
    let point = receiver.position.geographic();
    let crs = if receiver.region == "norway" {
        TerrainCrs::for_norway_longitude(point.longitude)?
    } else {
        TerrainCrs::for_wgs84_location(point)?
    };
    let center = Transformer::new(crs)?.forward(point)?;
    let area = bounds(center, center, RADIUS + 100.0);
    let plan = plan(downloader, crs, receiver.region, area, DownloadArea::Bounds)?;
    Ok(ReceiverPlan {
        receiver,
        crs,
        center,
        plan,
    })
}

#[derive(Default)]
struct Estimate {
    bytes: f64,
    hits: usize,
    tiles: usize,
}

impl Estimate {
    fn add(
        &mut self,
        downloader: &TerrainDownloader,
        crs: TerrainCrs,
        plan: &SourcePlan,
        cache: &Path,
    ) -> Result<()> {
        let estimate = downloader.estimate(crs, plan, cache)?;
        self.bytes += 2.0 * estimate.estimated_uncached_raw_mib * 1024.0 * 1024.0;
        self.hits += estimate.cache_hits;
        self.tiles += estimate.cache_total;
        Ok(())
    }

    fn print(&self, stage: &str) {
        println!(
            "{stage} terrain estimate: {:.0} new bytes ({:.3} GB), {}/{} composed tiles cached",
            self.bytes,
            self.bytes / 1e9,
            self.hits,
            self.tiles
        );
    }
}

#[derive(Clone, Copy, Serialize)]
struct Signal {
    count: i64,
    min: f64,
    max: f64,
    mean: f64,
}

fn signal(row: &Row<'_>, index: usize, kind: &str) -> rusqlite::Result<Option<Signal>> {
    if row.get::<_, Option<String>>(index)?.as_deref() != Some(kind) {
        return Ok(None);
    }
    let count = row.get(index + 1)?;
    let min = row.get::<_, Option<f64>>(index + 2)?;
    let max = row.get::<_, Option<f64>>(index + 3)?;
    let mean = row.get::<_, Option<f64>>(index + 4)?;
    Ok(match (min, max, mean) {
        (Some(min), Some(max), Some(mean))
            if count > 0 && min.is_finite() && max.is_finite() && mean.is_finite() =>
        {
            Some(Signal {
                count,
                min,
                max,
                mean,
            })
        }
        _ => None,
    })
}

#[derive(Serialize)]
struct Link {
    schema: u32,
    link_id: i64,
    receiver: String,
    flight: String,
    observed_at_ms: i64,
    bin_start: i64,
    bin_seconds: i64,
    tx: Position,
    tx_alt_m: f64,
    tx_alt_datum: String,
    tx_msl_m: Option<f64>,
    tx_agl_m: Option<f64>,
    tx_height_basis: Option<&'static str>,
    distance_m: f64,
    azimuth_deg: f64,
    frequency_hz: i64,
    sonde_type: String,
    tx_power_mw: Option<f64>,
    tx_power_basis: Option<String>,
    lna: bool,
    rssi: Option<Signal>,
    snr: Option<Signal>,
    quality_flags: i64,
    flight_witness_after_ms: Option<i64>,
}

#[derive(Clone, Copy)]
struct Fix {
    position: Position,
    msl: f64,
    agl: f64,
}

fn load_links(
    db: &Connection,
    salt: &str,
    geoid: &Geoid,
    selected: &BTreeMap<i64, &Receiver>,
) -> Result<BTreeMap<i64, Vec<Link>>> {
    let mut statement = db.prepare(
        "SELECT id, receiver_id, source_id, observed_at_ms, bin_start, bin_seconds,
                tx_lat, tx_lon, tx_alt_m, tx_alt_datum, tx_agl_m, rx_lat, rx_lon, frequency_hz,
                sonde_type, tx_power_mw, tx_power_basis, lna, quality_flags,
                rssi_kind, rssi_count, rssi_min, rssi_max, rssi_mean,
                snr_kind, snr_count, snr_min, snr_max, snr_mean
         FROM measured_link WHERE rssi_kind='rdzTTGOsonde' OR snr_kind='radiosonde_auto_rx'
         ORDER BY id",
    )?;
    let mut rows = statement.query([])?;
    let mut links = BTreeMap::<_, Vec<_>>::new();
    let mut fixes = BTreeMap::<i64, BTreeMap<i64, Fix>>::new();
    let mut witnesses = BTreeMap::<i64, BTreeMap<i64, i64>>::new();
    let mut flight_numbers = BTreeMap::new();
    while let Some(row) = rows.next()? {
        let (id, rx_id, flight, time) = (row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?);
        let flags: i64 = row.get(18)?;
        let tx = Position::from_row(row, 6)?;
        let raw_alt: f64 = row.get(8)?;
        if !tx.valid() || !raw_alt.is_finite() {
            continue;
        }
        let datum = row.get::<_, String>(9)?;
        let agl = row.get::<_, Option<f64>>(10)?.filter(|h| h.is_finite());
        let msl = geoid.msl(tx, raw_alt, &datum);
        if flags & BAD_GEOMETRY == 0 {
            // The latest sighting by each receiver establishes independent flight coverage.
            witnesses
                .entry(flight)
                .or_default()
                .entry(rx_id)
                .and_modify(|old| *old = (*old).max(time))
                .or_insert(time);
            if let (Some(msl), Some(agl)) = (msl, agl) {
                fixes.entry(flight).or_default().entry(time).or_insert(Fix {
                    position: tx,
                    msl,
                    agl,
                });
            }
        }
        let Some(rx) = selected.get(&rx_id) else {
            continue;
        };
        if Position::from_row(row, 11)?.cell() != rx.position.cell() {
            continue;
        }
        let frequency_hz = row.get(13)?;
        let (distance_m, bearing) = rx.position.geometry(tx);
        if !(400_000_000..=406_000_000).contains(&frequency_hz) || distance_m < 30.0 {
            continue;
        }
        let flight_id = pseudonym(salt, "sondehub-flight:", &flight.to_string());
        flight_numbers.insert(flight_id.clone(), flight);
        links.entry(rx_id).or_default().push(Link {
            schema: SCHEMA_VERSION,
            link_id: id,
            receiver: rx.receiver.clone(),
            flight: flight_id,
            observed_at_ms: time,
            bin_start: row.get(4)?,
            bin_seconds: row.get(5)?,
            tx,
            tx_alt_m: raw_alt,
            tx_alt_datum: datum,
            tx_msl_m: msl,
            tx_agl_m: agl,
            tx_height_basis: msl.map(|_| "reported_datum"),
            distance_m,
            azimuth_deg: bearing.to_degrees().rem_euclid(360.0),
            frequency_hz,
            sonde_type: row.get(14)?,
            tx_power_mw: row
                .get::<_, Option<f64>>(15)?
                .filter(|p| p.is_finite() && *p > 0.0),
            tx_power_basis: row.get(16)?,
            lna: row.get::<_, i64>(17)? != 0,
            rssi: signal(row, 19, "rdzTTGOsonde")?,
            snr: signal(row, 24, "radiosonde_auto_rx")?,
            quality_flags: flags,
            flight_witness_after_ms: None,
        });
    }
    for (&id, rows) in &mut links {
        for row in rows {
            let flight = flight_numbers[&row.flight];
            row.flight_witness_after_ms = witnesses
                .get(&flight)
                .and_then(|latest| {
                    latest
                        .iter()
                        .filter(|&(&other, _)| other != id)
                        .map(|(_, &at)| at - row.observed_at_ms)
                        .max()
                })
                .filter(|&delta| delta > 0);
            if row.tx_msl_m.is_none()
                && let Some(fix) = fixes.get(&flight).and_then(|track| nearby_fix(track, row))
            {
                row.tx_msl_m = Some(fix.msl);
                row.tx_agl_m = Some(fix.agl);
                row.tx_height_basis = Some("same_flight_fix");
            }
        }
    }
    Ok(links)
}

fn nearby_fix(track: &BTreeMap<i64, Fix>, link: &Link) -> Option<Fix> {
    let time = link.observed_at_ms;
    track
        .range(time.saturating_sub(30_000)..=time.saturating_add(30_000))
        .filter(|&(_, fix)| {
            (fix.msl - link.tx_alt_m).abs() <= 250.0 && fix.position.geometry(link.tx).0 <= 3_000.0
        })
        .min_by_key(|&(&at, _)| at.abs_diff(time))
        .map(|(_, &fix)| fix)
}

#[derive(Serialize)]
struct Horizon {
    schema: u32,
    link_id: i64,
    elevation_k1_deg: Option<f64>,
    elevation_k133_deg: Option<f64>,
    terrain_k1_deg: Option<f64>,
    terrain_k133_deg: Option<f64>,
    terrain_radius_m: f64,
    missing_samples: usize,
    slant_m: Option<f64>,
}

#[derive(Clone, Copy)]
struct RaySample {
    horizon: [f64; 2],
    missing: usize,
}

fn direction(azimuth_deg: f64) -> i32 {
    ((azimuth_deg * f64::from(BEARINGS) / 360.0).round() as i32).rem_euclid(BEARINGS)
}

fn rays(
    raster: &RasterTerrain,
    plan: &ReceiverPlan,
    links: &[Link],
    height: f64,
) -> Result<BTreeMap<i32, Vec<RaySample>>> {
    let transform = Transformer::new(plan.crs)?;
    let step = resolution(plan.receiver.region);
    let sectors = links
        .iter()
        .map(|link| direction(link.azimuth_deg))
        .collect::<BTreeSet<_>>();
    let mut result = BTreeMap::new();
    for sector in sectors {
        let bearing = (f64::from(sector) * 360.0 / f64::from(BEARINGS)).to_radians();
        let end = transform.forward(plan.receiver.position.along(RADIUS, bearing).geographic())?;
        let points: Vec<_> = (1..=(RADIUS / step) as usize)
            .map(|index| {
                let fraction = index as f64 * step / RADIUS;
                Point {
                    x: plan.center.x + fraction * (end.x - plan.center.x),
                    y: plan.center.y + fraction * (end.y - plan.center.y),
                }
            })
            .collect();
        let terrain = raster.sample(&points, TerrainLayer::GroundDtm)?;
        let mut ray = RaySample {
            horizon: [f64::NEG_INFINITY; 2],
            missing: 0,
        };
        let mut prefix = vec![ray];
        for (index, ground) in terrain.into_iter().enumerate() {
            let distance = (index + 1) as f64 * step;
            if ground.is_finite() {
                for (angle, k) in ray.horizon.iter_mut().zip([1.0, BASE_K]) {
                    *angle = angle.max(elevation(distance, ground - height, k));
                }
            } else {
                ray.missing += 1;
            }
            prefix.push(ray);
        }
        result.insert(sector, prefix);
    }
    Ok(result)
}

fn elevation(distance: f64, height_difference: f64, k: f64) -> f64 {
    (height_difference / distance - distance / (2.0 * k * EARTH_MEAN_RADIUS_M))
        .atan()
        .to_degrees()
}

fn geometry(link: &Link, receiver: &Receiver, ray: Option<RaySample>, ray_radius: f64) -> Horizon {
    let difference = link
        .tx_msl_m
        .zip(receiver.antenna_msl_m)
        .map(|(tx, rx)| tx - rx);
    let terrain = |index: usize| {
        ray.filter(|r| r.missing == 0 && r.horizon[index].is_finite())
            .map(|r| r.horizon[index])
    };
    let slant_m = link.tx_msl_m.zip(receiver.antenna_msl_m).map(|(tx, rx)| {
        let horizontal = 2.0
            * ((EARTH_MEAN_RADIUS_M + rx) * (EARTH_MEAN_RADIUS_M + tx)).sqrt()
            * (link.distance_m / (2.0 * EARTH_MEAN_RADIUS_M)).sin();
        horizontal.hypot(tx - rx)
    });
    Horizon {
        schema: SCHEMA_VERSION,
        link_id: link.link_id,
        elevation_k1_deg: difference.map(|dh| elevation(link.distance_m, dh, 1.0)),
        elevation_k133_deg: difference.map(|dh| elevation(link.distance_m, dh, BASE_K)),
        terrain_k1_deg: terrain(0),
        terrain_k133_deg: terrain(1),
        terrain_radius_m: ray_radius,
        missing_samples: ray.map_or(0, |r| r.missing),
        slant_m,
    }
}

#[derive(Default)]
struct FetchStats {
    bytes: u64,
    hits: usize,
    tiles: usize,
    failed: usize,
}

impl FetchStats {
    fn raster(
        &mut self,
        downloader: &TerrainDownloader,
        crs: TerrainCrs,
        plan: &SourcePlan,
        region: &str,
    ) -> Result<Option<RasterTerrain>> {
        let grid = plan.dtm.as_ref().context("terrain plan has no DTM")?;
        let mut paths = Vec::new();
        let mut single_tile = grid.clone();
        let choice = choice(region);
        for &tile in &grid.tiles {
            self.tiles += 1;
            single_tile.tiles.clear();
            single_tile.tiles.push(tile);
            let fetched = downloader.fetch_plan(
                Product::Dtm,
                crs,
                &single_tile,
                &choice,
                Arc::new(|| false),
                |tile| {
                    if tile.cached {
                        self.hits += 1;
                    } else {
                        thread::sleep(Duration::from_millis(250));
                    }
                },
            );
            match fetched {
                Ok(fetched) => {
                    self.bytes += fetched.bytes_fetched;
                    paths.extend(fetched.paths);
                }
                Err(error) => {
                    self.failed += 1;
                    eprintln!("Skipping unavailable {region} terrain tile: {error}");
                }
            }
        }
        Ok(if paths.is_empty() {
            None
        } else {
            Some(RasterTerrain::open(&paths, &[])?)
        })
    }
}

#[derive(Serialize)]
struct Shadow {
    schema: u32,
    link_id: i64,
    distance_m: f64,
    step_m: f64,
    crs_epsg: u16,
    dtm: Vec<f64>,
    bullington_k1_db: f64,
    bullington_k133_db: f64,
}

struct ShadowPlan<'a> {
    link: &'a Link,
    plan: SourcePlan,
    crs: TerrainCrs,
    center: Point,
    end: Point,
    receiver: &'a Receiver,
}

fn shadow_plan<'a>(
    downloader: &TerrainDownloader,
    plan_rx: &'a ReceiverPlan,
    link: &'a Link,
) -> Result<ShadowPlan<'a>> {
    let end = Transformer::new(plan_rx.crs)?.forward(link.tx.geographic())?;
    let points = [plan_rx.center, end];
    let plan = plan(
        downloader,
        plan_rx.crs,
        plan_rx.receiver.region,
        bounds(points[0], points[1], 100.0),
        DownloadArea::Corridor {
            points: &points,
            half_width_m: 100.0,
            extra_points: &[],
        },
    )?;
    Ok(ShadowPlan {
        link,
        plan,
        crs: plan_rx.crs,
        center: points[0],
        end,
        receiver: &plan_rx.receiver,
    })
}

fn sample_shadow(raster: &RasterTerrain, plan: &ShadowPlan<'_>) -> Result<Shadow> {
    let [ground] = raster.sample(&[plan.end], TerrainLayer::GroundDtm)?[..] else {
        anyhow::bail!("expected one endpoint sample");
    };
    let rx_ground = plan.receiver.ground_m.context("missing receiver ground")?;
    let rx_height = plan
        .receiver
        .antenna_msl_m
        .context("missing receiver height")?;
    let tx_height = plan.link.tx_msl_m.context("missing sonde height")?;
    ensure!(
        ground.is_finite() && tx_height >= ground,
        "invalid sonde terrain endpoint"
    );
    let mut rx = Site::new(&plan.receiver.receiver, plan.center.x, plan.center.y);
    rx.ground_elevation_m = rx_ground;
    rx.antenna_height_m = rx_height - rx_ground;
    let mut tx = Site::new(&plan.link.flight, plan.end.x, plan.end.y);
    tx.ground_elevation_m = ground;
    tx.antenna_height_m = tx_height - ground;
    let freq = plan.link.frequency_hz as f64 / 1e6;
    let at_k = |k| -> Result<_> {
        let profile = sample_profile(raster, &rx, &tx, freq, k, None)?;
        let loss = bullington_loss_db(
            &profile.distances_m,
            &profile.effective_obstruction_m,
            &profile.los_elevation_m,
            freq,
        )?
        .loss_db;
        Ok((profile, loss))
    };
    let (profile, bullington_k1_db) = at_k(1.0)?;
    let (_, bullington_k133_db) = at_k(BASE_K)?;
    Ok(Shadow {
        schema: SCHEMA_VERSION,
        link_id: plan.link.link_id,
        distance_m: *profile.distances_m.last().context("empty profile")?,
        step_m: profile.distances_m[1],
        crs_epsg: plan.crs.epsg(),
        dtm: profile.dtm_elevation_m,
        bullington_k1_db,
        bullington_k133_db,
    })
}

fn open_snapshot() -> Result<Connection> {
    let directory = paths::repeaternet_data_root()?.join("calibration/snapshots");
    let file = directory.join(SNAPSHOT);
    paths::refuse_user_data(&file)?;
    ensure!(
        !fs::symlink_metadata(&file)?.is_symlink(),
        "refusing aliased SondeHub snapshot"
    );
    let actual = fs::canonicalize(&file)?;
    let expected = fs::canonicalize(paths::repeaternet_data_root()?)?
        .join("calibration/snapshots")
        .join(SNAPSHOT);
    ensure!(
        actual == expected,
        "refusing aliased SondeHub snapshot directory"
    );
    Ok(Connection::open_with_flags(
        actual,
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?)
}

pub fn run(work: &Path, cap_gb: f64, estimate_only: bool) -> Result<()> {
    ensure!(
        (0.0..=20.0).contains(&cap_gb),
        "--max-download-gb must be between 0 and 20"
    );
    let cap = cap_gb * 1e9;
    let db = open_snapshot()?;
    let salt_path = work.join("salt.txt");
    paths::refuse_user_data(&salt_path)?;
    let salt = fs::read_to_string(salt_path).context("read existing calibration salt")?;
    let salt = salt.trim();
    ensure!(!salt.is_empty(), "calibration salt is empty");

    let registry = Arc::new(ProviderRegistry::builtin());
    let coverage = &registry
        .terrain()
        .iter()
        .find(|p| p.id == "kartverket-nhm")
        .context("missing Kartverket NHM provider")?
        .coverage;
    let cache = paths::shared_cache_dir()?;
    fs::create_dir_all(&cache)?;
    let downloader = TerrainDownloader::new(registry.clone(), &cache, TileLimits::default())?;
    let mut norway = Estimate::default();
    let mut world = Estimate::default();
    let mut plans = Vec::new();
    let mut unplanned = 0;
    for (id, receiver) in inventories(&db, salt, coverage)? {
        match plan_receiver(&downloader, receiver) {
            Ok(plan) => {
                let estimate = if plan.receiver.region == "norway" {
                    &mut norway
                } else {
                    &mut world
                };
                estimate.add(&downloader, plan.crs, &plan.plan, &cache)?;
                plans.push((id, plan));
            }
            Err(_) => unplanned += 1,
        }
    }
    norway.print("Norway receiver");
    world.print("World receiver");
    ensure!(
        norway.bytes <= cap,
        "Norway receiver terrain estimate exceeds {cap_gb} GB"
    );
    if norway.bytes + world.bytes > cap {
        println!("Skipping world sample because the combined estimate exceeds {cap_gb} GB");
        plans.retain(|(_, plan)| plan.receiver.region == "norway");
    }
    let receiver_estimate = if plans.iter().any(|(_, p)| p.receiver.region == "world") {
        norway.bytes + world.bytes
    } else {
        norway.bytes
    };
    println!(
        "Planning {} Norway receivers and {} world receivers ({unplanned} unavailable)",
        plans
            .iter()
            .filter(|(_, p)| p.receiver.region == "norway")
            .count(),
        plans
            .iter()
            .filter(|(_, p)| p.receiver.region == "world")
            .count()
    );
    if estimate_only {
        return Ok(());
    }

    let output = work.join("sondehub");
    for filename in [
        "links.jsonl",
        "receivers.jsonl",
        "horizons.jsonl",
        "profiles.jsonl",
    ] {
        paths::refuse_user_data(&output.join(filename))?;
    }
    let geoid = Geoid::load()?;
    let selected = plans.iter().map(|(id, p)| (*id, &p.receiver)).collect();
    let links = load_links(&db, salt, &geoid, &selected)?;
    drop(db);
    let mut horizons = Vec::new();
    let mut stats = FetchStats::default();
    for (index, (id, plan)) in plans.iter_mut().enumerate() {
        let Some(rows) = links.get(id) else { continue };
        let before = stats.bytes;
        let raster = stats.raster(&downloader, plan.crs, &plan.plan, plan.receiver.region)?;
        if (index + 1) % 10 == 0 || index == 0 {
            println!(
                "Receiver terrain {0}: {1} upstream bytes total ({2} for this site)",
                index + 1,
                stats.bytes,
                stats.bytes - before
            );
        }
        let ground = raster
            .as_ref()
            .map(|raster| raster.sample(&[plan.center], TerrainLayer::GroundDtm))
            .transpose()?
            .and_then(|samples| samples.first().copied())
            .filter(|ground| ground.is_finite());
        if let Some(ground) = ground {
            let rx = &mut plan.receiver;
            let reported = rx
                .reported_alt_m
                .and_then(|alt| {
                    if rx.reported_alt_datum == "unknown" {
                        Some(alt)
                    } else {
                        geoid.msl(rx.position, alt, &rx.reported_alt_datum)
                    }
                })
                .filter(|h| (2.0..=100.0).contains(&(h - ground)));
            rx.ground_m = Some(ground);
            rx.antenna_msl_m = Some(reported.unwrap_or(ground + 2.0));
            rx.height_basis = Some(if reported.is_some() {
                "reported_near_dem"
            } else {
                "dem_plus_2m"
            });
        }
        let rays = match (raster, plan.receiver.antenna_msl_m) {
            (Some(raster), Some(height)) => rays(&raster, plan, rows, height)?,
            _ => BTreeMap::new(),
        };
        for link in rows {
            let step = resolution(plan.receiver.region);
            let index =
                ((link.distance_m / step).ceil() - 1.0).min((RADIUS / step).floor()) as usize;
            let ray = rays
                .get(&direction(link.azimuth_deg))
                .map(|prefix| prefix[index]);
            horizons.push(geometry(link, &plan.receiver, ray, index as f64 * step));
        }
    }
    println!("Receiver terrain fetched {} upstream bytes", stats.bytes);

    let horizon_by_link: BTreeMap<_, _> = horizons.iter().map(|row| (row.link_id, row)).collect();
    let mut shadows = Vec::new();
    let mut estimate = Estimate::default();
    let mut selected = BTreeSet::new();
    // One path per receiver and flight, favouring the deepest local terrain shadows.
    let mut candidates: Vec<_> = plans
        .iter()
        .flat_map(|(id, plan)| {
            links
                .get(id)
                .into_iter()
                .flatten()
                .map(move |link| (plan, link))
        })
        .filter_map(|(plan, link)| {
            let rx = &plan.receiver;
            let h = horizon_by_link[&link.link_id];
            let elevation = h.elevation_k133_deg?;
            let obstruction = h.terrain_k133_deg? - elevation;
            (rx.region == "norway"
                && coverage.contains(link.tx.geographic())
                && (500.0..=80_000.0).contains(&link.distance_m)
                && (-1.0..=2.0).contains(&elevation)
                && obstruction > 0.1
                && link.quality_flags & BAD_GEOMETRY == 0
                && link.tx_power_mw.is_some()
                && (link.snr.is_some() || (link.rssi.is_some() && link.quality_flags & 128 == 0)))
                .then_some((obstruction, plan, link))
        })
        .collect();
    candidates.sort_unstable_by(|a, b| b.0.total_cmp(&a.0).then(a.2.link_id.cmp(&b.2.link_id)));
    let mut receiver_counts = BTreeMap::<_, usize>::new();
    for (_, plan, link) in candidates {
        let count = receiver_counts.entry(&link.receiver).or_default();
        if *count >= 4 || !selected.insert((&link.receiver, &link.flight)) {
            continue;
        }
        let Ok(shadow) = shadow_plan(&downloader, plan, link) else {
            continue;
        };
        estimate.add(&downloader, shadow.crs, &shadow.plan, &cache)?;
        shadows.push(shadow);
        *count += 1;
        if shadows.len() == SHADOW_LIMIT {
            break;
        }
    }
    estimate.print("Shadow profile");
    let mut profiles = Vec::new();
    if receiver_estimate.max(stats.bytes as f64) + estimate.bytes <= cap {
        for shadow in shadows {
            if let Some(raster) = stats.raster(
                &downloader,
                shadow.crs,
                &shadow.plan,
                shadow.receiver.region,
            )? && let Ok(profile) = sample_shadow(&raster, &shadow)
            {
                profiles.push(profile);
            }
        }
    } else {
        println!("Skipping shadow profiles because the combined estimate exceeds {cap_gb} GB");
    }
    println!(
        "Read {} confirmed upstream terrain bytes, {}/{} composed tiles cached, {} unavailable",
        stats.bytes, stats.hits, stats.tiles, stats.failed
    );

    let mut links: Vec<_> = links.values().flatten().collect();
    links.sort_unstable_by_key(|row| row.link_id);
    horizons.sort_unstable_by_key(|row| row.link_id);
    profiles.sort_unstable_by_key(|row| row.link_id);
    let receivers: Vec<_> = plans.iter().map(|(_, plan)| &plan.receiver).collect();
    jsonl::write(&output.join("receivers.jsonl"), &receivers)?;
    jsonl::write(&output.join("horizons.jsonl"), &horizons)?;
    jsonl::write(&output.join("profiles.jsonl"), &profiles)?;
    jsonl::write(&output.join("links.jsonl"), &links)?;
    println!(
        "Wrote {} links, {} receiver rows and {} shadow profiles",
        links.len(),
        receivers.len(),
        profiles.len()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_rows_use_private_ids_and_match_sonde_height_from_the_same_flight() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let file = temp.path().join(SNAPSHOT);
        let db = Connection::open(&file)?;
        db.execute_batch(include_str!("../../sondehub-import/src/schema.sql"))?;
        db.execute_batch(
            "INSERT INTO receiver VALUES (1,'TEST-Private'),(2,'TEST-Witness');
             INSERT INTO source_object
                 (id, locator, archive_day, retrieved_at, licence, status, download_bytes,
                  input_rows, kept_rows, located_links, reject_count, rejects)
                 VALUES (1,'test-fixture','2026-01-01',0,'test','complete',0,2,2,2,0,'{}');
             INSERT INTO measured_link
                (id, source_id, receiver_id, serial, frame, observed_at_ms,
                 tx_lat,tx_lon,tx_alt_m,tx_alt_datum,tx_agl_m,
                 rx_lat,rx_lon,rx_alt_m,rx_alt_datum,frequency_hz,frequency_basis,
                 sonde_type,tx_power_mw,tx_power_basis,decoder,rx_antenna,lna,
                 bin_start,bin_seconds,bin_count,rssi_kind,rssi_count,rssi_min,rssi_max,rssi_mean,
                 snr_kind,snr_count,snr_min,snr_max,snr_mean,quality_flags)
             VALUES
                (1,1,1,'TEST-SERIAL',3,20000,60.1,10,2002,'unknown',NULL,
                 60,10,100,'unknown',403000000,'reported_tx','RS41',60,
                 'nominal_model_spec','rdz','TEST-Private antenna',1,0,300,2,
                 'rdzTTGOsonde',2,-102,-98,-100,NULL,0,NULL,NULL,NULL,80),
                (2,1,2,'TEST-SERIAL',3,25000,60.1,10,2000,'MSL',1800,
                 61,10,100,'unknown',403000000,'reported_tx','RS41',60,
                 'nominal_model_spec','auto_rx',NULL,0,0,30,1,
                 NULL,0,NULL,NULL,NULL,'radiosonde_auto_rx',1,10,10,10,64);",
        )?;
        drop(db);
        let db = Connection::open_with_flags(&file, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        assert!(db.execute("DELETE FROM receiver", []).is_err());
        let coverage = Coverage::Bbox {
            bbox: [9.0, 59.0, 11.0, 60.5],
        };
        let receivers = inventories(&db, "test-salt", &coverage)?;
        assert_eq!(receivers[0].1.region, "norway");
        assert_eq!(receivers[1].1.region, "world");
        let selected = BTreeMap::from([(1, &receivers[0].1)]);
        let links = load_links(&db, "test-salt", &Geoid::load()?, &selected)?;
        let link = &links[&1][0];
        assert_eq!(links.len(), 1);
        assert_eq!(link.receiver, "c6eceda440296248");
        assert_eq!(link.tx_height_basis, Some("same_flight_fix"));
        assert_eq!(link.tx_agl_m, Some(1800.0));
        assert_eq!(link.flight_witness_after_ms, Some(5000));
        assert_eq!(link.rssi.unwrap().mean, -100.0);
        assert!(link.snr.is_none());
        let json = serde_json::to_string(&link)?;
        assert!(!json.contains("TEST-Private") && !json.contains("TEST-SERIAL"));

        let outfile = temp.path().join("links.jsonl");
        jsonl::write(&outfile, &[link])?;
        let first = fs::read(&outfile)?;
        jsonl::write(&outfile, &[link])?;
        assert_eq!(first, fs::read(&outfile)?);
        Ok(())
    }

    #[test]
    fn earth_curvature_lowers_the_local_angle_and_bearing_wraps() {
        let at = Position {
            lat: 60.0,
            lon: 179.99,
        };
        let target = at.along(120_000.0, 90.0_f64.to_radians());
        let (distance, azimuth) = at.geometry(target);
        assert!(target.lon < 0.0);
        assert!((distance - 120_000.0).abs() < 0.01);
        assert!((azimuth.to_degrees() - 90.0).abs() < 0.001);
        assert!(elevation(distance, 1000.0, 1.0) < elevation(distance, 1000.0, BASE_K));
        assert_eq!(direction(359.9), direction(0.0));
    }
}
