use std::collections::{BTreeMap, BTreeSet, HashMap};

use repeaternet_terrain::LonLat;

use super::model::{Position, Source};
use crate::dataset::Radio;

const EARTH_RADIUS_M: f64 = 6_371_000.0;
const DUPLICATE_M: f64 = 5.0;
type CoordinateKey = (u64, u64);
type Cell = [i32; 3];

pub(super) fn point(lat: f64, lon: f64) -> Option<LonLat> {
    if !(-90.0..=90.0).contains(&lat)
        || !(-180.0..=180.0).contains(&lon)
        || (lat == 0.0 && lon == 0.0)
    {
        return None;
    }
    Some(LonLat {
        latitude: lat + 0.0,
        longitude: lon + 0.0,
    })
}

fn key(point: LonLat) -> CoordinateKey {
    (point.latitude.to_bits(), point.longitude.to_bits())
}

pub(super) fn distance(a: LonLat, b: LonLat) -> f64 {
    let (lat_a, lat_b) = (a.latitude.to_radians(), b.latitude.to_radians());
    let h = ((lat_b - lat_a) / 2.0).sin().powi(2)
        + lat_a.cos()
            * lat_b.cos()
            * ((b.longitude - a.longitude).to_radians() / 2.0)
                .sin()
                .powi(2);
    2.0 * EARTH_RADIUS_M * h.clamp(0.0, 1.0).sqrt().asin()
}

fn cell(point: LonLat) -> Cell {
    let (sin_lat, cos_lat) = point.latitude.to_radians().sin_cos();
    let (sin_lon, cos_lon) = point.longitude.to_radians().sin_cos();
    [cos_lat * cos_lon, cos_lat * sin_lon, sin_lat]
        .map(|unit| (unit * EARTH_RADIUS_M / DUPLICATE_M).floor() as i32)
}

pub(super) fn position_spread(points: impl Iterator<Item = LonLat>) -> (u32, Option<f64>) {
    let distinct: BTreeMap<_, _> = points.map(|p| (key(p), p)).collect();
    let points: Vec<_> = distinct.into_values().collect();
    let mut spread: f64 = 0.0;
    for (index, &a) in points.iter().enumerate() {
        for &b in &points[..index] {
            spread = spread.max(distance(a, b));
        }
    }
    (points.len() as u32, (!points.is_empty()).then_some(spread))
}

#[derive(Default)]
pub(super) struct PositionQuality {
    defaults: BTreeSet<CoordinateKey>,
    duplicates: BTreeSet<CoordinateKey>,
}

impl PositionQuality {
    pub(super) fn from_sources(sources: &[Source]) -> Self {
        let mut points = BTreeMap::new();
        for source in sources {
            for (id, node) in &source.nodes {
                for entry in &node.positions {
                    let point = entry.position.point;
                    let (_, ids) = points
                        .entry(key(point))
                        .or_insert_with(|| (point, BTreeSet::new()));
                    ids.insert(id.as_str());
                }
            }
        }
        let mut grid: HashMap<Cell, Vec<(LonLat, bool, &str)>> = HashMap::new();
        let mut quality = Self::default();
        for (&key, (point, ids)) in &points {
            if ids.len() >= 3 {
                quality.defaults.insert(key);
            }
            if ids.len() >= 2 {
                quality.duplicates.insert(key);
            }
            grid.entry(cell(*point)).or_default().push((
                *point,
                ids.len() >= 2,
                ids.first().unwrap(),
            ));
        }
        for (&key, (point, ids)) in &points {
            if quality.duplicates.contains(&key) {
                continue;
            }
            let [x, y, z] = cell(*point);
            let id = ids.first().unwrap();
            'neighbors: for i in x - 1..=x + 1 {
                for j in y - 1..=y + 1 {
                    for k in z - 1..=z + 1 {
                        if let Some(neighbors) = grid.get(&[i, j, k])
                            && neighbors.iter().any(|(other, shared, other_id)| {
                                (id != other_id || *shared)
                                    && distance(*point, *other) <= DUPLICATE_M
                            })
                        {
                            quality.duplicates.insert(key);
                            break 'neighbors;
                        }
                    }
                }
            }
        }
        quality
    }

    pub(super) fn flags(
        &self,
        position: Option<&Position>,
        saw_default: bool,
        spread: Option<f64>,
    ) -> Vec<String> {
        let coordinate = position.map(|p| key(p.point));
        let has = |set: &BTreeSet<_>| coordinate.is_some_and(|c| set.contains(&c));
        let precision = position.and_then(|p| p.precision);
        [
            (
                "coarse_position",
                precision.is_some_and(|r| r > 100.0)
                    || position.and_then(|p| p.bits).is_some_and(|bits| bits < 16),
            ),
            (
                "default_position",
                has(&self.defaults) || (position.is_none() && saw_default),
            ),
            ("duplicate_position", has(&self.duplicates)),
            ("moved", spread.is_some_and(|m| m > 200.0)),
            ("unknown_precision", precision.is_none()),
        ]
        .into_iter()
        .filter(|&(_, flagged)| flagged)
        .map(|(flag, _)| flag.to_owned())
        .collect()
    }
}

pub(super) fn radio_mismatch(tx: &Radio, rx: &Radio) -> bool {
    fn differs(a: Option<f64>, b: Option<f64>) -> bool {
        a.zip(b).is_some_and(|(a, b)| (a - b).abs() > 0.001)
    }
    differs(tx.freq_mhz, rx.freq_mhz)
        || differs(tx.bw_khz, rx.bw_khz)
        || tx.sf.zip(rx.sf).is_some_and(|(a, b)| a != b)
}

/// Largest plausible EIRP plus receive antenna gain, dBm: 30 dBm into a
/// 6 dBi antenna at both ends. A link received more strongly than free space
/// allows with this budget has a wrong position or a misattributed relay.
pub(super) const MAX_PLAUSIBLE_BUDGET_DBM: f64 = 30.0 + 6.0 + 6.0;
pub(super) const DEFAULT_NOISE_DBM: f64 = -117.0;

pub(super) fn detection_floor_dbm(radio: &Radio, noise_dbm: Option<f64>) -> f64 {
    let sf = radio.sf.filter(|sf| (5..=12).contains(sf)).unwrap_or(8);
    noise_dbm.unwrap_or(DEFAULT_NOISE_DBM) - 2.5 * f64::from(sf - 4)
}

/// True when `signal_dbm` exceeds what free space allows over `distance_m`.
pub(super) fn implausible_signal(signal_dbm: f64, distance_m: f64, freq_mhz: f64) -> bool {
    if distance_m <= 1.0 {
        return false;
    }
    let fspl = 32.45 + 20.0 * freq_mhz.log10() + 20.0 * (distance_m / 1000.0).log10();
    signal_dbm > MAX_PLAUSIBLE_BUDGET_DBM - fspl
}
