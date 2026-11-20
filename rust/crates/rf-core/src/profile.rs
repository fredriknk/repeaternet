//! Terrain sampling and RF path geometry.

use crate::{require_positive, Point, RfError, Site, EARTH_MEAN_RADIUS_M, SPEED_OF_LIGHT_M_S};

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
    sample_path(
        terrain,
        source,
        target,
        frequency_mhz,
        k_factor,
        sample_step_m,
    )?
    .into_profile()
}

pub(crate) struct TerrainSamples {
    distances: Vec<f64>,
    points: Vec<Point>,
    ground: Vec<f64>,
    surface: Option<Vec<f64>>,
    geometry: PathGeometry,
}

struct PathGeometry {
    distance: f64,
    tx_elevation: f64,
    rx_elevation: f64,
    earth_diameter: f64,
    wavelength: f64,
}

impl PathGeometry {
    fn at(&self, d: f64, surface: f64) -> Result<[f64; 6], RfError> {
        let remaining = self.distance - d;
        let bulge = d * remaining / self.earth_diameter;
        let obstruction = surface + bulge;
        let los = self.tx_elevation + (d / self.distance) * (self.rx_elevation - self.tx_elevation);
        if !d.is_finite() || d < 0.0 || !remaining.is_finite() || remaining < 0.0 {
            return Err(RfError::InvalidInput(
                "Distances must be finite and nonnegative",
            ));
        }
        let denominator = d + remaining;
        let radius = if denominator == 0.0 {
            0.0
        } else {
            (self.wavelength * d * remaining / denominator).sqrt()
        };
        let clearance = los - obstruction;
        let ratio = if radius > 1e-9 {
            clearance / radius
        } else {
            f64::INFINITY
        };
        Ok([bulge, obstruction, los, radius, clearance, ratio])
    }
}

pub(crate) struct ClearanceMetrics {
    pub clearance: f64,
    pub ratio: f64,
    pub ratio_distance: f64,
    pub maximum_radius: f64,
}

impl ClearanceMetrics {
    fn new() -> Self {
        Self {
            clearance: 0.0,
            ratio: 0.0,
            ratio_distance: 0.0,
            maximum_radius: 0.0,
        }
    }

    fn observe(
        &mut self,
        index: usize,
        last: usize,
        d: f64,
        radius: f64,
        clearance: f64,
        ratio: f64,
    ) {
        self.maximum_radius = self.maximum_radius.max(radius);
        if index > 0 && index < last {
            if index == 1 || clearance < self.clearance {
                self.clearance = clearance;
            }
            if index == 1 || ratio < self.ratio {
                self.ratio = ratio;
                self.ratio_distance = d;
            }
        }
    }
}

pub(crate) struct ScalarProfile {
    pub distances: Vec<f64>,
    pub obstructions: Vec<f64>,
    pub los: Vec<f64>,
    pub metrics: ClearanceMetrics,
}

impl TerrainSamples {
    pub(crate) fn clutter_excess(&self) -> Option<f64> {
        let surface = self.surface.as_ref()?;
        Some(
            surface
                .iter()
                .zip(&self.ground)
                .map(|(surface, ground)| surface - ground)
                .fold(0.0, f64::max),
        )
    }

    pub(crate) fn into_scalar(self) -> Result<ScalarProfile, RfError> {
        // Diffraction needs the obstruction and chord elevations, so keep just
        // those arrays and overwrite the sampled terrain with the obstructions.
        let mut obstructions = self.surface.unwrap_or(self.ground);
        let mut los = Vec::with_capacity(obstructions.len());
        let mut metrics = ClearanceMetrics::new();
        let last = self.distances.len() - 1;
        for (index, (&d, elevation)) in self.distances.iter().zip(&mut obstructions).enumerate() {
            let [_, obstruction, chord, radius, clearance, ratio] =
                self.geometry.at(d, *elevation)?;
            *elevation = obstruction;
            los.push(chord);
            metrics.observe(index, last, d, radius, clearance, ratio);
        }
        Ok(ScalarProfile {
            distances: self.distances,
            obstructions,
            los,
            metrics,
        })
    }

    pub(crate) fn into_profile(self) -> Result<TerrainProfile, RfError> {
        let count = self.distances.len();
        let surface_available = self.surface.is_some();
        let mut profile = TerrainProfile {
            distances_m: self.distances,
            points: self.points,
            surface_elevation_m: self.surface.unwrap_or_else(|| self.ground.clone()),
            dtm_elevation_m: self.ground,
            earth_bulge_m: Vec::with_capacity(count),
            effective_obstruction_m: Vec::with_capacity(count),
            los_elevation_m: Vec::with_capacity(count),
            fresnel_radius_m: Vec::with_capacity(count),
            clearance_m: Vec::with_capacity(count),
            fresnel_clearance_ratio: Vec::with_capacity(count),
            surface_available,
        };
        for (&d, &surface) in profile.distances_m.iter().zip(&profile.surface_elevation_m) {
            let [bulge, obstruction, los, radius, clearance, ratio] =
                self.geometry.at(d, surface)?;
            profile.earth_bulge_m.push(bulge);
            profile.effective_obstruction_m.push(obstruction);
            profile.los_elevation_m.push(los);
            profile.fresnel_radius_m.push(radius);
            profile.clearance_m.push(clearance);
            profile.fresnel_clearance_ratio.push(ratio);
        }
        Ok(profile)
    }
}

impl TerrainProfile {
    pub(crate) fn clearance_metrics(&self) -> ClearanceMetrics {
        let last = self.distances_m.len() - 1;
        let mut metrics = ClearanceMetrics::new();
        for (index, &d) in self.distances_m.iter().enumerate() {
            metrics.observe(
                index,
                last,
                d,
                self.fresnel_radius_m[index],
                self.clearance_m[index],
                self.fresnel_clearance_ratio[index],
            );
        }
        metrics
    }
}

pub(crate) fn sample_path<T: TerrainSource + ?Sized>(
    terrain: &T,
    source: &Site,
    target: &Site,
    frequency_mhz: f64,
    k_factor: f64,
    sample_step_m: Option<f64>,
) -> Result<TerrainSamples, RfError> {
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
    let surface = if terrain.has_surface() {
        let mut surface = sample_layer(terrain, &points, TerrainLayer::SurfaceDom)?;
        for (surface, &ground) in surface.iter_mut().zip(&dtm_elevation_m) {
            if !surface.is_finite() {
                *surface = ground;
            }
        }
        Some(surface)
    } else {
        None
    };
    Ok(TerrainSamples {
        distances: distances_m,
        points,
        ground: dtm_elevation_m,
        surface,
        geometry: PathGeometry {
            distance,
            tx_elevation,
            rx_elevation,
            earth_diameter: 2.0 * EARTH_MEAN_RADIUS_M * k_factor,
            wavelength: SPEED_OF_LIGHT_M_S / (frequency_mhz * 1_000_000.0),
        },
    })
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
