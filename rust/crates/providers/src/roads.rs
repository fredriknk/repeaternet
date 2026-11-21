use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs,
    io::BufReader,
    path::{Path, PathBuf},
    sync::{Mutex, TryLockError},
    time::Duration,
};

use geo::Distance;
use geo::{
    BooleanOps, BoundingRect, Euclidean, Length, LineString, MultiLineString, Point as GeoPoint,
    Polygon, Simplify,
};
use repeaternet_terrain::{LonLat, Point, TerrainCrs, Transformer};
use reqwest::blocking::Client;
use rstar::{RTree, RTreeObject, AABB};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{invalid, response_error, transport, ProviderError, Result};

const PROVIDER: &str = "Overpass";
pub const DEFAULT_ENDPOINT: &str = "https://overpass-api.de/api/interpreter";
pub const LOADING_STAGE: &str = "Loading road network";
const BODY_LIMIT: u64 = 64 * 1024 * 1024;
const PUBLIC_TAGS: &str = "motorway|trunk|primary|secondary|tertiary|unclassified|residential|service|living_street|motorway_link|trunk_link|primary_link|secondary_link|tertiary_link|unclassified_link|residential_link|service_link|living_street_link";
static OVERPASS_SLOT: Mutex<()> = Mutex::new(());

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RoadAccessClasses {
    PublicRoads,
    #[default]
    AllDrivable,
}

