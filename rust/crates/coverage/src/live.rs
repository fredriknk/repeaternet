use std::{f64::consts::TAU, thread};

use repeaternet_rf_core::{
    bullington_edge_loss_db, calibration::step_allowance_db, free_space_path_loss_db,
    p2108_terminal_correction_db, received_power_dbm, spherical_earth_loss_db, AntennaPattern,
    DiffractionModel, ObstacleSurface, Point, RadioBudget, RfError, RfSettings, Site,
    TerminalClutter, ValidationMode, EARTH_MEAN_RADIUS_M, SPEED_OF_LIGHT_M_S,
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
    propagation: LivePropagation,
}

/// Propagation model and constant loss terms used by the sweep.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct LivePropagation {
    pub diffraction_model: DiffractionModel,
    pub diffraction_model_configured: DiffractionModel,
    pub model_correction_db: f64,
    pub step_allowance_db: f64,
    pub terminal_clutter_db: f64,
}

impl PolarMargins {
    pub fn propagation(&self) -> LivePropagation {
        self.propagation
    }

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

struct SmoothEarth {
    raw_hull: UpperHull,
    max_slope: f64,
    sum: f64,
    moment: f64,
}

impl Default for SmoothEarth {
    fn default() -> Self {
        Self {
            raw_hull: UpperHull::default(),
            max_slope: f64::NEG_INFINITY,
            sum: 0.0,
            moment: 0.0,
        }
    }
}

impl SmoothEarth {
    fn push(&mut self, index: usize, point: Point, slope: f64) {
        // The raw hull and prefix sums use the transmit antenna as the datum.
        self.raw_hull.push(point);
        self.max_slope = self.max_slope.max(slope);
        self.sum += point.y;
        self.moment += index as f64 * point.y;
    }

    fn antenna_heights(
        &self,
        [ground_tx, ground_rx]: [f64; 2],
        receiver: Point,
        direct: f64,
        index: usize,
        sample: &RaySample,
    ) -> Option<[f64; 2]> {
        // For a uniform grid, the trapezoid coefficients of each interior
        // elevation in v1 / step and v2 / step^2 sum to 2 and 6*i.
        let j = index as f64;
        let v1 = ground_tx + ground_rx + 2.0 * self.sum;
        let v2 = ground_tx + (3.0 * j - 1.0) * ground_rx + 6.0 * self.moment;
        let mut st = (2.0 * v1 * j - v2) / (j * j);
        let mut sr = (v2 - v1 * j) / (j * j);
        let heights = |st: f64, sr: f64| [-st.min(ground_tx), receiver.y - sr.min(ground_rx)];
        let at = self.max_slope - direct;
        if at > 0.0 {
            // Obstruction adjustments only lower the fitted terrain. If the
            // unadjusted heights suffice, skip both hull queries.
            if sample.smooth_path_clear(heights(st, sr)) {
                return None;
            }
            let highest = self.raw_hull.support(direct).max(0.0);
            let ar = (self.raw_hull.tangent(receiver) + direct).max(0.0);
            st -= highest * at / (at + ar);
            sr -= highest * ar / (at + ar);
        }
        let adjusted = heights(st, sr);
        (!sample.smooth_path_clear(adjusted)).then_some(adjusted)
    }
}

struct Sweep<'a> {
    site: &'a Site,
    rf: &'a RfSettings,
    client: &'a RadioBudget,
    mode: LiveMode,
    step: f64,
    samples: Vec<RaySample>,
    ed: f64,
    wavelength: f64,
    ht: f64,
    sensitivity: f64,
    site_pattern: &'a AntennaPattern,
    directional_gain: bool,
    propagation: LivePropagation,
    clear_prefix: usize,
}

struct RaySample {
    distance: f64,
    curvature: f64,
    loss_db: f64,
    clear_height_sq: f64,
}

impl RaySample {
    fn smooth_path_clear(&self, [ht, hr]: [f64; 2]) -> bool {
        // With t = x/d, chord height / sqrt(t(1-t)) is at least 2*sqrt(ht*hr).
        // The curvature term in the same units is at most d^2/(2*ED).
        ht >= 0.0 && hr >= 0.0 && ht * hr >= self.clear_height_sq
    }
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
    terrain_resolution_m: f64,
) -> Result<PolarMargins, CoverageError> {
    let sweep = Sweep::new(site, rf, client, mode, grid, terrain_resolution_m)?;
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
        propagation: sweep.propagation,
    })
}

