//! Terrain sampling and RF path geometry.

use crate::{earth_bulge_m, first_fresnel_radius_m, require_positive, Point, RfError, Site};

/// Raster layer sampled along the path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerrainLayer {
    GroundDtm,
    SurfaceDom,
}

/// A terrain backend for batch sampling in projected coordinates.
pub trait TerrainSource {
    /// Native horizontal raster resolution in metres.
    fn resolution_m(&self) -> f64;

    /// Whether the terrain source has a surface model.
    fn has_surface(&self) -> bool;

    /// Return one elevation per input point in order, using nonfinite values
    /// for missing pixels. Missing ground pixels invalidate the profile.
    /// Missing surface pixels fall back to their ground elevations.
    fn sample(&self, points: &[Point], layer: TerrainLayer) -> Result<Vec<f64>, RfError>;
}

/// Sampled terrain and radio geometry with a shared index across all vectors.
#[derive(Clone, Debug, PartialEq)]
pub struct TerrainProfile {
    pub distances_m: Vec<f64>,
    pub points: Vec<Point>,
    pub dtm_elevation_m: Vec<f64>,
    pub surface_elevation_m: Vec<f64>,
    pub earth_bulge_m: Vec<f64>,
    pub effective_obstruction_m: Vec<f64>,
    pub los_elevation_m: Vec<f64>,
    pub fresnel_radius_m: Vec<f64>,
    pub clearance_m: Vec<f64>,
    /// Line-of-sight clearance divided by the first Fresnel radius.
    pub fresnel_clearance_ratio: Vec<f64>,
    pub surface_available: bool,
}

/// Sample the endpoint chord and calculate effective Earth curvature and Fresnel clearance.
/// Uses at least three samples including both endpoints. The nominal sampling step
/// defaults to terrain resolution and is clamped to at least that resolution.
pub fn sample_profile<T: TerrainSource + ?Sized>(
    terrain: &T,
    source: &Site,
    target: &Site,
    frequency_mhz: f64,
    k_factor: f64,
    sample_step_m: Option<f64>,
) -> Result<TerrainProfile, RfError> {
    let distance = source.distance_to(target);
    require_positive(
        distance,
        "Link endpoints must have finite, distinct coordinates",
    )?;
    require_positive(frequency_mhz, "Frequency must be positive")?;
    require_positive(k_factor, "K-factor must be positive")?;
    let tx_elevation = source.antenna_absolute_elevation_m();
    let rx_elevation = target.antenna_absolute_elevation_m();
    if !tx_elevation.is_finite() || !rx_elevation.is_finite() {
        return Err(RfError::InvalidInput("Antenna elevations must be finite"));
    }
    let resolution = terrain.resolution_m();
    require_positive(resolution, "Terrain resolution must be positive")?;
    let requested_step = sample_step_m.unwrap_or(resolution);
    if !requested_step.is_finite() {
        return Err(RfError::InvalidInput("Sample step must be finite"));
    }
    let step = requested_step.max(resolution);
    let count = ((distance / step).ceil() + 1.0).max(3.0);
    if count >= usize::MAX as f64 {
        return Err(RfError::InvalidInput("Link profile has too many samples"));
    }
    let count = count as usize;
    let mut points = Vec::new();
    points
        .try_reserve_exact(count)
        .map_err(|_| RfError::InvalidInput("Link profile has too many samples"))?;
    let mut distances_m = Vec::with_capacity(count);
    let spacing = distance / (count - 1) as f64;
    for index in 0..count {
        let d = if index + 1 == count {
            distance
        } else {
            index as f64 * spacing
        };
        let fraction = d / distance;
        distances_m.push(d);
        points.push(Point {
            x: source.position.x + fraction * (target.position.x - source.position.x),
            y: source.position.y + fraction * (target.position.y - source.position.y),
        });
    }

    let dtm_elevation_m = sample_layer(terrain, &points, TerrainLayer::GroundDtm)?;
    if dtm_elevation_m
        .iter()
        .any(|elevation| !elevation.is_finite())
    {
        return Err(RfError::MissingTerrain);
    }
    let surface_available = terrain.has_surface();
    let mut surface_elevation_m = if surface_available {
        sample_layer(terrain, &points, TerrainLayer::SurfaceDom)?
    } else {
        dtm_elevation_m.clone()
    };
    for (surface, &ground) in surface_elevation_m.iter_mut().zip(&dtm_elevation_m) {
        if !surface.is_finite() {
            *surface = ground;
        }
    }
    let mut profile = TerrainProfile {
        distances_m,
        points,
        dtm_elevation_m,
        surface_elevation_m,
        earth_bulge_m: Vec::with_capacity(count),
        effective_obstruction_m: Vec::with_capacity(count),
        los_elevation_m: Vec::with_capacity(count),
        fresnel_radius_m: Vec::with_capacity(count),
        clearance_m: Vec::with_capacity(count),
        fresnel_clearance_ratio: Vec::with_capacity(count),
        surface_available,
    };
    for (&d, &surface) in profile.distances_m.iter().zip(&profile.surface_elevation_m) {
        let bulge = earth_bulge_m(d, distance, k_factor)?;
        let obstruction = surface + bulge;
        let los = tx_elevation + (d / distance) * (rx_elevation - tx_elevation);
        let radius = first_fresnel_radius_m(d, distance - d, frequency_mhz)?;
        let clearance = los - obstruction;
        let ratio = if radius > 1e-9 {
            clearance / radius
        } else {
            f64::INFINITY
        };
        profile.earth_bulge_m.push(bulge);
        profile.effective_obstruction_m.push(obstruction);
        profile.los_elevation_m.push(los);
        profile.fresnel_radius_m.push(radius);
        profile.clearance_m.push(clearance);
        profile.fresnel_clearance_ratio.push(ratio);
    }
    Ok(profile)
}

fn sample_layer<T: TerrainSource + ?Sized>(
    terrain: &T,
    points: &[Point],
    layer: TerrainLayer,
) -> Result<Vec<f64>, RfError> {
    let elevations = terrain.sample(points, layer)?;
    if elevations.len() != points.len() {
        return Err(RfError::TerrainSampleCount {
            expected: points.len(),
            actual: elevations.len(),
        });
    }
    Ok(elevations)
}
