use repeaternet_optimizer::*;
use repeaternet_rf_core::{
    sample_profile, DirectionResult, LinkEvaluator, LinkResult, Point, RfError, RfSettings, Site,
    SiteKind, TerrainLayer, TerrainSource, ValidationMode,
};
use std::cell::Cell;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use std::sync::{mpsc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

fn site(id: &str, x: f64, y: f64) -> PlanningSite {
    let mut rf = Site::new(id, x, y);
    rf.kind = match id {
        "A" => SiteKind::EndpointA,
        "B" => SiteKind::EndpointB,
        _ => SiteKind::Candidate,
    };
    rf.antenna_height_m = 10.0;
    rf.into()
}

fn link(source: &str, target: &str, margin: f64) -> LinkResult {
    let direction = |a: &str, b: &str| DirectionResult {
        source_id: a.into(),
        target_id: b.into(),
        distance_m: 1_000.0,
        fspl_db: 0.0,
        diffraction_loss_db: 0.0,
        clutter_loss_db: 0.0,
        total_path_loss_db: 0.0,
        tx_gain_dbi: 0.0,
        rx_gain_dbi: 0.0,
        received_power_dbm: 0.0,
        sensitivity_dbm: -130.0,
        raw_margin_db: margin + 10.0,
        usable_margin_db: margin,
        departure_angle_deg: 0.0,
        arrival_angle_deg: 0.0,
        valid: true,
        tx_power_dbm: 0.0,
        tx_feed_loss_db: 0.0,
        rx_feed_loss_db: 0.0,
        miscellaneous_loss_db: 0.0,
    };
    LinkResult {
        source_id: source.into(),
        target_id: target.into(),
        distance_m: 1_000.0,
        forward: direction(source, target),
        reverse: direction(target, source),
        valid: true,
        los_clear: true,
        fresnel_clear: true,
        minimum_clearance_m: 5.0,
        minimum_fresnel_clearance_ratio: 1.0,
        minimum_clearance_distance_m: 500.0,
        maximum_fresnel_radius_m: 5.0,
        diffraction_loss_db: 0.0,
        dominant_obstacles: Vec::new(),
        profile: None,
    }
}

struct Terrain {
    resolution: f64,
    ridges: &'static [f64],
}

impl TerrainSource for Terrain {
    fn resolution_m(&self) -> f64 {
        self.resolution
    }
    fn has_surface(&self) -> bool {
        false
    }

    fn sample(&self, points: &[Point], _: TerrainLayer) -> Result<Vec<f64>, RfError> {
        Ok(points
            .iter()
            .map(|p| {
                if self
                    .ridges
                    .iter()
                    .any(|&x| (p.x - x).abs() < self.resolution / 2.0)
                {
                    500.0
                } else {
                    0.0
                }
            })
            .collect())
    }
}

fn strict_settings() -> RfSettings {
    let mut settings = RfSettings {
        receiver_sensitivity_dbm: -150.0,
        fade_margin_db: 0.0,
        validation_mode: ValidationMode::StrictLos,
        required_fresnel_clearance: 0.0,
        ..RfSettings::default()
    };
    settings.router.height_agl_m = 10.0;
    settings
}

fn request(sites: Vec<PlanningSite>) -> OptimizationRequest {
    OptimizationRequest {
        clients: sites.iter().filter(|s| s.is_client()).cloned().collect(),
        candidates: Some(sites),
        ..OptimizationRequest::default()
    }
}

#[test]
fn graph_retains_better_fresnel_prefix_after_later_margin_bottleneck() {
    let sites = ["A", "X", "Y", "M", "B"].map(|id| site(id, 0.0, 0.0));
    let mut links = [
        link("A", "X", 20.0),
        link("X", "M", 20.0),
        link("A", "Y", 10.0),
        link("Y", "M", 10.0),
        link("M", "B", 5.0),
    ];
    links[0].minimum_fresnel_clearance_ratio = 0.2;
    let graph = RoutingGraph::new(&sites, &links).unwrap();
    assert_eq!(
        graph
            .select_path("A", "B", OptimizationPriority::MinimumRouters)
            .unwrap(),
        ["A", "Y", "M", "B"]
    );
}

fn matches_edge(edge: &LinkResult, a: &str, b: &str) -> bool {
    (edge.source_id == a && edge.target_id == b) || (edge.source_id == b && edge.target_id == a)
}

fn all_paths(
    links: &[LinkResult],
    path: &mut Vec<usize>,
    result: &mut Vec<Vec<usize>>,
    names: &[&str],
    target: usize,
) {
    let source = *path.last().unwrap();
    if source == target {
        result.push(path.clone());
        return;
    }
    for (i, name) in names.iter().enumerate() {
        if !path.contains(&i)
            && links
                .iter()
                .any(|link| matches_edge(link, names[source], name))
        {
            path.push(i);
            all_paths(links, path, result, names, target);
            path.pop();
        }
    }
}

fn random(seed: &mut u64, limit: u64) -> u64 {
    *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
    (*seed >> 32) % limit
}

#[test]
fn four_graph_objectives_match_exhaustive_simple_path_oracle() {
    let names = ["A", "C", "D", "E", "F", "B"];
    let mut seed = 20_260_926;
    let priorities = [
        OptimizationPriority::MinimumRouters,
        OptimizationPriority::MaximumReliability,
        OptimizationPriority::FewestNewInstallations,
        OptimizationPriority::MinimumInfrastructure,
    ];
    for _ in 0..60 {
        let sites: Vec<_> = names
            .iter()
            .enumerate()
            .map(|(i, id)| {
                let mut site = site(id, i as f64, 0.0);
                site.rf.antenna_height_m = (1 + random(&mut seed, 20)) as f64;
                site.site_quality = random(&mut seed, 100) as f64;
                if i % 2 == 1 {
                    site.origin = SiteOrigin::Known;
                }
                site
            })
            .collect();
        let mut links = Vec::new();
        for (i, a) in names.iter().enumerate() {
            for (j, b) in names.iter().enumerate().skip(i + 1) {
                if random(&mut seed, 100) >= 55 {
                    continue;
                }
                let mut edge = link(a, b, (1 + random(&mut seed, 4)) as f64);
                edge.distance_m = (1 + random(&mut seed, 100) + (1 << (i * 6 + j))) as f64 / 1e6;
                edge.minimum_fresnel_clearance_ratio =
                    [0.2, 0.6, 1.0][random(&mut seed, 3) as usize];
                links.push(edge);
            }
        }
        let mut paths = Vec::new();
        all_paths(&links, &mut vec![0], &mut paths, &names, 5);
        let graph = RoutingGraph::new(&sites, &links).unwrap();
        for priority in priorities {
            let score = |path: &[usize]| {
                let edges: Vec<_> = path
                    .windows(2)
                    .map(|p| {
                        links
                            .iter()
                            .find(|link| matches_edge(link, names[p[0]], names[p[1]]))
                            .unwrap()
                    })
                    .collect();
                let margin = edges
                    .iter()
                    .map(|link| link.worst_margin_db())
                    .reduce(f64::min)
                    .unwrap();
                let fresnel = edges
                    .iter()
                    .map(|link| link.minimum_fresnel_clearance_ratio)
                    .reduce(f64::min)
                    .unwrap();
                let inner = &path[1..path.len() - 1];
                let height = inner
                    .iter()
                    .map(|&i| sites[i].rf.antenna_height_m)
                    .sum::<f64>();
                let quality = path[1..]
                    .iter()
                    .map(|&i| sites[i].site_quality)
                    .sum::<f64>();
                let distance = edges.iter().map(|link| link.distance_m).sum::<f64>();
                let hops = -(path.len() as f64);
                match priority {
                    OptimizationPriority::MinimumRouters => {
                        vec![hops, margin, fresnel, -height, quality, -distance]
                    }
                    OptimizationPriority::MaximumReliability => {
                        vec![margin, hops, fresnel, -height, quality, -distance]
                    }
                    OptimizationPriority::FewestNewInstallations => {
                        let new = inner
                            .iter()
                            .filter(|&&i| sites[i].origin != SiteOrigin::Known)
                            .count();
                        vec![
                            -(new as f64),
                            hops,
                            margin,
                            fresnel,
                            -height,
                            quality,
                            -distance,
                        ]
                    }
                    OptimizationPriority::MinimumInfrastructure => {
                        vec![-(100.0 * inner.len() as f64 + height + distance / 1e6)]
                    }
                }
            };
            let expected = paths
                .iter()
                .max_by(|a, b| {
                    score(a)
                        .iter()
                        .zip(score(b))
                        .map(|(x, y)| x.partial_cmp(&y).unwrap())
                        .find(|cmp| *cmp != Ordering::Equal)
                        .unwrap_or_else(|| {
                            b.iter().map(|&i| names[i]).cmp(a.iter().map(|&i| names[i]))
                        })
                })
                .map(|path| path.iter().map(|&i| names[i].to_owned()).collect());
            assert_eq!(
                graph.select_path("A", "B", priority),
                expected,
                "{priority:?}"
            );
        }
    }
}

#[test]
fn exact_counts_keep_induced_links_and_required_multiclient_router() {
    let mut c = site("C", 5.0, 8.0);
    c.rf.kind = SiteKind::Client;
    let mut manual = site("manual", 5.0, 3.0);
    manual.origin = SiteOrigin::Manual;
    manual.policy = RouterPolicy::Required;
    let mut excluded = site("excluded", 5.0, 2.0);
    excluded.policy = RouterPolicy::Excluded;
    let sites = vec![
        site("A", 0.0, 0.0),
        site("B", 10.0, 0.0),
        c,
        manual,
        site("other", 3.0, 4.0),
        excluded,
    ];
    let links: Vec<_> = sites
        .iter()
        .enumerate()
        .flat_map(|(i, a)| {
            sites[i + 1..]
                .iter()
                .map(move |b| link(&a.rf.id, &b.rf.id, 10.0))
        })
        .collect();
    let settings = CandidateSettings {
        maximum_solution_routers: 2,
        ..CandidateSettings::default()
    };
    let alternatives = solve_topologies(&sites, &links, &settings, &|| false, &[]).unwrap();
    assert_eq!(
        alternatives
            .iter()
            .map(NetworkSolution::router_count)
            .collect::<Vec<_>>(),
        [1, 2]
    );
    assert_eq!(
        alternatives
            .iter()
            .map(|s| s.links.len())
            .collect::<Vec<_>>(),
        [6, 10]
    );
    assert_eq!(alternatives[0].router_ids, ["manual"]);
    assert_eq!(alternatives[1].client_ids, ["A", "B", "C"]);
    assert_eq!(alternatives[1].client_paths.len(), 3);
    let mut reversed = sites;
    reversed.reverse();
    let alternatives_reversed =
        solve_topologies(&reversed, &links, &settings, &|| false, &[]).unwrap();
    assert_eq!(
        alternatives[1].client_paths,
        alternatives_reversed[1].client_paths
    );
}

#[test]
fn redundancy_reroutes_earlier_flow_and_reliability_prefers_stronger_resilient_mesh() {
    let ids = ["A", "B", "X", "Y", "Z", "W", "P", "Q", "R"];
    let sites: Vec<_> = ids
        .iter()
        .enumerate()
        .map(|(i, id)| site(id, i as f64, 0.0))
        .collect();
    let trapped = [
        link("A", "X", 5.0),
        link("X", "Y", 5.0),
        link("Y", "B", 5.0),
        link("X", "Z", 5.0),
        link("Z", "B", 5.0),
        link("A", "W", 5.0),
        link("W", "Y", 5.0),
    ];
    let reliability = CandidateSettings {
        maximum_solution_routers: 4,
        reliability_paths: 2,
        priority: OptimizationPriority::MaximumReliability,
        ..CandidateSettings::default()
    };
    let result = solve_topologies(&sites, &trapped, &reliability, &|| false, &[]).unwrap();
    let active = &result[select_active_solution_index(&result, reliability.priority).unwrap()];
    assert_eq!(active.router_count(), 4);
    assert_eq!(active.achieved_path_count, 2);
    let paths = &active.client_paths[&("A".into(), "B".into())];
    assert!(paths.contains(&vec!["A".into(), "X".into(), "Z".into(), "B".into()]));
    assert!(paths.contains(&vec!["A".into(), "W".into(), "Y".into(), "B".into()]));

    let simple = [
        link("A", "X", 5.0),
        link("X", "B", 5.0),
        link("A", "Y", 5.0),
        link("Y", "B", 5.0),
        link("A", "P", 30.0),
        link("P", "Q", 30.0),
        link("Q", "R", 30.0),
        link("R", "B", 30.0),
    ];
    let result = solve_topologies(&sites, &simple, &reliability, &|| false, &[]).unwrap();
    let active = &result[select_active_solution_index(&result, reliability.priority).unwrap()];
    assert_eq!(active.router_ids, ["P", "Q", "R", "X"]);
    assert!(active.resilient());
}

#[test]
fn paired_seeds_strengthen_the_backup_and_preserve_incumbent_path_quality() {
    let settings = CandidateSettings {
        maximum_solution_routers: 4,
        reliability_paths: 2,
        priority: OptimizationPriority::MaximumReliability,
        ..CandidateSettings::default()
    };
    let mut sites: Vec<_> = ["A", "B", "P", "Q", "X", "Y"]
        .iter()
        .enumerate()
        .map(|(i, id)| site(id, i as f64, 0.0))
        .collect();
    for i in 0..100 {
        sites.push(site(&format!("D{i:03}"), i as f64, 1.0));
    }

    for (short_margin, short_clear, expected_margins) in [
        (5.0, true, [30.0, 20.0]),
        (40.0, true, [40.0, 40.0]),
        (25.0, false, [30.0, 25.0]),
    ] {
        let mut links = vec![
            link("A", "P", 30.0),
            link("P", "Q", 30.0),
            link("Q", "B", 30.0),
            link("A", "X", 20.0),
            link("X", "Y", 20.0),
            link("Y", "B", 20.0),
        ];
        // Short routes fill the beam pool. The two routers for the 20 dB
        // backup enter the four-router search together through a paired seed.
        for decoy in &sites[6..] {
            let mut first = link("A", &decoy.rf.id, short_margin);
            first.los_clear = short_clear;
            first.fresnel_clear = short_clear;
            links.extend([first, link(&decoy.rf.id, "B", short_margin)]);
        }
        let incumbent = [vec!["P".into(), "Q".into(), "D000".into(), "D001".into()]];
        let alternatives =
            solve_topologies(&sites, &links, &settings, &|| false, &incumbent).unwrap();
        let four = alternatives.iter().find(|s| s.router_count() == 4).unwrap();
        let paths = &four.client_paths[&("A".into(), "B".into())];
        let margins: Vec<_> = paths
            .iter()
            .map(|path| {
                path.windows(2)
                    .map(|p| {
                        links
                            .iter()
                            .find(|link| matches_edge(link, &p[0], &p[1]))
                            .unwrap()
                            .worst_margin_db()
                    })
                    .fold(f64::INFINITY, f64::min)
            })
            .collect();
        assert_eq!(margins, expected_margins);
        assert_eq!(four.achieved_path_count, 2);

        let active =
            &alternatives[select_active_solution_index(&alternatives, settings.priority).unwrap()];
        if short_margin == 5.0 {
            assert_eq!(active.router_ids, ["P", "Q", "X", "Y"]);
        }
    }
}

#[test]
fn reliability_ranks_backups_for_every_client_pair() {
    let mut c = site("C", 20.0, 0.0);
    c.rf.kind = SiteKind::Client;
    let sites = [
        site("A", 0.0, 0.0),
        site("B", 10.0, 0.0),
        c,
        site("R", 10.0, 10.0),
    ];
    let settings = CandidateSettings {
        maximum_solution_routers: 1,
        reliability_paths: 3,
        priority: OptimizationPriority::MaximumReliability,
        ..CandidateSettings::default()
    };
    let alternatives = [5.0, 15.0].map(|c_margin| {
        let links = [
            link("A", "B", 30.0),
            link("A", "C", 30.0),
            link("B", "C", 30.0),
            link("A", "R", 20.0),
            link("B", "R", 10.0),
            link("C", "R", c_margin),
        ];
        solve_topologies(&sites, &links, &settings, &|| false, &[])
            .unwrap()
            .pop()
            .unwrap()
    });
    assert!(alternatives.iter().all(|s| s.achieved_path_count == 3));
    assert_eq!(
        select_active_solution_index(&alternatives, settings.priority),
        Some(1)
    );
}

#[test]
fn topology_selection_can_prefer_two_short_existing_masts() {
    let mut tall = site("tall", 5.0, 0.0);
    tall.rf.antenna_height_m = 200.0;
    let mut left = site("left", 3.0, 0.0);
    let mut right = site("right", 7.0, 0.0);
    left.origin = SiteOrigin::Known;
    right.origin = SiteOrigin::Known;
    let sites = [site("A", 0.0, 0.0), site("B", 10.0, 0.0), tall, left, right];
    let links = [
        link("A", "tall", 30.0),
        link("tall", "B", 30.0),
        link("A", "left", 5.0),
        link("left", "right", 5.0),
        link("right", "B", 5.0),
    ];
    let mut settings = CandidateSettings {
        maximum_solution_routers: 2,
        ..CandidateSettings::default()
    };
    for priority in [
        OptimizationPriority::MinimumInfrastructure,
        OptimizationPriority::FewestNewInstallations,
    ] {
        settings.priority = priority;
        let alternatives = solve_topologies(&sites, &links, &settings, &|| false, &[]).unwrap();
        let selected =
            &alternatives[select_active_solution_index(&alternatives, priority).unwrap()];
        assert_eq!(selected.router_ids, ["left", "right"]);
        assert_eq!(selected.proposed_router_count(), 0);
        assert_eq!(selected.primary_path_ids(), ["A", "left", "right", "B"]);
    }
}

#[test]
fn fewest_new_does_not_count_one_direct_link_twice() {
    let sites = [site("A", 0.0, 0.0), site("B", 10.0, 0.0)];
    let settings = CandidateSettings {
        priority: OptimizationPriority::FewestNewInstallations,
        reliability_paths: 2,
        ..CandidateSettings::default()
    };
    let solutions =
        solve_topologies(&sites, &[link("A", "B", 10.0)], &settings, &|| false, &[]).unwrap();
    assert_eq!(solutions[0].primary_path_ids(), ["A", "B"]);
    assert_eq!(solutions[0].achieved_path_count, 1);
    assert!(!solutions[0].resilient());
}

#[test]
fn beam_seeds_protect_long_chains_among_dense_candidates() {
    let mut sites = vec![site("A", 0.0, 0.0), site("B", 1_000.0, 0.0)];
    let chain = ["A", "S1", "S2", "S3", "S4", "B"];
    sites.extend(
        chain[1..5]
            .iter()
            .enumerate()
            .map(|(i, id)| site(id, 100.0 * (i + 1) as f64, 0.0)),
    );
    let mut links: Vec<_> = chain.windows(2).map(|p| link(p[0], p[1], 5.0)).collect();
    for i in 0..45 {
        let id = format!("decoy-{i:02}");
        links.push(link("A", &id, 40.0));
        links.extend((0..i).map(|j| link(&id, &format!("decoy-{j:02}"), 40.0)));
        sites.push(site(&id, i as f64, 3.0));
    }
    let settings = CandidateSettings {
        maximum_solution_routers: 4,
        ..CandidateSettings::default()
    };
    let alternatives = solve_topologies(&sites, &links, &settings, &|| false, &[]).unwrap();
    assert_eq!(alternatives.len(), 1);
    assert_eq!(alternatives[0].primary_path_ids(), chain);
    assert!(alternatives[0].diagnostics[0].contains("Heuristic"));
    let polls = Cell::new(0);
    let cancelled = || {
        polls.set(polls.get() + 1);
        polls.get() >= 45
    };
    assert!(solve_topologies(&sites, &links, &settings, &cancelled, &[])
        .unwrap()
        .is_empty());
}

#[test]
fn native_rf_on_flat_ground_and_one_or_two_ridges() {
    for (ridges, distance) in [
        (&[][..], 10_000.0),
        (&[5_000.0][..], 10_000.0),
        (&[4_000.0, 8_000.0][..], 12_000.0),
    ] {
        let terrain = Terrain {
            resolution: 10.0,
            ridges,
        };
        let rf = strict_settings();
        let settings = CandidateSettings {
            final_sample_step_m: 10.0,
            coarse_sample_step_m: 20.0,
            maximum_solution_routers: 3,
            ..CandidateSettings::default()
        };
        let mut a = site("A", 0.0, 10.0);
        let mut b = site("B", distance, 10.0);
        a.rf.antenna_height_m = 20.0;
        b.rf.antenna_height_m = 20.0;
        let mut sites = vec![a, b];
        let mut expected = vec!["A".to_owned()];
        for (i, &x) in ridges.iter().enumerate() {
            let id = format!("S{}", i + 1);
            let mut summit = site(&id, x, 10.0);
            summit.rf.ground_elevation_m = 500.0;
            sites.push(summit);
            expected.push(id);
        }
        expected.push("B".into());
        let result = RouteOptimizer::new(&terrain, &rf, &settings)
            .optimize(request(sites), SearchHooks::default())
            .unwrap();
        let active = result.active_solution().unwrap();
        assert_eq!(active.router_count(), ridges.len());
        assert_eq!(active.primary_path_ids(), expected);
        assert_eq!(active.links.len(), ridges.len() + 1);
        assert!(active.links.iter().all(|l| l.valid && l.profile.is_some()));
        assert!(result.search_links.iter().all(|l| l.profile.is_none()));
    }
}

#[test]
fn candidate_grid_respects_refinement_spacing_and_applies_filter_before_thinning() {
    let terrain = Terrain {
        resolution: 250.0,
        ridges: &[500.0],
    };
    let settings = CandidateSettings {
        corridor_width_m: 1_000.0,
        grid_spacing_m: 1_000.0,
        cell_size_m: 1_000.0,
        candidates_per_cell: 1,
        maximum_candidates: 100,
        refine_radius_m: 250.0,
        ..CandidateSettings::default()
    };
    let exclusions = [vec![
        Point { x: 490.0, y: -10.0 },
        Point { x: 510.0, y: -10.0 },
        Point { x: 510.0, y: 250.0 },
        Point { x: 490.0, y: 250.0 },
    ]];
    let filter = |points: &[Point]| {
        assert!(points.iter().all(|p| p.x != 500.0));
        Ok(points
            .iter()
            .map(|p| p.x == 250.0 || p.x >= 1_250.0)
            .collect())
    };
    let generated = generate_candidates(
        &terrain,
        [0.0, 0.0, 2_000.0, 0.0],
        [Point::default(), Point { x: 2_000.0, y: 0.0 }],
        &settings,
        &exclusions,
        Some(&filter),
        &|| false,
    )
    .unwrap();
    assert_eq!(candidate_grid_spacing(&terrain, &settings), 250.0);
    assert_eq!(
        generated
            .iter()
            .map(|s| s.rf.position.x)
            .collect::<Vec<_>>(),
        [250.0, 1_250.0, 2_000.0]
    );
}

#[test]
fn fixed_router_policies_budget_and_height_override() {
    let terrain = Terrain {
        resolution: 10.0,
        ridges: &[],
    };
    let rf = strict_settings();
    let mut settings = CandidateSettings {
        maximum_candidates: 2,
        ..CandidateSettings::default()
    };
    let mut required = site("K-required", 500.0, 0.0);
    required.origin = SiteOrigin::Known;
    required.policy = RouterPolicy::Required;
    required.height_override = true;
    required.rf.antenna_height_m = 37.0;
    let mut excluded = required.clone();
    excluded.rf.id = "K-excluded".into();
    excluded.rf.position.y = 5.0;
    excluded.policy = RouterPolicy::Excluded;
    let mut optional = site("K-optional", 750.0, 10.0);
    optional.origin = SiteOrigin::Known;
    optional.locked = true;
    let make_request = || OptimizationRequest {
        clients: vec![site("A", 0.0, 0.0), site("B", 1_000.0, 0.0)],
        fixed_routers: vec![required.clone(), optional.clone(), excluded.clone()],
        terrain_bounds: Some([0.0, 0.0, 1_000.0, 10.0]),
        ..OptimizationRequest::default()
    };
    let check = RouteOptimizer::new(&terrain, &rf, &settings)
        .with_site_filter(&|_| panic!("fixed routers consume full generation budget"))
        .optimize(make_request(), SearchHooks::default())
        .unwrap();
    assert_eq!(check.candidates.len(), 4);
    assert_eq!(check.active_solution().unwrap().router_ids, ["K-required"]);
    // Adding K-optional keeps the same bottleneck hop, so the route with more
    // routers is not offered as an alternative.
    assert_eq!(check.alternatives.len(), 1);
    assert_eq!(
        check
            .candidates
            .iter()
            .find(|s| s.rf.id == "K-required")
            .unwrap()
            .rf
            .antenna_height_m,
        37.0
    );
    settings.infrastructure_policy = InfrastructurePolicy::ProposedOnly;
    let result = RouteOptimizer::new(&terrain, &rf, &settings)
        .optimize(make_request(), SearchHooks::default())
        .unwrap();
    assert!(result
        .candidates
        .iter()
        .all(|s| s.origin != SiteOrigin::Known));
    assert_eq!(result.router_count(), 0);
    settings.infrastructure_policy = InfrastructurePolicy::ExistingOnly;
    settings.maximum_solution_routers = 0;
    assert!(RouteOptimizer::new(&terrain, &rf, &settings)
        .optimize(make_request(), SearchHooks::default())
        .is_err());
}

#[test]
fn native_optimizer_certifies_all_three_clients_with_locked_manual_router() {
    let terrain = Terrain {
        resolution: 100.0,
        ridges: &[],
    };
    let rf = strict_settings();
    let settings = CandidateSettings {
        maximum_solution_routers: 2,
        ..CandidateSettings::default()
    };
    let mut c = site("C3", 5_000.0, 8_000.0);
    c.rf.kind = SiteKind::Client;
    let mut manual = site("M1", 5_000.0, 3_000.0);
    manual.rf.kind = SiteKind::Router;
    manual.origin = SiteOrigin::Manual;
    manual.policy = RouterPolicy::Required;
    manual.locked = true;
    let mut optimize = request(vec![
        site("A", 0.0, 0.0),
        site("B", 10_000.0, 0.0),
        c,
        site("N1", 5_000.0, 5_000.0),
    ]);
    optimize.fixed_routers.push(manual);
    let result = RouteOptimizer::new(&terrain, &rf, &settings)
        .optimize(optimize, SearchHooks::default())
        .unwrap();
    assert_eq!(
        result.active_solution().unwrap().client_ids,
        ["A", "B", "C3"]
    );
    assert_eq!(
        result
            .alternatives
            .iter()
            .map(|s| s.router_count())
            .collect::<Vec<_>>(),
        [1, 2]
    );
    assert!(result
        .alternatives
        .iter()
        .all(|s| s.router_ids.contains(&"M1".into())));
    assert_eq!(result.links().len(), 6);
    assert!(result
        .links()
        .iter()
        .all(|link| link.valid && link.profile.is_some()));
}

#[test]
fn search_effort_clamps_only_candidate_and_neighbor_caps() {
    let settings = CandidateSettings {
        maximum_candidates: 5000,
        candidates_per_cell: 6,
        maximum_neighbors_per_site: 20,
        maximum_solution_routers: 4,
        ..CandidateSettings::default()
    };
    for (effort, expected) in [
        (SearchEffort::Quick, (600, 2, 8)),
        (SearchEffort::Balanced, (2000, 3, 12)),
        (SearchEffort::Thorough, (5000, 6, 20)),
    ] {
        let resolved = effort.resolve(&settings);
        assert_eq!(
            (
                resolved.maximum_candidates,
                resolved.candidates_per_cell,
                resolved.maximum_neighbors_per_site
            ),
            expected
        );
        assert_eq!(resolved.maximum_solution_routers, 4);
        assert!(resolved.exhaustive_screening);
    }
    let lower = CandidateSettings {
        maximum_candidates: 300,
        candidates_per_cell: 1,
        ..settings
    };
    let resolved = SearchEffort::Balanced.resolve(&lower);
    assert_eq!(
        (resolved.maximum_candidates, resolved.candidates_per_cell),
        (300, 1)
    );
}

#[test]
fn indexed_screening_matches_exhaustive_neighbors_and_terminal_pairs() {
    let mut seed = 20260926;
    let sites: Vec<_> = (0..200)
        .map(|i| {
            site(
                &i.to_string(),
                random(&mut seed, 30) as f64,
                random(&mut seed, 30) as f64,
            )
        })
        .collect();
    let terminals = BTreeSet::from([0, 93, 199]);
    for limit in [1, 4, 12, 250] {
        let mut expected = BTreeSet::new();
        for (i, a) in sites.iter().enumerate() {
            let mut others: Vec<_> = (0..sites.len()).filter(|&j| j != i).collect();
            others.sort_by(|&j, &k| {
                a.rf.distance_to(&sites[j].rf)
                    .total_cmp(&a.rf.distance_to(&sites[k].rf))
                    .then_with(|| j.cmp(&k))
            });
            let count = if terminals.contains(&i) {
                sites.len()
            } else {
                limit
            };
            expected.extend(others.into_iter().take(count).map(|j| (i.min(j), i.max(j))));
        }
        assert_eq!(
            screening_pair_indices(&sites, limit, &terminals),
            expected.into_iter().collect::<Vec<_>>()
        );
    }
}

#[test]
fn backbone_keeps_spread_summits_in_rotated_corridors() {
    for angle in [0.0_f64, 45.0, 120.0] {
        let (sin, cos) = angle.to_radians().sin_cos();
        let place = |id: &str, along: f64, across: f64, quality: f64| {
            let mut result = site(id, along * cos - across * sin, along * sin + across * cos);
            result.site_quality = quality;
            result
        };
        let mut seed = 7;
        let mut sites = vec![place("A", 0.0, 0.0, 0.0), place("B", 180_000.0, 0.0, 0.0)];
        for i in 0..300 {
            sites.push(place(
                &format!("clutter-{i}"),
                random(&mut seed, 4_000) as f64,
                random(&mut seed, 1_000) as f64 - 500.0,
                0.9,
            ));
            sites.push(place(
                &format!("filler-{i}"),
                random(&mut seed, 180_000) as f64,
                random(&mut seed, 10_000) as f64 - 5_000.0,
                0.1,
            ));
        }
        for i in 0..120 {
            let mut router = place(
                &format!("known-{i}"),
                random(&mut seed, 2_000) as f64,
                random(&mut seed, 400) as f64 - 200.0,
                0.95,
            );
            router.origin = SiteOrigin::Known;
            sites.push(router);
        }
        for i in 1..=5 {
            sites.push(place(&format!("S{i}"), 30_000.0 * i as f64, 0.0, 0.8));
        }
        let chosen = backbone_site_indices(&sites, 64, &BTreeSet::from([0, 1]));
        assert_eq!(chosen.len(), 64);
        assert_eq!(
            chosen
                .iter()
                .filter(|&&i| sites[i].rf.id.starts_with('S'))
                .count(),
            5
        );
        assert!(
            chosen
                .iter()
                .filter(|&&i| sites[i].origin == SiteOrigin::Known)
                .count()
                <= 16
        );
    }
}

type Edge = (String, String);

fn edge(a: &str, b: &str) -> Edge {
    (a.min(b).into(), a.max(b).into())
}

#[derive(Default)]
struct FakeRf {
    margins: BTreeMap<Edge, (Option<f64>, Option<f64>)>,
    calls: Mutex<Vec<(Edge, f64, bool)>>,
}

impl FakeRf {
    fn add(&mut self, a: &str, b: &str, coarse: Option<f64>, final_margin: Option<f64>) {
        self.margins.insert(edge(a, b), (coarse, final_margin));
    }
}

impl LinkEvaluation for FakeRf {
    fn optimistic_margin_db(&self, _: &Site, _: &Site) -> Result<f64, RfError> {
        Ok(100.0)
    }

    fn evaluate(
        &self,
        a: &Site,
        b: &Site,
        step: f64,
        profile: bool,
    ) -> Result<LinkResult, RfError> {
        let key = edge(&a.id, &b.id);
        let values = self.margins.get(&key).copied().unwrap_or_default();
        self.calls.lock().unwrap().push((key, step, profile));
        let margin = if step == 1.0 { values.1 } else { values.0 };
        let mut result = link(&a.id, &b.id, margin.unwrap_or(-1.0));
        result.valid = margin.is_some();
        result.distance_m = a.distance_to(b);
        if profile {
            result.profile = Some(sample_profile(
                &Terrain {
                    resolution: 1.0,
                    ridges: &[],
                },
                a,
                b,
                869.5,
                1.0,
                Some(step),
            )?);
        }
        Ok(result)
    }
}

fn mock_settings(
    priority: OptimizationPriority,
    maximum_solution_routers: usize,
) -> CandidateSettings {
    CandidateSettings {
        priority,
        maximum_solution_routers,
        maximum_neighbors_per_site: 1,
        final_sample_step_m: 1.0,
        // These scenarios exercise the neighbour-screening fallback.
        exhaustive_screening: false,
        ..CandidateSettings::default()
    }
}

#[test]
fn interior_bridge_recovers_a_certified_route_outside_the_screened_pairs() {
    let terrain = Terrain {
        resolution: 1.0,
        ridges: &[],
    };
    let rf = strict_settings();
    let settings = mock_settings(OptimizationPriority::MaximumReliability, 3);
    let mut sites = vec![
        site("A", 0.0, 0.0),
        site("left", 100.0, 0.0),
        site("middle", 300.0, 0.0),
        site("right", 500.0, 0.0),
    ];
    for bridge in &mut sites[1..] {
        bridge.site_quality = -100.0;
    }
    for x in [100.0, 300.0, 500.0] {
        for index in 1..=25 {
            sites.push(site(&format!("decoy-{x}-{index}"), x, index as f64));
        }
    }
    sites.push(site("B", 600.0, 0.0));
    let terminals = BTreeSet::from([0, sites.len() - 1]);
    let backbone = backbone_site_indices(&sites, 64, &terminals);
    assert!(!backbone.contains(&2));
    let screened = screening_pair_indices(&sites, 1, &terminals);
    assert!(!screened.contains(&(1, 2)) && !screened.contains(&(2, 3)));
    let mut fake = FakeRf::default();
    for (a, b) in [
        ("A", "left"),
        ("left", "middle"),
        ("middle", "right"),
        ("right", "B"),
    ] {
        fake.add(a, b, Some(10.0), Some(10.0));
    }
    let mut bridge_pairs = 0;
    let result = RouteOptimizer::new(&terrain, &rf, &settings)
        .with_link_evaluation(&fake)
        .optimize(
            request(sites),
            SearchHooks {
                progress: Some(&mut |progress| {
                    if progress.stage == Stage::CheckingInteriorBridgeLinks && progress.done == 0 {
                        bridge_pairs += progress.total;
                    }
                }),
                ..SearchHooks::default()
            },
        )
        .unwrap();
    assert!(bridge_pairs > 0 && bridge_pairs < 300);
    let solution = result.active_solution().unwrap();
    assert_eq!(
        solution.primary_path_ids(),
        ["A", "left", "middle", "right", "B"]
    );
    assert!(solution
        .links
        .iter()
        .all(|link| link.valid && link.profile.is_some()));
    let calls = fake.calls.lock().unwrap();
    for bridge in [edge("left", "middle"), edge("middle", "right")] {
        assert!(calls
            .iter()
            .any(|(pair, step, profile)| pair == &bridge && *step == 1.0 && !profile));
        assert!(!calls
            .iter()
            .any(|(pair, step, _)| pair == &bridge && *step != 1.0));
    }
}

#[test]
fn no_route_diagnostics_distinguish_terminal_access_connectivity_and_budget() {
    let terrain = Terrain {
        resolution: 1.0,
        ridges: &[],
    };
    let rf = strict_settings();
    let settings = mock_settings(OptimizationPriority::MaximumReliability, 1);
    let sites = vec![
        site("A", 0.0, 0.0),
        site("left", 100.0, 0.0),
        site("right", 200.0, 0.0),
        site("B", 300.0, 0.0),
    ];
    let mut fake = FakeRf::default();
    for ((a, b), diagnostic) in [
        (("A", "left"), "no usable links in the search graph: B"),
        (("B", "right"), "disconnected components"),
        (("left", "right"), "over budget: at least 2 routers"),
    ] {
        fake.add(a, b, Some(10.0), Some(10.0));
        let result = RouteOptimizer::new(&terrain, &rf, &settings)
            .with_link_evaluation(&fake)
            .optimize(request(sites.clone()), SearchHooks::default())
            .unwrap();
        assert!(!result.found());
        assert!(
            result
                .diagnostics
                .iter()
                .any(|message| message.contains(diagnostic)),
            "{diagnostic}: {:?}",
            result.diagnostics
        );
    }
}

#[test]
fn low_hop_frontier_beats_sparse_three_router_chain() {
    let terrain = Terrain {
        resolution: 1.0,
        ridges: &[],
    };
    let rf = strict_settings();
    let sites = vec![
        site("A", 0.0, 0.0),
        site("X", 10.0, 0.0),
        site("Y", 20.0, 0.0),
        site("Z", 30.0, 0.0),
        site("L", 0.0, 30.0),
        site("LD", 0.0, 31.0),
        site("R", 100.0, 30.0),
        site("RD", 100.0, 31.0),
        site("B", 100.0, 0.0),
    ];
    let mut fake = FakeRf::default();
    for (a, b) in [
        ("A", "X"),
        ("X", "Y"),
        ("Y", "Z"),
        ("Z", "B"),
        ("A", "L"),
        ("L", "R"),
        ("R", "B"),
    ] {
        fake.add(a, b, Some(10.0), Some(10.0));
    }
    assert!(!screening_pair_indices(&sites, 1, &BTreeSet::new()).contains(&(4, 6)));
    let settings = mock_settings(OptimizationPriority::MinimumRouters, 3);
    let mut snapshots = Vec::new();
    let result = RouteOptimizer::new(&terrain, &rf, &settings)
        .with_link_evaluation(&fake)
        .optimize(
            request(sites),
            SearchHooks {
                solution_progress: Some(&mut |snapshot| snapshots.push(snapshot)),
                ..SearchHooks::default()
            },
        )
        .unwrap();
    assert_eq!(
        result.active_solution().unwrap().primary_path_ids(),
        ["A", "L", "R", "B"]
    );
    assert_eq!(snapshots.len(), 2);
    assert_eq!(snapshots[0].router_count(), 2);
    assert_eq!(snapshots[0].status, SearchStatus::Searching);
    assert!(!snapshots[0].search_complete);
    assert!(snapshots[0]
        .active_solution()
        .unwrap()
        .links
        .iter()
        .all(|link| link.profile.is_some()));
    assert!(snapshots[1].search_complete);
    assert!(snapshots
        .iter()
        .all(|snapshot| snapshot.search_links.is_empty()));
}

#[test]
fn certified_router_headroom_recovers_unscreened_shortcut_within_limit() {
    let terrain = Terrain {
        resolution: 1.0,
        ridges: &[],
    };
    let rf = strict_settings();
    let mut a = site("A", 0.0, 0.0);
    let mut b = site("B", 180.0, 0.0);
    a.rf.antenna_height_m = 20.0;
    b.rf.antenna_height_m = 20.0;
    let mut sites = vec![
        a,
        b,
        site("X", 60.0, 0.0),
        site("P", 90.0, 0.0),
        site("Q", 120.0, 0.0),
    ];
    sites[2].site_quality = -10.0;
    sites[4].site_quality = -10.0;
    for i in 0..75 {
        let point = match i {
            0 => Point { x: 60.0, y: 1.0 },
            1 => Point { x: 120.0, y: 1.0 },
            _ => Point {
                x: i as f64 * 16.0,
                y: 300.0,
            },
        };
        sites.push(site(&format!("D{i:02}"), point.x, point.y));
    }
    let terminals = BTreeSet::from([0, 1]);
    let neighbor_pairs = screening_pair_indices(&sites, 2, &terminals);
    assert!(neighbor_pairs.contains(&(2, 3)) && neighbor_pairs.contains(&(3, 4)));
    assert!(!neighbor_pairs.contains(&(2, 4)));
    let backbone = backbone_site_indices(&sites, 64, &terminals);
    assert!(!backbone.contains(&2) || !backbone.contains(&4));
    let mut fake = FakeRf::default();
    for (from, to) in [("A", "X"), ("X", "P"), ("P", "Q"), ("Q", "B"), ("X", "Q")] {
        fake.add(from, to, Some(10.0), Some(10.0));
    }
    let settings = CandidateSettings {
        maximum_neighbors_per_site: 2,
        ..mock_settings(OptimizationPriority::MaximumReliability, 2)
    };
    let result = RouteOptimizer::new(&terrain, &rf, &settings)
        .with_link_evaluation(&fake)
        .optimize(request(sites), SearchHooks::default())
        .unwrap();
    assert_eq!(
        result.active_solution().unwrap().primary_path_ids(),
        ["A", "X", "Q", "B"]
    );
    assert!(result.alternatives.iter().all(|s| s.router_count() <= 2));
    assert!(!fake.calls.lock().unwrap().contains(&(
        edge("X", "Q"),
        settings.coarse_sample_step_m,
        false
    )));
    assert!(fake.calls.lock().unwrap().contains(&(
        edge("X", "Q"),
        settings.final_sample_step_m,
        true
    )));
}

#[test]
fn certification_retries_many_coarse_winners_and_accepts_coarse_rejections() {
    let terrain = Terrain {
        resolution: 1.0,
        ridges: &[],
    };
    let rf = strict_settings();
    let mut sites = vec![site("A", 0.0, 0.0), site("B", 100.0, 0.0)];
    let mut fake = FakeRf::default();
    for i in 0..30 {
        let router = format!("R{i:02}");
        let margin = 100.0 - i as f64;
        sites.push(site(&router, 50.0, i as f64 + 1.0));
        fake.add("A", &router, Some(margin), Some(margin));
        fake.add(
            &router,
            "B",
            (i < 29).then_some(margin),
            (i == 29).then_some(margin),
        );
    }
    let settings = mock_settings(OptimizationPriority::MaximumReliability, 1);
    let result = RouteOptimizer::new(&terrain, &rf, &settings)
        .with_link_evaluation(&fake)
        .optimize(request(sites), SearchHooks::default())
        .unwrap();
    assert_eq!(
        result.active_solution().unwrap().primary_path_ids(),
        ["A", "R29", "B"]
    );
    for i in 0..30 {
        assert!(fake
            .calls
            .lock()
            .unwrap()
            .contains(&(edge("B", &format!("R{i:02}")), 1.0, false)));
    }
}

#[test]
fn cancellation_stops_before_cartesian_frontier_and_can_keep_certified_snapshot() {
    let terrain = Terrain {
        resolution: 1.0,
        ridges: &[],
    };
    let rf = strict_settings();
    let settings = mock_settings(OptimizationPriority::MinimumRouters, 2);
    let mut sites = vec![site("A", 0.0, 0.0), site("B", 100.0, 0.0)];
    let mut fake = FakeRf::default();
    for i in 0..250 {
        let l = format!("L{i}");
        let r = format!("R{i}");
        sites.push(site(&l, 1.0, i as f64 + 1.0));
        sites.push(site(&r, 99.0, i as f64 + 1.0));
        fake.add("A", &l, Some(10.0), Some(10.0));
        fake.add(&r, "B", Some(10.0), Some(10.0));
    }
    let stop = Cell::new(Cancellation::Continue);
    let result = RouteOptimizer::new(&terrain, &rf, &settings)
        .with_link_evaluation(&fake)
        .optimize(
            request(sites),
            SearchHooks {
                cancellation: Some(&|| stop.get()),
                progress: Some(&mut |progress| {
                    if progress.stage == Stage::CheckingTwoRepeaterSummitLinks {
                        assert_eq!((progress.done, progress.total), (0, 62_500));
                        stop.set(Cancellation::Discard);
                    }
                }),
                ..SearchHooks::default()
            },
        )
        .unwrap();
    assert_eq!(result.status, SearchStatus::Cancelled);
    assert!(!result.found());
    assert_eq!(fake.calls.lock().unwrap().len(), 1_001);

    fake.add("A", "B", Some(20.0), Some(20.0));
    for cancellation in [Cancellation::KeepCertified, Cancellation::Discard] {
        stop.set(Cancellation::Continue);
        let result = RouteOptimizer::new(&terrain, &rf, &settings)
            .with_link_evaluation(&fake)
            .optimize(
                request(vec![site("A", 0.0, 0.0), site("B", 100.0, 0.0)]),
                SearchHooks {
                    cancellation: Some(&|| stop.get()),
                    solution_progress: Some(&mut |mut snapshot| {
                        snapshot.alternatives.clear();
                        stop.set(cancellation);
                    }),
                    ..SearchHooks::default()
                },
            )
            .unwrap();
        assert!(!result.search_complete);
        if cancellation == Cancellation::KeepCertified {
            assert_eq!(result.status, SearchStatus::Stopped);
            assert!(result.active_solution().unwrap().links[0].profile.is_some());
        } else {
            assert_eq!(result.status, SearchStatus::Cancelled);
            assert!(!result.found());
        }
    }
}

#[test]
fn cancellation_prevents_generation_and_prevents_uncertified_profile_publication() {
    let terrain = Terrain {
        resolution: 1.0,
        ridges: &[],
    };
    let rf = strict_settings();
    let settings = mock_settings(OptimizationPriority::MaximumReliability, 2);
    let mut fake = FakeRf::default();
    fake.add("A", "B", Some(10.0), Some(10.0));
    let optimizer = RouteOptimizer::new(&terrain, &rf, &settings).with_link_evaluation(&fake);
    let result = optimizer
        .optimize(
            OptimizationRequest::default(),
            SearchHooks {
                cancellation: Some(&|| Cancellation::Discard),
                ..SearchHooks::default()
            },
        )
        .unwrap();
    assert_eq!(result.status, SearchStatus::Cancelled);
    assert!(fake.calls.lock().unwrap().is_empty());
    for stage in [
        Stage::ValidatingSolutionAlternatives,
        Stage::PreparingSolutionProfiles,
    ] {
        let stop = Cell::new(Cancellation::Continue);
        let result = optimizer
            .optimize(
                request(vec![site("A", 0.0, 0.0), site("B", 100.0, 0.0)]),
                SearchHooks {
                    cancellation: Some(&|| stop.get()),
                    progress: Some(&mut |progress| {
                        if progress.stage == stage {
                            stop.set(Cancellation::KeepCertified);
                        }
                    }),
                    solution_progress: Some(&mut |_| {
                        panic!("no certified snapshot before profile restoration")
                    }),
                },
            )
            .unwrap();
        assert_eq!(result.status, SearchStatus::Cancelled);
        assert!(!result.found());
    }
}

#[test]
fn profiles_cover_all_induced_links_and_cache_is_revision_scoped() {
    let terrain = Terrain {
        resolution: 1.0,
        ridges: &[],
    };
    let rf = strict_settings();
    let settings = mock_settings(OptimizationPriority::MaximumReliability, 3);
    let sites: Vec<_> = ["A", "R1", "R2", "R3", "B"]
        .iter()
        .enumerate()
        .map(|(i, id)| site(id, 10.0 * i as f64, 0.0))
        .collect();
    let mut fake = FakeRf::default();
    for (i, a) in sites.iter().enumerate() {
        for b in &sites[i + 1..] {
            fake.add(&a.rf.id, &b.rf.id, Some(10.0), Some(10.0));
        }
    }
    let cache = LinkMetricsCache::new(100);
    let run = |namespace, revision| {
        RouteOptimizer::new(&terrain, &rf, &settings)
            .with_link_evaluation(&fake)
            .with_cache(&cache, namespace, revision)
            .optimize(request(sites.clone()), SearchHooks::default())
            .unwrap()
    };
    let mut result = run("project", "v1");
    let alternative = result.select_solution(3).unwrap();
    assert_eq!(alternative.router_count(), 3);
    assert_eq!(alternative.links.len(), 10);
    assert!(alternative.links.iter().all(|link| link.profile.is_some()));
    assert_eq!(result.router_count(), 3);
    assert_eq!(cache.stats("project").entries, 20);
    assert_eq!(cache.stats("project").hits, 0);
    let calls = fake.calls.lock().unwrap().len();
    run("project", "v1");
    assert_eq!(cache.stats("project").hits, 20);
    assert!(fake.calls.lock().unwrap()[calls..]
        .iter()
        .all(|call| call.2));
    run("project", "v2");
    run("other", "v1");
    assert_eq!(cache.stats("project").entries, 40);
    assert_eq!(cache.clear("project"), 40);
    assert_eq!(cache.stats("other").entries, 20);
}

struct FunctionRf<F>(F);

impl<F> LinkEvaluation for FunctionRf<F>
where
    F: Fn(&Site, &Site, f64, bool) -> Result<LinkResult, RfError> + Sync,
{
    fn optimistic_margin_db(&self, _: &Site, _: &Site) -> Result<f64, RfError> {
        Ok(100.0)
    }

    fn evaluate(
        &self,
        a: &Site,
        b: &Site,
        step: f64,
        profile: bool,
    ) -> Result<LinkResult, RfError> {
        (self.0)(a, b, step, profile)
    }
}

#[test]
fn zero_width_grid_keeps_only_in_bounds_unexcluded_points_for_filter() {
    let terrain = Terrain {
        resolution: 10.0,
        ridges: &[],
    };
    let settings = CandidateSettings {
        corridor_width_m: 0.0,
        unrestricted_bounding_area: true,
        grid_spacing_m: 10.0,
        ..CandidateSettings::default()
    };
    let observed = Cell::new(0);
    let filter = |points: &[Point]| {
        assert_eq!(
            points,
            [Point { x: 0.0, y: 0.0 }, Point { x: 10.0, y: 0.0 }]
        );
        observed.set(observed.get() + 1);
        Ok(vec![false, true])
    };
    let candidates = generate_candidates(
        &terrain,
        [0.0, 0.0, 30.0, 20.0],
        [Point { x: 0.0, y: 0.0 }, Point { x: 30.0, y: 0.0 }],
        &settings,
        &[vec![
            Point { x: 15.0, y: -1.0 },
            Point { x: 31.0, y: -1.0 },
            Point { x: 31.0, y: 1.0 },
            Point { x: 15.0, y: 1.0 },
        ]],
        Some(&filter),
        &|| false,
    )
    .unwrap();
    assert_eq!(observed.get(), 1);
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].rf.position, Point { x: 10.0, y: 0.0 });
}

