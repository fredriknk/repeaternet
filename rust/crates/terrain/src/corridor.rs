//! Square-ended projected route corridors and round fixed-site buffers.

use geo::{
    orient::{Direction, Orient},
    BoundingRect, Buffer, ConvexHull, Coord, Distance, Euclidean, LineString, MultiPoint,
    MultiPolygon, Polygon, Rect,
};

use crate::{Bounds, LonLat, Point, TerrainError};

/// The projected hull shared by mesh candidate generation and terrain planning.
pub struct MeshArea(Polygon<f64>);

impl MeshArea {
    pub fn new(points: &[Point]) -> Self {
        let points = MultiPoint::from_iter(points.iter().map(|point| (point.x, point.y)));
        Self(points.convex_hull())
    }

    pub fn distance_to(&self, point: Point) -> f64 {
        Euclidean.distance(&geo::Point::new(point.x, point.y), &self.0)
    }

    pub fn buffered(&self, width: f64) -> MultiPolygon<f64> {
        // Geo's hull of collinear sites has zero area. Buffer the line
        // itself so two-client meshes and collinear layouts get round ends.
        let ring = self.0.exterior();
        if ring.0.len() <= 3 {
            ring.buffer(width)
        } else {
            self.0.buffer(width)
        }
    }
}

/// Inclusive membership in a straight corridor extending one half-width
/// along the line past each endpoint.
pub fn inside_corridor(point: Point, start: Point, end: Point, half_width_m: f64) -> bool {
    let dx = end.x - start.x;
    let dy = end.y - start.y;
    let length = dx.hypot(dy);
    let x = point.x - start.x;
    let y = point.y - start.y;
    if length == 0.0 {
        return x.abs() <= half_width_m && y.abs() <= half_width_m;
    }
    let ux = dx / length;
    let uy = dy / length;
    let along = x * ux + y * uy;
    let across = -x * uy + y * ux;
    along >= -half_width_m && along <= length + half_width_m && across.abs() <= half_width_m
}

/// The planner's local metric approximation for WGS84 MeshCore lookups.
pub fn within_corridor_latlon(
    point: LonLat,
    start: LonLat,
    end: LonLat,
    half_width_m: f64,
) -> bool {
    let [point, start, end] = local_metres(point, start, end);
    inside_corridor(point, start, end, half_width_m)
}

/// Distance to the finite route segment using the MeshCore local metric approximation.
pub fn distance_to_segment_m(point: LonLat, start: LonLat, end: LonLat) -> f64 {
    let [point, start, end] = local_metres(point, start, end);
    let segment = difference(end, start);
    let length_squared = dot(segment, segment);
    if length_squared == 0.0 {
        return point.distance_to(start);
    }
    let fraction = (dot(difference(point, start), segment) / length_squared).clamp(0.0, 1.0);
    point.distance_to(offset(start, segment, fraction))
}

fn local_metres(point: LonLat, start: LonLat, end: LonLat) -> [Point; 3] {
    let longitude_scale = 111_320.0
        * ((start.latitude + end.latitude) / 2.0)
            .to_radians()
            .cos()
            .max(0.1);
    [point, start, end].map(|value| Point {
        x: value.longitude * longitude_scale,
        y: value.latitude * 111_320.0,
    })
}

/// Buffer a route with square caps and mitered bends. Round buffers around
/// additional sites can form disconnected components.
pub fn corridor_geometry(
    points: &[Point],
    half_width_m: f64,
    extra_points: &[Point],
) -> Result<MultiPolygon<f64>, TerrainError> {
    validate_buffer(points.iter().chain(extra_points), half_width_m)?;
    let mut path = points.to_vec();
    path.dedup();
    let first = path.first().ok_or(TerrainError::InvalidInput(
        "A corridor needs at least one route point",
    ))?;
    let mut polygons = if path.len() == 1 {
        vec![Rect::new(
            (first.x - half_width_m, first.y - half_width_m),
            (first.x + half_width_m, first.y + half_width_m),
        )
        .to_polygon()
        .orient(Direction::Default)]
    } else {
        stroke_polygons(&path, half_width_m)?
    };
    polygons.extend(
        extra_points
            .iter()
            .map(|&point| round_buffer(point, half_width_m)),
    );
    Ok(if polygons.len() == 1 {
        MultiPolygon::new(polygons)
    } else {
        geo::unary_union(&polygons)
    })
}

fn stroke_polygons(path: &[Point], width: f64) -> Result<Vec<Polygon<f64>>, TerrainError> {
    let closed = path.first() == path.last();
    let directions = path
        .windows(2)
        .map(|segment| normalise(difference(segment[1], segment[0])))
        .collect::<Result<Vec<_>, _>>()?;
    let mut polygons = Vec::new();
    for (i, (&direction, segment)) in directions.iter().zip(path.windows(2)).enumerate() {
        let mut start = segment[0];
        let mut end = segment[1];
        if !closed && i == 0 {
            start = offset(start, direction, -width);
        }
        if !closed && i + 1 == directions.len() {
            end = offset(end, direction, width);
        }
        let left = Point {
            x: -direction.y,
            y: direction.x,
        };
        polygons.push(polygon([
            offset(start, left, -width),
            offset(end, left, -width),
            offset(end, left, width),
            offset(start, left, width),
        ]));
        if i + 1 < directions.len() || closed {
            let next_direction = directions[(i + 1) % directions.len()];
            if let Some(join) = miter_join(segment[1], direction, next_direction, width)? {
                polygons.push(join);
            }
        }
    }
    Ok(polygons)
}

