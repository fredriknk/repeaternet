use repeaternet_optimizer::{
    CandidateSettings, OptimizationPriority, OptimizationRequest, OptimizationResult, PlanningSite,
    RouteOptimizer, SearchHooks, SearchStatus,
};
use repeaternet_rf_core::{
    AntennaSettings, Point, RfError, RfSettings, Site, SiteKind, TerrainLayer, TerrainSource,
    ValidationMode,
};

struct NarrowRidge;

impl TerrainSource for NarrowRidge {
    fn resolution_m(&self) -> f64 {
        10.0
    }

    fn has_surface(&self) -> bool {
        false
    }

    fn sample(&self, points: &[Point], _: TerrainLayer) -> Result<Vec<f64>, RfError> {
        Ok(points
            .iter()
            .map(|point| {
                if (point.x - 450.0).abs() < 5.0 && point.y.abs() < 25.0 {
                    25.0
                } else {
                    0.0
                }
            })
            .collect())
    }
}

fn site(id: &str, kind: SiteKind, x: f64, y: f64) -> PlanningSite {
    let mut site = Site::new(id, x, y);
    site.kind = kind;
    site.antenna_height_m = 10.0;
    PlanningSite {
        height_override: true,
        ..site.into()
    }
}

fn detour() -> PlanningSite {
    site("detour", SiteKind::Candidate, 500.0, 300.0)
}

fn candidate_settings() -> CandidateSettings {
    CandidateSettings {
        coarse_sample_step_m: 200.0,
        final_sample_step_m: 10.0,
        maximum_solution_routers: 1,
        parallel_workers: 1,
        ..CandidateSettings::default()
    }
}

fn optimize(
    rf: &RfSettings,
    settings: &CandidateSettings,
    candidates: Vec<PlanningSite>,
) -> OptimizationResult {
    let result = RouteOptimizer::new(&NarrowRidge, rf, settings)
        .optimize(
            OptimizationRequest {
                clients: vec![
                    site("A", SiteKind::EndpointA, 0.0, 0.0),
                    site("B", SiteKind::EndpointB, 1_000.0, 0.0),
                ],
                candidates: Some(candidates),
                ..OptimizationRequest::default()
            },
            SearchHooks::default(),
        )
        .unwrap();
    assert_eq!(result.status, SearchStatus::Complete);
    assert!(result.search_complete);
    result
}

#[test]
fn final_certification_replaces_a_coarse_direct_link_that_misses_the_ridge() {
    let rf = RfSettings {
        receiver_sensitivity_dbm: -107.0,
        validation_mode: ValidationMode::StrictLos,
        ..RfSettings::default()
    };
    let settings = CandidateSettings {
        // Exercise coarse screening before the final-resolution topology checks.
        priority: OptimizationPriority::MinimumInfrastructure,
        ..candidate_settings()
    };
    let coarse_only = CandidateSettings {
        final_sample_step_m: settings.coarse_sample_step_m,
        ..settings.clone()
    };

    let coarse = optimize(&rf, &coarse_only, vec![detour()]);
    assert_eq!(
        coarse.active_solution().unwrap().primary_path_ids(),
        ["A", "B"]
    );
    assert!(coarse.links()[0].los_clear);

    let fine = optimize(&rf, &settings, vec![detour()]);
    assert_eq!(fine.alternatives.len(), 1);
    let solution = fine.active_solution().unwrap();
    assert_eq!(solution.primary_path_ids(), ["A", "detour", "B"]);
    assert_eq!(solution.links.len(), 2);
    assert!(solution.links.iter().all(|link| {
        let profile = link.profile.as_ref().expect("certified link profile");
        link.valid && link.los_clear && profile.distances_m[1] <= settings.final_sample_step_m
    }));
}