#[test]
fn refinement_ranks_bottlenecks_within_disk_and_honors_road_and_site_constraints() {
    struct Surface;
    impl TerrainSource for Surface {
        fn resolution_m(&self) -> f64 {
            10.0
        }
        fn has_surface(&self) -> bool {
            true
        }
        fn sample(&self, points: &[Point], layer: TerrainLayer) -> Result<Vec<f64>, RfError> {
            Ok(points
                .iter()
                .map(|p| match layer {
                    TerrainLayer::GroundDtm if p.y == 20.0 => f64::NAN,
                    TerrainLayer::GroundDtm => p.x / 10.0,
                    TerrainLayer::SurfaceDom => p.x / 10.0 + 3.0,
                })
                .collect())
        }
    }
    let settings = CandidateSettings {
        refine_radius_m: 10.0,
        refine_step_m: 2.0,
        ..Default::default()
    };
    let rf = strict_settings();
    let start = site("A", 0.0, 10.0);
    let base = site("R", 100.0, 10.0);
    let end = site("B", 200.0, 10.0);
    let evaluated = Mutex::new(Vec::new());
    let fake = FunctionRf(|a: &Site, b: &Site, step, _| {
        assert_eq!(step, settings.medium_sample_step_m);
        let trial = if a.id == "R" { a } else { b };
        evaluated.lock().unwrap().push(trial.position);
        assert_eq!(trial.ground_elevation_m, trial.position.x / 10.0);
        assert_eq!(
            trial.surface_elevation_m,
            Some(trial.ground_elevation_m + 3.0)
        );
        let mut link = link(
            &a.id,
            &b.id,
            if trial.position.x == 110.0 && a.id == "R" {
                1.0
            } else {
                10.0
            },
        );
        link.minimum_fresnel_clearance_ratio = if trial.position.y == 0.0 {
            0.5
        } else if trial.position.x == 90.0 {
            2.0
        } else {
            1.0
        };
        Ok(link)
    });
    let optimizer = RouteOptimizer::new(&Surface, &rf, &settings).with_link_evaluation(&fake);
    let refined = optimizer.refine_site(&start, &base, &end).unwrap();
    assert_eq!(refined.rf.position, Point { x: 90.0, y: 10.0 });
    assert_eq!(refined.rf.surface_elevation_m, Some(12.0));
    assert_eq!(evaluated.lock().unwrap().len(), 8);

    let road = |points: &[Point]| {
        assert!(points.iter().all(|p| p.y != 20.0));
        Ok(points.iter().map(|p| p.x >= 100.0).collect())
    };
    let optimizer = optimizer.with_site_filter(&road);
    assert_eq!(
        optimizer
            .refine_site(&start, &base, &end)
            .unwrap()
            .rf
            .position,
        base.rf.position
    );
    let reject_all = |points: &[Point]| Ok(vec![false; points.len()]);
    let calls = evaluated.lock().unwrap().len();
    for protected in [
        PlanningSite {
            locked: true,
            ..base.clone()
        },
        PlanningSite {
            origin: SiteOrigin::Known,
            ..base.clone()
        },
        PlanningSite {
            policy: RouterPolicy::Required,
            ..base.clone()
        },
        start.clone(),
    ] {
        assert_eq!(
            optimizer.refine_site(&start, &protected, &end).unwrap(),
            protected
        );
    }
    assert_eq!(
        optimizer
            .with_site_filter(&reject_all)
            .refine_site(&start, &base, &end)
            .unwrap(),
        base
    );
    assert_eq!(evaluated.lock().unwrap().len(), calls);
}

