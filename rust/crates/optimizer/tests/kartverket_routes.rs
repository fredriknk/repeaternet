use std::path::PathBuf;

use repeaternet_optimizer::{
    CandidateSettings, OptimizationPriority, OptimizationRequest, RouteOptimizer, SearchHooks,
};
use repeaternet_rf_core::{
    RfSettings, Site, SiteKind, TerrainLayer, TerrainSource, ValidationMode,
};
use repeaternet_terrain::RasterTerrain;

#[test]
#[ignore = "requires the documented Kartverket NHM cache fixture"]
fn strict_route_across_95_km_fits_the_router_budget() {
    let path = std::env::var_os("REPEATERNET_TERRAIN_KARTVERKET_FIXTURE")
        .expect("set REPEATERNET_TERRAIN_KARTVERKET_FIXTURE to the documented raster");
    let terrain = RasterTerrain::open(&[PathBuf::from(path)], &[]).unwrap();
    let mut a = Site::new("A", 552_000.0, 6_630_000.0);
    let mut b = Site::new("B", 647_000.0, 6_630_000.0);
    let positions = [a.position, b.position];
    let ground = TerrainSource::sample(&terrain, &positions, TerrainLayer::GroundDtm).unwrap();
    assert!((ground[0] - 198.9562).abs() < 0.01);
    assert!((ground[1] - 250.0942).abs() < 0.01);
    a.kind = SiteKind::EndpointA;
    b.kind = SiteKind::EndpointB;
    a.ground_elevation_m = ground[0];
    b.ground_elevation_m = ground[1];
    let rf = RfSettings {
        validation_mode: ValidationMode::StrictLos,
        ..RfSettings::default()
    };
    for (spacing, neighbors, limit) in [(125.0, 16, 6), (250.0, 32, 10)] {
        let settings = CandidateSettings {
            priority: OptimizationPriority::MaximumReliability,
            grid_spacing_m: spacing,
            maximum_neighbors_per_site: neighbors,
            maximum_candidates: 800,
            maximum_solution_routers: limit,
            ..CandidateSettings::default()
        };
        let result = RouteOptimizer::new(&terrain, &rf, &settings)
            .optimize(
                OptimizationRequest {
                    clients: vec![a.clone().into(), b.clone().into()],
                    terrain_bounds: Some([550_050.0, 6_620_050.0, 649_950.0, 6_639_950.0]),
                    ..OptimizationRequest::default()
                },
                SearchHooks::default(),
            )
            .unwrap();
        assert!(result.search_complete);
        assert!(result.candidates.len() <= settings.maximum_candidates + 2);
        assert!(result.diagnostics.iter().any(|diagnostic| {
            diagnostic.contains(&format!("Effective candidate grid spacing: {spacing:.1} m"))
        }));
        let solution = result
            .active_solution()
            .expect("expected a certified route");
        assert!(solution.router_count() <= settings.maximum_solution_routers);
        assert!(solution.primary_path_ids().len() <= settings.maximum_solution_routers + 2);
        assert!(solution.links.iter().all(|link| {
            link.valid && link.los_clear && link.fresnel_clear && link.profile.is_some()
        }));
    }
}
