use geo::{Area, BoundingRect, Intersects, LineString, MultiPolygon, Polygon, Validation};
use repeaternet_rf_core::{Point, Site, SiteKind, TerrainSource};
use repeaternet_terrain::{
    corridor::{corridor_geometry, site_buffer},
    LonLat, Transformer,
};

use crate::{
    invalid, AreaMode, CoverageCell, CoverageError, CoverageGrid, CoverageSettings, CoverageState,
};

/// Projected bounds in [min_x, min_y, max_x, max_y] order.
pub type ProjectedBounds = [f64; 4];
/// WGS84 bounds in [south, west, north, east] order.
pub type MapBounds = [f64; 4];

#[derive(Clone, Debug)]
pub struct CoverageArea {
    bounds: ProjectedBounds,
    geometry: MultiPolygon<f64>,
}

impl CoverageArea {
    pub fn rectangle(bounds: ProjectedBounds) -> Result<Self, CoverageError> {
        validate_bounds(bounds)?;
        let [left, bottom, right, top] = bounds;
        let rectangle = geo::Rect::new((left, bottom), (right, top));
        Ok(Self {
            bounds,
            geometry: MultiPolygon::new(vec![rectangle.to_polygon()]),
        })
    }

    pub fn from_geometry(geometry: MultiPolygon<f64>) -> Result<Self, CoverageError> {
        let rect = geometry
            .bounding_rect()
            .ok_or_else(|| invalid("Coverage area must not be empty"))?;
        let bounds = [rect.min().x, rect.min().y, rect.max().x, rect.max().y];
        validate_bounds(bounds)?;
        if !geometry.is_valid() || geometry.unsigned_area() <= 0.0 {
            return Err(invalid(
                "Coverage area is invalid in the terrain coordinate system",
            ));
        }
        Ok(Self { bounds, geometry })
    }

    pub fn bounds(&self) -> ProjectedBounds {
        self.bounds
    }

    pub fn geometry(&self) -> &MultiPolygon<f64> {
        &self.geometry
    }

    pub fn includes(&self, point: Point) -> bool {
        self.geometry.intersects(&geo::Point::new(point.x, point.y))
    }
}

fn validate_bounds([left, bottom, right, top]: ProjectedBounds) -> Result<(), CoverageError> {
    if !(right - left).is_finite() || !(top - bottom).is_finite() || left >= right || bottom >= top
    {
        return Err(invalid(
            "Coverage area bounds must be finite and have positive width and height",
        ));
    }
    Ok(())
}

pub(crate) fn drawn_polygon(
    ring: &[[f64; 2]],
    transform: Option<&Transformer>,
) -> Result<Polygon<f64>, CoverageError> {
    if !(4..=501).contains(&ring.len()) || (ring.len() == 501 && ring.first() != ring.last()) {
        return Err(invalid("Drawn coverage areas need 3 to 500 vertices"));
    }
    let coordinates = ring
        .iter()
        .map(|&[longitude, latitude]| {
            if !(-180.0..=180.0).contains(&longitude) || !(-90.0..=90.0).contains(&latitude) {
                return Err(invalid("Coverage polygon coordinates must be WGS84"));
            }
            if let Some(transform) = transform {
                let point = transform.forward(LonLat {
                    longitude,
                    latitude,
                })?;
                Ok((point.x, point.y))
            } else {
                Ok((longitude, latitude))
            }
        })
        .collect::<Result<Vec<_>, CoverageError>>()?;
    let polygon = Polygon::new(LineString::from(coordinates), vec![]);
    if !polygon.is_valid() || polygon.unsigned_area() <= 0.0 {
        return Err(invalid(
            "Coverage polygon must be a valid, non-self-intersecting area",
        ));
    }
    Ok(polygon)
}

pub(crate) fn is_endpoint(kind: SiteKind) -> bool {
    matches!(
        kind,
        SiteKind::EndpointA | SiteKind::EndpointB | SiteKind::Client
    )
}