#[test]
fn mast_reduction_is_sequential_and_opt_in() {
    let terrain = Terrain {
        resolution: 1.0,
        ridges: &[],
    };
    let rf = strict_settings();
    let settings = CandidateSettings {
        optimize_heights: true,
        minimum_router_height_m: 2.0,
        maximum_router_height_m: 5.0,
        final_sample_step_m: 1.0,
        ..Default::default()
    };
    let fake = FunctionRf(|a: &Site, b: &Site, step, _| {
        assert_eq!(step, settings.final_sample_step_m);
        let mut link = link(&a.id, &b.id, 5.0);
        link.valid = match (a.id.as_str(), b.id.as_str()) {
            ("A", "X") => b.antenna_height_m >= 3.0,
            ("X", "Y") => a.antenna_height_m + b.antenna_height_m >= 8.0,
            ("Y", "Z") => true,
            ("Z", "B") => b.antenna_height_m + a.antenna_height_m >= 16.0,
            _ => panic!("unexpected pair"),
        };
        Ok(link)
    });
    let mut route = [
        site("A", 0.0, 0.0),
        site("X", 10.0, 0.0),
        site("Y", 20.0, 0.0),
        site("Z", 30.0, 0.0),
        site("B", 40.0, 0.0),
    ];
    for router in &mut route[1..4] {
        router.rf.antenna_height_m = 9.0;
    }
    RouteOptimizer::new(&terrain, &rf, &settings)
        .with_link_evaluation(&fake)
        .reduce_mast_heights(&mut route)
        .unwrap();
    assert_eq!(
        route.each_ref().map(|site| site.rf.antenna_height_m),
        [10.0, 3.0, 5.0, 9.0, 10.0]
    );

    let actual = RouteOptimizer::new(&terrain, &rf, &settings)
        .optimize(
            request(route[..2].iter().chain(&route[4..]).cloned().collect()),
            SearchHooks::default(),
        )
        .unwrap();
    let router = actual
        .candidates
        .iter()
        .find(|site| site.rf.id == "X")
        .unwrap();
    assert_eq!(router.rf.antenna_height_m, 5.0);
    assert_eq!(router.rf.position, route[1].rf.position);
}

