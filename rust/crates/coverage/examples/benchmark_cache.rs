//! Local engine benchmark matching the Python tool's 8-source, 1,024-cell flat
//! or GeoTIFF case. Both timed phases stream and hash JSON chunks.
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        OnceLock,
    },
    time::Instant,
};

use repeaternet_coverage::{
    write_cell_chunk, CoverageArea, CoverageEvaluator, CoverageMetricsCache, CoverageSettings,
    CoverageState, RunOptions,
};
use repeaternet_rf_core::{
    Point, RfError, RfSettings, Site, SiteKind, TerrainLayer, TerrainSource,
};
use repeaternet_terrain::{ArrayTerrain, RasterTerrain, TerrainCrs, Transformer};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const SOURCES: usize = 8;
const SIDE: usize = 32;
const CELLS: usize = SIDE * SIDE;

struct Metered<'a> {
    terrain: &'a (dyn TerrainSource + Sync),
    reads: AtomicUsize,
    cancel_at: Option<usize>,
    cancelled_at: OnceLock<Instant>,
}

impl TerrainSource for Metered<'_> {
    fn resolution_m(&self) -> f64 {
        self.terrain.resolution_m()
    }

    fn has_surface(&self) -> bool {
        self.terrain.has_surface()
    }

    fn sample(&self, points: &[Point], layer: TerrainLayer) -> Result<Vec<f64>, RfError> {
        let result = self.terrain.sample(points, layer);
        let reads = self.reads.fetch_add(1, Ordering::Relaxed) + 1;
        if self.cancel_at == Some(reads) {
            let _ = self.cancelled_at.set(Instant::now());
        }
        result
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if let Some(path) = std::env::args_os().nth(1).map(PathBuf::from) {
        let terrain = RasterTerrain::open(&[path], &[])?;
        let [left, bottom, right, top] = terrain.bounds().as_array();
        let width = (right - left).min(top - bottom) - 2.0 * terrain.resolution_m();
        run(
            &terrain,
            terrain.crs(),
            (left + right - width) / 2.0,
            (bottom + top - width) / 2.0,
            width,
            "local_geotiff",
        )
    } else {
        let width = SIDE as f64 * 1_000.0;
        let (left, bottom) = (400_000.0, 6_500_000.0);
        let crs = TerrainCrs::Etrs89Utm33;
        let samples = ((width + 2_000.0) / 200.0) as usize + 1;
        let terrain = ArrayTerrain::new(
            samples,
            samples,
            vec![0.0; samples * samples],
            None,
            Point {
                x: left - 1_000.0,
                y: bottom - 1_000.0,
            },
            200.0,
            crs,
        )?;
        run(&terrain, crs, left, bottom, width, "flat")
    }
}

fn run(
    terrain: &(dyn TerrainSource + Sync),
    crs: TerrainCrs,
    left: f64,
    bottom: f64,
    width: f64,
    fixture: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let sources: Vec<_> = (0..SOURCES)
        .map(|index| {
            let mut site = Site::new(
                format!("R{index:02}"),
                left + width * (index + 1) as f64 / (SOURCES + 1) as f64,
                bottom + width / 2.0,
            );
            site.kind = SiteKind::Router;
            site.antenna_height_m = 50.0;
            site
        })
        .collect();
    let area = CoverageArea::rectangle([left, bottom, left + width, bottom + width])?;
    let settings = CoverageSettings {
        cell_size_m: width / SIDE as f64,
        maximum_cells: CELLS,
        profile_step_m: terrain.resolution_m(),
        ..CoverageSettings::default()
    };
    let rf = RfSettings {
        fade_margin_db: 0.0,
        ..RfSettings::default()
    };
    let transform = Transformer::new(crs)?;
    let cache = CoverageMetricsCache::new(50_000);
    let measure = |revision, cancel: bool| -> Result<Value, repeaternet_coverage::CoverageError> {
        let start = Instant::now();
        let terrain = Metered {
            terrain,
            reads: AtomicUsize::new(0),
            cancel_at: cancel.then_some(CELLS.div_ceil(512) + SOURCES * 2),
            cancelled_at: OnceLock::new(),
        };
        let eval = CoverageEvaluator::new(&terrain, &rf, &transform).with_cache(&cache, revision);
        let mut covered = 0;
        let mut encoded = Vec::new();
        let mut hash = Sha256::new();
        let mut first_chunk = None;
        let grid = eval.calculate(
            &sources,
            &settings,
            &area,
            RunOptions {
                cancelled: &|| terrain.cancelled_at.get().is_some(),
                on_chunk: Some(&mut |cells| {
                    first_chunk.get_or_insert_with(|| start.elapsed().as_secs_f64());
                    covered += cells
                        .iter()
                        .filter(|cell| cell.state == CoverageState::Covered)
                        .count();
                    encoded.clear();
                    write_cell_chunk(&mut encoded, cells)?;
                    hash.update(&encoded);
                    Ok(())
                }),
                ..RunOptions::default()
            },
        )?;
        let finished = Instant::now();
        let stats = cache.stats();
        Ok(json!({
            "seconds": (finished - start).as_secs_f64(),
            "first_chunk_seconds": first_chunk,
            "peak_working_set_mib": peak_working_set_mib(),
            "terrain_reads": terrain.reads.load(Ordering::Relaxed),
            "covered_cells": covered,
            "evaluated_cells": grid.evaluated_cells,
            "unknown_cells": grid.unknown_cells,
            "completed_cells": grid.completed_cells,
            "result_sha256": format!("{:x}", hash.finalize()),
            "cache": { "entries": stats.entries, "hits": stats.hits, "misses": stats.misses },
            "cancel_ack_seconds": terrain.cancelled_at.get().map(|&at| (finished - at).as_secs_f64()),
        }))
    };
    let cold = measure([1; 32], false)?;
    let warm = measure([1; 32], false)?;
    assert_eq!(cold["result_sha256"], warm["result_sha256"]);
    assert_eq!(cold["completed_cells"], CELLS);
    let cancelled = measure([2; 32], true)?;
    assert!(cancelled["cancel_ack_seconds"].is_number());
    println!(
        "{}",
        json!({ "fixture": fixture, "cold": cold, "warm": warm, "cancelled": cancelled })
    );
    Ok(())
}

#[cfg(windows)]
fn peak_working_set_mib() -> Option<f64> {
    use std::{ffi::c_void, mem::size_of};

    #[repr(C)]
    struct Counters {
        size: u32,
        page_faults: u32,
        peak_working_set: usize,
        other_counters: [usize; 7],
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentProcess() -> *mut c_void;
        fn K32GetProcessMemoryInfo(process: *mut c_void, counters: *mut Counters, size: u32)
            -> i32;
    }

    let size = size_of::<Counters>() as u32;
    let mut counters = Counters {
        size,
        page_faults: 0,
        peak_working_set: 0,
        other_counters: [0; 7],
    };
    // The pseudo-handle is borrowed and Counters has the PROCESS_MEMORY_COUNTERS layout.
    let success = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, size) };
    (success != 0).then_some(counters.peak_working_set as f64 / 1_048_576.0)
}

#[cfg(not(windows))]
fn peak_working_set_mib() -> Option<f64> {
    None
}