#[test]
fn obstructed_direct_link_requires_a_relay_when_usable_margin_crosses_zero() {
    let rf = RfSettings {
        receiver_sensitivity_dbm: -107.0,
        validation_mode: ValidationMode::Propagation,
        ..RfSettings::default()
    };
    let settings = candidate_settings();

    let weak = optimize(&rf, &settings, vec![detour()]);
    assert_eq!(
        weak.active_solution().unwrap().primary_path_ids(),
        ["A", "B"]
    );
    let direct = &weak.links()[0];
    assert!(!direct.los_clear);
    assert!(direct.diffraction_loss_db > 0.0);
    assert!(direct.profile.is_some());
    assert!(direct.valid && (0.0..1.0).contains(&direct.worst_margin_db()));

    let raised_fade_margin = RfSettings {
        fade_margin_db: rf.fade_margin_db + 1.0,
        ..rf
    };
    let without_relay = optimize(&raised_fade_margin, &settings, vec![]);
    assert!(!without_relay.found());
    assert!(without_relay.alternatives.is_empty());

    let with_relay = optimize(&raised_fade_margin, &settings, vec![detour()]);
    assert_eq!(with_relay.alternatives.len(), 1);
    let solution = with_relay.active_solution().unwrap();
    assert_eq!(solution.primary_path_ids(), ["A", "detour", "B"]);
    assert_eq!(solution.links.len(), 2);
    assert!(solution
        .links
        .iter()
        .all(|link| { link.valid && link.worst_margin_db() > 1.0 && link.profile.is_some() }));
}

#[test]
fn reliability_orders_the_weak_clear_path_before_the_stronger_obstructed_path() {
    let rf = RfSettings {
        receiver_sensitivity_dbm: -108.25,
        router: AntennaSettings {
            // Keep the clear relay hops viable with less margin than the direct link.
            feed_loss_db: 36.5,
            ..AntennaSettings::default()
        },
        ..RfSettings::default()
    };
    let settings = CandidateSettings {
        reliability_paths: 2,
        ..candidate_settings()
    };
    let minimum_routers = optimize(&rf, &settings, vec![detour()]);
    assert_eq!(
        minimum_routers
            .active_solution()
            .unwrap()
            .primary_path_ids(),
        ["A", "B"]
    );

    let reliability = CandidateSettings {
        priority: OptimizationPriority::MaximumReliability,
        ..settings
    };
    let result = optimize(&rf, &reliability, vec![detour()]);
    let solution = result.active_solution().unwrap();
    assert_eq!(solution.achieved_path_count, 2);
    assert_eq!(solution.links.len(), 3);
    let direct = solution
        .links
        .iter()
        .find(|link| link.source_id == "A" && link.target_id == "B")
        .unwrap();
    assert!(direct.valid && !direct.los_clear);
    for hop in solution
        .links
        .iter()
        .filter(|link| link.source_id == "detour" || link.target_id == "detour")
    {
        let margin = hop.worst_margin_db();
        assert!(hop.valid && hop.los_clear && hop.fresnel_clear);
        assert!((0.0..1.0).contains(&margin), "margin: {margin}");
        assert!(margin < direct.worst_margin_db());
    }
    let paths = &solution.client_paths[&("A".into(), "B".into())];
    assert_eq!(paths.len(), 2);
    assert_eq!(solution.primary_path_ids(), ["A", "detour", "B"]);
    assert_eq!(paths[1], ["A", "B"]);
}

#[test]
fn minimum_router_search_prefers_the_stronger_of_two_single_relay_routes() {
    let rf = RfSettings {
        receiver_sensitivity_dbm: -77.0,
        validation_mode: ValidationMode::StrictLos,
        ..RfSettings::default()
    };
    let settings = candidate_settings();
    let weak_relay = site("weak", SiteKind::Candidate, 100.0, 150.0);

    let weak = optimize(&rf, &settings, vec![weak_relay.clone()]);
    let weak_solution = weak.active_solution().unwrap();
    assert_eq!(weak_solution.primary_path_ids(), ["A", "weak", "B"]);
    let weak_margin = weak_solution.minimum_margin_db();
    assert!((0.0..1.0).contains(&weak_margin), "margin: {weak_margin}");

    let competing = optimize(&rf, &settings, vec![weak_relay, detour()]);
    let stronger = competing.active_solution().unwrap();
    assert_eq!(stronger.primary_path_ids(), ["A", "detour", "B"]);
    assert_eq!(stronger.router_count(), weak_solution.router_count());
    assert!(stronger.minimum_margin_db() > weak_margin);
}