#[test]
fn real_rf_serial_parallel_and_bounded_cache_return_identical_meshes() {
    let terrain = Terrain {
        resolution: 5.0,
        ridges: &[500.0],
    };
    let rf = strict_settings();
    let mut sites = vec![site("A", 0.0, 0.0), site("B", 1_000.0, 0.0)];
    sites.extend((0..22).map(|i| {
        let mut s = site(
            &format!("R{i:02}"),
            500.0 + f64::from(i / 3) * 19.0,
            15.0 + f64::from(i) * 11.0,
        );
        if i < 3 {
            s.rf.ground_elevation_m = 500.0;
        }
        s
    }));
    let cache = LinkMetricsCache::new(7);
    let run = |workers, namespace| {
        let settings = CandidateSettings {
            parallel_workers: workers,
            maximum_neighbors_per_site: 6,
            maximum_solution_routers: 1,
            final_sample_step_m: 5.0,
            priority: OptimizationPriority::MaximumReliability,
            ..Default::default()
        };
        let mut progress = Vec::new();
        let result = RouteOptimizer::new(&terrain, &rf, &settings)
            .with_cache(&cache, namespace, "v1")
            .optimize(
                request(sites.clone()),
                SearchHooks {
                    progress: Some(&mut |p| progress.push(p)),
                    ..Default::default()
                },
            )
            .unwrap();
        (result, progress)
    };
    let (serial, serial_progress) = run(1, "serial");
    assert!(serial.found());
    for workers in [2, 4, 0, 4] {
        let (parallel, progress) = run(workers, "parallel");
        assert_eq!(progress, serial_progress);
        assert_eq!(parallel.active_index, serial.active_index);
        assert_eq!(parallel.search_links, serial.search_links);
        assert_eq!(parallel.candidates, serial.candidates);
        assert_eq!(parallel.alternatives.len(), serial.alternatives.len());
        for (parallel, serial) in parallel.alternatives.iter().zip(&serial.alternatives) {
            assert_eq!(parallel.sites, serial.sites);
            assert_eq!(parallel.links, serial.links);
            assert_eq!(parallel.client_paths, serial.client_paths);
            assert_eq!(parallel.achieved_path_count, serial.achieved_path_count);
            assert_eq!(parallel.diagnostics, serial.diagnostics);
        }
        assert_eq!(
            cache.stats("parallel").entries + cache.stats("serial").entries,
            7
        );
        assert!(cache.stats("parallel").evictions > 0);
    }
}

