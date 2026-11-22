use std::{f64::consts::TAU, thread};

use repeaternet_rf_core::{
    bullington_edge_loss_db, free_space_path_loss_db, knife_edge_v, received_power_dbm,
    AntennaPattern, Point, RadioBudget, RfError, RfSettings, Site, ValidationMode,
    EARTH_MEAN_RADIUS_M, SPEED_OF_LIGHT_M_S,
};
use repeaternet_terrain::{LonLat, Transformer};
use serde::{Deserialize, Serialize};

use crate::{parallel::worker_count, CoverageError};

/// Elevations in the terrain CRS. Nonfinite ground marks a missing sample.
pub trait ElevationLookup: Sync {
    fn ground(&self, point: Point) -> f64;
    fn surface(&self, point: Point) -> Option<f64>;
    fn has_surface(&self) -> bool;

    /// Lookup both layers, falling back to ground where the surface is missing.
    fn elevations(&self, point: Point) -> (f64, f64) {
        let ground = self.ground(point);
        let surface = self
            .surface(point)
            .filter(|z| z.is_finite())
            .unwrap_or(ground);
        (ground, surface)
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LiveMode {
    #[default]
    TwoWay,
    Downlink,
    Uplink,
}

#[derive(Clone, Copy, Debug)]
pub struct SweepGrid {
    radius_m: f64,
    step_m: f64,
    rays: usize,
    samples_per_ray: usize,
}

impl SweepGrid {
    pub fn new(radius_m: f64, step_m: f64) -> Result<Self, CoverageError> {
        if !radius_m.is_finite() || radius_m <= 0.0 || !step_m.is_finite() || step_m <= 0.0 {
            return Err(crate::invalid(
                "Coverage radius and resolution must be finite and positive",
            ));
        }
        let samples = (radius_m / step_m).ceil();
        if samples >= usize::MAX as f64 / 16_384.0 {
            return Err(crate::invalid("Live coverage grid has too many samples"));
        }
        Ok(Self {
            radius_m,
            step_m,
            rays: ((TAU * radius_m / step_m).ceil().min(16_384.0) as usize)
                .next_multiple_of(8)
                .max(360),
            samples_per_ray: samples.max(1.0) as usize,
        })
    }

    /// The last sample can extend beyond the requested radius.
    pub fn sample_radius_m(self) -> f64 {
        self.samples_per_ray as f64 * self.step_m
    }
}

/// Ray zero points north and the remaining rays run clockwise.
pub struct PolarMargins {
    grid: SweepGrid,
    site: Point,
    margins: Vec<f32>,
}

impl PolarMargins {
    /// k=0 repeats the k=1 margin. NaN indicates unknown terrain.
    pub fn ray(&self, index: usize) -> &[f32] {
        let stride = self.grid.samples_per_ray + 1;
        &self.margins[index * stride..(index + 1) * stride]
    }

    pub fn ray_count(&self) -> usize {
        self.grid.rays
    }

    /// Number of samples beyond the site on each ray.
    pub fn samples_per_ray(&self) -> usize {
        self.grid.samples_per_ray
    }

    fn margin_at(&self, dx: f64, dy: f64, radius: f64) -> f64 {
        let bearing = dx.atan2(dy) * (self.grid.rays as f64 / TAU);
        let first = bearing.floor();
        let ray = (first as isize).rem_euclid(self.grid.rays as isize) as usize;
        let next = (ray + 1) % self.grid.rays;
        let angular = bearing - first;
        let sample = (radius / self.grid.step_m).clamp(1.0, self.grid.samples_per_ray as f64);
        let index = sample as usize;
        let farther = if radius < self.grid.step_m {
            index
        } else {
            (index + 1).min(self.grid.samples_per_ray)
        };
        let radial = sample - index as f64;
        let a = self.ray(ray);
        let b = self.ray(next);
        let corners = [a[index], a[farther], b[index], b[farther]];
        if corners.iter().any(|margin| margin.is_nan()) {
            // Preserve the state of the nearest sample at terrain boundaries.
            return f64::from(
                corners[2 * usize::from(angular >= 0.5) + usize::from(radial >= 0.5)],
            );
        }
        let [a, b, c, d] = corners.map(f64::from);
        mix_margin(mix_margin(a, b, radial), mix_margin(c, d, radial), angular)
    }
}

fn mix_margin(a: f64, b: f64, weight: f64) -> f64 {
    // Avoid 0 * -inf (and keep the exact boundary values).
    if weight == 0.0 {
        a
    } else if weight == 1.0 {
        b
    } else {
        a * (1.0 - weight) + b * weight
    }
}

#[derive(Default)]
struct UpperHull(Vec<Point>);

impl UpperHull {
    fn push(&mut self, point: Point) {
        while let [.., a, b] = self.0.as_slice() {
            if (b.y - a.y) * (point.x - b.x) > (point.y - b.y) * (b.x - a.x) {
                break;
            }
            self.0.pop();
        }
        self.0.push(point);
    }

    // Along the upper hull, each of these objectives increases up to its
    // supporting vertex and then decreases. Compare adjacent vertices.
    fn maximum(&self, value: impl Fn(Point) -> f64) -> f64 {
        let mut lo = 0;
        let mut hi = self.0.len() - 1;
        while lo < hi {
            let mid = (lo + hi) / 2;
            if value(self.0[mid]) < value(self.0[mid + 1]) {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        value(self.0[lo])
    }

    fn support(&self, slope: f64) -> f64 {
        self.maximum(|p| p.y - slope * p.x)
    }

    fn tangent(&self, receiver: Point) -> f64 {
        self.maximum(|p| (p.y - receiver.y) / (receiver.x - p.x))
    }
}

struct Sweep<'a> {
    site: &'a Site,
    rf: &'a RfSettings,
    client: &'a RadioBudget,
    mode: LiveMode,
    step: f64,
    fspl: Vec<f64>,
    ed: f64,
    wavelength: f64,
    ht: f64,
    sensitivity: f64,
    site_pattern: &'a AntennaPattern,
    directional_gain: bool,
}

/// Compute handheld margins on radial paths. StrictLos tests geometric
/// clearance. Live coverage does not apply the Fresnel clearance ratio.
pub fn sweep(
    terrain: &impl ElevationLookup,
    site: &Site,
    rf: &RfSettings,
    client: &RadioBudget,
    mode: LiveMode,
    grid: SweepGrid,
) -> Result<PolarMargins, CoverageError> {
    let sweep = Sweep::new(site, rf, client, mode, grid)?;
    let stride = grid.samples_per_ray + 1;
    let count = grid.rays * stride;
    let mut margins = Vec::new();
    margins
        .try_reserve_exact(count)
        .map_err(|_| crate::invalid("Live coverage grid has too many samples"))?;
    margins.resize(count, f32::NAN);
    parallel_rows(&mut margins, stride, |first, rows| {
        for (index, output) in rows.chunks_exact_mut(stride).enumerate() {
            let ray = first + index;
            sweep.ray(terrain, ray as f64 * TAU / grid.rays as f64, |k, margin| {
                output[k] = margin as f32;
            });
            output[0] = output[1];
        }
    });
    Ok(PolarMargins {
        grid,
        site: site.position,
        margins,
    })
}

impl<'a> Sweep<'a> {
    fn new(
        site: &'a Site,
        rf: &'a RfSettings,
        client: &'a RadioBudget,
        mode: LiveMode,
        grid: SweepGrid,
    ) -> Result<Self, CoverageError> {
        let ht = site.antenna_absolute_elevation_m();
        if !site.ground_elevation_m.is_finite() {
            return Err(RfError::MissingTerrain.into());
        }
        if !ht.is_finite() || !rf.k_factor.is_finite() || rf.k_factor <= 0.0 {
            return Err(crate::invalid(
                "Live coverage requires a finite antenna elevation and positive K-factor",
            ));
        }
        let fspl = (1..=grid.samples_per_ray)
            .map(|k| free_space_path_loss_db(k as f64 * grid.step_m, rf.frequency_mhz))
            .collect::<Result<_, _>>()?;
        let site_pattern = &rf.antenna_settings(site.kind).pattern;
        Ok(Self {
            site,
            rf,
            client,
            mode,
            step: grid.step_m,
            fspl,
            ed: 2.0 * EARTH_MEAN_RADIUS_M * rf.k_factor,
            wavelength: SPEED_OF_LIGHT_M_S / (rf.frequency_mhz * 1e6),
            ht,
            sensitivity: rf.effective_sensitivity_dbm()?,
            site_pattern,
            directional_gain: matches!(site_pattern, AntennaPattern::Elevation(_))
                || matches!(client.antenna.pattern, AntennaPattern::Elevation(_)),
        })
    }

    fn ray(&self, terrain: &impl ElevationLookup, angle: f64, mut accept: impl FnMut(usize, f64)) {
        let (sin, cos) = angle.sin_cos();
        let mut hull = UpperHull::default();
        let mut obstructions = Vec::with_capacity(self.fspl.len());
        let mut max_slope = f64::NEG_INFINITY;
        let has_surface = terrain.has_surface();
        let clutter_enabled = has_surface && self.rf.clutter_loss_enabled;
        let mut clutter = self
            .site
            .surface_elevation_m
            .filter(|z| z.is_finite())
            .map_or(0.0, |z| (z - self.site.ground_elevation_m).max(0.0));
        for (index, &fspl) in self.fspl.iter().enumerate() {
            let j = index + 1;
            let d = j as f64 * self.step;
            let point = Point {
                x: self.site.position.x + d * sin,
                y: self.site.position.y + d * cos,
            };
            let (ground, surface) = if has_surface {
                terrain.elevations(point)
            } else {
                let ground = terrain.ground(point);
                (ground, ground)
            };
            if !ground.is_finite() {
                break;
            }
            let hr = ground + self.client.antenna.height_agl_m;
            let curvature = d / self.ed;
            let direct = (hr - self.ht) / d;
            // Every interior transmit slope gains the same D/ED term.
            let slope_tx = max_slope + curvature;
            let strict = self.rf.validation_mode == ValidationMode::StrictLos;
            let height = if j > 1 && (strict || slope_tx < direct) {
                hull.support(direct - curvature) - self.ht
            } else {
                f64::NEG_INFINITY
            };

            if clutter_enabled {
                clutter = clutter.max(surface - ground);
            }
            let clutter_loss = if clutter_enabled {
                clutter * self.rf.clutter_loss_db_per_m
            } else {
                0.0
            };
            let margin = if strict && height > 0.0 {
                f64::NEG_INFINITY
            } else {
                let diffraction = if j == 1 {
                    0.0
                } else if slope_tx < direct {
                    // The smallest Fresnel weight is at the midpoint. A
                    // negative height times that weight bounds every edge.
                    let min_weight = (8.0 / (self.wavelength * d)).sqrt();
                    if height < 0.0 && height * min_weight <= -0.78 {
                        0.0
                    } else {
                        let v = obstructions
                            .iter()
                            .enumerate()
                            .map(|(i, &z)| {
                                let x = (i + 1) as f64 * self.step;
                                let remaining = d - x;
                                let h = z + x * remaining / self.ed - (self.ht + direct * x);
                                h * (2.0 * d / (self.wavelength * x * remaining)).sqrt()
                            })
                            .fold(f64::NEG_INFINITY, f64::max);
                        bullington_edge_loss_db(v, d)
                    }
                } else {
                    // Transform the receiver to the hull's curvature datum.
                    let slope_rx = hull.tangent(Point {
                        x: d,
                        y: hr - d * curvature,
                    }) - curvature;
                    let denominator = slope_tx + slope_rx;
                    let edge = if denominator > 0.0 {
                        (hr - self.ht + slope_rx * d) / denominator
                    } else {
                        d / 2.0
                    }
                    .clamp(self.step, index as f64 * self.step);
                    let height = (self.ht + slope_tx * edge) - (self.ht + direct * edge);
                    bullington_edge_loss_db(
                        knife_edge_v(height, edge, d - edge, self.rf.frequency_mhz),
                        d,
                    )
                };
                self.margin(d, hr, fspl + diffraction + clutter_loss)
            };
            accept(j, margin);

            hull.push(Point {
                x: d,
                y: surface - d * curvature,
            });
            obstructions.push(surface);
            max_slope = max_slope.max((surface - self.ht) / d - curvature);
        }
    }

    fn margin(&self, d: f64, hr: f64, path_loss: f64) -> f64 {
        let site_antenna = self.rf.antenna_settings(self.site.kind);
        let client_antenna = &self.client.antenna;
        let angle = if self.directional_gain {
            (hr - self.ht).atan2(d).to_degrees()
        } else {
            0.0
        };
        let site_gain = self.site_pattern.gain_at(angle);
        let client_gain = client_antenna.pattern.gain_at(-angle);
        let downlink = || {
            received_power_dbm(
                self.rf.tx_power_dbm,
                site_gain,
                site_antenna.feed_loss_db,
                path_loss,
                client_gain,
                client_antenna.feed_loss_db,
                self.rf.miscellaneous_loss_db,
            ) - self.client.sensitivity_dbm
                - self.rf.fade_margin_db
        };
        let uplink = || {
            received_power_dbm(
                self.client.tx_power_dbm,
                client_gain,
                client_antenna.feed_loss_db,
                path_loss,
                site_gain,
                site_antenna.feed_loss_db,
                self.client.miscellaneous_loss_db,
            ) - self.sensitivity
                - self.rf.fade_margin_db
        };
        match self.mode {
            LiveMode::Downlink => downlink(),
            LiveMode::Uplink => uplink(),
            LiveMode::TwoWay => downlink().min(uplink()),
        }
    }
}

fn parallel_rows<T: Send, R: Send>(
    output: &mut [T],
    stride: usize,
    work: impl Fn(usize, &mut [T]) -> R + Sync + Copy + Send,
) -> Vec<R> {
    let rows = output.len() / stride;
    let workers = worker_count(0, rows, output.len());
    if workers == 1 {
        return vec![work(0, output)];
    }
    let rows_per_worker = rows.div_ceil(workers);
    thread::scope(|scope| {
        let workers: Vec<_> = output
            .chunks_mut(rows_per_worker * stride)
            .enumerate()
            .map(|(i, rows)| scope.spawn(move || work(i * rows_per_worker, rows)))
            .collect();
        workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect()
    })
}

const MERCATOR_RADIUS: f64 = 6_378_137.0;
pub const MARGIN_STEP_DB: f64 = 0.5;

#[derive(Debug, Serialize)]
pub struct MercatorRaster {
    pub width: usize,
    pub height: usize,
    pub origin_x: f64,
    pub origin_y: f64,
    pub pixel_size: f64,
    pub bounds_wgs84: [f64; 4],
    pub unknown_fraction: f64,
    pub covered_fraction: f64,
    #[serde(skip)]
    pub pixels: Vec<u8>,
}

fn mercator_inverse(x: f64, y: f64) -> LonLat {
    LonLat {
        longitude: (x / MERCATOR_RADIUS).to_degrees(),
        latitude: (y / MERCATOR_RADIUS).sinh().atan().to_degrees(),
    }
}

/// Resample the sweeps through a 33 x 33 exact projection control mesh.
pub fn resample(
    polar: &PolarMargins,
    location: LonLat,
    transform: &Transformer,
    floor_db: f64,
) -> Result<MercatorRaster, CoverageError> {
    const SEGMENTS: usize = 32;
    let latitude = location.latitude.to_radians();
    let pixel_size = polar.grid.step_m / latitude.cos();
    let size = (2.0 * polar.grid.radius_m / polar.grid.step_m).ceil() as usize + 1;
    let span = size as f64 * pixel_size;
    let origin_x = MERCATOR_RADIUS * location.longitude.to_radians() - span / 2.0;
    let origin_y = MERCATOR_RADIUS * latitude.tan().asinh() + span / 2.0;
    let nw = mercator_inverse(origin_x, origin_y);
    let se = mercator_inverse(origin_x + span, origin_y - span);
    let bounds_wgs84 = [se.latitude, nw.longitude, nw.latitude, se.longitude];
    let mut mesh = [[Point::default(); SEGMENTS + 1]; SEGMENTS + 1];
    for (row, points) in mesh.iter_mut().enumerate() {
        for (column, point) in points.iter_mut().enumerate() {
            let mut location = mercator_inverse(
                origin_x + span * column as f64 / SEGMENTS as f64,
                origin_y - span * row as f64 / SEGMENTS as f64,
            );
            location.longitude = (location.longitude + 180.0).rem_euclid(360.0) - 180.0;
            let projected = transform.forward(location)?;
            *point = Point {
                x: projected.x - polar.site.x,
                y: projected.y - polar.site.y,
            };
        }
    }
    let count = size
        .checked_mul(size)
        .ok_or_else(|| crate::invalid("Live coverage raster is too large"))?;
    let mut pixels = Vec::new();
    pixels
        .try_reserve_exact(count)
        .map_err(|_| crate::invalid("Live coverage raster is too large"))?;
    pixels.resize(count, 0);
    let mesh_axis: Vec<_> = (0..size)
        .map(|i| {
            let k = (i as f64 + 0.5) * SEGMENTS as f64 / size as f64;
            (k as usize, k.fract())
        })
        .collect();
    let radius_sq = polar.grid.radius_m.powi(2);
    let counts = parallel_rows(&mut pixels, size, |first, pixels| {
        let (mut inside, mut unknown, mut covered) = (0, 0, 0);
        for (row, pixels) in pixels.chunks_exact_mut(size).enumerate() {
            let (r, dy) = mesh_axis[first + row];
            let mut line = [Point::default(); SEGMENTS + 1];
            for (i, point) in line.iter_mut().enumerate() {
                let a = mesh[r][i];
                let b = mesh[r + 1][i];
                *point = Point {
                    x: a.x + (b.x - a.x) * dy,
                    y: a.y + (b.y - a.y) * dy,
                };
            }
            for (pixel, &(c, dx)) in pixels.iter_mut().zip(&mesh_axis) {
                let a = line[c];
                let b = line[c + 1];
                let x = a.x + (b.x - a.x) * dx;
                let y = a.y + (b.y - a.y) * dx;
                let r_sq = x * x + y * y;
                if r_sq <= radius_sq {
                    let margin = polar.margin_at(x, y, r_sq.sqrt());
                    *pixel = quantise(margin, floor_db);
                    inside += 1;
                    unknown += usize::from(margin.is_nan());
                    covered += usize::from(margin >= 0.0);
                }
            }
        }
        (inside, unknown, covered)
    });
    let (inside, unknown, covered) = counts
        .into_iter()
        .fold((0, 0, 0), |a, b| (a.0 + b.0, a.1 + b.1, a.2 + b.2));
    Ok(MercatorRaster {
        width: size,
        height: size,
        origin_x,
        origin_y,
        pixel_size,
        bounds_wgs84,
        unknown_fraction: unknown as f64 / inside.max(1) as f64,
        covered_fraction: covered as f64 / inside.max(1) as f64,
        pixels,
    })
}

fn quantise(margin: f64, floor: f64) -> u8 {
    if margin.is_nan() {
        1
    } else if margin < floor {
        2
    } else {
        (3.0 + ((margin - floor) / MARGIN_STEP_DB).round()) as u8
    }
}

#[cfg(test)]
mod tests;