fn miter_join(
    point: Point,
    incoming: Point,
    outgoing: Point,
    width: f64,
) -> Result<Option<Polygon<f64>>, TerrainError> {
    let turn = incoming.x * outgoing.y - incoming.y * outgoing.x;
    if turn == 0.0 {
        return Ok(None);
    }
    let side = turn.signum();
    let before = Point {
        x: side * incoming.y,
        y: -side * incoming.x,
    };
    let after = Point {
        x: side * outgoing.y,
        y: -side * outgoing.x,
    };
    // The normal sum loses precision near a reversal. The direction
    // difference points along the same exterior bisector.
    let bisector = normalise(difference(incoming, outgoing))?;
    let alignment = dot(before, bisector);
    let first = offset(point, before, width);
    let last = offset(point, after, width);
    let join = if alignment >= 0.2 {
        polygon([
            point,
            first,
            offset(point, bisector, width / alignment),
            last,
        ])
    } else {
        // GEOS clips the miter across the exterior bisector at five widths.
        let limit = 5.0 * width;
        let first_cut = (limit - width * alignment) / dot(incoming, bisector);
        let last_cut = (limit - width * dot(after, bisector)) / dot(outgoing, bisector);
        polygon([
            point,
            first,
            offset(first, incoming, first_cut),
            offset(last, outgoing, last_cut),
            last,
        ])
    };
    Ok(Some(join))
}

fn polygon(points: impl IntoIterator<Item = Point>) -> Polygon<f64> {
    let ring = points
        .into_iter()
        .map(|point| Coord {
            x: point.x,
            y: point.y,
        })
        .collect();
    Polygon::new(LineString::new(ring), Vec::new()).orient(Direction::Default)
}

fn dot(a: Point, b: Point) -> f64 {
    a.x * b.x + a.y * b.y
}

fn difference(a: Point, b: Point) -> Point {
    Point {
        x: a.x - b.x,
        y: a.y - b.y,
    }
}

fn offset(point: Point, direction: Point, distance: f64) -> Point {
    Point {
        x: point.x + direction.x * distance,
        y: point.y + direction.y * distance,
    }
}

fn normalise(direction: Point) -> Result<Point, TerrainError> {
    let length = direction.x.hypot(direction.y);
    if !length.is_finite() || length == 0.0 {
        return Err(TerrainError::InvalidInput(
            "Invalid corridor segment length",
        ));
    }
    Ok(Point {
        x: direction.x / length,
        y: direction.y / length,
    })
}

pub fn corridor_bounds(
    points: &[Point],
    half_width_m: f64,
    extra_points: &[Point],
) -> Result<Bounds, TerrainError> {
    let rect = corridor_geometry(points, half_width_m, extra_points)?
        .bounding_rect()
        .ok_or(TerrainError::InvalidInput("Corridor geometry is empty"))?;
    Ok(Bounds {
        min_x: rect.min().x,
        min_y: rect.min().y,
        max_x: rect.max().x,
        max_y: rect.max().y,
    })
}

pub fn site_buffer(point: Point, radius_m: f64) -> Result<MultiPolygon<f64>, TerrainError> {
    validate_buffer(std::iter::once(&point), radius_m)?;
    Ok(MultiPolygon::new(vec![round_buffer(point, radius_m)]))
}

fn round_buffer(point: Point, radius_m: f64) -> Polygon<f64> {
    let ring = (0..64)
        .map(|index| {
            let angle = f64::from(index) * (std::f64::consts::TAU / 64.0);
            let (sin, cos) = angle.sin_cos();
            Coord {
                x: point.x + radius_m * cos,
                y: point.y + radius_m * sin,
            }
        })
        .collect();
    Polygon::new(LineString::new(ring), Vec::new())
}

fn validate_buffer<'a>(
    points: impl IntoIterator<Item = &'a Point>,
    width: f64,
) -> Result<(), TerrainError> {
    if !width.is_finite() || width <= 0.0 {
        return Err(TerrainError::InvalidInput(
            "Corridor width must be finite and positive",
        ));
    }
    if points.into_iter().any(|point| {
        !point.x.is_finite()
            || !point.y.is_finite()
            || !(point.x.abs() + 5.0 * width).is_finite()
            || !(point.y.abs() + 5.0 * width).is_finite()
    }) {
        return Err(TerrainError::InvalidInput(
            "Corridor coordinates must be finite",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod mesh_tests {
    use super::*;
    use geo::Contains;

    #[test]
    fn mesh_buffer_includes_hull_and_round_ends() {
        let point = |x, y| Point { x, y };
        let endpoints = [point(0.0, 0.0), point(4.0, 0.0)];
        let line = MeshArea::new(&endpoints);
        assert_eq!(line.distance_to(point(2.0, 0.0)), 0.0);
        assert!(line.buffered(1.0).contains(&geo::Point::new(-0.8, 0.0)));
        assert!(!line.buffered(1.0).contains(&geo::Point::new(-0.9, 0.9)));
        let triangle = MeshArea::new(&[endpoints[0], endpoints[1], point(2.0, 4.0)]);
        assert_eq!(triangle.distance_to(point(2.0, 3.0)), 0.0);
        assert!(triangle.buffered(1.0).contains(&geo::Point::new(2.0, 4.8)));
        assert!(triangle.distance_to(point(0.0, 3.0)) > 1.0);
    }
}