#[derive(Default)]
struct Gate {
    open: Mutex<bool>,
    changed: Condvar,
}

impl Gate {
    fn wait(&self) {
        let open = self.open.lock().unwrap();
        let (open, _) = self
            .changed
            .wait_timeout_while(open, Duration::from_secs(5), |open| !*open)
            .unwrap();
        assert!(*open, "controller did not release RF evaluation");
    }

    fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.changed.notify_all();
    }
}

#[test]
fn parallel_screen_cancels_while_workers_are_busy_and_dispatches_no_more_work() {
    let terrain = Terrain {
        resolution: 1.0,
        ridges: &[],
    };
    let rf = strict_settings();
    let settings = CandidateSettings {
        parallel_workers: 3,
        ..mock_settings(OptimizationPriority::MaximumReliability, 1)
    };
    let stop = AtomicBool::new(false);
    let gate = Gate::default();
    let calls = AtomicUsize::new(0);
    let (started, work) = mpsc::channel();
    let (observed, cancelled) = mpsc::channel();
    let fake = FunctionRf(|a: &Site, b: &Site, step, profile| {
        assert_eq!((step, profile), (settings.coarse_sample_step_m, false));
        calls.fetch_add(1, Relaxed);
        started.send(thread::current().id()).unwrap();
        gate.wait();
        Ok(link(&a.id, &b.id, 10.0))
    });
    thread::scope(|scope| {
        let job = scope.spawn(|| {
            let main = thread::current().id();
            let cancelled = || {
                assert_eq!(thread::current().id(), main);
                if stop.load(Relaxed) {
                    observed.send(()).unwrap();
                    Cancellation::Discard
                } else {
                    Cancellation::Continue
                }
            };
            RouteOptimizer::new(&terrain, &rf, &settings)
                .with_link_evaluation(&fake)
                .optimize(
                    request(vec![
                        site("A", 0.0, 0.0),
                        site("X", 10.0, 0.0),
                        site("Y", 20.0, 0.0),
                        site("B", 30.0, 0.0),
                    ]),
                    SearchHooks {
                        cancellation: Some(&cancelled),
                        progress: Some(&mut |p| {
                            assert_eq!(thread::current().id(), main);
                            if p.stage == Stage::EvaluatingRfLinks {
                                assert_eq!((p.done, p.total), (0, 6));
                            }
                        }),
                        ..Default::default()
                    },
                )
                .unwrap()
        });
        let workers: Vec<_> = (0..3)
            .map(|_| work.recv_timeout(Duration::from_secs(3)).unwrap())
            .collect();
        assert_ne!(workers[0], workers[1]);
        assert_ne!(workers[0], workers[2]);
        assert_ne!(workers[1], workers[2]);
        stop.store(true, Relaxed);
        cancelled.recv_timeout(Duration::from_secs(3)).unwrap();
        gate.release();
        assert_eq!(job.join().unwrap().status, SearchStatus::Cancelled);
    });
    assert_eq!(calls.load(Relaxed), 3);
}

