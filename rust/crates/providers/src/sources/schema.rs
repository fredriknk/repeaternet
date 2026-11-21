use std::{
    collections::{BTreeMap, BTreeSet},
    net::IpAddr,
};

use geo::{Contains, LineString, MultiPolygon, Point, Polygon, Rect, Validation};
use repeaternet_terrain::{LonLat, SourceCrs};
use reqwest::{
    header::{HeaderName, HeaderValue},
    Url,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::Value;

use crate::{invalid, Result};

use super::Product;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CustomFile {
    pub version: u32,
    #[serde(default)]
    pub disabled_builtins: Vec<String>,
    #[serde(default)]
    pub terrain: Vec<TerrainProvider>,
    #[serde(default)]
    pub basemaps: Vec<MapLayer>,
}

impl Default for CustomFile {
    fn default() -> Self {
        Self {
            version: 1,
            disabled_builtins: Vec::new(),
            terrain: Vec::new(),
            basemaps: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TerrainProvider {
    pub id: String,
    pub name: String,
    pub kind: TerrainKind,
    pub products: Products,
    pub coverage: Coverage,
    /// Short place name for people, e.g. "Norway". Display only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    pub resolution_m: f64,
    #[serde(default)]
    pub priority: i32,
    #[serde(default)]
    pub surface_model: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub missing_tile: Option<MissingTile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crs_rule: Option<CrsRule>,
    #[serde(default = "default_concurrency")]
    pub max_concurrency: usize,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    pub attribution: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub homepage: Option<String>,
    #[serde(default = "yes")]
    pub enabled: bool,
}

const fn yes() -> bool {
    true
}
const fn one() -> f64 {
    1.0
}
const fn default_concurrency() -> usize {
    2
}
const fn default_max_zoom() -> u8 {
    19
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TerrainKind {
    Wcs,
    Cog,
    CogTiles,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MissingTile {
    Nodata,
    SeaLevel,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CrsRule {
    NearestUtm,
    NorwayUtm,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Products {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dtm: Option<ProductDefinition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dom: Option<ProductDefinition>,
}

impl Products {
    pub fn get(&self, product: Product) -> Option<&ProductDefinition> {
        match product {
            Product::Dtm => self.dtm.as_ref(),
            Product::Dom => self.dom.as_ref(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum ProductDefinition {
    Wcs(Wcs),
    Cog(Cog),
    CogTiles(CogTiles),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Cog {
    pub url: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CogTiles {
    pub url_template: String,
    pub tile_degrees: f64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Wcs {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coverage: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub native_crs: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub per_crs: BTreeMap<String, WcsService>,
    pub version: WcsVersion,
    pub format: WcsFormat,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub params: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub axis_labels: Option<[String; 2]>,
}

impl Wcs {
    pub fn crs_codes(&self) -> impl Iterator<Item = &str> {
        self.native_crs
            .iter()
            .chain(self.per_crs.keys())
            .map(String::as_str)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WcsService {
    pub endpoint: String,
    pub coverage: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum WcsVersion {
    #[serde(rename = "1.0.0")]
    V100,
    #[serde(rename = "2.0.1")]
    V201,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub enum WcsFormat {
    #[serde(rename = "GeoTIFF")]
    GeoTiff,
    #[serde(rename = "image/tiff")]
    ImageTiff,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Coverage {
    #[default]
    Global,
    Bbox {
        bbox: [f64; 4],
    },
    MultiBbox {
        bboxes: Vec<[f64; 4]>,
    },
    Polygon {
        rings: Vec<Vec<[f64; 2]>>,
    },
}

impl Coverage {
    pub fn geometry(&self) -> MultiPolygon<f64> {
        match self {
            Self::Global => MultiPolygon(vec![rectangle([-180., -90., 180., 90.])]),
            Self::Bbox { bbox } => MultiPolygon(vec![rectangle(*bbox)]),
            Self::MultiBbox { bboxes } => {
                geo::unary_union(&bboxes.iter().copied().map(rectangle).collect::<Vec<_>>())
            }
            Self::Polygon { rings } => {
                MultiPolygon(rings.iter().map(|ring| ring_polygon(ring)).collect())
            }
        }
    }

    pub fn contains(&self, location: LonLat) -> bool {
        let x = location.longitude;
        let y = location.latitude;
        let inside = |[w, s, e, n]: [f64; 4]| (w..=e).contains(&x) && (s..=n).contains(&y);
        match self {
            Self::Global => true,
            Self::Bbox { bbox } => inside(*bbox),
            Self::MultiBbox { bboxes } => bboxes.iter().copied().any(inside),
            Self::Polygon { .. } => self.geometry().contains(&Point::new(x, y)),
        }
    }
}

pub(super) fn rectangle([w, s, e, n]: [f64; 4]) -> Polygon<f64> {
    Rect::new((w, s), (e, n)).to_polygon()
}

fn ring_polygon(ring: &[[f64; 2]]) -> Polygon<f64> {
    Polygon::new(
        LineString::from_iter(ring.iter().map(|&[x, y]| (x, y))),
        Vec::new(),
    )
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MapLayer {
    pub id: String,
    pub name: String,
    pub attribution: String,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub coverage: Coverage,
    /// Short place name for people, e.g. "Norway". Display only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    pub kind: MapKind,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subdomains: Option<String>,
    #[serde(default)]
    pub min_zoom: u8,
    #[serde(default = "default_max_zoom")]
    pub max_zoom: u8,
    #[serde(default)]
    pub overlay: bool,
    #[serde(default = "one")]
    pub opacity: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wms: Option<Wms>,
    #[serde(default)]
    pub default: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MapKind {
    Xyz,
    Wms,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Wms {
    pub layers: String,
    #[serde(default)]
    pub styles: String,
    #[serde(default = "png")]
    pub format: String,
    #[serde(default = "yes")]
    pub transparent: bool,
    #[serde(default = "wms_version")]
    pub version: String,
}

fn png() -> String {
    "image/png".into()
}
fn wms_version() -> String {
    "1.3.0".into()
}

#[derive(Clone, Debug, Serialize)]
pub struct ValidationIssue {
    pub path: String,
    pub message: String,
}

#[derive(Default)]
struct Validator(Vec<ValidationIssue>);

impl Validator {
    fn check(&mut self, valid: bool, path: impl Into<String>, message: impl Into<String>) {
        if !valid {
            self.0.push(ValidationIssue {
                path: path.into(),
                message: message.into(),
            });
        }
    }

    fn parsed<T: DeserializeOwned>(&mut self, value: &Value, prefix: &str) -> Option<T> {
        match serde_path_to_error::deserialize(value) {
            Ok(parsed) => Some(parsed),
            Err(error) => {
                let at = error.path().to_string();
                let message = error.inner().to_string();
                let mut path = if prefix.is_empty() || at == "." {
                    if prefix.is_empty() {
                        at
                    } else {
                        prefix.to_owned()
                    }
                } else {
                    format!("{prefix}.{at}")
                };
                for label in ["unknown field `", "missing field `"] {
                    if let Some((field, _)) = message
                        .strip_prefix(label)
                        .and_then(|rest| rest.split_once('`'))
                    {
                        if path.rsplit('.').next() != Some(field) {
                            path = if path == "." {
                                field.into()
                            } else {
                                format!("{path}.{field}")
                            };
                        }
                    }
                }
                // Some Serde type errors contain the input string. It may be a header secret.
                let detail = if message.starts_with("invalid type:") {
                    "invalid field type".into()
                } else if message.starts_with("unknown variant") {
                    "unsupported value".into()
                } else {
                    message
                };
                self.check(false, path, detail);
                None
            }
        }
    }

    fn name_fields(
        &mut self,
        ids: &mut BTreeSet<String>,
        path: &str,
        id: &str,
        name: &str,
        attribution: &str,
    ) {
        let valid_id = !id.is_empty()
            && id.len() <= 48
            && !id.starts_with('-')
            && id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        self.check(
            valid_id,
            format!("{path}.id"),
            "use 1–48 lowercase letters, digits or hyphens, starting with a letter or digit",
        );
        self.check(
            ids.insert(id.to_owned()),
            format!("{path}.id"),
            "duplicate provider id",
        );
        self.check(
            !name.trim().is_empty() && name.chars().count() <= 80,
            format!("{path}.name"),
            "name must contain 1–80 characters",
        );
        self.check(
            !attribution.trim().is_empty() && attribution.chars().count() <= 300,
            format!("{path}.attribution"),
            "attribution must contain 1–300 characters",
        );
    }

    fn region(&mut self, path: &str, region: Option<&str>) {
        if let Some(region) = region {
            self.check(
                !region.trim().is_empty() && region.chars().count() <= 40,
                format!("{path}.region"),
                "region must contain 1–40 characters",
            );
        }
    }

    fn coverage(&mut self, path: &str, coverage: &Coverage) {
        let valid_lon = |x: f64| (-180.0..=180.0).contains(&x);
        let valid_lat = |y: f64| (-90.0..=90.0).contains(&y);
        let valid_box = |[w, s, e, n]: [f64; 4]| {
            valid_lon(w) && valid_lon(e) && valid_lat(s) && valid_lat(n) && w < e && s < n
        };
        match coverage {
            Coverage::Global => {}
            Coverage::Bbox { bbox } => self.check(
                valid_box(*bbox),
                format!("{path}.bbox"),
                "provide [west, south, east, north] in order within WGS84 bounds",
            ),
            Coverage::MultiBbox { bboxes } => {
                self.check(
                    !bboxes.is_empty(),
                    format!("{path}.bboxes"),
                    "provide at least one bounding box",
                );
                for (i, bbox) in bboxes.iter().enumerate() {
                    self.check(
                        valid_box(*bbox),
                        format!("{path}.bboxes[{i}]"),
                        "provide [west, south, east, north] in order within WGS84 bounds",
                    );
                }
            }
            Coverage::Polygon { rings } => {
                self.check(
                    !rings.is_empty(),
                    format!("{path}.rings"),
                    "provide at least one polygon ring",
                );
                for (i, ring) in rings.iter().enumerate() {
                    let valid = ring.iter().all(|&[x, y]| valid_lon(x) && valid_lat(y))
                        && ring_polygon(ring).exterior().0.len() >= 4
                        && ring_polygon(ring).is_valid();
                    self.check(
                        valid,
                        format!("{path}.rings[{i}]"),
                        "provide a valid WGS84 outer ring",
                    );
                }
                if rings.iter().all(|ring| ring_polygon(ring).is_valid()) {
                    self.check(
                        coverage.geometry().is_valid(),
                        format!("{path}.rings"),
                        "polygon outer rings must not overlap",
                    );
                }
            }
        }
    }

    fn template(&mut self, path: &str, text: &str, tokens: &[&str]) {
        let expanded = substitute(text, |token| {
            if tokens.contains(&token) {
                Some("1".into())
            } else {
                self.check(false, path, format!("unknown placeholder {{{token}}}"));
                Some("1".into())
            }
        });
        match expanded {
            Some(expanded) => self.check(
                valid_url(&expanded),
                path,
                "use an HTTPS URL or a loopback HTTP URL",
            ),
            None => self.check(false, path, "malformed placeholder"),
        }
    }

    fn wcs(&mut self, path: &str, wcs: &Wcs) {
        if wcs.per_crs.is_empty() {
            self.check(
                wcs.endpoint.as_ref().is_some_and(|url| valid_url(url)),
                format!("{path}.endpoint"),
                "use an HTTPS URL or a loopback HTTP URL",
            );
            self.check(
                wcs.coverage.as_ref().is_some_and(|s| !s.trim().is_empty()),
                format!("{path}.coverage"),
                "provide a coverage name",
            );
            self.check(
                !wcs.native_crs.is_empty(),
                format!("{path}.native_crs"),
                "provide at least one supported CRS",
            );
            let mut native = BTreeSet::new();
            for (i, code) in wcs.native_crs.iter().enumerate() {
                self.check(
                    parse_crs(code).is_some() && native.insert(code),
                    format!("{path}.native_crs[{i}]"),
                    "unsupported or duplicate EPSG CRS",
                );
            }
        } else {
            self.check(
                wcs.endpoint.is_none() && wcs.coverage.is_none() && wcs.native_crs.is_empty(),
                format!("{path}.per_crs"),
                "per_crs replaces endpoint, coverage and native_crs",
            );
            for (code, service) in &wcs.per_crs {
                let path = format!("{path}.per_crs.{code}");
                self.check(parse_crs(code).is_some(), &path, "unsupported EPSG CRS");
                self.check(
                    valid_url(&service.endpoint),
                    format!("{path}.endpoint"),
                    "use an HTTPS URL or a loopback HTTP URL",
                );
                self.check(
                    !service.coverage.trim().is_empty(),
                    format!("{path}.coverage"),
                    "provide a coverage name",
                );
            }
        }
        if let Some(labels) = &wcs.axis_labels {
            self.check(
                labels[0] != labels[1]
                    && labels.iter().all(|s| {
                        !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                    }),
                format!("{path}.axis_labels"),
                "provide two distinct axis labels",
            );
        }
        const RESERVED: &[&str] = &[
            "service",
            "version",
            "request",
            "coverage",
            "coverageid",
            "format",
            "crs",
            "bbox",
            "width",
            "height",
            "subset",
            "subsettingcrs",
            "outputcrs",
            "scalesize",
        ];
        for key in wcs.params.keys() {
            self.check(
                !key.is_empty() && !RESERVED.contains(&key.to_ascii_lowercase().as_str()),
                format!("{path}.params.{key}"),
                "parameter is reserved for the WCS request",
            );
        }
    }
}

pub(super) fn parse_crs(code: &str) -> Option<SourceCrs> {
    let number = code.strip_prefix("EPSG:")?;
    let epsg: u16 = number.parse().ok()?;
    (epsg.to_string() == number)
        .then(|| SourceCrs::from_epsg(epsg).ok())
        .flatten()
}

pub(super) fn valid_url(text: &str) -> bool {
    let Ok(url) = Url::parse(text) else {
        return false;
    };
    let Some(host) = url.host_str() else {
        return false;
    };
    let loopback = host == "localhost"
        || host
            .trim_matches(['[', ']'])
            .parse::<IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    url.username().is_empty()
        && url.password().is_none()
        && url.fragment().is_none()
        && (url.scheme() == "https" || url.scheme() == "http" && loopback)
}

pub(super) const TILE_TOKENS: &[&str] = &[
    "NS",
    "ns",
    "EW",
    "ew",
    "lat",
    "lat2",
    "lon",
    "lon3",
    "lat_n",
    "lat_n2",
    "lat_signed",
    "lon_signed",
];

pub(super) fn substitute(
    template: &str,
    mut value: impl FnMut(&str) -> Option<String>,
) -> Option<String> {
    let mut out = String::new();
    let mut rest = template;
    while let Some((before, after)) = rest.split_once('{') {
        if before.contains('}') {
            return None;
        }
        let (token, tail) = after.split_once('}')?;
        out.push_str(before);
        out.push_str(&value(token)?);
        rest = tail;
    }
    if rest.contains('}') {
        return None;
    }
    out.push_str(rest);
    Some(out)
}

impl CogTiles {
    pub fn cell(&self, location: LonLat) -> (i32, i32) {
        let lon = ((location.longitude + 180.0).rem_euclid(360.0) - 180.0) / self.tile_degrees;
        let lat = location.latitude.clamp(-90.0, 90.0_f64.next_down()) / self.tile_degrees;
        (lat.floor() as i32, lon.floor() as i32)
    }

    pub fn url_for_cell(&self, (row, column): (i32, i32)) -> Result<String> {
        let south = f64::from(row) * self.tile_degrees;
        let west = f64::from(column) * self.tile_degrees;
        let north = (f64::from(row) + 1.0) * self.tile_degrees;
        let ns = if south < 0.0 { "S" } else { "N" };
        let ew = if west < 0.0 { "W" } else { "E" };
        let padded = |v: f64, width| {
            let text = v.abs().to_string();
            let integers = text.split('.').next().unwrap_or("").len();
            format!(
                "{}{text}",
                "0".repeat(usize::saturating_sub(width, integers))
            )
        };
        substitute(&self.url_template, |token| {
            Some(match token {
                "NS" => ns.into(),
                "ns" => ns.to_ascii_lowercase(),
                "EW" => ew.into(),
                "ew" => ew.to_ascii_lowercase(),
                "lat" => south.abs().to_string(),
                "lat2" => padded(south, 2),
                "lon" => west.abs().to_string(),
                "lon3" => padded(west, 3),
                "lat_n" => north.abs().to_string(),
                "lat_n2" => padded(north, 2),
                "lat_signed" => south.to_string(),
                "lon_signed" => west.to_string(),
                _ => return None,
            })
        })
        .ok_or_else(|| invalid("Invalid COG tile URL template"))
    }
}

fn env_name(name: &str) -> bool {
    !name.is_empty()
        && name.as_bytes()[0].is_ascii_alphabetic()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        || name.starts_with('_')
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

pub(super) fn env_reference(value: &str) -> bool {
    value
        .strip_prefix("${")
        .and_then(|value| value.strip_suffix('}'))
        .is_some_and(env_name)
}

pub(super) fn expand_header(
    value: &str,
    mut env: impl FnMut(&str) -> Result<String>,
) -> Result<String> {
    let mut result = String::new();
    let mut rest = value;
    while let Some((prefix, tail)) = rest.split_once("${") {
        result.push_str(prefix);
        let (name, tail) = tail
            .split_once('}')
            .filter(|(name, _)| env_name(name))
            .ok_or_else(|| invalid("Invalid environment variable reference in header"))?;
        result.push_str(&env(name)?);
        rest = tail;
    }
    result.push_str(rest);
    Ok(result)
}

pub(super) fn validate(
    value: &Value,
    builtins: &CustomFile,
) -> std::result::Result<CustomFile, Vec<ValidationIssue>> {
    let mut v = Validator::default();
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Envelope {
        version: u32,
        #[serde(default)]
        disabled_builtins: Vec<String>,
        #[serde(default)]
        terrain: Vec<Value>,
        #[serde(default)]
        basemaps: Vec<Value>,
    }
    let Some(raw) = v.parsed::<Envelope>(value, "") else {
        return Err(v.0);
    };
    v.check(raw.version == 1, "version", "supported version is 1");
    let mut ids: BTreeSet<_> = builtins
        .terrain
        .iter()
        .map(|p| p.id.clone())
        .chain(builtins.basemaps.iter().map(|p| p.id.clone()))
        .collect();
    let mut disabled = BTreeSet::new();
    for (i, id) in raw.disabled_builtins.iter().enumerate() {
        v.check(
            ids.contains(id) && disabled.insert(id),
            format!("disabled_builtins[{i}]"),
            "unknown or duplicate built-in provider id",
        );
    }
    let mut out = CustomFile {
        version: raw.version,
        disabled_builtins: raw.disabled_builtins.clone(),
        ..CustomFile::default()
    };
    for (i, entry) in raw.terrain.iter().enumerate() {
        let path = format!("terrain[{i}]");
        let start = v.0.len();
        if let Some(kind) = entry.get("kind").and_then(Value::as_str) {
            if let Some(products) = entry.get("products").and_then(Value::as_object) {
                for (product, definition) in products {
                    let path = format!("{path}.products.{product}");
                    if !matches!(product.as_str(), "dtm" | "dom") {
                        v.check(false, path, "unsupported product");
                        continue;
                    }
                    match kind {
                        "wcs" => {
                            v.parsed::<Wcs>(definition, &path);
                        }
                        "cog" => {
                            v.parsed::<Cog>(definition, &path);
                        }
                        "cog_tiles" => {
                            v.parsed::<CogTiles>(definition, &path);
                        }
                        _ => {}
                    }
                }
            }
        }
        if v.0.len() != start {
            continue;
        }
        let Some(provider) = v.parsed::<TerrainProvider>(entry, &path) else {
            continue;
        };
        v.name_fields(
            &mut ids,
            &path,
            &provider.id,
            &provider.name,
            &provider.attribution,
        );
        v.check(
            provider.id != "auto",
            format!("{path}.id"),
            "auto is reserved for automatic source selection",
        );
        v.coverage(&format!("{path}.coverage"), &provider.coverage);
        v.region(&path, provider.region.as_deref());
        v.check(
            (0.1..=10_000.0).contains(&provider.resolution_m),
            format!("{path}.resolution_m"),
            "resolution must be 0.1–10000 m",
        );
        v.check(
            (-1000..=1000).contains(&provider.priority),
            format!("{path}.priority"),
            "priority must be -1000–1000",
        );
        v.check(
            (1..=8).contains(&provider.max_concurrency),
            format!("{path}.max_concurrency"),
            "concurrency must be 1–8",
        );
        v.check(
            provider.missing_tile.is_none() || provider.kind == TerrainKind::CogTiles,
            format!("{path}.missing_tile"),
            "only tiled COGs support this field",
        );
        v.check(
            provider.crs_rule.is_none() || provider.kind == TerrainKind::Wcs,
            format!("{path}.crs_rule"),
            "only WCS supports this field",
        );
        v.check(
            provider
                .homepage
                .as_ref()
                .is_none_or(|url| url.starts_with("https://") && valid_url(url)),
            format!("{path}.homepage"),
            "use an HTTPS URL",
        );
        for (key, value) in &provider.headers {
            let reserved = [
                "range",
                "if-range",
                "host",
                "content-length",
                "accept-encoding",
            ];
            v.check(
                HeaderName::from_bytes(key.as_bytes()).is_ok()
                    && !reserved.contains(&key.to_ascii_lowercase().as_str())
                    && HeaderValue::from_str(value).is_ok()
                    && expand_header(value, |_| Ok("x".into())).is_ok(),
                format!("{path}.headers.{key}"),
                "invalid or reserved HTTP header",
            );
        }
        v.check(
            provider.products.dtm.is_some() || provider.products.dom.is_some(),
            format!("{path}.products"),
            "provide DTM or DOM",
        );
        for product in [Product::Dtm, Product::Dom] {
            let path = format!("{path}.products.{}", product.name());
            match provider.products.get(product) {
                Some(ProductDefinition::Wcs(wcs)) => v.wcs(&path, wcs),
                Some(ProductDefinition::Cog(cog)) => {
                    v.template(&format!("{path}.url"), &cog.url, &[])
                }
                Some(ProductDefinition::CogTiles(cog)) => {
                    v.template(
                        &format!("{path}.url_template"),
                        &cog.url_template,
                        TILE_TOKENS,
                    );
                    let step = cog.tile_degrees;
                    v.check(
                        step.is_finite() && step > 0.0 && 180.0 / step <= f64::from(i32::MAX),
                        format!("{path}.tile_degrees"),
                        "tile size must be positive and fit the supported cell indices",
                    );
                }
                None => {}
            }
        }
        out.terrain.push(provider);
    }
    let mut default_used = builtins
        .basemaps
        .iter()
        .any(|layer| layer.default && layer.enabled && !disabled.contains(&layer.id));
    for (i, entry) in raw.basemaps.iter().enumerate() {
        let path = format!("basemaps[{i}]");
        let Some(layer) = v.parsed::<MapLayer>(entry, &path) else {
            continue;
        };
        v.name_fields(&mut ids, &path, &layer.id, &layer.name, &layer.attribution);
        v.coverage(&format!("{path}.coverage"), &layer.coverage);
        v.region(&path, layer.region.as_deref());
        v.check(
            layer.min_zoom <= layer.max_zoom && layer.max_zoom <= 24,
            format!("{path}.max_zoom"),
            "zoom must be ordered between 0 and 24",
        );
        v.check(
            (0.0..=1.0).contains(&layer.opacity),
            format!("{path}.opacity"),
            "opacity must be 0–1",
        );
        v.check(
            !layer.default || !layer.overlay && (!layer.enabled || !default_used),
            format!("{path}.default"),
            "at most one enabled base layer can be the default",
        );
        default_used |= layer.default && layer.enabled;
        match layer.kind {
            MapKind::Xyz => {
                v.template(
                    &format!("{path}.url"),
                    &layer.url,
                    &["z", "x", "y", "s", "r"],
                );
                v.check(
                    ["{z}", "{x}", "{y}"]
                        .iter()
                        .all(|token| layer.url.contains(token)),
                    format!("{path}.url"),
                    "XYZ URL needs {z}, {x} and {y}",
                );
                v.check(
                    layer.wms.is_none(),
                    format!("{path}.wms"),
                    "only WMS supports this field",
                );
            }
            MapKind::Wms => {
                v.template(&format!("{path}.url"), &layer.url, &[]);
                v.check(
                    layer.wms.as_ref().is_some_and(|wms| {
                        !wms.layers.trim().is_empty()
                            && !wms.format.trim().is_empty()
                            && matches!(wms.version.as_str(), "1.1.0" | "1.1.1" | "1.3.0")
                    }),
                    format!("{path}.wms"),
                    "provide layers, an image format and a supported WMS version",
                );
            }
        }
        out.basemaps.push(layer);
    }
    if v.0.is_empty() {
        Ok(out)
    } else {
        Err(v.0)
    }
}
