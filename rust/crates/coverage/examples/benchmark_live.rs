use std::{hint::black_box, time::Instant};

use repeaternet_coverage::{
    live::{sweep, ElevationLookup, LiveMode, SweepGrid},
    ClientRadioProfile,
};
use repeaternet_rf_core::{DiffractionModel, Point, RfSettings, Site, SiteKind};

const PIXEL_M: f64 = 10.0;
const SIDE: usize = 4017;
const CENTRE: usize = SIDE / 2;
const RUNS: usize = 9;

struct FractalRidges(Vec<f32>);

impl FractalRidges {
    fn new() -> Self {
        let mut seed = 0x7cf9_b321_u32;
        let mut elevations = vec![100.0_f32; SIDE * SIDE];
        for (stride, amplitude) in [
            (512, 245.0),
            (256, 122.0),
            (128, 60.0),
            (64, 24.0),
            (32, 10.0),
            (16, 4.0),
        ] {
            let side = SIDE.div_ceil(stride) + 1;
            let noise: Vec<_> = (0..side * side)
                .map(|_| {
                    seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    (seed >> 8) as f32 / (1 << 24) as f32
                })
                .collect();
            let axis: Vec<_> = (0..SIDE)
                .map(|i| {
                    let t = (i % stride) as f32 / stride as f32;
                    (i / stride, t * t * (3.0 - 2.0 * t))
                })
                .collect();
            let mix = |a: f32, b: f32, t: f32| a + (b - a) * t;
            let (rows, _) = elevations.as_chunks_mut::<SIDE>();
            for (row, &(y, dy)) in rows.iter_mut().zip(&axis) {
                let top = y * side;
                let bottom = top + side;
                for (z, &(x, dx)) in row.iter_mut().zip(&axis) {
                    let a = mix(noise[top + x], noise[top + x + 1], dx);
                    let b = mix(noise[bottom + x], noise[bottom + x + 1], dx);
                    let ridge = 1.0 - (2.0 * mix(a, b, dy) - 1.0).abs();
                    *z += amplitude * ridge;
                }
            }
        }
        Self(elevations)
    }
}

impl ElevationLookup for FractalRidges {
    fn ground(&self, point: Point) -> f64 {
        let x = point.x / PIXEL_M + CENTRE as f64;
        let y = point.y / PIXEL_M + CENTRE as f64;
        let column = x as usize;
        let row = y as usize;
        let i = row * SIDE + column;
        let dx = x - column as f64;
        let dy = y - row as f64;
        let mix = |a: f32, b: f32| f64::from(a) + dx * f64::from(b - a);
        let a = mix(self.0[i], self.0[i + 1]);
        let b = mix(self.0[i + SIDE], self.0[i + SIDE + 1]);
        a + dy * (b - a)
    }

    fn surface(&self, _: Point) -> Option<f64> {
        None
    }

    fn has_surface(&self) -> bool {
        false
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let terrain = FractalRidges::new();
    let site = Site {
        ground_elevation_m: terrain.ground(Point::default()),
        kind: SiteKind::Router,
        ..Site::new("router", 0.0, 0.0)
    };
    let client = ClientRadioProfile::default().as_radio_budget();
    let models = [
        DiffractionModel::Bullington,
        DiffractionModel::DeltaBullington,
        DiffractionModel::Deygout3,
    ];
    println!("Median of {RUNS} runs after one warm-up per grid and model (milliseconds)");
    println!("| Step | Radius | Bullington | Delta-Bullington | Deygout3 via delta | Delta / Bullington |");
    println!("| ---: | ---: | ---: | ---: | ---: | ---: |");
    for (step, radius) in [(30.0, 20_000.0), (120.0, 20_000.0), (10.0, 10_000.0)] {
        let grid = SweepGrid::new(radius, step)?;
        let mut timings = [[0.0_f64; RUNS]; 3];
        for run in 0..=RUNS {
            // Rotate the order so no model always follows the same prior workload.
            for offset in 0..models.len() {
                let index = (run + offset) % models.len();
                let rf = RfSettings {
                    diffraction_model: models[index],
                    ..RfSettings::default()
                };
                let start = Instant::now();
                let polar = sweep(
                    &terrain,
                    &site,
                    &rf,
                    &client,
                    LiveMode::TwoWay,
                    grid,
                    PIXEL_M,
                )?;
                let elapsed = start.elapsed().as_secs_f64() * 1000.0;
                black_box(polar);
                if run != 0 {
                    timings[index][run - 1] = elapsed;
                }
            }
        }
        let [bull, delta, deygout] = timings.map(|mut runs| {
            runs.sort_unstable_by(f64::total_cmp);
            runs[RUNS / 2]
        });
        println!(
            "| {step:.0} m | {:.0} km | {bull:.2} | {delta:.2} | {deygout:.2} | {:.3}x |",
            radius / 1000.0,
            delta / bull
        );
    }
    Ok(())
}