impl<'a> Sweep<'a> {
    fn new(
        site: &'a Site,
        rf: &'a RfSettings,
        client: &'a RadioBudget,
        mode: LiveMode,
        grid: SweepGrid,
        terrain_resolution_m: f64,
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
        if !terrain_resolution_m.is_finite() || terrain_resolution_m <= 0.0 {
            return Err(crate::invalid("Terrain resolution must be positive"));
        }
        let diffraction_model = match rf.diffraction_model {
            DiffractionModel::Deygout3 => DiffractionModel::DeltaBullington,
            model => model,
        };
        let terminal_clutter_db = match (site.terminal_clutter, site.clutter_height_m) {
            (TerminalClutter::P2108Dom, Some(height)) => {
                p2108_terminal_correction_db(rf.frequency_mhz, ht - site.ground_elevation_m, height)
            }
            _ => 0.0,
        };
        let propagation = LivePropagation {
            diffraction_model,
            diffraction_model_configured: rf.diffraction_model,
            model_correction_db: rf.model_correction_db,
            step_allowance_db: step_allowance_db(grid.step_m, terrain_resolution_m),
            terminal_clutter_db,
        };
        let constant_loss = propagation.model_correction_db
            + propagation.step_allowance_db
            + propagation.terminal_clutter_db;
        let ed = 2.0 * EARTH_MEAN_RADIUS_M * rf.k_factor;
        let wavelength = SPEED_OF_LIGHT_M_S / (rf.frequency_mhz * 1e6);
        let samples: Vec<_> = (1..=grid.samples_per_ray)
            .map(|k| {
                let distance = k as f64 * grid.step_m;
                let curvature = distance / ed;
                let loss_db = free_space_path_loss_db(distance, rf.frequency_mhz)? + constant_loss;
                // P.526 clearance criterion with the normalized Fresnel radius sqrt(t(1-t)).
                let fresnel_clearance = 0.552 * (wavelength * distance).sqrt();
                let clear_height = (distance * curvature + 2.0 * fresnel_clearance) / 4.0;
                Ok::<_, RfError>(RaySample {
                    distance,
                    curvature,
                    loss_db,
                    clear_height_sq: clear_height.powi(2),
                })
            })
            .collect::<Result<_, _>>()?;
        let clear_prefix = if diffraction_model == DiffractionModel::DeltaBullington {
            let minimum_height =
                (ht - site.ground_elevation_m).max(0.0) * client.antenna.height_agl_m.max(0.0);
            samples.partition_point(|sample| sample.clear_height_sq <= minimum_height)
        } else {
            samples.len()
        };
        let site_pattern = &rf.antenna_settings(site.kind).pattern;
        Ok(Self {
            site,
            rf,
            client,
            mode,
            step: grid.step_m,
            samples,
            ed,
            wavelength,
            ht,
            sensitivity: rf.effective_sensitivity_dbm()?,
            site_pattern,
            directional_gain: matches!(site_pattern, AntennaPattern::Elevation(_))
                || matches!(client.antenna.pattern, AntennaPattern::Elevation(_)),
            propagation,
            clear_prefix,
        })
    }