#[test]
fn concurrent_jobs_share_one_scalar_calculation_per_key() {
    let terrain = Terrain {
        resolution: 1.0,
        ridges: &[],
    };
    let rf = strict_settings();
    let settings = CandidateSettings {
        parallel_workers: 1,
        ..mock_settings(OptimizationPriority::MaximumReliability, 0)
    };
    let cache = LinkMetricsCache::new(2);
    let builtin = LinkEvaluator::new(&terrain, &rf);
    let gate = Gate::default();
    let (started, work) = mpsc::channel();
    let fake = FunctionRf(|a: &Site, b: &Site, step, profile| {
        if step == settings.coarse_sample_step_m {
            started.send(()).unwrap();
            gate.wait();
        }
        LinkEvaluation::evaluate(&builtin, a, b, step, profile)
    });
    let optimizer = || {
        RouteOptimizer::new(&terrain, &rf, &settings)
            .with_link_evaluation(&fake)
            .with_cache(&cache, "shared", "v1")
    };
    let inputs = || request(vec![site("A", 0.0, 0.0), site("B", 100.0, 0.0)]);
    let stop = AtomicBool::new(false);
    thread::scope(|scope| {
        let first = scope.spawn(|| {
            optimizer()
                .optimize(inputs(), SearchHooks::default())
                .unwrap()
        });
        work.recv_timeout(Duration::from_secs(3)).unwrap();

        let (waiting, waiter) = mpsc::channel();
        let (ended, outcome) = mpsc::channel();
        let stop = &stop;
        scope.spawn(move || {
            let polls = Cell::new(None);
            let result = optimizer()
                .optimize(
                    inputs(),
                    SearchHooks {
                        progress: Some(&mut |p| {
                            if p.stage == Stage::EvaluatingRfLinks && p.done == 0 {
                                polls.set(Some(0));
                            }
                        }),
                        cancellation: Some(&|| {
                            if let Some(count) = polls.get() {
                                // Let the waiter poll again while the owner still holds the key.
                                polls.set(Some(count + 1));
                                if count == 2 {
                                    waiting.send(()).unwrap();
                                }
                            }
                            if stop.load(Relaxed) {
                                Cancellation::Discard
                            } else {
                                Cancellation::Continue
                            }
                        }),
                        ..Default::default()
                    },
                )
                .unwrap();
            ended.send(result.status).unwrap();
        });
        waiter.recv_timeout(Duration::from_secs(3)).unwrap();
        stop.store(true, Relaxed);
        assert_eq!(
            outcome.recv_timeout(Duration::from_secs(3)).unwrap(),
            SearchStatus::Cancelled
        );

        let (ready, peer) = mpsc::channel();
        let second = scope.spawn(move || {
            optimizer()
                .optimize(
                    inputs(),
                    SearchHooks {
                        progress: Some(&mut |p| {
                            if p.stage == Stage::EvaluatingRfLinks && p.done == 0 {
                                ready.send(()).unwrap();
                            }
                        }),
                        ..Default::default()
                    },
                )
                .unwrap()
        });
        peer.recv_timeout(Duration::from_secs(3)).unwrap();
        let duplicate = work.recv_timeout(Duration::from_millis(50)).is_ok();
        gate.release();
        assert!(!duplicate);
        let first = first.join().unwrap();
        let second = second.join().unwrap();
        assert!(first.found());
        assert_eq!(first.links(), second.links());
    });
    assert_eq!(
        cache.stats("shared"),
        CacheStats {
            hits: 2,
            misses: 2,
            evictions: 0,
            entries: 2,
            capacity: 2
        }
    );
}

