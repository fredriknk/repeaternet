use std::{
    f64::consts::{PI, TAU},
    sync::{LazyLock, Mutex},
    thread,
};

use repeaternet_rf_core::{
    bullington_edge_loss_db, calibration::step_allowance_db, free_space_path_loss_db,
    knife_edge_loss_db, p2108_terminal_correction_db, received_power_dbm, spherical_earth_loss_db,
    AntennaPattern, DiffractionModel, ObstacleSurface, Point, RadioBudget, RfError, RfSettings,
    Site, TerminalClutter, ValidationMode, EARTH_MEAN_RADIUS_M, SPEED_OF_LIGHT_M_S,
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

    /// Interpolated usable margin at a projected point. Outside the sweep
    /// radius there is no prediction.
    pub fn margin_at_point(&self, point: Point) -> f64 {
        let dx = point.x - self.site.x;
        let dy = point.y - self.site.y;
        let radius = dx.hypot(dy);
        if radius > self.grid.radius_m {
            f64::NAN
        } else {
            self.margin_at(dx, dy, radius)
        }
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

    fn raw_tangent(&self, receiver: Point) -> f64 {
        let mut lo = 0;
        let mut hi = self.0.len() - 1;
        while lo < hi {
            let mid = (lo + hi) / 2;
            let a = self.0[mid];
            let b = self.0[mid + 1];
            // Both denominators are positive for interior samples.
            if (a.y - receiver.y) * (receiver.x - b.x) < (b.y - receiver.y) * (receiver.x - a.x) {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        let p = self.0[lo];
        (p.y - receiver.y) / (receiver.x - p.x)
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
        let inverse_j_squared = (j * j).recip();
        let mut st = (2.0 * v1 * j - v2) * inverse_j_squared;
        let mut sr = (v2 - v1 * j) * inverse_j_squared;
        let heights = |st: f64, sr: f64| [-st.min(ground_tx), receiver.y - sr.min(ground_rx)];
        let at = self.max_slope - direct;
        if at > 0.0 {
            // Obstruction adjustments only lower the fitted terrain. If the
            // unadjusted heights suffice, skip both hull queries.
            if sample.smooth_path_clear(heights(st, sr)) {
                return None;
            }
            let highest = self.raw_hull.support(direct).max(0.0);
            let ar = (self.raw_hull.raw_tangent(receiver) + direct).max(0.0);
            let adjustment = highest / (at + ar);
            st -= adjustment * at;
            sr -= adjustment * ar;
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
    spherical: Option<SphericalLoss>,
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

struct SphericalLoss {
    radius: f64,
    wavelength: f64,
    horizon: f64,
    distance_scale: f64,
    height_scale: f64,
    admittance: f64,
    fixed_height_scale: f64,
    fixed_admittance: f64,
    tables: &'static DeltaTables,
    samples: Vec<SphericalSample>,
}

struct SphericalSample {
    fixed_distance_loss: f64,
    marginal_radius_scale: f64,
    curvature: f64,
    fresnel: f64,
    edge_weight: f64,
    index_scale: f64,
}

// Uniform mantissa intervals (64 per octave) keep the relative spacing
// small for both distance loss and height gain, including near zero.
struct DiffractionTable {
    first: u64,
    values: Vec<f64>,
}

impl DiffractionTable {
    const SHIFT: u32 = 52 - 6;

    fn new(first: f64, last: f64, compute: fn(f64) -> f64) -> Self {
        let first = first.to_bits() >> Self::SHIFT;
        let last = last.to_bits() >> Self::SHIFT;
        let values = (first..=last)
            .map(|index| compute(f64::from_bits(index << Self::SHIFT)))
            .collect();
        Self { first, values }
    }

    fn get(&self, value: f64, exact: fn(f64) -> f64) -> f64 {
        let bits = value.to_bits();
        let index = (bits >> Self::SHIFT).wrapping_sub(self.first) as usize;
        if index >= self.values.len() - 1 {
            return exact(value);
        }
        let fraction = (bits & ((1 << Self::SHIFT) - 1)) as f64 / (1_u64 << Self::SHIFT) as f64;
        let lower = self.values[index];
        lower + fraction * (self.values[index + 1] - lower)
    }
}

struct DeltaTables {
    distance: DiffractionTable,
    low_gain: DiffractionTable,
    high_gain: DiffractionTable,
    radius: DiffractionTable,
    clearance_root: [f64; 1025],
    smooth_edge: [(f64, f64); 129],
}

impl DeltaTables {
    const MIN_EDGE: f64 = -0.78;
    const EDGE_SCALE: f64 = 128.0 / -Self::MIN_EDGE;
    const ROOT_LIMIT: f64 = 0.12;
    const ROOT_SCALE: f64 = 1024.0 / Self::ROOT_LIMIT;

    fn clearance_root(&self, p: f64, initial: f64) -> Option<f64> {
        // Factor b = initial * r out of the cubic. Then r - t*r³ = 1
        // depends on t alone. Stay away from the repeated root at t = 4/27.
        let position = (p * initial.powi(2)) * Self::ROOT_SCALE;
        if position >= 1024.0 || !position.is_finite() {
            return None;
        }
        let index = position as usize;
        let fraction = position - index as f64;
        let lower = self.clearance_root[index];
        let r = lower + fraction * (self.clearance_root[index + 1] - lower);
        Some(initial * r)
    }

    fn smooth_edge_loss(&self, v: f64, distance: f64) -> f64 {
        if v <= Self::MIN_EDGE {
            return 0.0;
        }
        if v >= 0.0 {
            return bullington_edge_loss_db(v, distance);
        }
        let position = (v - Self::MIN_EDGE) * Self::EDGE_SCALE;
        let index = (position as usize).min(self.smooth_edge.len() - 2);
        let fraction = position - index as f64;
        let (knife, excess) = self.smooth_edge[index];
        let (next_knife, next_excess) = self.smooth_edge[index + 1];
        (knife + fraction * (next_knife - knife))
            + (excess + fraction * (next_excess - excess)) * (10.0 + 0.02 * distance / 1000.0)
    }

    fn height_gain(&self, b: f64, admittance: f64) -> f64 {
        if b <= 2.0 {
            let amplitude = (b + 0.1 * b.powi(3)).max(admittance);
            return self.low_gain.get(amplitude, Self::low_gain);
        }
        let gain = self.high_gain.get(b, Self::high_gain);
        // The high branch is increasing and starts above 20 log10(2).
        if admittance < 2.0 {
            gain
        } else {
            gain.max(Self::low_gain(admittance))
        }
    }

    fn low_gain(amplitude: f64) -> f64 {
        20.0 * amplitude.log10()
    }

    fn high_gain(b: f64) -> f64 {
        let x = b - 1.1;
        17.6 * x.sqrt() - 5.0 * x.log10() - 8.0
    }
}

static DELTA_TABLES: LazyLock<DeltaTables> = LazyLock::new(|| DeltaTables {
    distance: DiffractionTable::new(1.0 / 64.0, 64.0, SphericalLoss::distance_loss),
    low_gain: DiffractionTable::new(1.0 / 1024.0, 4.0, DeltaTables::low_gain),
    high_gain: DiffractionTable::new(2.0, 256.0, DeltaTables::high_gain),
    radius: DiffractionTable::new(1.0 / 64.0, 4096.0, f64::cbrt),
    clearance_root: std::array::from_fn(|index| {
        let t = index as f64 / DeltaTables::ROOT_SCALE;
        let mut r = 1.0_f64;
        for _ in 0..6 {
            let tr2 = t * r.powi(2);
            r += (1.0 - r * (1.0 - tr2)) / (1.0 - 3.0 * tr2);
        }
        r
    }),
    smooth_edge: std::array::from_fn(|index| {
        // Sample the right-hand limit. The cut-off itself is handled exactly.
        let v = (DeltaTables::MIN_EDGE + index as f64 / DeltaTables::EDGE_SCALE)
            .max(DeltaTables::MIN_EDGE.next_up());
        let knife = knife_edge_loss_db(v);
        (knife, -(-knife / 6.0).exp_m1())
    }),
});

impl SphericalLoss {
    fn new(samples: &[RaySample], radius: f64, wavelength: f64) -> Self {
        // P.526 vertical polarization over land, beta = 1. Factor the
        // inverse cube root of the radius out of X, B and the gain floor.
        let q = 60.0 * wavelength * 0.003;
        let admittance = (2.0 * PI / wavelength).powf(-1.0 / 3.0)
            * (21.0_f64.powi(2) + q * q).powf(-0.25)
            * 22.0_f64.hypot(q);
        let distance_scale = (PI / wavelength).cbrt();
        let height_scale = 2.0 * (PI / wavelength).powf(2.0 / 3.0);
        // Include the additive 2 dB in the linear gain floor amplitude.
        let admittance = admittance * 10.0_f64.powf(0.1);
        let fixed_distance_scale = (PI / (wavelength * radius * radius)).cbrt();
        Self {
            radius,
            wavelength,
            horizon: (2.0 * radius).sqrt(),
            distance_scale,
            height_scale,
            admittance,
            fixed_height_scale: 2.0 * (PI.powi(2) / (wavelength.powi(2) * radius)).cbrt(),
            fixed_admittance: admittance / radius.cbrt(),
            tables: &DELTA_TABLES,
            samples: samples
                .iter()
                .enumerate()
                .map(|(index, sample)| {
                    let d = sample.distance;
                    SphericalSample {
                        fixed_distance_loss: Self::distance_loss(fixed_distance_scale * d),
                        marginal_radius_scale: (2.0 / d.powi(2)).cbrt(),
                        curvature: d.powi(2) / (4.0 * radius),
                        fresnel: 0.552 * (d * wavelength).sqrt(),
                        edge_weight: (2.0 / (d * wavelength)).sqrt(),
                        index_scale: 1.0 / (index + 1) as f64,
                    }
                })
                .collect(),
        }
    }

    fn distance_loss(x: f64) -> f64 {
        if x >= 1.6 {
            17.6 * x - 10.0 * x.log10() - 11.0
        } else {
            20.0 * x.log10() + 5.6488 * x.powf(1.425)
        }
    }

    fn loss(&self, index: usize, d: f64, heights: [f64; 2]) -> (f64, Option<(f64, f64)>) {
        let [ht, hr] = heights;
        if !(0.0..=1000.0).contains(&ht) || !(0.0..=1000.0).contains(&hr) {
            return (
                spherical_earth_loss_db(d, heights, self.radius, self.wavelength),
                None,
            );
        }
        let sample = &self.samples[index];
        let root_heights = ht.sqrt() + hr.sqrt();
        if d >= self.horizon * root_heights {
            let gain = |h| {
                self.tables
                    .height_gain(self.fixed_height_scale * h, self.fixed_admittance)
            };
            return (sample.fixed_distance_loss - gain(ht) - gain(hr), None);
        }
        // The minimum Fresnel clearance is at x = d(1+b)/2, with b
        // the central root of (1+m)b - mb³ = (ht-hr)/(ht+hr).
        let scale = (sample.curvature + ht + hr).recip();
        let p = sample.curvature * scale;
        let initial = (ht - hr) * scale;
        let Some(b) = self.tables.clearance_root(p, initial) else {
            return (
                spherical_earth_loss_db(d, heights, self.radius, self.wavelength),
                None,
            );
        };
        let product = (1.0 - b.powi(2)) * 0.25;
        if product <= 0.01 {
            // Near an endpoint the Fresnel radius becomes ill-conditioned.
            return (
                spherical_earth_loss_db(d, heights, self.radius, self.wavelength),
                None,
            );
        }
        let clearance = (ht + hr + (hr - ht) * b) * 0.5 - 2.0 * sample.curvature * product;
        let required = sample.fresnel * product.sqrt();
        if clearance >= required {
            return (0.0, None);
        }
        let roots_squared = root_heights.powi(2);
        let mut radius_scale =
            sample.marginal_radius_scale * self.tables.radius.get(roots_squared, f64::cbrt);
        let near_gain_boundary = |height| {
            let b = self.height_scale * radius_scale * height;
            // The cube-root interpolation has relative error below 1e-5.
            // Resolve the small jump at B=2 with the exact cube root.
            (1.9999..=2.0001).contains(&b)
        };
        if near_gain_boundary(ht) || near_gain_boundary(hr) {
            radius_scale = sample.marginal_radius_scale * roots_squared.cbrt();
        }
        let x = self.distance_scale * d * radius_scale.powi(2);
        let floor = self.admittance * radius_scale;
        let gain = |h| {
            self.tables
                .height_gain(self.height_scale * radius_scale * h, floor)
        };
        // Keep the P.526 branch change at X=1.6 out of interpolation.
        let distance_loss = if (1.59375..1.609375).contains(&x) {
            Self::distance_loss(x)
        } else {
            self.tables.distance.get(x, Self::distance_loss)
        };
        let first_term = distance_loss - gain(ht) - gain(hr);
        let clearance_ratio = clearance / required;
        let continuous_v = -0.552 * std::f64::consts::SQRT_2 * clearance_ratio;
        (
            (1.0 - clearance_ratio) * first_term.max(0.0),
            Some((b, continuous_v)),
        )
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
    let run_rays = |first: usize, rows: &mut [f32]| {
        for (index, output) in rows.chunks_exact_mut(stride).enumerate() {
            let ray = first + index;
            sweep.ray(terrain, ray as f64 * TAU / grid.rays as f64, |k, margin| {
                output[k] = margin as f32;
            });
            output[0] = output[1];
        }
    };
    // Ray cost varies strongly with terrain (open water or flat rays take the
    // line-of-sight scan), so balance batches dynamically for every model.
    sweep_balanced(&mut margins, stride, run_rays);
    Ok(PolarMargins {
        grid,
        site: site.position,
        margins,
        propagation: sweep.propagation,
    })
}

fn sweep_balanced(output: &mut [f32], stride: usize, work: impl Fn(usize, &mut [f32]) + Sync) {
    const RAYS_PER_BATCH: usize = 8;
    let workers = worker_count(0, output.len() / stride, output.len());
    if workers == 1 {
        work(0, output);
        return;
    }
    // The number of costly near-horizon samples can vary by bearing.
    // Give each worker another strip of rays as it finishes its last one.
    let batches = Mutex::new(output.chunks_mut(stride * RAYS_PER_BATCH).enumerate());
    thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                let batch = batches.lock().unwrap().next();
                let Some((index, rows)) = batch else { break };
                work(index * RAYS_PER_BATCH, rows);
            });
        }
    });
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
        let spherical = (clear_prefix < samples.len()
            && (150.0..=3000.0).contains(&rf.frequency_mhz)
            && (0.5..=4.0 / 3.0).contains(&rf.k_factor)
            && grid.step_m >= 5.0
            && grid.sample_radius_m() <= 80_500.0)
            .then(|| SphericalLoss::new(&samples, ed / 2.0, wavelength));
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
            spherical,
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

        self.smooth_earth_correction(sample, j, ht, hr)
    }

    fn smooth_earth_correction(&self, sample: &RaySample, j: usize, ht: f64, hr: f64) -> f64 {
        let Some(spherical_loss) = &self.spherical else {
            let spherical =
                spherical_earth_loss_db(sample.distance, [ht, hr], self.ed / 2.0, self.wavelength);
            if spherical <= 0.0 {
                return 0.0;
            }
            return (spherical - self.smooth_bullington(sample, j, ht, hr)).max(0.0);
        };
        let (spherical, minimum) = spherical_loss.loss(j - 1, sample.distance, [ht, hr]);
        if spherical <= 0.0 {
            return 0.0;
        }
        let smooth = if let Some((b, continuous_v)) = minimum {
            // At the continuous minimum the first derivative vanishes.
            // For |b| <= 0.75 and j >= 128, the nearest grid point differs
            // in v by < 0.0011 (see the bound below). This costs < 0.029 dB
            // in Bullington loss if the knife-edge cutoff is not crossed.
            //
            // Write the clearance ratio as N(u)/(F*sqrt(1-u²)), where
            // u = 2*x/d - 1, F = 0.552*sqrt(lambda*d), and N'' = 2*M
            // with M = d²/(4*a). The nearest grid point is at most 1/j
            // away in u. The tangent error of sqrt(1-u²) is <= 1.9/j²
            // here, sqrt(1-u²) > 0.65, and the supported grids have
            // M/F < 11. Thus the ratio increases by < 20/j² at that point.
            if j >= 128 && b.abs() <= 0.75 && continuous_v > DeltaTables::MIN_EDGE + 0.0011 {
                return (spherical
                    - spherical_loss
                        .tables
                        .smooth_edge_loss(continuous_v, sample.distance))
                .max(0.0);
            }
            // Fresnel clearance decreases to the continuous minimum and
            // increases afterwards. Its two grid neighbors bracket the
            // discrete minimum, up to the root interpolation error.
            let spherical_sample = &spherical_loss.samples[j - 1];
            let nearer = ((1.0 + b) * 0.5 * j as f64) as usize;
            let a = nearer.clamp(1, j - 1);
            let b = (nearer + 1).clamp(1, j - 1);
            let obstruction = |index: usize| {
                let fraction = index as f64 * spherical_sample.index_scale;
                let product = fraction * (1.0 - fraction);
                let above_chord =
                    2.0 * spherical_sample.curvature * product - (ht + (hr - ht) * fraction);
                above_chord / product.sqrt()
            };
            let v = obstruction(a).max(obstruction(b)) * spherical_sample.edge_weight;
            if (v - DeltaTables::MIN_EDGE).abs() < 1e-4 {
                // Resolve the knife-edge cut-off on the exact discrete grid.
                self.smooth_bullington(sample, j, ht, hr)
            } else {
                spherical_loss.tables.smooth_edge_loss(v, sample.distance)
            }
        } else {
            self.smooth_bullington(sample, j, ht, hr)
        };
        (spherical - smooth).max(0.0)
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
        let clearance = self.smooth_clearance(sample, j, ht, hr);
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

    fn smooth_clearance(
        &self,
        sample: &RaySample,
        j: usize,
        ht: f64,
        hr: f64,
    ) -> impl Fn(usize) -> f64 + '_ {
        let d = sample.distance;
        let direct = (hr - ht) / d;
        move |index: usize| {
            debug_assert!((1..j).contains(&index));
            let x = self.samples[index - 1].distance;
            let q = x * (d - x);
            (ht + direct * x - q / self.ed) / q.sqrt()
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
        let site_gain = self
            .rf
            .site_gain_override(self.site)
            .unwrap_or_else(|| self.site_pattern.gain_at(angle));
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