    fn ray(&self, terrain: &impl ElevationLookup, angle: f64, mut accept: impl FnMut(usize, f64)) {
        let (sin, cos) = angle.sin_cos();
        let mut hull = UpperHull::default();
        let mut obstructions = Vec::with_capacity(self.samples.len());
        let mut smooth = SmoothEarth::default();
        let needs_delta = self.clear_prefix < self.samples.len();
        let mut max_slope = f64::NEG_INFINITY;
        let use_surface = self.rf.obstacle_surface == ObstacleSurface::Surface;
        let has_surface = (use_surface || self.rf.clutter_loss_enabled) && terrain.has_surface();
        let clutter_enabled = self.rf.clutter_loss_enabled && has_surface;
        let strict = self.rf.validation_mode == ValidationMode::StrictLos;
        let mut clutter = self
            .site
            .surface_elevation_m
            .filter(|z| z.is_finite())
            .map_or(0.0, |z| (z - self.site.ground_elevation_m).max(0.0));
        for (index, sample) in self.samples.iter().enumerate() {
            let j = index + 1;
            let d = sample.distance;
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
            let obstacle = if use_surface { surface } else { ground };
            let hr = ground + self.client.antenna.height_agl_m;
            let curvature = sample.curvature;
            let direct = (hr - self.ht) / d;
            // Every interior transmit slope gains the same D/ED term.
            let slope_tx = max_slope + curvature;
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
                    self.transhorizon_loss(d, index, direct, slope_tx, slope_rx)
                };
                let delta = if index >= self.clear_prefix && j > 1 {
                    self.delta_correction(&smooth, sample, j, ground, hr, direct)
                } else {
                    0.0
                };
                self.margin(d, hr, sample.loss_db + diffraction + delta + clutter_loss)
            };
            accept(j, margin);

            hull.push(Point {
                x: d,
                y: obstacle - d * curvature,
            });
            obstructions.push(obstacle);
            let raw_height = obstacle - self.ht;
            let raw_slope = raw_height / d;
            max_slope = max_slope.max(raw_slope - curvature);
            if needs_delta {
                smooth.push(
                    j,
                    Point {
                        x: d,
                        y: raw_height,
                    },
                    raw_slope,
                );
            }
        }
    }

    fn transhorizon_loss(&self, d: f64, last: usize, direct: f64, tx: f64, rx: f64) -> f64 {
        let edge = if tx + rx > 0.0 {
            (direct + rx) * d / (tx + rx)
        } else {
            d / 2.0
        }
        .clamp(self.step, self.samples[last - 1].distance);
        let v = (tx - direct) * edge * (2.0 * d / (self.wavelength * edge * (d - edge))).sqrt();
        bullington_edge_loss_db(v, d)
    }

    fn delta_correction(
        &self,
        smooth: &SmoothEarth,
        sample: &RaySample,
        j: usize,
        ground: f64,
        hr: f64,
        direct: f64,
    ) -> f64 {
        let Some([ht, hr]) = smooth.antenna_heights(
            [self.site.ground_elevation_m - self.ht, ground - self.ht],
            Point {
                x: sample.distance,
                y: hr - self.ht,
            },
            direct,
            j,
            sample,
        ) else {
            return 0.0;
        };
        let spherical =
            spherical_earth_loss_db(sample.distance, [ht, hr], self.ed / 2.0, self.wavelength);
        if spherical <= 0.0 {
            return 0.0;
        }
        (spherical - self.smooth_bullington(sample, j, ht, hr)).max(0.0)
    }

    fn smooth_bullington(&self, sample: &RaySample, j: usize, ht: f64, hr: f64) -> f64 {
        let d = sample.distance;
        let tangent = |height: f64| {
            // Slope = (d-x)/ED - height/x. Its continuous maximum is at sqrt(ED*height).
            let index = ((self.ed * height.max(0.0)).sqrt() / self.step) as usize;
            let slope = |index: usize| {
                let x = self.samples[index.clamp(1, j - 1) - 1].distance;
                (d - x) / self.ed - height / x
            };
            slope(index).max(slope(index + 1))
        };
        let direct = (hr - ht) / d;
        let tx = tangent(ht);
        if tx >= direct {
            return self.transhorizon_loss(d, j - 1, direct, tx, tangent(hr));
        }

        // For LOS the clearance ratio is nonnegative at each sample. Its sublevel
        // sets are intervals: chord - bulge is convex and the Fresnel radius is
        // concave. Adjacent comparisons therefore find the discrete minimum.
        let clearance = |index: usize| {
            let x = self.samples[index - 1].distance;
            let q = x * (d - x);
            (ht + direct * x - q / self.ed) / q.sqrt()
        };
        let (mut lo, mut hi) = (1, j - 1);
        while lo < hi {
            let mid = (lo + hi) / 2;
            if clearance(mid) > clearance(mid + 1) {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        bullington_edge_loss_db(-clearance(lo) * (2.0 * d / self.wavelength).sqrt(), d)
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