#[test]
fn failed_cache_fill_releases_the_key_for_a_retry() {
    let terrain = Terrain {
        resolution: 1.0,
        ridges: &[],
    };
    let rf = strict_settings();
    let settings = CandidateSettings {
        parallel_workers: 1,
        ..mock_settings(OptimizationPriority::MaximumReliability, 0)
    };
    let cache = LinkMetricsCache::new(2);
    let builtin = LinkEvaluator::new(&terrain, &rf);
    let first = AtomicBool::new(true);
    let fake = FunctionRf(|a: &Site, b: &Site, step, profile| {
        if first.swap(false, Relaxed) {
            return Err(RfError::InvalidInput("transient test error"));
        }
        LinkEvaluation::evaluate(&builtin, a, b, step, profile)
    });
    let optimizer = RouteOptimizer::new(&terrain, &rf, &settings)
        .with_link_evaluation(&fake)
        .with_cache(&cache, "project", "v1");
    let run = || {
        optimizer.optimize(
            request(vec![site("A", 0.0, 0.0), site("B", 100.0, 0.0)]),
            SearchHooks::default(),
        )
    };
    assert!(matches!(
        run(),
        Err(OptimizationError::Rf(RfError::InvalidInput(
            "transient test error"
        )))
    ));
    assert_eq!(cache.stats("project").entries, 0);
    assert!(run().unwrap().found());
    assert_eq!(cache.stats("project").misses, 3);
    assert_eq!(cache.stats("project").entries, 2);
}