impl RoadAccessClasses {
    fn tags(self) -> String {
        let mut tags = PUBLIC_TAGS.to_owned();
        if self == Self::AllDrivable {
            tags.push_str("|track");
        }
        tags
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RoadTile {
    pub row: i32,
    pub col: i32,
}

impl RoadTile {
    pub fn query(self, classes: RoadAccessClasses) -> String {
        let (south, west, north, east) = self.bounds();
        let tags = classes.tags();
        format!("[out:json][timeout:180];way[\"highway\"~\"^({tags})$\"]({south:.1},{west:.1},{north:.1},{east:.1});out skel geom;")
    }

    fn bounds(self) -> (f64, f64, f64, f64) {
        (
            f64::from(self.row) / 10.0,
            f64::from(self.col) / 5.0,
            (f64::from(self.row) + 1.0) / 10.0,
            (f64::from(self.col) + 1.0) / 5.0,
        )
    }

    fn projected(self, transform: &Transformer) -> Result<Polygon<f64>> {
        let (south, west, north, east) = self.bounds();
        let mut ring = Vec::with_capacity(80);
        for (a, b) in [
            ((west, south), (east, south)),
            ((east, south), (east, north)),
            ((east, north), (west, north)),
            ((west, north), (west, south)),
        ] {
            for step in 0..20 {
                let t = f64::from(step) / 20.0;
                let point = transform.forward(LonLat {
                    longitude: a.0 + (b.0 - a.0) * t,
                    latitude: a.1 + (b.1 - a.1) * t,
                })?;
                ring.push((point.x, point.y));
            }
        }
        Ok(Polygon::new(LineString::from(ring), vec![]))
    }
}

fn cache_path(
    directory: &Path,
    endpoint: &str,
    classes: RoadAccessClasses,
    tile: RoadTile,
) -> PathBuf {
    let endpoint = serde_json::to_string(endpoint).expect("JSON string");
    let mut ascii_endpoint = String::new();
    for c in endpoint.chars() {
        if c.is_ascii() && c != '\u{7f}' {
            ascii_endpoint.push(c);
        } else {
            use std::fmt::Write;
            for unit in c.encode_utf16(&mut [0; 2]) {
                write!(ascii_endpoint, "\\u{unit:04x}").expect("write to String");
            }
        }
    }
    let key = format!(
        "[{ascii_endpoint}, \"{}\", [{}, {}], 1]",
        classes.tags(),
        tile.row,
        tile.col
    );
    directory.join(format!(
        "overpass-tile-{}.json",
        crate::lower_hex(&Sha256::digest(key.as_bytes()))
    ))
}

struct IndexedLine {
    index: usize,
    envelope: AABB<[f64; 2]>,
}

impl RTreeObject for IndexedLine {
    type Envelope = AABB<[f64; 2]>;
    fn envelope(&self) -> Self::Envelope {
        self.envelope
    }
}

pub type RouterLocations = BTreeMap<String, Point>;
pub type NearestDistances = BTreeMap<String, Option<f64>>;

pub struct RoadAccessIndex {
    lines: Vec<LineString<f64>>,
    index: RTree<IndexedLine>,
    max_distance_m: f64,
}

impl RoadAccessIndex {
    pub fn new(lines: Vec<LineString<f64>>, max_distance_m: f64) -> Result<Self> {
        validate_radius(max_distance_m)?;
        if lines.iter().any(|line| {
            line.0.len() < 2 || line.0.iter().any(|c| !c.x.is_finite() || !c.y.is_finite())
        }) {
            return Err(invalid("Invalid road geometry"));
        }
        let index = RTree::bulk_load(
            lines
                .iter()
                .enumerate()
                .filter_map(|(index, line)| {
                    let bounds = line.bounding_rect()?;
                    Some(IndexedLine {
                        index,
                        envelope: AABB::from_corners(bounds.min().into(), bounds.max().into()),
                    })
                })
                .collect(),
        );
        Ok(Self {
            lines,
            index,
            max_distance_m,
        })
    }

    pub fn allowed(&self, points: &[Point]) -> Vec<bool> {
        points
            .iter()
            .map(|&point| self.nearest_distance(point, self.max_distance_m).is_some())
            .collect()
    }

    pub fn nearest_distance(&self, point: Point, radius: f64) -> Option<f64> {
        if !point.x.is_finite() || !point.y.is_finite() || !radius.is_finite() || radius < 0.0 {
            return None;
        }
        let envelope = AABB::from_corners(
            [point.x - radius, point.y - radius],
            [point.x + radius, point.y + radius],
        );
        let center = GeoPoint::new(point.x, point.y);
        self.index
            .locate_in_envelope_intersecting(envelope)
            .map(|item| Euclidean.distance(&center, &self.lines[item.index]))
            .filter(|&distance| distance <= radius)
            .min_by(f64::total_cmp)
    }

    pub fn nearby(
        &self,
        locations: &RouterLocations,
        radius: f64,
    ) -> Result<(NearestDistances, Vec<LineString<f64>>)> {
        validate_radius(radius)?;
        let nearest = locations
            .iter()
            .map(|(id, &point)| (id.clone(), self.nearest_distance(point, radius)))
            .collect();
        if locations.is_empty() {
            return Ok((nearest, vec![]));
        }
        if locations
            .values()
            .any(|p| !p.x.is_finite() || !p.y.is_finite())
        {
            return Err(invalid("Router locations must be finite"));
        }
        let buffers = locations
            .values()
            .map(|point| {
                let ring = (0..128)
                    .map(|index| {
                        let (sin, cos) =
                            (f64::from(index) * std::f64::consts::TAU / 128.0).sin_cos();
                        (point.x + radius * cos, point.y + radius * sin)
                    })
                    .collect::<Vec<_>>();
                Polygon::new(LineString::from(ring), vec![])
            })
            .collect::<Vec<_>>();
        let area = geo::unary_union(&buffers);
        let Some(bounds) = area.bounding_rect() else {
            return Ok((nearest, vec![]));
        };
        let envelope = AABB::from_corners(bounds.min().into(), bounds.max().into());
        let mut indices: Vec<_> = self
            .index
            .locate_in_envelope_intersecting(envelope)
            .map(|item| item.index)
            .collect();
        indices.sort_unstable();
        let mut clipped = Vec::new();
        for index in indices {
            for line in area
                .clip(&MultiLineString(vec![self.lines[index].clone()]), false)
                .0
            {
                if Euclidean.length(&line) > 0.0 {
                    let mut line = line.simplify(7.0);
                    if line
                        .0
                        .last()
                        .zip(line.0.first())
                        .is_some_and(|(last, first)| {
                            last.x
                                .total_cmp(&first.x)
                                .then(last.y.total_cmp(&first.y))
                                .is_lt()
                        })
                    {
                        line.0.reverse();
                    }
                    clipped.push(line);
                }
            }
        }
        Ok((nearest, clipped))
    }
}

#[derive(Debug, Serialize)]
pub struct RoadOverlay {
    /// Shared polylines, with Leaflet coordinates in latitude/longitude order.
    pub polylines: Vec<Vec<[f64; 2]>>,
    pub polyline_indices: Vec<Vec<usize>>,
    pub nearest_distances_m: Vec<NearestDistances>,
}

pub struct TiledRoadAccess {
    transform: Transformer,
    endpoint: String,
    classes: RoadAccessClasses,
    max_distance_m: f64,
    cache: PathBuf,
    client: Client,
    retry_base: Duration,
    retry_cap: Duration,
    loaded: HashSet<RoadTile>,
    lines: Vec<LineString<f64>>,
    line_keys: HashSet<Vec<[u64; 2]>>,
    index: Option<RoadAccessIndex>,
    /// Tiles requested for download across all batches; progress is cumulative
    /// so it never runs backwards when later batches need more tiles.
    tiles_requested: usize,
    tiles_fetched: usize,
}

impl TiledRoadAccess {
    pub fn new(
        crs: TerrainCrs,
        classes: RoadAccessClasses,
        max_distance_m: f64,
        cache: impl Into<PathBuf>,
    ) -> Result<Self> {
        let endpoint =
            std::env::var("RF_PLANNER_OVERPASS_URL").unwrap_or_else(|_| DEFAULT_ENDPOINT.into());
        Self::with_client(
            crs,
            classes,
            max_distance_m,
            cache,
            endpoint,
            transport::client(PROVIDER, 10, 195)?,
        )
    }

    pub fn with_client(
        crs: TerrainCrs,
        classes: RoadAccessClasses,
        max_distance_m: f64,
        cache: impl Into<PathBuf>,
        endpoint: impl Into<String>,
        client: Client,
    ) -> Result<Self> {
        validate_radius(max_distance_m)?;
        Ok(Self {
            transform: Transformer::new(crs)?,
            endpoint: endpoint.into(),
            classes,
            max_distance_m,
            cache: cache.into(),
            client,
            retry_base: Duration::from_secs(1),
            retry_cap: Duration::from_secs(195),
            loaded: HashSet::new(),
            lines: Vec::new(),
            line_keys: HashSet::new(),
            index: None,
            tiles_requested: 0,
            tiles_fetched: 0,
        })
    }

    pub fn with_retry_timing(mut self, base: Duration, cap: Duration) -> Self {
        self.retry_base = base;
        self.retry_cap = cap;
        self
    }

    pub fn cache_path(&self, tile: RoadTile) -> PathBuf {
        cache_path(&self.cache, &self.endpoint, self.classes, tile)
    }

    fn index(&mut self) -> Result<&RoadAccessIndex> {
        if self.index.is_none() {
            self.index = Some(RoadAccessIndex::new(
                self.lines.clone(),
                self.max_distance_m,
            )?);
        }
        Ok(self.index.as_ref().expect("index is initialized"))
    }

    pub fn allowed(
        &mut self,
        points: &[Point],
        cancelled: &dyn Fn() -> bool,
        progress: impl FnMut(&str, usize, usize),
    ) -> Result<Vec<bool>> {
        self.load_for_points(points, self.max_distance_m, cancelled, progress)?;
        let index = self.index()?;
        let mut allowed = Vec::with_capacity(points.len());
        for chunk in points.chunks(4096) {
            transport::check(cancelled)?;
            allowed.extend(index.allowed(chunk));
        }
        Ok(allowed)
    }

    /// Pass only router sites belonging to each alternative, excluding endpoints and clients.
    pub fn overlay(
        &mut self,
        alternatives: &[RouterLocations],
        cancelled: &dyn Fn() -> bool,
        progress: impl FnMut(&str, usize, usize),
    ) -> Result<RoadOverlay> {
        let points: Vec<_> = alternatives
            .iter()
            .flat_map(|locations| locations.values().copied())
            .collect();
        let radius = self.max_distance_m * 1.5;
        self.load_for_points(&points, radius, cancelled, progress)?;
        self.index()?;
        let index = self.index.as_ref().expect("index is initialized");
        let mut key_to_index = HashMap::new();
        let mut overlay = RoadOverlay {
            polylines: vec![],
            polyline_indices: vec![],
            nearest_distances_m: vec![],
        };
        for locations in alternatives {
            transport::check(cancelled)?;
            let (distances, lines) = index.nearby(locations, radius)?;
            overlay.nearest_distances_m.push(distances);
            let mut indices = BTreeSet::new();
            for line in lines {
                let key = line
                    .0
                    .iter()
                    .map(|p| {
                        let ll = self.transform.reverse(Point { x: p.x, y: p.y })?;
                        Ok([
                            (ll.latitude * 1e6).round_ties_even() as i64,
                            (ll.longitude * 1e6).round_ties_even() as i64,
                        ])
                    })
                    .collect::<Result<Vec<_>>>()?;
                if key.windows(2).any(|pair| pair[0] != pair[1]) {
                    let index = *key_to_index.entry(key.clone()).or_insert_with(|| {
                        let index = overlay.polylines.len();
                        overlay.polylines.push(
                            key.iter()
                                .map(|[lat, lon]| [*lat as f64 / 1e6, *lon as f64 / 1e6])
                                .collect(),
                        );
                        index
                    });
                    indices.insert(index);
                }
            }
            overlay.polyline_indices.push(indices.into_iter().collect());
        }
        Ok(overlay)
    }

    fn load_for_points(
        &mut self,
        points: &[Point],
        radius: f64,
        cancelled: &dyn Fn() -> bool,
        mut progress: impl FnMut(&str, usize, usize),
    ) -> Result<()> {
        let tiles = tiles_for_points(points, radius, &self.transform, cancelled)?;
        let missing = tiles
            .iter()
            .filter(|tile| !self.loaded.contains(tile))
            .count();
        if missing == 0 {
            return Ok(());
        }
        self.tiles_requested += missing;
        progress(LOADING_STAGE, self.tiles_fetched, self.tiles_requested);
        for &tile in &tiles {
            transport::check(cancelled)?;
            if !self.loaded.contains(&tile) {
                let lines = self
                    .load_tile(tile, cancelled)
                    .map_err(|error| match error {
                        ProviderError::Cancelled => error,
                        _ => ProviderError::RoadTile {
                            total: tiles.len(),
                            row: tile.row,
                            col: tile.col,
                            source: Box::new(error),
                        },
                    })?;
                for line in lines {
                    let key = line
                        .0
                        .iter()
                        .map(|p| [p.x.to_bits(), p.y.to_bits()])
                        .collect();
                    if self.line_keys.insert(key) {
                        self.lines.push(line);
                        self.index = None;
                    }
                }
                self.loaded.insert(tile);
                self.tiles_fetched += 1;
                progress(LOADING_STAGE, self.tiles_fetched, self.tiles_requested);
            }
        }
        transport::check(cancelled)
    }

    fn load_tile(
        &self,
        tile: RoadTile,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Vec<LineString<f64>>> {
        let _slot = loop {
            transport::check(cancelled)?;
            match OVERPASS_SLOT.try_lock() {
                Ok(guard) => break guard,
                Err(TryLockError::Poisoned(error)) => break error.into_inner(),
                Err(TryLockError::WouldBlock) => {
                    transport::wait(Duration::from_millis(50), cancelled)?
                }
            }
        };
        let destination = self.cache_path(tile);
        if let Ok(file) = fs::File::open(&destination) {
            if file.metadata()?.len() <= BODY_LIMIT {
                if let Ok(payload) = serde_json::from_reader(BufReader::new(file)) {
                    match self.project_lines(&payload, cancelled) {
                        Ok(lines) => return Ok(lines),
                        Err(ProviderError::Cancelled) => return Err(ProviderError::Cancelled),
                        Err(_) => {}
                    }
                }
            }
        }
        let payload = self.request_tile(&tile.query(self.classes), cancelled)?;
        let lines = self.project_lines(&payload, cancelled)?;
        fs::create_dir_all(&self.cache)?;
        let temporary = tempfile::NamedTempFile::new_in(&self.cache)?;
        serde_json::to_writer(temporary.as_file(), &payload).map_err(std::io::Error::other)?;
        temporary.as_file().sync_all()?;
        transport::check(cancelled)?;
        temporary
            .persist(destination)
            .map_err(|error| error.error)?;
        Ok(lines)
    }

    fn project_lines(
        &self,
        payload: &Value,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Vec<LineString<f64>>> {
        let elements = payload
            .get("elements")
            .and_then(Value::as_array)
            .ok_or_else(|| response_error(PROVIDER, "missing elements list"))?;
        if let Some(remark) = payload.get("remark").filter(|value| {
            !value.is_null() && value.as_bool() != Some(false) && value.as_str() != Some("")
        }) {
            return Err(response_error(
                PROVIDER,
                format!("Overpass reported {remark}"),
            ));
        }
        let mut lines = Vec::new();
        let invalid_geometry = || response_error(PROVIDER, "invalid road geometry");
        for way in elements {
            transport::check(cancelled)?;
            let kind = way
                .get("type")
                .and_then(Value::as_str)
                .ok_or_else(invalid_geometry)?;
            if kind != "way" {
                continue;
            }
            let geometry = way
                .get("geometry")
                .and_then(Value::as_array)
                .filter(|g| g.len() >= 2)
                .ok_or_else(invalid_geometry)?;
            let mut line = Vec::with_capacity(geometry.len());
            for point in geometry {
                let coordinate = |name| {
                    point
                        .get(name)
                        .and_then(|value| value.as_f64().or_else(|| value.as_str()?.parse().ok()))
                        .ok_or_else(invalid_geometry)
                };
                let p = self.transform.forward(LonLat {
                    longitude: coordinate("lon")?,
                    latitude: coordinate("lat")?,
                })?;
                line.push((p.x, p.y));
            }
            lines.push(LineString::from(line));
        }
        Ok(lines)
    }

    fn request_tile(&self, query: &str, cancelled: &dyn Fn() -> bool) -> Result<Value> {
        for attempt in 0..4 {
            transport::check(cancelled)?;
            let (error, retry, retry_after) = match self
                .client
                .post(&self.endpoint)
                .form(&[("data", query)])
                .header("User-Agent", transport::user_agent("road access planner"))
                .send()
            {
                Ok(response) if response.status().is_success() => {
                    return transport::json(PROVIDER, response, BODY_LIMIT, cancelled)
                }
                Ok(response) => {
                    let status = response.status().as_u16();
                    (
                        ProviderError::Http {
                            provider: PROVIDER,
                            status,
                        },
                        matches!(status, 429 | 502 | 503 | 504),
                        transport::retry_seconds(&response, true),
                    )
                }
                Err(source) => {
                    let retry = source.is_connect() || source.is_timeout();
                    (
                        ProviderError::Transport {
                            provider: PROVIDER,
                            source,
                        },
                        retry,
                        None,
                    )
                }
            };
            transport::check(cancelled)?;
            if !retry || attempt == 3 {
                return Err(error);
            }
            let seconds = self
                .retry_base
                .saturating_mul(2u32.pow(attempt))
                .as_secs_f64()
                .max(retry_after.unwrap_or(0.0))
                .min(self.retry_cap.as_secs_f64());
            transport::wait(Duration::from_secs_f64(seconds), cancelled)?;
        }
        unreachable!("the final attempt returns")
    }
}

fn validate_radius(radius: f64) -> Result<()> {
    if radius.is_finite() && radius > 0.0 {
        Ok(())
    } else {
        Err(invalid("Road distance must be finite and positive"))
    }
}

/// Select only tiles within the radius (plus one metre) of an actual candidate.
pub fn tiles_for_points(
    points: &[Point],
    radius: f64,
    transform: &Transformer,
    cancelled: &dyn Fn() -> bool,
) -> Result<Vec<RoadTile>> {
    transport::check(cancelled)?;
    validate_radius(radius)?;
    let radius = radius + 1.0;
    let mut polygons = HashMap::new();
    let mut selected = BTreeSet::new();
    for &point in points {
        transport::check(cancelled)?;
        let center = transform.reverse(point)?;
        let mut west = center.longitude;
        let mut east = center.longitude;
        let mut south = center.latitude;
        let mut north = center.latitude;
        // Include edge midpoints to account for curved inverse-projection edges.
        for (dx, dy) in [
            (-1., -1.),
            (0., -1.),
            (1., -1.),
            (1., 0.),
            (1., 1.),
            (0., 1.),
            (-1., 1.),
            (-1., 0.),
        ] {
            let ll = transform.reverse(Point {
                x: point.x + dx * radius,
                y: point.y + dy * radius,
            })?;
            let lon = center.longitude
                + (ll.longitude - center.longitude + 180.0).rem_euclid(360.0)
                - 180.0;
            west = west.min(lon);
            east = east.max(lon);
            south = south.min(ll.latitude);
            north = north.max(ll.latitude);
        }
        for row in
            ((south * 10.0).floor() as i32).max(-900)..=((north * 10.0).floor() as i32).min(899)
        {
            transport::check(cancelled)?;
            for col in (west * 5.0).floor() as i32..=(east * 5.0).floor() as i32 {
                let tile = RoadTile {
                    row,
                    col: (col + 900).rem_euclid(1800) - 900,
                };
                if selected.contains(&tile) {
                    continue;
                }
                let polygon = match polygons.entry(tile) {
                    std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
                    std::collections::hash_map::Entry::Vacant(entry) => {
                        entry.insert(tile.projected(transform)?)
                    }
                };
                if Euclidean.distance(&GeoPoint::new(point.x, point.y), &*polygon) <= radius {
                    selected.insert(tile);
                }
            }
        }
    }
    Ok(selected.into_iter().collect())
}
