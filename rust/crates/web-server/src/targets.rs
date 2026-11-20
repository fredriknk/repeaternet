use std::collections::HashSet;

use geo::{Area, Coord, LineString, Polygon, Validation};
use serde_json::{Map, Value, json};

pub(crate) const IMPORT_LIMIT: usize = 5 * 1024 * 1024;
const MAX_VERTICES: usize = 50_000;

pub(crate) fn normalize(collection: &Value) -> Result<Value, &'static str> {
    if collection["type"] != "FeatureCollection" {
        return Err("Import a GeoJSON FeatureCollection");
    }
    let features = collection["features"]
        .as_array()
        .filter(|items| items.len() <= 500)
        .ok_or("A target collection must contain between 0 and 500 features")?;
    let mut ids = HashSet::new();
    let mut vertices = 0;
    let mut normalized = Vec::with_capacity(features.len());
    for (index, feature) in features.iter().enumerate() {
        if feature["type"] != "Feature" {
            return Err("Every target must be a GeoJSON Feature");
        }
        let geometry = feature["geometry"]
            .as_object()
            .ok_or("Each target needs geometry and a properties object")?;
        let empty = Map::new();
        let properties = match &feature["properties"] {
            Value::Null | Value::Bool(false) => &empty,
            Value::String(value) if value.is_empty() => &empty,
            Value::Array(value) if value.is_empty() => &empty,
            Value::Number(value) if value.as_f64() == Some(0.0) => &empty,
            value => value
                .as_object()
                .ok_or("Each target needs geometry and a properties object")?,
        };
        let kind = geometry
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let coordinates = geometry.get("coordinates").unwrap_or(&Value::Null);
        validate_geometry(kind, coordinates, &mut vertices)?;

        let id = match feature.get("id").or_else(|| properties.get("id")) {
            None | Some(Value::Null) => {
                let mut bytes = [0; 8];
                getrandom::fill(&mut bytes).map_err(|_| "Could not generate a target ID")?;
                format!("T-{:016x}", u64::from_be_bytes(bytes))
            }
            Some(value) => value
                .as_str()
                .filter(|value| !value.is_empty() && value.chars().count() <= 64)
                .ok_or("Target IDs must be 1–64 characters")?
                .to_owned(),
        };
        if !ids.insert(id.clone()) {
            return Err("Target IDs must be unique");
        }
        let default_name = format!("Target {}", index + 1);
        let name = properties
            .get("name")
            .map_or(Some(default_name.as_str()), Value::as_str)
            .map(|name| {
                name.trim_matches(|c: char| c.is_whitespace() || matches!(c, '\u{1c}'..='\u{1f}'))
            })
            .filter(|name| !name.is_empty() && name.chars().count() <= 120)
            .ok_or("Target names must be 1–120 characters")?;
        let margin = properties
            .get("minimum_margin_db")
            .map_or(Some(0.0), Value::as_f64)
            .filter(|value| (-100.0..=100.0).contains(value))
            .ok_or("Minimum margin must be between -100 and 100 dB")?;
        let reference = properties.get("reference_id").unwrap_or(&Value::Null);
        if !reference.is_null()
            && !reference
                .as_str()
                .is_some_and(|value| value.chars().count() <= 64)
        {
            return Err("Reference router IDs must be at most 64 characters");
        }
        normalized.push(json!({
            "type": "Feature", "id": id,
            "geometry": {"type": kind, "coordinates": coordinates},
            "properties": {"name": name, "minimum_margin_db": margin, "reference_id": reference},
        }));
    }
    Ok(json!({"type": "FeatureCollection", "features": normalized}))
}

fn validate_geometry(
    kind: &str,
    coordinates: &Value,
    vertices: &mut usize,
) -> Result<(), &'static str> {
    const INVALID: &str = "Target geometry must be nonempty and valid";
    match kind {
        "Point" => {
            position(coordinates, vertices)?;
        }
        "LineString" => {
            let points = line(coordinates, vertices)?;
            if points.0.len() == 1 {
                return Err("Target geometry is malformed");
            }
            if points.0.is_empty() || points.0.windows(2).all(|pair| pair[0] == pair[1]) {
                return Err(INVALID);
            }
        }
        "Polygon" => {
            let rings = coordinates
                .as_array()
                .filter(|rings| !rings.is_empty())
                .ok_or("Polygon coordinates need at least one closed ring")?;
            let mut lines = Vec::with_capacity(rings.len());
            for ring in rings {
                let items = ring
                    .as_array()
                    .filter(|items| items.len() >= 4)
                    .ok_or("Polygon rings must contain at least four positions and be closed")?;
                let first = items.first().unwrap().as_array();
                let last = items.last().unwrap().as_array();
                // Python compares numeric coordinate values across int and float types.
                if !matches!((first, last), (Some(a), Some(b)) if a.len() == b.len()
                    && a.iter().zip(b).all(|(left, right)| left == right || left.as_f64().is_some_and(|left| Some(left) == right.as_f64())))
                {
                    return Err("Polygon rings must contain at least four positions and be closed");
                }
                lines.push(line(ring, vertices)?);
            }
            let mut lines = lines.into_iter();
            let polygon = Polygon::new(lines.next().unwrap(), lines.collect());
            if !polygon.is_valid() {
                return Err(INVALID);
            }
            if polygon.unsigned_area() <= 0.0 {
                return Err("Polygon rings must be closed and enclose an area");
            }
        }
        _ => return Err("Targets must be Point, Polygon, or LineString geometries"),
    }
    Ok(())
}

fn line(value: &Value, vertices: &mut usize) -> Result<LineString<f64>, &'static str> {
    let points = value.as_array().ok_or("Target coordinates are malformed")?;
    points
        .iter()
        .map(|point| position(point, vertices))
        .collect::<Result<Vec<_>, _>>()
        .map(LineString)
}

fn position(value: &Value, vertices: &mut usize) -> Result<Coord<f64>, &'static str> {
    let point = value.as_array().ok_or("Target coordinates are malformed")?;
    if *vertices >= MAX_VERTICES {
        return Err("Target collection exceeds 50000 vertices");
    }
    *vertices += 1;
    let [x, y, ..] = point.as_slice() else {
        return Err("Target coordinates are malformed");
    };
    let (Some(x), Some(y)) = (x.as_f64(), y.as_f64()) else {
        if x.is_boolean() || y.is_boolean() {
            return Err("Target coordinates must be finite WGS84 longitude/latitude");
        }
        return Err("Target coordinates are malformed");
    };
    if !(-180.0..=180.0).contains(&x) || !(-90.0..=90.0).contains(&y) {
        return Err("Target coordinates must be finite WGS84 longitude/latitude");
    }
    if point.len() > 3 || (point.len() == 3 && !point[2].is_number()) {
        return Err("Target geometry is malformed");
    }
    Ok(Coord { x, y })
}