/// Resolve the analysis area using the active solution and the selected radio sources.
pub fn resolve_area(
    settings: &CoverageSettings,
    transform: &Transformer,
    sources: &[Site],
    active_sites: &[Site],
    corridor_half_width_m: f64,
    map_bounds: Option<MapBounds>,
) -> Result<CoverageArea, CoverageError> {
    settings.validate()?;
    if sources.is_empty() {
        return Err(invalid(
            "Select at least one mesh router to calculate coverage",
        ));
    }
    match settings.area_mode {
        AreaMode::View => {
            let [south, west, north, east] = map_bounds
                .ok_or_else(|| invalid("Map coverage needs south, west, north, and east bounds"))?;
            if south >= north || west >= east {
                return Err(invalid(
                    "Map coverage bounds must have positive width and height",
                ));
            }
            let first = transform.forward(LonLat {
                longitude: west,
                latitude: south,
            })?;
            let last = transform.forward(LonLat {
                longitude: east,
                latitude: north,
            })?;
            CoverageArea::rectangle([
                first.x.min(last.x),
                first.y.min(last.y),
                first.x.max(last.x),
                first.y.max(last.y),
            ])
        }
        AreaMode::Drawn => CoverageArea::from_geometry(MultiPolygon::new(vec![drawn_polygon(
            settings.area_polygon_wgs84.as_deref().unwrap_or_default(),
            Some(transform),
        )?])),
        AreaMode::Mesh if settings.area_includes_clients => {
            let mut parts = Vec::new();
            for site in sources
                .iter()
                .chain(active_sites.iter().filter(|site| is_endpoint(site.kind)))
            {
                parts.extend(site_buffer(site.position, settings.area_buffer_m)?.0);
            }
            let a = active_sites.iter().find(|s| s.kind == SiteKind::EndpointA);
            let b = active_sites.iter().find(|s| s.kind == SiteKind::EndpointB);
            if let (Some(a), Some(b)) = (a, b) {
                parts.extend(
                    corridor_geometry(&[a.position, b.position], corridor_half_width_m, &[])?.0,
                );
            }
            CoverageArea::from_geometry(geo::unary_union(&parts))
        }
        AreaMode::Mesh => {
            let [mut left, mut bottom, mut right, mut top] = [
                f64::INFINITY,
                f64::INFINITY,
                f64::NEG_INFINITY,
                f64::NEG_INFINITY,
            ];
            for site in sources {
                let Point { x, y } = site.position;
                if !x.is_finite() || !y.is_finite() {
                    return Err(invalid("Coverage source positions must be finite"));
                }
                left = left.min(x);
                bottom = bottom.min(y);
                right = right.max(x);
                top = top.max(y);
            }
            let buffer = settings.area_buffer_m;
            CoverageArea::rectangle([left - buffer, bottom - buffer, right + buffer, top + buffer])
        }
    }
}

pub fn area_bounds_wgs84(
    bounds: ProjectedBounds,
    transform: &Transformer,
) -> Result<MapBounds, CoverageError> {
    validate_bounds(bounds)?;
    let [left, bottom, right, top] = bounds;
    let mut result = [
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    ];
    for (x, y) in [(left, bottom), (left, top), (right, top), (right, bottom)] {
        let LonLat {
            latitude,
            longitude,
        } = transform.reverse(Point { x, y })?;
        result[0] = result[0].min(latitude);
        result[1] = result[1].min(longitude);
        result[2] = result[2].max(latitude);
        result[3] = result[3].max(longitude);
    }
    Ok(result)
}

pub(crate) fn clipped_cell_bounds(
    position: Point,
    bounds: ProjectedBounds,
    size_m: f64,
) -> ProjectedBounds {
    let half = size_m / 2.0;
    [
        bounds[0].max(position.x - half),
        bounds[1].max(position.y - half),
        bounds[2].min(position.x + half),
        bounds[3].min(position.y + half),
    ]
}

