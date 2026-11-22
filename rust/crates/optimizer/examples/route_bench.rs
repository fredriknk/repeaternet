//! Real-terrain route search benchmark.
//!
//! ```sh
//! cargo run --release -p repeaternet-optimizer --example route_bench -- \
//!     <terrain-generation-dir> <lat-a> <lon-a> <lat-b> <lon-b> [quick|balanced|thorough] [lat,lon;lat,lon required routers]
//! ```
//!
//! The terrain directory holds `dtm/*.tif` and optional `dom/*.tif`, as written by a
//! route terrain download. Defaults match the dashboard.
use std::path::{Path, PathBuf};
use std::time::Instant;

use repeaternet_optimizer::{
    CandidateSettings, OptimizationRequest, PlanningSite, Progress, RouteOptimizer, RouterPolicy,
    SearchEffort, SearchHooks, SiteOrigin,
};
use repeaternet_rf_core::{RfSettings, Site, SiteKind, TerrainLayer, TerrainSource};
use repeaternet_terrain::{LonLat, RasterTerrain, Transformer};

fn tifs(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                .filter(|path| path.extension().is_some_and(|ext| ext == "tif"))
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    files
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dir = PathBuf::from(&args[0]);
    let number = |i: usize| args[i].parse::<f64>().expect("number");
    let effort = match args.get(5).map(String::as_str) {
        Some("quick") => SearchEffort::Quick,
        Some("thorough") => SearchEffort::Thorough,
        _ => SearchEffort::Balanced,
    };
    let terrain = RasterTerrain::open(&tifs(&dir.join("dtm")), &tifs(&dir.join("dom"))).unwrap();
    let transform = Transformer::new(terrain.crs()).unwrap();
    let project = |lat: f64, lon: f64| {
        transform
            .forward(LonLat {
                longitude: lon,
                latitude: lat,
            })
            .unwrap()
    };
    let rf = RfSettings::default();
    let site = |id: &str, kind, lat, lon| {
        let mut site = Site::new(id, 0.0, 0.0);
        site.position = project(lat, lon);
        site.kind = kind;
        site.ground_elevation_m =
            TerrainSource::sample(&terrain, &[site.position], TerrainLayer::GroundDtm).unwrap()[0];
        site.antenna_height_m = rf.router.height_agl_m;
        site
    };
    let a = site("A", SiteKind::EndpointA, number(1), number(2));
    let b = site("B", SiteKind::EndpointB, number(3), number(4));
    let mut fixed = Vec::new();
    if let Some(list) = args.get(6).filter(|list| !list.is_empty()) {
        for (i, item) in list.split(';').enumerate() {
            let (lat, lon) = item.split_once(',').expect("lat,lon");
            let rf = site(
                &format!("M{}", i + 1),
                SiteKind::Router,
                lat.parse().unwrap(),
                lon.parse().unwrap(),
            );
            fixed.push(PlanningSite {
                rf,
                origin: SiteOrigin::Manual,
                policy: RouterPolicy::Required,
                locked: true,
                ..PlanningSite::from(Site::new("", 0.0, 0.0))
            });
        }
    }
    let mut settings = effort.resolve(&CandidateSettings::default());
    let env = |name: &str| std::env::var(name).ok().and_then(|v| v.parse::<f64>().ok());
    if let Some(value) = env("BENCH_CELL") {
        settings.cell_size_m = value;
    }
    if let Some(value) = env("BENCH_PER_CELL") {
        settings.candidates_per_cell = value as usize;
    }
    if let Some(value) = env("BENCH_CORRIDOR") {
        settings.corridor_width_m = value;
    }
    if let Some(value) = env("BENCH_SPACING") {
        settings.grid_spacing_m = value;
        settings.refine_radius_m = value;
    }
    let bounds = terrain.bounds();
    println!(
        "terrain {:.1} m, {:.1} x {:.1} km, A-B {:.1} km, effort {effort:?}",
        terrain.resolution_m(),
        (bounds.max_x - bounds.min_x) / 1000.0,
        (bounds.max_y - bounds.min_y) / 1000.0,
        a.distance_to(&b) / 1000.0
    );
    let route_km = a.distance_to(&b) / 1000.0;
    let started = Instant::now();
    let mut last_stage = None;
    let mut stage_started = Instant::now();
    let mut progress = |progress: Progress| {
        if last_stage != Some(progress.stage) {
            if let Some(stage) = last_stage {
                println!(
                    "  {:<40} {:>7.2} s",
                    format!("{stage:?}"),
                    stage_started.elapsed().as_secs_f64()
                );
            }
            last_stage = Some(progress.stage);
            stage_started = Instant::now();
        }
    };
    let result = RouteOptimizer::new(&terrain, &rf, &settings)
        .optimize(
            OptimizationRequest {
                clients: vec![a.into(), b.into()],
                fixed_routers: fixed,
                terrain_bounds: Some(bounds.as_array()),
                ..OptimizationRequest::default()
            },
            SearchHooks {
                progress: Some(&mut progress),
                ..SearchHooks::default()
            },
        )
        .unwrap();
    println!(
        "elapsed {:.2} s, candidates {}, search links {}",
        started.elapsed().as_secs_f64(),
        result.candidates.len(),
        result.search_links.len()
    );
    let summary: Vec<_> = result
        .alternatives
        .iter()
        .map(|solution| {
            let bottleneck = solution
                .primary_links()
                .map(|link| link.worst_margin_db())
                .fold(f64::INFINITY, f64::min);
            format!("{}:{bottleneck:.1}", solution.router_count())
        })
        .collect();
    println!(
        "SUMMARY km={route_km:.1} candidates={} seconds={:.2} alternatives={}",
        result.candidates.len(),
        started.elapsed().as_secs_f64(),
        summary.join(" ")
    );
    for diagnostic in &result.diagnostics {
        println!("  diag: {diagnostic}");
    }
    for (index, solution) in result.alternatives.iter().enumerate() {
        let active = if Some(index) == result.active_index {
            "*"
        } else {
            " "
        };
        let path = solution.primary_path_ids();
        let hops: Vec<_> = solution
            .primary_links()
            .map(|link| {
                format!(
                    "{}-{} {:.1}km {:.1}dB{}",
                    link.source_id,
                    link.target_id,
                    link.distance_m / 1000.0,
                    link.worst_margin_db(),
                    if link.los_clear && link.fresnel_clear {
                        ""
                    } else if link.los_clear {
                        " (fresnel)"
                    } else {
                        " (nlos)"
                    }
                )
            })
            .collect();
        let bottleneck = solution
            .primary_links()
            .map(|link| link.worst_margin_db())
            .fold(f64::INFINITY, f64::min);
        println!(
            "{active} {:>2} routers  min {:>5.1} dB  {}  [{}]",
            solution.router_count(),
            bottleneck,
            path.join("-"),
            hops.join(", ")
        );
    }
}
