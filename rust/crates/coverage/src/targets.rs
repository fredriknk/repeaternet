use std::collections::{BTreeMap, BTreeSet};

use geo::{Area, BooleanOps, Intersects, Polygon, Rect, Validation};
use repeaternet_rf_core::{Point, Site, TerrainSource};
use repeaternet_terrain::{LonLat, Transformer};
use serde::{Deserialize, Serialize};

use crate::{
    analyze_node_failures,
    grid::{clipped_cell_bounds, drawn_polygon},
    invalid, AreaMode, CoverageArea, CoverageCell, CoverageError, CoverageEvaluator, CoverageGrid,
    CoverageNetwork, CoverageSettings, CoverageSourceResult, CoverageState, ExactPointOptions,
    PointState, Rejection, MAX_CELLS,
};

pub const MAX_TARGET_IMPORT_BYTES: usize = 5 * 1024 * 1024;
const MAX_TARGETS: usize = 500;
const MAX_VERTICES: usize = 50_000;
const MAX_LINE_SAMPLES: usize = 20_000;
const MAX_EXACT_EVALUATIONS: usize = 50_000;
const MAX_POLYGONS: usize = 32;
const MAX_POLYGON_CELL_SCANS: usize = 250_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CollectionType {
    FeatureCollection,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FeatureType {
    Feature,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type", content = "coordinates")]
pub enum TargetGeometry {
    Point(Vec<f64>),
    LineString(Vec<Vec<f64>>),
    Polygon(Vec<Vec<Vec<f64>>>),
}

// Decode coordinates through serde_json::Value rather than Serde's tagged-enum
// buffer, which cannot retain arbitrary-precision JSON number representations.
impl<'de> Deserialize<'de> for TargetGeometry {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Geometry {
            #[serde(rename = "type")]
            kind: String,
            coordinates: serde_json::Value,
        }
        let geometry = Geometry::deserialize(deserializer)?;
        match geometry.kind.as_str() {
            "Point" => serde_json::from_value(geometry.coordinates).map(Self::Point),
            "LineString" => serde_json::from_value(geometry.coordinates).map(Self::LineString),
            "Polygon" => serde_json::from_value(geometry.coordinates).map(Self::Polygon),
            _ => return Err(serde::de::Error::custom("Unsupported target geometry type")),
        }
        .map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum GeometryType {
    Point,
    LineString,
    Polygon,
}

impl TargetGeometry {
    fn kind(&self) -> GeometryType {
        match self {
            Self::Point(_) => GeometryType::Point,
            Self::LineString(_) => GeometryType::LineString,
            Self::Polygon(_) => GeometryType::Polygon,
        }
    }

    fn positions(&self) -> Box<dyn Iterator<Item = &[f64]> + '_> {
        match self {
            Self::Point(position) => Box::new(std::iter::once(position.as_slice())),
            Self::LineString(positions) => Box::new(positions.iter().map(Vec::as_slice)),
            Self::Polygon(rings) => Box::new(rings.iter().flatten().map(Vec::as_slice)),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TargetProperties {
    pub name: String,
    pub minimum_margin_db: f64,
    pub reference_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TargetFeature {
    #[serde(rename = "type")]
    pub feature_type: FeatureType,
    pub id: String,
    pub geometry: TargetGeometry,
    pub properties: TargetProperties,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TargetCollection {
    #[serde(rename = "type")]
    pub collection_type: CollectionType,
    pub features: Vec<TargetFeature>,
}

#[derive(Deserialize)]
struct RawCollection {
    #[serde(rename = "type")]
    collection_type: CollectionType,
    features: Vec<RawFeature>,
}

#[derive(Deserialize)]
struct RawFeature {
    #[serde(rename = "type")]
    feature_type: FeatureType,
    #[serde(default)]
    id: Option<serde_json::Value>,
    geometry: TargetGeometry,
    properties: Option<RawProperties>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct RawProperties {
    id: Option<String>,
    name: Option<String>,
    minimum_margin_db: f64,
    reference_id: Option<String>,
}

/// Parse bounded GeoJSON, normalize properties and allocate IDs for unnamed features.
pub fn normalize_targets(json: &[u8]) -> Result<TargetCollection, CoverageError> {
    if json.len() > MAX_TARGET_IMPORT_BYTES {
        return Err(invalid("Target import exceeds 5 MiB"));
    }
    let raw: RawCollection = serde_json::from_slice(json)?;
    if raw.features.len() > MAX_TARGETS {
        return Err(invalid(
            "A target collection must contain between 0 and 500 features",
        ));
    }
    let mut features = Vec::with_capacity(raw.features.len());
    for (index, feature) in raw.features.into_iter().enumerate() {
        let properties = feature.properties.unwrap_or_default();
        let id = match feature.id {
            Some(serde_json::Value::String(id)) => id,
            Some(_) => return Err(invalid("Target IDs must be 1–64 characters")),
            None => match properties.id {
                Some(id) => id,
                None => {
                    let mut bytes = [0; 8];
                    getrandom::fill(&mut bytes).map_err(|error| {
                        invalid(format!("Could not allocate target ID: {error}"))
                    })?;
                    format!("T-{:016x}", u64::from_be_bytes(bytes))
                }
            },
        };
        features.push(TargetFeature {
            feature_type: feature.feature_type,
            id,
            geometry: feature.geometry,
            properties: TargetProperties {
                name: properties
                    .name
                    .unwrap_or_else(|| format!("Target {}", index + 1))
                    .trim()
                    .to_owned(),
                minimum_margin_db: properties.minimum_margin_db,
                reference_id: properties.reference_id,
            },
        });
    }
    let targets = TargetCollection {
        collection_type: raw.collection_type,
        features,
    };
    targets.validate()?;
    Ok(targets)
}

impl TargetCollection {
    pub fn validate(&self) -> Result<(), CoverageError> {
        if self.features.len() > MAX_TARGETS {
            return Err(invalid(
                "A target collection must contain between 0 and 500 features",
            ));
        }
        let mut ids = BTreeSet::new();
        let mut vertices = 0;
        for feature in &self.features {
            if !(1..=64).contains(&feature.id.chars().count()) {
                return Err(invalid("Target IDs must be 1–64 characters"));
            }
            if !ids.insert(&feature.id) {
                return Err(invalid("Target IDs must be unique"));
            }
            let properties = &feature.properties;
            if !(1..=120).contains(&properties.name.trim().chars().count()) {
                return Err(invalid("Target names must be 1–120 characters"));
            }
            if !(-100.0..=100.0).contains(&properties.minimum_margin_db) {
                return Err(invalid("Minimum margin must be between -100 and 100 dB"));
            }
            if properties
                .reference_id
                .as_ref()
                .is_some_and(|id| id.chars().count() > 64)
            {
                return Err(invalid(
                    "Reference router IDs must be at most 64 characters",
                ));
            }
            for position in feature.geometry.positions() {
                vertices += 1;
                if vertices > MAX_VERTICES {
                    return Err(invalid("Target collection exceeds 50,000 vertices"));
                }
                if !(2..=3).contains(&position.len())
                    || !position.iter().all(|v| v.is_finite())
                    || !(-180.0..=180.0).contains(&position[0])
                    || !(-90.0..=90.0).contains(&position[1])
                {
                    return Err(invalid(
                        "Target coordinates must be finite WGS84 longitude/latitude",
                    ));
                }
            }
            match &feature.geometry {
                TargetGeometry::Point(_) => {}
                TargetGeometry::LineString(positions) => {
                    if !positions
                        .windows(2)
                        .any(|pair| pair[0][..2] != pair[1][..2])
                    {
                        return Err(invalid(
                            "Road or route targets need at least two distinct vertices",
                        ));
                    }
                }
                TargetGeometry::Polygon(rings) => {
                    polygon(rings, None)?;
                }
            }
        }
        Ok(())
    }
}

fn lon_lat(position: &[f64]) -> LonLat {
    LonLat {
        longitude: position[0],
        latitude: position[1],
    }
}

fn polygon(
    rings: &[Vec<Vec<f64>>],
    transform: Option<&Transformer>,
) -> Result<Polygon<f64>, CoverageError> {
    let mut lines = Vec::with_capacity(rings.len());
    for ring in rings {
        if ring.len() < 4 || ring.first() != ring.last() {
            return Err(invalid(
                "Polygon rings must contain at least four positions and be closed",
            ));
        }
        let projected = ring
            .iter()
            .map(|position| {
                if let Some(transform) = transform {
                    let point = transform.forward(lon_lat(position))?;
                    Ok((point.x, point.y))
                } else {
                    Ok((position[0], position[1]))
                }
            })
            .collect::<Result<Vec<_>, CoverageError>>()?;
        lines.push(geo::LineString::from(projected));
    }
    let mut lines = lines.into_iter();
    let exterior = lines
        .next()
        .ok_or_else(|| invalid("Polygon coordinates need at least one closed ring"))?;
    let polygon = Polygon::new(exterior, lines.collect());
    if !polygon.is_valid() || polygon.unsigned_area() <= 0.0 {
        return Err(invalid("Target geometry must be nonempty and valid"));
    }
    Ok(polygon)
}

/// Distances measured from the start of a road and the length represented by each sample.
pub fn line_sample_positions(
    length_m: f64,
    spacing_m: f64,
) -> Result<Vec<(f64, f64)>, CoverageError> {
    if !length_m.is_finite() || length_m <= 0.0 {
        return Err(invalid("Road target length must be positive"));
    }
    if !spacing_m.is_finite() || spacing_m <= 0.0 {
        return Err(invalid("Road sample spacing must be positive"));
    }
    let intervals = (length_m / spacing_m).ceil();
    if intervals >= MAX_LINE_SAMPLES as f64 {
        return Err(invalid(
            "Road target exceeds the 20,000-sample limit; increase sample spacing",
        ));
    }
    let mut distances: Vec<_> = (0..intervals.max(1.0) as usize)
        .map(|i| i as f64 * spacing_m)
        .filter(|&d| d < length_m)
        .collect();
    distances.push(length_m);
    Ok(distances
        .iter()
        .enumerate()
        .map(|(i, &distance)| {
            let left = if i == 0 {
                0.0
            } else {
                (distances[i - 1] + distance) / 2.0
            };
            let right = distances
                .get(i + 1)
                .map_or(length_m, |next| (distance + next) / 2.0);
            (distance, right - left)
        })
        .collect())
}

fn road_samples(
    positions: &[Vec<f64>],
    transform: &Transformer,
    spacing: f64,
) -> Result<Vec<(Point, f64)>, CoverageError> {
    let mut points: Vec<_> = positions
        .iter()
        .map(|p| transform.forward(lon_lat(p)))
        .collect::<Result<_, _>>()?;
    points.dedup();
    let mut ends = Vec::with_capacity(points.len());
    ends.push(0.0);
    for pair in points.windows(2) {
        ends.push(ends.last().expect("initial distance") + pair[0].distance_to(pair[1]));
    }
    let mut segment = 0;
    Ok(
        line_sample_positions(*ends.last().expect("initial distance"), spacing)?
            .into_iter()
            .map(|(distance, weight)| {
                while ends[segment + 1] < distance {
                    segment += 1;
                }
                let a = points[segment];
                let b = points[segment + 1];
                let frac = (distance - ends[segment]) / (ends[segment + 1] - ends[segment]);
                (
                    Point {
                        x: a.x + frac * (b.x - a.x),
                        y: a.y + frac * (b.y - a.y),
                    },
                    weight,
                )
            })
            .collect(),
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetState {
    Pass,
    Fail,
    Unknown,
    Unresolved,
    OutsideAnalysedArea,
    NeedsFinerSampling,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UnresolvedSourceDetail {
    pub source_id: String,
    pub detail: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PointTargetStats {
    pub latitude: f64,
    pub longitude: f64,
    pub evaluated_samples: usize,
    #[serde(flatten)]
    pub links: Option<PointTargetLinks>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PointTargetLinks {
    pub unresolved_sources: usize,
    pub unresolved_source_details: Vec<UnresolvedSourceDetail>,
    pub passing_sources: Vec<String>,
    pub best_two_way_margin_db: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RoadTargetStats {
    pub total_length_m: f64,
    pub covered_length_m: f64,
    pub failed_length_m: f64,
    pub unknown_length_m: f64,
    pub unresolved_length_m: f64,
    pub outside_analysed_area_length_m: f64,
    pub sample_spacing_m: f64,
    pub sample_count: usize,
    pub worst_passing_margin_db: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PolygonTargetStats {
    pub requested_area_m2: f64,
    pub analysed_area_m2: f64,
    pub outside_analysed_area_m2: f64,
    pub evaluated_area_m2: f64,
    pub covered_area_m2: f64,
    pub failed_area_m2: f64,
    pub unknown_area_m2: f64,
    pub unresolved_area_m2: f64,
    pub grid_cell_size_m: f64,
    pub sampled_cells: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TargetStats {
    Point(PointTargetStats),
    Road(RoadTargetStats),
    Polygon(PolygonTargetStats),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TargetResult {
    pub id: String,
    pub name: String,
    pub geometry_type: GeometryType,
    pub minimum_margin_db: f64,
    pub reference_id: Option<String>,
    pub state: TargetState,
    pub message: String,
    #[serde(flatten)]
    pub stats: TargetStats,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TargetAssumptions {
    pub coverage_job_id: Option<String>,
    pub model: String,
    pub minimum_margin_db: String,
    pub road_sample_spacing_m: f64,
    pub polygon_grid_cell_size_m: f64,
    pub reference_connected_source_ids: BTreeMap<String, Vec<String>>,
    pub terrain_surface_available: bool,
    pub warning: String,
    pub router_evaluations: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TargetAssessment {
    pub targets: Vec<TargetResult>,
    pub assumptions: TargetAssumptions,
}

pub struct TargetAssessmentInput<'a> {
    pub targets: &'a TargetCollection,
    pub grid: &'a CoverageGrid,
    pub cells: &'a [CoverageCell],
    pub network: &'a CoverageNetwork,
    pub sources: &'a [Site],
    pub settings: &'a CoverageSettings,
    pub final_sample_step_m: f64,
    pub road_spacing_m: f64,
    pub cancelled: &'a dyn Fn() -> bool,
}

impl<T: TerrainSource + ?Sized> CoverageEvaluator<'_, T> {
    pub fn assess_targets(
        &self,
        input: TargetAssessmentInput<'_>,
    ) -> Result<TargetAssessment, CoverageError> {
        input.targets.validate()?;
        input.settings.validate()?;
        if !(25.0..=5000.0).contains(&input.road_spacing_m) {
            return Err(invalid(
                "Road sample spacing must be between 25 and 5,000 m",
            ));
        }
        if input.sources.is_empty() || input.sources.len() > input.settings.maximum_sources {
            return Err(invalid(
                "Target assessments require a bounded nonempty source selection",
            ));
        }
        let grid = input.grid;
        let polygons = input
            .targets
            .features
            .iter()
            .filter(|feature| feature.geometry.kind() == GeometryType::Polygon)
            .count();
        if polygons > MAX_POLYGONS {
            return Err(invalid("Assess at most 32 polygon targets at once"));
        }
        if polygons.saturating_mul(grid.requested_cells) > MAX_POLYGON_CELL_SCANS {
            return Err(invalid("Polygon assessment exceeds the bounded grid-scan limit; assess fewer areas at a time"));
        }
        if !(1..=MAX_CELLS).contains(&grid.requested_cells)
            || grid.requested_cells != input.cells.len()
            || !grid.effective_cell_size_m.is_finite()
            || grid.effective_cell_size_m <= 0.0
            || input.cells.iter().enumerate().any(|(index, cell)| {
                cell.index != index
                    || !cell.x.is_finite()
                    || !cell.y.is_finite()
                    || cell.state == CoverageState::NotEvaluated
            })
        {
            return Err(invalid(
                "Coverage results are incomplete; calculate again first",
            ));
        }
        let mut area = CoverageArea::rectangle(grid.bounds)?.geometry().clone();
        if input.settings.area_mode == AreaMode::Drawn {
            let drawn = drawn_polygon(
                input
                    .settings
                    .area_polygon_wgs84
                    .as_deref()
                    .unwrap_or_default(),
                Some(self.transform),
            )?;
            area = area.intersection(&drawn);
        }
        let all_sources: BTreeSet<_> = input.sources.iter().map(|site| site.id.as_str()).collect();
        let mut connected = BTreeMap::new();
        let mut roads = BTreeMap::new();
        let mut exact_work = 0;
        for feature in &input.targets.features {
            if (input.cancelled)() {
                return Err(CoverageError::Cancelled);
            }
            if let Some(reference) = &feature.properties.reference_id {
                if !connected.contains_key(reference) {
                    if !all_sources.contains(reference.as_str()) {
                        return Err(invalid(
                            "Reference router must be an enabled coverage source",
                        ));
                    }
                    let network = analyze_node_failures(
                        std::iter::empty::<CoverageCell>(),
                        input.network,
                        &[],
                        reference,
                    )?;
                    connected.insert(reference.clone(), network.connected_source_ids);
                }
            }
            let samples = match &feature.geometry {
                TargetGeometry::Point(_) => 1,
                TargetGeometry::LineString(positions) => {
                    let samples = road_samples(positions, self.transform, input.road_spacing_m)?;
                    let count = samples.len();
                    roads.insert(feature.id.as_str(), samples);
                    count
                }
                TargetGeometry::Polygon(_) => 0,
            };
            exact_work += samples * input.sources.len();
            if exact_work > MAX_EXACT_EVALUATIONS {
                return Err(invalid("Point and road targets exceed the 50,000-router-evaluation limit; increase road spacing or assess fewer targets"));
            }
        }

        let mut results = Vec::with_capacity(input.targets.features.len());
        for feature in &input.targets.features {
            if (input.cancelled)() {
                return Err(CoverageError::Cancelled);
            }
            let properties = &feature.properties;
            let reference_sources = properties.reference_id.as_ref().map(|id| {
                connected[id]
                    .iter()
                    .map(String::as_str)
                    .collect::<BTreeSet<_>>()
            });
            let selection = TargetSelection {
                sources: reference_sources.as_ref().unwrap_or(&all_sources),
                minimum_margin_db: properties.minimum_margin_db,
            };
            let (state, message, stats) = match &feature.geometry {
                TargetGeometry::Point(position) => {
                    let location = lon_lat(position);
                    let projected = self.transform.forward(location)?;
                    if area.intersects(&geo::Point::new(projected.x, projected.y)) {
                        let (stats, state, message) =
                            self.point_target(projected, location, &input, &selection)?;
                        (state, message, TargetStats::Point(stats))
                    } else {
                        (
                            TargetState::OutsideAnalysedArea,
                            "This point is outside the requested coverage analysis area.".into(),
                            TargetStats::Point(PointTargetStats {
                                latitude: location.latitude,
                                longitude: location.longitude,
                                evaluated_samples: 0,
                                links: None,
                            }),
                        )
                    }
                }
                TargetGeometry::LineString(_) => {
                    let (stats, state, message) =
                        self.road_target(&roads[feature.id.as_str()], &area, &input, &selection)?;
                    (state, message.into(), TargetStats::Road(stats))
                }
                TargetGeometry::Polygon(rings) => {
                    let projected = polygon(rings, Some(self.transform))?;
                    let (stats, state, message) =
                        polygon_target(&projected, &area, &input, &selection)?;
                    (state, message.into(), TargetStats::Polygon(stats))
                }
            };
            results.push(TargetResult {
                id: feature.id.clone(),
                name: properties.name.clone(),
                minimum_margin_db: properties.minimum_margin_db,
                reference_id: properties.reference_id.clone(),
                geometry_type: feature.geometry.kind(),
                state,
                message,
                stats,
            });
        }
        Ok(TargetAssessment {
            targets: results,
            assumptions: TargetAssumptions {
                coverage_job_id: None,
                model: "two-way terrain-based RF link prediction".into(),
                minimum_margin_db: "per target".into(),
                road_sample_spacing_m: input.road_spacing_m,
                polygon_grid_cell_size_m: grid.effective_cell_size_m,
                reference_connected_source_ids: connected,
                terrain_surface_available: self.terrain.has_surface(),
                warning: "Predictions are not measurements or guarantees of packet delivery; polygon results represent grid samples only.".into(),
                router_evaluations: exact_work,
            },
        })
    }

    fn point_target(
        &self,
        point: Point,
        location: LonLat,
        input: &TargetAssessmentInput<'_>,
        selection: &TargetSelection<'_>,
    ) -> Result<(PointTargetStats, TargetState, String), CoverageError> {
        let evaluated = self
            .evaluate_projected_point(
                point,
                location,
                input.sources,
                input.settings,
                input.point_options(),
            )?
            .coverage;
        let state = selection.classify(
            evaluated.sources.iter().map(|row| &row.result),
            evaluated.state.into(),
        );
        let mut stats = PointTargetLinks {
            unresolved_sources: 0,
            unresolved_source_details: Vec::new(),
            passing_sources: Vec::new(),
            best_two_way_margin_db: None,
        };
        for row in evaluated
            .sources
            .iter()
            .map(|row| &row.result)
            .filter(|row| selection.sources.contains(row.source_id.as_str()))
        {
            if row.rejection == Some(Rejection::ProfileSampleLimit) {
                stats
                    .unresolved_source_details
                    .push(UnresolvedSourceDetail {
                        source_id: row.source_id.clone(),
                        detail: row.rejection_detail.clone(),
                    });
            }
            if let Some(margin) = row.usable_margin(crate::CoverageMode::TwoWay) {
                if stats
                    .best_two_way_margin_db
                    .is_none_or(|best| margin > best)
                {
                    stats.best_two_way_margin_db = Some(margin);
                }
                if margin >= selection.minimum_margin_db {
                    stats.passing_sources.push(row.source_id.clone());
                }
            }
        }
        stats.unresolved_sources = stats.unresolved_source_details.len();
        let mut message = match state {
            TargetState::Pass => "At least one selected source meets the two-way target margin.",
            TargetState::Unresolved => "No selected router fits the configured terrain-profile sample limit; increase the profile step explicitly or raise the cap.",
            TargetState::Unknown => "Terrain or a selected source link is unknown at this location.",
            _ => "No selected source meets the two-way target margin.",
        }.to_owned();
        if state == TargetState::Unresolved {
            if let Some(detail) = stats
                .unresolved_source_details
                .iter()
                .find_map(|row| row.detail.as_ref())
            {
                message.push(' ');
                message.push_str(detail);
            }
        }
        Ok((
            PointTargetStats {
                latitude: location.latitude,
                longitude: location.longitude,
                evaluated_samples: usize::from(!matches!(
                    evaluated.state,
                    PointState::UnknownTerrain | PointState::UnknownLinks
                )),
                links: Some(stats),
            },
            state,
            message,
        ))
    }

    fn road_target(
        &self,
        samples: &[(Point, f64)],
        area: &geo::MultiPolygon<f64>,
        input: &TargetAssessmentInput<'_>,
        selection: &TargetSelection<'_>,
    ) -> Result<(RoadTargetStats, TargetState, &'static str), CoverageError> {
        let mut stats = RoadTargetStats {
            total_length_m: samples.iter().map(|(_, weight)| weight).sum(),
            covered_length_m: 0.0,
            failed_length_m: 0.0,
            unknown_length_m: 0.0,
            unresolved_length_m: 0.0,
            outside_analysed_area_length_m: 0.0,
            sample_spacing_m: input.road_spacing_m,
            sample_count: samples.len(),
            worst_passing_margin_db: None,
        };
        for &(point, weight) in samples {
            if (input.cancelled)() {
                return Err(CoverageError::Cancelled);
            }
            if !area.intersects(&geo::Point::new(point.x, point.y)) {
                stats.outside_analysed_area_length_m += weight;
                continue;
            }
            let location = self.transform.reverse(point)?;
            let evaluated = self
                .evaluate_projected_point(
                    point,
                    location,
                    input.sources,
                    input.settings,
                    input.point_options(),
                )?
                .coverage;
            let rows = evaluated.sources.iter().map(|row| &row.result);
            match selection.classify(rows.clone(), evaluated.state.into()) {
                TargetState::Pass => {
                    stats.covered_length_m += weight;
                    stats.worst_passing_margin_db = rows
                        .filter_map(|row| selection.passing_margin(row))
                        .chain(stats.worst_passing_margin_db)
                        .reduce(f64::min);
                }
                TargetState::Unresolved => stats.unresolved_length_m += weight,
                TargetState::Unknown => stats.unknown_length_m += weight,
                _ => stats.failed_length_m += weight,
            }
        }
        let tolerance = (stats.total_length_m * 1e-9).max(1e-6);
        let unknown = stats.unknown_length_m + stats.outside_analysed_area_length_m;
        let (state, message) = if stats.outside_analysed_area_length_m
            >= stats.total_length_m - tolerance
        {
            (
                TargetState::OutsideAnalysedArea,
                "Every sampled road segment lies outside the requested analysis area.",
            )
        } else if unknown + stats.unresolved_length_m <= tolerance
            && stats.failed_length_m <= tolerance
        {
            (
                TargetState::Pass,
                "Every sampled road segment meets the two-way target margin.",
            )
        } else if unknown > tolerance {
            (TargetState::Unknown, "Some road length lies outside the analysis area or has unknown terrain/source links.")
        } else if stats.unresolved_length_m > tolerance {
            (TargetState::Unresolved, "Some road segments exceed the configured terrain-profile sample limit; increase the profile step explicitly or raise the cap.")
        } else {
            (
                TargetState::Fail,
                "Some sampled road length does not meet the two-way target margin.",
            )
        };
        Ok((stats, state, message))
    }
}

impl TargetAssessmentInput<'_> {
    fn point_options(&self) -> ExactPointOptions<'_> {
        ExactPointOptions {
            final_sample_step_m: self.final_sample_step_m,
            include_serving_profile: false,
            cancelled: self.cancelled,
        }
    }
}

fn polygon_target(
    projected: &Polygon<f64>,
    area: &geo::MultiPolygon<f64>,
    input: &TargetAssessmentInput<'_>,
    selection: &TargetSelection<'_>,
) -> Result<(PolygonTargetStats, TargetState, &'static str), CoverageError> {
    let clip = area.intersection(projected);
    let requested = projected.unsigned_area();
    let analysed = clip.unsigned_area();
    let mut stats = PolygonTargetStats {
        requested_area_m2: requested,
        analysed_area_m2: analysed,
        outside_analysed_area_m2: (requested - analysed).max(0.0),
        evaluated_area_m2: 0.0,
        covered_area_m2: 0.0,
        failed_area_m2: 0.0,
        unknown_area_m2: 0.0,
        unresolved_area_m2: 0.0,
        grid_cell_size_m: input.grid.effective_cell_size_m,
        sampled_cells: 0,
    };
    for cell in input.cells {
        if (input.cancelled)() {
            return Err(CoverageError::Cancelled);
        }
        let [west, south, east, north] =
            clipped_cell_bounds(cell.position(), input.grid.bounds, stats.grid_cell_size_m);
        if west >= east || south >= north {
            continue;
        }
        let footprint = Rect::new((west, south), (east, north)).to_polygon();
        if !clip.intersects(&footprint) {
            continue;
        }
        let overlap = clip.intersection(&footprint).unsigned_area();
        if overlap <= 0.0 {
            continue;
        }
        if !projected.intersects(&geo::Point::new(cell.x, cell.y)) {
            stats.unknown_area_m2 += overlap;
            continue;
        }
        stats.sampled_cells += 1;
        let initial = match cell.state {
            CoverageState::UnknownTerrain
            | CoverageState::NotEvaluated
            | CoverageState::OutsideArea => TargetState::Unknown,
            CoverageState::Unresolved => TargetState::Unresolved,
            _ => TargetState::Fail,
        };
        match selection.classify(cell.sources.iter(), initial) {
            TargetState::Pass => stats.covered_area_m2 += overlap,
            TargetState::Unresolved => stats.unresolved_area_m2 += overlap,
            TargetState::Unknown => stats.unknown_area_m2 += overlap,
            _ => stats.failed_area_m2 += overlap,
        }
    }
    stats.evaluated_area_m2 = stats.covered_area_m2 + stats.failed_area_m2;
    stats.unknown_area_m2 +=
        (analysed - stats.evaluated_area_m2 - stats.unknown_area_m2 - stats.unresolved_area_m2)
            .max(0.0);
    let tolerance = (requested * 1e-9).max(1e-3);
    let (state, message) = if analysed <= tolerance {
        (
            TargetState::OutsideAnalysedArea,
            "Target area is outside the analysed coverage grid.",
        )
    } else if stats.sampled_cells == 0 {
        (
            TargetState::NeedsFinerSampling,
            "No coverage-grid sample falls inside this area; calculate a finer grid.",
        )
    } else if stats.unknown_area_m2 > tolerance || stats.outside_analysed_area_m2 > tolerance {
        (TargetState::Unknown, "Some target area lies outside the requested analysis area or has unknown terrain/source links.")
    } else if stats.unresolved_area_m2 > tolerance {
        (TargetState::Unresolved, "Some target area has no result because router paths exceed the configured profile sample limit; increase the profile step explicitly or raise the cap.")
    } else if stats.failed_area_m2 > tolerance {
        (
            TargetState::Fail,
            "Some sampled target area does not meet the two-way target margin.",
        )
    } else {
        (
            TargetState::Pass,
            "All represented area meets the two-way target margin at this grid resolution.",
        )
    };
    Ok((stats, state, message))
}

impl From<PointState> for TargetState {
    fn from(state: PointState) -> Self {
        match state {
            PointState::UnknownTerrain | PointState::UnknownLinks => Self::Unknown,
            PointState::Unresolved => Self::Unresolved,
            PointState::Evaluated => Self::Fail,
        }
    }
}

struct TargetSelection<'a> {
    sources: &'a BTreeSet<&'a str>,
    minimum_margin_db: f64,
}

impl TargetSelection<'_> {
    fn passing_margin(&self, row: &CoverageSourceResult) -> Option<f64> {
        if self.sources.contains(row.source_id.as_str()) {
            row.usable_margin(crate::CoverageMode::TwoWay)
                .filter(|margin| *margin >= self.minimum_margin_db)
        } else {
            None
        }
    }

    fn classify<'r>(
        &self,
        rows: impl Iterator<Item = &'r CoverageSourceResult>,
        mut state: TargetState,
    ) -> TargetState {
        for row in rows.filter(|row| self.sources.contains(row.source_id.as_str())) {
            if self.passing_margin(row).is_some() {
                return TargetState::Pass;
            }
            match row.rejection {
                Some(Rejection::ProfileSampleLimit) => state = TargetState::Unresolved,
                Some(Rejection::UnknownTerrain) if state != TargetState::Unresolved => {
                    state = TargetState::Unknown
                }
                _ => {}
            }
        }
        state
    }
}
