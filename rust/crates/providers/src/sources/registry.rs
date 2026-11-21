use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
    sync::OnceLock,
};

use geo::{
    Area, BooleanOps, BoundingRect, Centroid, Intersects, LineString, MapCoords, MultiPolygon,
    Polygon,
};
use repeaternet_terrain::{Bounds, LonLat, Point, SourceCrs, TerrainCrs, Transformer};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{
    invalid,
    kartverket::{WcsConfig, WcsService},
    Result,
};

use super::{
    atomic_write, hash,
    schema::{env_reference, parse_crs, rectangle, validate},
    Coverage, CrsRule, CustomFile, MapLayer, Product, ProductDefinition, TerrainProvider,
    ValidationIssue,
};

const BUILTINS: &str = include_str!("../builtin_providers.json");

fn builtin_file() -> &'static CustomFile {
    static FILE: OnceLock<CustomFile> = OnceLock::new();
    FILE.get_or_init(|| {
        let json = serde_json::from_str(BUILTINS).expect("embedded providers are JSON");
        validate(&json, &CustomFile::default()).expect("embedded provider definitions are valid")
    })
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SourceChoice {
    pub source: String,
    pub fallback: bool,
}

impl Default for SourceChoice {
    fn default() -> Self {
        Self {
            source: "auto".into(),
            fallback: true,
        }
    }
}

impl SourceChoice {
    fn pin(&self) -> Option<&str> {
        (self.source != "auto").then_some(&self.source)
    }
}

#[derive(Clone)]
pub struct ProviderRegistry {
    terrain: Vec<TerrainProvider>,
    basemaps: Vec<MapLayer>,
    custom: CustomFile,
    fingerprint: String,
    file: PathBuf,
    errors: Vec<ValidationIssue>,
}

impl ProviderRegistry {
    pub fn builtin() -> Self {
        Self::merge(
            PathBuf::from("providers.json"),
            CustomFile::default(),
            Vec::new(),
        )
    }