/// Clipped cell outline in Leaflet [latitude, longitude] order.
pub fn cell_footprint(
    cell: &CoverageCell,
    bounds: ProjectedBounds,
    size_m: f64,
    transform: &Transformer,
) -> Result<Option<[[f64; 2]; 5]>, CoverageError> {
    if cell.state == CoverageState::OutsideArea {
        return Ok(None);
    }
    validate_bounds(bounds)?;
    if !size_m.is_finite() || size_m <= 0.0 {
        return Err(invalid("Cell spacing must be finite and positive"));
    }
    let [west, south, east, north] = clipped_cell_bounds(cell.position(), bounds, size_m);
    let corners = [
        (west, south),
        (east, south),
        (east, north),
        (west, north),
        (west, south),
    ];
    let mut footprint = [[0.0; 2]; 5];
    for (corner, (x, y)) in footprint.iter_mut().zip(corners) {
        let LonLat {
            latitude,
            longitude,
        } = transform.reverse(Point { x, y })?;
        *corner = [latitude, longitude];
    }
    Ok(Some(footprint))
}

pub(crate) fn make_grid(
    terrain: &(impl TerrainSource + ?Sized),
    transform: &Transformer,
    area: &CoverageArea,
    settings: &CoverageSettings,
    source_count: usize,
    cancelled: &dyn Fn() -> bool,
) -> Result<(CoverageGrid, Vec<CoverageCell>), CoverageError> {
    settings.validate()?;
    if source_count == 0 {
        return Err(invalid(
            "Select at least one mesh router to calculate coverage",
        ));
    }
    if source_count > settings.maximum_sources {
        return Err(invalid(format!(
            "Select no more than {} sources for coverage",
            settings.maximum_sources
        )));
    }
    let limit = settings
        .maximum_cells
        .min(settings.maximum_evaluations / source_count);
    if limit == 0 {
        return Err(invalid("Coverage settings allow no source evaluations"));
    }
    let [left, bottom, right, top] = area.bounds;
    let width = right - left;
    let height = top - bottom;
    let mut size = settings.cell_size_m;
    let (rows, columns) = loop {
        let rows = (height / size).ceil().max(1.0);
        let columns = (width / size).ceil().max(1.0);
        let count = rows * columns;
        if count <= limit as f64 {
            break (rows as usize, columns as usize);
        }
        size = if count.is_finite() {
            size * (count / limit as f64).sqrt() * 1.001
        } else {
            // A subnormal requested spacing can overflow the ratio before coarsening.
            width.max(height) / limit as f64
        };
        if !size.is_finite() || size <= 0.0 {
            return Err(invalid("Could not derive a finite coverage grid spacing"));
        }
    };
    let mut cells = Vec::with_capacity(rows * columns);
    let mut inside = 0;
    for row in 0..rows {
        if cancelled() {
            return Err(CoverageError::Cancelled);
        }
        for column in 0..columns {
            let x = left + (column as f64 + 0.5) * size;
            let y = top - (row as f64 + 0.5) * size;
            let position = Point { x, y };
            let LonLat {
                latitude,
                longitude,
            } = transform.reverse(position)?;
            let state = if area.includes(position) {
                inside += 1;
                CoverageState::NotEvaluated
            } else {
                CoverageState::OutsideArea
            };
            cells.push(CoverageCell {
                index: cells.len(),
                x,
                y,
                latitude,
                longitude,
                state,
                ..CoverageCell::default()
            });
        }
    }
    Ok((
        CoverageGrid {
            bounds: area.bounds,
            requested_cell_size_m: settings.cell_size_m,
            effective_cell_size_m: size,
            rows,
            columns,
            requested_cells: rows * columns,
            inside_area_cells: inside,
            outside_area_cells: rows * columns - inside,
            surface_available: terrain.has_surface(),
            ..CoverageGrid::default()
        },
        cells,
    ))
}