#[test]
fn parallel_screen_can_stop_with_the_prior_certified_snapshot() {
    let terrain = Terrain {
        resolution: 1.0,
        ridges: &[],
    };
    let rf = strict_settings();
    let settings = CandidateSettings {
        parallel_workers: 2,
        ..mock_settings(OptimizationPriority::MinimumRouters, 1)
    };
    let stop = Cell::new(Cancellation::Continue);
    let result = RouteOptimizer::new(&terrain, &rf, &settings)
        .optimize(
            request(vec![
                site("A", 0.0, 0.0),
                site("X", 20.0, 0.0),
                site("Y", 40.0, 0.0),
                site("B", 60.0, 0.0),
            ]),
            SearchHooks {
                cancellation: Some(&|| stop.get()),
                progress: Some(&mut |p| {
                    if p.stage == Stage::EvaluatingRfLinks && p.done == 1 {
                        stop.set(Cancellation::KeepCertified);
                    }
                }),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(result.status, SearchStatus::Stopped);
    assert!(!result.search_complete);
    assert_eq!(
        result.active_solution().unwrap().primary_path_ids(),
        ["A", "B"]
    );
    assert!(result.links()[0].profile.is_some());
}

fn exhaustive_settings(maximum_solution_routers: usize) -> CandidateSettings {
    CandidateSettings {
        maximum_solution_routers,
        final_sample_step_m: 1.0,
        ..CandidateSettings::default()
    }
}

#[test]
fn required_router_is_a_via_point_on_the_primary_route() {
    let terrain = Terrain {
        resolution: 1.0,
        ridges: &[],
    };
    let rf = strict_settings();
    let mut required = site("R", 200.0, 50.0);
    required.policy = RouterPolicy::Required;
    let sites = vec![
        site("A", 0.0, 0.0),
        site("X", 100.0, 0.0),
        required,
        site("Y", 300.0, 0.0),
        site("W", 150.0, 80.0),
        site("B", 400.0, 0.0),
    ];
    let mut fake = FakeRf::default();
    // X alone would bridge A and B and leave R as a spur.
    fake.add("A", "X", Some(20.0), Some(20.0));
    fake.add("X", "B", Some(20.0), Some(20.0));
    fake.add("X", "R", Some(20.0), Some(20.0));
    fake.add("R", "Y", Some(15.0), Some(15.0));
    fake.add("Y", "B", Some(15.0), Some(15.0));
    // A coarse shortcut through W that fails at final resolution.
    fake.add("A", "W", Some(30.0), Some(30.0));
    fake.add("W", "R", Some(40.0), None);
    let result = RouteOptimizer::new(&terrain, &rf, &exhaustive_settings(4))
        .with_link_evaluation(&fake)
        .optimize(request(sites), SearchHooks::default())
        .unwrap();
    let solution = result
        .active_solution()
        .unwrap_or_else(|| panic!("no route: {:?}", result.diagnostics));
    assert_eq!(solution.primary_path_ids(), ["A", "X", "R", "Y", "B"]);
    assert!(solution.router_ids.contains(&"R".into()));
    assert!(solution
        .links
        .iter()
        .all(|link| link.valid && link.profile.is_some()));
    assert!(!solution
        .links
        .iter()
        .any(|link| edge(&link.source_id, &link.target_id) == edge("W", "R")));
    assert!(result
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.contains("passes through all 1 required router")));
}

#[test]
fn exhaustive_search_finds_long_hops_outside_nearest_neighbours() {
    let terrain = Terrain {
        resolution: 1.0,
        ridges: &[],
    };
    let rf = strict_settings();
    // Two distant summits connect A and B; a crowd of nearby decoys only offers
    // weak short hops. Nearest-neighbour screening would never pair the summits.
    let mut sites = vec![site("A", 0.0, 0.0), site("north", 100.0, 0.0)];
    for index in 0..40 {
        sites.push(site(&format!("decoy{index:02}"), 100.0 + index as f64, 5.0));
    }
    sites.push(site("south", 900.0, 0.0));
    sites.push(site("B", 1_000.0, 0.0));
    let mut fake = FakeRf::default();
    fake.add("A", "north", Some(20.0), Some(20.0));
    fake.add("north", "south", Some(18.0), Some(18.0));
    fake.add("south", "B", Some(20.0), Some(20.0));
    fake.add("A", "decoy00", Some(2.0), Some(2.0));
    fake.add("decoy00", "B", Some(1.0), Some(1.0));
    let mut settings = exhaustive_settings(4);
    settings.maximum_neighbors_per_site = 2;
    let result = RouteOptimizer::new(&terrain, &rf, &settings)
        .with_link_evaluation(&fake)
        .optimize(request(sites), SearchHooks::default())
        .unwrap();
    let routes: Vec<_> = result
        .alternatives
        .iter()
        .map(|solution| solution.primary_path_ids().to_vec())
        .collect();
    assert_eq!(
        routes,
        [
            vec!["A".to_string(), "decoy00".into(), "B".into()],
            vec!["A".into(), "north".into(), "south".into(), "B".into()],
        ]
    );
    // The fewest-router route is active even though the two-router route is stronger.
    assert_eq!(result.router_count(), 1);
}