    pub fn load(data_dir: &Path) -> Result<Self> {
        let file = Self::file_path(data_dir);
        let (custom, errors) = match fs::read(&file) {
            Ok(bytes) => {
                let result = serde_json::from_slice(&bytes)
                    .map_err(|error| {
                        vec![ValidationIssue {
                            path: ".".into(),
                            message: format!(
                                "Invalid JSON at line {}, column {}",
                                error.line(),
                                error.column()
                            ),
                        }]
                    })
                    .and_then(|json| Self::validate_custom(&json));
                match result {
                    Ok(custom) => (custom, Vec::new()),
                    Err(errors) => (CustomFile::default(), errors),
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                (CustomFile::default(), Vec::new())
            }
            Err(error) => return Err(error.into()),
        };
        Ok(Self::merge(file, custom, errors))
    }

    pub fn file_path(data_dir: &Path) -> PathBuf {
        std::env::var_os("RF_PLANNER_PROVIDERS_FILE")
            .map_or_else(|| data_dir.join("providers.json"), PathBuf::from)
    }

    fn merge(file: PathBuf, custom: CustomFile, errors: Vec<ValidationIssue>) -> Self {
        let builtin = builtin_file();
        let mut terrain = builtin.terrain.clone();
        let mut basemaps = builtin.basemaps.clone();
        for entry in &mut terrain {
            entry.enabled &= !custom.disabled_builtins.contains(&entry.id);
        }
        for entry in &mut basemaps {
            entry.enabled &= !custom.disabled_builtins.contains(&entry.id);
        }
        terrain.extend(custom.terrain.clone());
        basemaps.extend(custom.basemaps.clone());
        let fingerprint =
            hash(&serde_json::to_vec(&(&terrain, &basemaps)).expect("registry serializes"));
        Self {
            terrain,
            basemaps,
            custom,
            fingerprint,
            file,
            errors,
        }
    }

    pub fn validate_custom(json: &Value) -> std::result::Result<CustomFile, Vec<ValidationIssue>> {
        validate(json, builtin_file())
    }

    pub fn validate_edit(
        &self,
        json: &Value,
    ) -> std::result::Result<CustomFile, Vec<ValidationIssue>> {
        let mut custom = Self::validate_custom(json)?;
        let mut issues = Vec::new();
        for (i, provider) in custom.terrain.iter_mut().enumerate() {
            for (key, value) in &mut provider.headers {
                if value != "***" {
                    continue;
                }
                let stored = self
                    .custom
                    .terrain
                    .iter()
                    .find(|old| old.id == provider.id)
                    .and_then(|old| {
                        old.headers
                            .iter()
                            .find(|(name, _)| name.eq_ignore_ascii_case(key))
                    })
                    .map(|(_, value)| value);
                if let Some(stored) = stored {
                    value.clone_from(stored);
                } else {
                    issues.push(ValidationIssue {
                        path: format!("terrain[{i}].headers.{key}"),
                        message: "There is no stored value for this redacted header".into(),
                    });
                }
            }
        }
        if issues.is_empty() {
            Ok(custom)
        } else {
            Err(issues)
        }
    }

    pub fn save_custom(data_dir: &Path, custom: &CustomFile) -> Result<()> {
        let json = serde_json::to_value(custom).map_err(|error| invalid(error.to_string()))?;
        Self::validate_custom(&json).map_err(|errors| {
            invalid(
                errors
                    .into_iter()
                    .map(|issue| format!("{}: {}", issue.path, issue.message))
                    .collect::<Vec<_>>()
                    .join(", "),
            )
        })?;
        atomic_write(
            &Self::file_path(data_dir),
            &serde_json::to_vec_pretty(&json).map_err(|e| invalid(e.to_string()))?,
        )
    }

    pub fn terrain(&self) -> &[TerrainProvider] {
        &self.terrain
    }

    pub fn basemaps(&self) -> &[MapLayer] {
        &self.basemaps
    }

    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    pub fn view(&self) -> Value {
        let redact = |provider: &mut TerrainProvider| {
            for value in provider.headers.values_mut() {
                if !env_reference(value) {
                    *value = "***".into();
                }
            }
        };
        let terrain: Vec<_> = self
            .terrain
            .iter()
            .cloned()
            .enumerate()
            .map(|(i, mut provider)| {
                redact(&mut provider);
                let mut view = json!(provider);
                view["builtin"] = json!(i < builtin_file().terrain.len());
                view["products"] = json!([Product::Dtm, Product::Dom]
                    .into_iter()
                    .filter(|&p| provider.products.get(p).is_some())
                    .collect::<Vec<_>>());
                view["coverage_bbox"] = json!(provider
                    .coverage
                    .geometry()
                    .bounding_rect()
                    .map(|r| [r.min().x, r.min().y, r.max().x, r.max().y]));
                view
            })
            .collect();
        let mut custom = self.custom.clone();
        for provider in &mut custom.terrain {
            redact(provider);
        }
        json!({ "terrain": terrain, "basemaps": self.basemaps, "custom": custom,
            "file": self.file, "errors": self.errors })
    }

    pub fn provider(&self, id: &str) -> Result<&TerrainProvider> {
        self.terrain
            .iter()
            .find(|p| p.enabled && p.id == id)
            .ok_or_else(|| invalid(format!("Terrain source {id} is not available")))
    }

    pub fn check_choice(&self, choice: &SourceChoice) -> Result<()> {
        if let Some(id) = choice.pin() {
            self.provider(id)?;
        }
        Ok(())
    }

    pub fn rank(
        &self,
        product: Product,
        footprint: &MultiPolygon<f64>,
        choice: &SourceChoice,
    ) -> Vec<&TerrainProvider> {
        let center = centroid(footprint);
        let mut candidates: Vec<_> = self
            .terrain
            .iter()
            .filter(|p| p.enabled && p.products.get(product).is_some())
            .filter(|p| {
                choice
                    .pin()
                    .is_none_or(|pin| choice.fallback || p.id == pin)
            })
            .filter(|p| {
                matches!(p.coverage, Coverage::Global)
                    || p.coverage.geometry().intersects(footprint)
            })
            .map(|p| (p, center.is_some_and(|center| p.coverage.contains(center))))
            .collect();
        candidates.sort_by(|(a, a_contains), (b, b_contains)| {
            let pinned = |p: &TerrainProvider| choice.pin() == Some(p.id.as_str());
            pinned(b)
                .cmp(&pinned(a))
                .then(b_contains.cmp(a_contains))
                .then(a.resolution_m.total_cmp(&b.resolution_m))
                .then(b.priority.cmp(&a.priority))
                .then(a.id.cmp(&b.id))
        });
        candidates
            .into_iter()
            .map(|(provider, _)| provider)
            .collect()
    }

    pub fn select_target_crs(&self, area: &MultiPolygon<f64>) -> Result<TerrainCrs> {
        self.target_crs(area, &SourceChoice::default())
    }

    pub fn target_crs(
        &self,
        area: &MultiPolygon<f64>,
        choice: &SourceChoice,
    ) -> Result<TerrainCrs> {
        self.check_choice(choice)?;
        let center = centroid(area).ok_or_else(|| invalid("Download area has no centroid"))?;
        let point = geo::Point::new(center.longitude, center.latitude);
        let area = MultiPolygon(vec![rectangle([
            point.x() - 1e-8,
            point.y() - 1e-8,
            point.x() + 1e-8,
            point.y() + 1e-8,
        ])]);
        if let Some(provider) = self
            .rank(Product::Dtm, &area, choice)
            .into_iter()
            .find(|provider| provider.coverage.contains(center))
        {
            if let Some(ProductDefinition::Wcs(wcs)) = provider.products.dtm.as_ref() {
                let projected: Vec<_> = wcs
                    .crs_codes()
                    .filter_map(|code| match parse_crs(code) {
                        Some(SourceCrs::Projected(crs)) => Some(crs),
                        _ => None,
                    })
                    .collect();
                if provider.crs_rule == Some(CrsRule::NorwayUtm) {
                    let norway = TerrainCrs::for_norway_longitude(center.longitude)?;
                    if projected.contains(&norway) {
                        return Ok(norway);
                    }
                }
                let distance = |zone: u16| {
                    let delta = f64::from(zone) * 6.0 - 183.0 - center.longitude;
                    ((delta + 180.0).rem_euclid(360.0) - 180.0).abs()
                };
                if let Some((crs, _)) = projected
                    .iter()
                    .filter_map(|&crs| crs.utm_zone().map(|z| (crs, distance(z))))
                    .min_by(|a, b| a.1.total_cmp(&b.1))
                {
                    return Ok(crs);
                }
                if let Some(&crs) = projected.first() {
                    return Ok(crs);
                }
            }
        }
        Ok(TerrainCrs::for_wgs84_location(center)?)
    }

    pub fn describe_area(
        &self,
        area: &MultiPolygon<f64>,
        include_dom: bool,
        choice: &SourceChoice,
    ) -> AreaSources {
        let dtm = self.rank(Product::Dtm, area, choice);
        let dom = self.rank(Product::Dom, area, choice);
        let total = area.unsigned_area();
        let mut sources = Vec::new();
        for (product, ranked) in [(Product::Dtm, &dtm), (Product::Dom, &dom)] {
            if product == Product::Dom && !include_dom {
                continue;
            }
            for provider in ranked {
                let coverage_share = if matches!(provider.coverage, Coverage::Global) {
                    1.0
                } else if total > 0.0 {
                    (area
                        .intersection(&provider.coverage.geometry())
                        .unsigned_area()
                        / total)
                        .clamp(0.0, 1.0)
                } else {
                    0.0
                };
                sources.push(AreaSource {
                    source: SourceInfo::new(provider, product),
                    coverage_share,
                });
            }
        }
        let description = |provider: &TerrainProvider| {
            format!(
                "{} ({} m{})",
                provider.name,
                provider.resolution_m,
                if provider.surface_model {
                    " surface model"
                } else {
                    ""
                }
            )
        };
        let summary = match dtm.as_slice() {
            [] => "No ground terrain source covers this area".into(),
            [first, rest @ ..] => {
                let product = if include_dom && dom.first().is_some_and(|p| p.id == first.id) {
                    "ground and surface"
                } else {
                    "ground"
                };
                let first_info = format!(
                    "{} · {} m {product}{}",
                    first.name,
                    first.resolution_m,
                    if first.surface_model {
                        " (surface model)"
                    } else {
                        ""
                    }
                );
                if rest.is_empty() {
                    first_info
                } else {
                    format!(
                        "{first_info}; gaps from {}",
                        rest.iter()
                            .map(|p| description(p))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                }
            }
        };
        AreaSources {
            sources,
            dom_available: !dom.is_empty(),
            summary,
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct SourceInfo {
    pub id: String,
    pub name: String,
    pub product: Product,
    pub resolution_m: f64,
    pub surface_model: bool,
    pub attribution: String,
}

impl SourceInfo {
    pub(super) fn new(provider: &TerrainProvider, product: Product) -> Self {
        Self {
            id: provider.id.clone(),
            name: provider.name.clone(),
            product,
            resolution_m: provider.resolution_m,
            surface_model: provider.surface_model,
            attribution: provider.attribution.clone(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct AreaSource {
    #[serde(flatten)]
    pub source: SourceInfo,
    pub coverage_share: f64,
}

#[derive(Debug, Serialize)]
pub struct AreaSources {
    pub sources: Vec<AreaSource>,
    pub dom_available: bool,
    pub summary: String,
}

pub fn centroid(area: &MultiPolygon<f64>) -> Option<LonLat> {
    let bounds = area.bounding_rect()?;
    let center = if bounds.width() > 180.0 {
        area.map_coords(|mut p| {
            if p.x < 0.0 {
                p.x += 360.0;
            }
            p
        })
        .centroid()?
    } else {
        area.centroid()?
    };
    Some(LonLat {
        longitude: wrap_lon(center.x()),
        latitude: center.y(),
    })
}

fn wrap_lon(x: f64) -> f64 {
    (x + 180.0).rem_euclid(360.0) - 180.0
}

pub fn tile_footprint(transform: &Transformer, bounds: Bounds) -> Result<MultiPolygon<f64>> {
    geographic_footprint(transform, &MultiPolygon(vec![rectangle(bounds.as_array())]))
}

/// Densify projected edges before transforming and split rings at ±180°.
pub fn geographic_footprint(
    transform: &Transformer,
    geometry: &MultiPolygon<f64>,
) -> Result<MultiPolygon<f64>> {
    let mut output = Vec::new();
    for poly in geometry {
        let ring = |line: &LineString<f64>, start: Option<f64>| -> Result<LineString<f64>> {
            let mut coords = Vec::new();
            let mut reference = start;
            for edge in line.lines() {
                let steps = (edge.dx().hypot(edge.dy()) / 2_000.0)
                    .ceil()
                    .clamp(1.0, 512.0) as u32;
                for i in 0..steps {
                    let t = f64::from(i) / f64::from(steps);
                    let loc = transform.reverse(Point {
                        x: edge.start.x + t * edge.dx(),
                        y: edge.start.y + t * edge.dy(),
                    })?;
                    let lon = reference
                        .map_or(loc.longitude, |last| last + wrap_lon(loc.longitude - last));
                    coords.push((lon, loc.latitude));
                    reference = Some(lon);
                }
            }
            Ok(LineString::from(coords))
        };
        let exterior = ring(poly.exterior(), None)?;
        let reference = exterior.0.first().map(|c| c.x);
        let interiors = poly
            .interiors()
            .iter()
            .map(|line| ring(line, reference))
            .collect::<Result<Vec<_>>>()?;
        let polygon = Polygon::new(exterior, interiors);
        let Some(bounds) = polygon.bounding_rect() else {
            continue;
        };
        if bounds.min().x >= -180.0 && bounds.max().x <= 180.0 {
            output.push(polygon);
            continue;
        }
        let min_cell = ((bounds.min().x + 180.0) / 360.0).floor() as i32;
        let max_cell = ((bounds.max().x + 180.0) / 360.0).floor() as i32;
        for cell in min_cell..=max_cell {
            let shift = f64::from(cell) * 360.0;
            let clip = rectangle([-180.0 + shift, -90.0, 180.0 + shift, 90.0]);
            output.extend(
                polygon
                    .intersection(&clip)
                    .map_coords(|mut p| {
                        p.x -= shift;
                        p
                    })
                    .0,
            );
        }
    }
    Ok(MultiPolygon(output))
}

pub(crate) fn legacy_kartverket_services() -> WcsConfig {
    let provider = builtin_file()
        .terrain
        .iter()
        .find(|p| p.id == "kartverket-nhm")
        .expect("Kartverket built-in");
    let (Some(ProductDefinition::Wcs(dtm)), Some(ProductDefinition::Wcs(dom))) =
        (&provider.products.dtm, &provider.products.dom)
    else {
        unreachable!("Kartverket WCS built-in");
    };
    let services: BTreeMap<_, _> = dtm
        .per_crs
        .iter()
        .map(|(crs, d)| {
            let s = &dom.per_crs[crs];
            (
                crs.clone(),
                WcsService {
                    dtm_endpoint: d.endpoint.clone(),
                    dtm_coverage: d.coverage.clone(),
                    dom_endpoint: s.endpoint.clone(),
                    dom_coverage: s.coverage.clone(),
                },
            )
        })
        .collect();
    WcsConfig { services }
}
