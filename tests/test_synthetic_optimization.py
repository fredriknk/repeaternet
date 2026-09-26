import numpy as np

from rf_router_planner.models.settings import CandidateSettings, RFSettings, ValidationMode
from rf_router_planner.models.site import Site, SiteKind, SiteOrigin
from rf_router_planner.optimization.optimizer import RouteOptimizer
from rf_router_planner.rf.propagation import LinkEvaluator
from rf_router_planner.terrain.candidate_sites import generate_candidates
from rf_router_planner.terrain.raster import ArrayTerrain


def endpoint(name: str, x: float, ground: float = 0, height: float = 10) -> Site:
    kind = SiteKind.ENDPOINT_A if name == "A" else SiteKind.ENDPOINT_B
    return Site(name, x, 10, kind=kind, ground_elevation_m=ground, antenna_height_m=height)


def strict_settings() -> RFSettings:
    settings = RFSettings(receiver_sensitivity_dbm=-150, fade_margin_db=0)
    settings.validation_mode = ValidationMode.STRICT_LOS
    settings.required_fresnel_clearance = 0
    settings.router.height_agl_m = 10
    return settings


def candidate_settings() -> CandidateSettings:
    return CandidateSettings(
        coarse_sample_step_m=20,
        medium_sample_step_m=10,
        final_sample_step_m=10,
        refine_radius_m=0,
        refine_step_m=10,
    )


def test_flat_ground_requires_no_router() -> None:
    terrain = ArrayTerrain(np.zeros((3, 1001)), resolution_m=10)
    a, b = endpoint("A", 0, height=20), endpoint("B", 10_000, height=20)
    result = RouteOptimizer(terrain, strict_settings(), candidate_settings()).optimize(
        a, b, candidates=[a, b]
    )
    assert result.found
    assert result.router_count == 0


def test_one_mountain_uses_summit_router() -> None:
    data = np.zeros((3, 1001))
    data[:, 500] = 500
    terrain = ArrayTerrain(data, resolution_m=10)
    a, b = endpoint("A", 0), endpoint("B", 10_000)
    summit = Site(
        "C1", 5_000, 10, kind=SiteKind.CANDIDATE, ground_elevation_m=500, antenna_height_m=10
    )
    result = RouteOptimizer(terrain, strict_settings(), candidate_settings()).optimize(
        a, b, candidates=[a, summit, b]
    )
    assert result.found
    assert result.router_count == 1


def test_optional_known_router_is_considered_but_not_forced() -> None:
    terrain = ArrayTerrain(np.zeros((3, 101)), resolution_m=10)
    a, b = endpoint("A", 0), endpoint("B", 1_000)
    known = Site(
        "K-abcdef1234",
        500,
        10,
        kind=SiteKind.ROUTER,
        antenna_height_m=10,
        origin=SiteOrigin.KNOWN,
        required=True,
        locked=True,
    )

    result = RouteOptimizer(terrain, strict_settings(), candidate_settings()).optimize(
        a, b, optional_routers=[known]
    )

    assert result.found
    assert result.router_count == 0
    included = next(site for site in result.candidates if site.id == known.id)
    assert included.origin == SiteOrigin.KNOWN
    assert included.locked
    assert not included.required
    assert any(known.id in solution.router_ids for solution in result.alternatives)


def test_two_separated_ridges_require_two_routers() -> None:
    data = np.zeros((3, 1201))
    data[:, 400] = data[:, 800] = 500
    terrain = ArrayTerrain(data, resolution_m=10)
    a, b = endpoint("A", 0), endpoint("B", 12_000)
    r1 = Site("C1", 4_000, 10, kind=SiteKind.CANDIDATE, ground_elevation_m=500, antenna_height_m=10)
    r2 = Site("C2", 8_000, 10, kind=SiteKind.CANDIDATE, ground_elevation_m=500, antenna_height_m=10)
    result = RouteOptimizer(terrain, strict_settings(), candidate_settings()).optimize(
        a, b, candidates=[a, r1, r2, b]
    )
    assert result.found
    assert result.router_count == 2


def test_candidate_sampling_covers_the_configured_refinement_radius() -> None:
    terrain = ArrayTerrain(np.zeros((5, 9)), resolution_m=250)
    a, b = endpoint("A", 0), endpoint("B", 2_000)
    settings = CandidateSettings(
        corridor_width_m=1_000,
        grid_spacing_m=1_000,
        cell_size_m=1_000,
        candidates_per_cell=10,
        maximum_candidates=100,
        refine_radius_m=250,
    )

    candidates = generate_candidates(terrain, a, b, settings)

    assert any(site.x == 250 for site in candidates[1:-1])


def test_los_can_exist_while_fresnel_fails() -> None:
    terrain = ArrayTerrain(np.zeros((3, 101)), resolution_m=10)
    settings = strict_settings()
    settings.required_fresnel_clearance = 0.6
    link = LinkEvaluator(terrain, settings).evaluate(
        endpoint("A", 0, height=2), endpoint("B", 1000, height=2), 10
    )
    assert link.los_clear
    assert not link.fresnel_clear
    assert not link.valid


def test_diffraction_mode_accepts_link_strict_mode_rejects() -> None:
    data = np.zeros((3, 101))
    data[:, 50] = 12
    terrain = ArrayTerrain(data, resolution_m=10)
    a, b = endpoint("A", 0), endpoint("B", 1000)
    propagation = strict_settings()
    propagation.validation_mode = ValidationMode.PROPAGATION
    propagation_link = LinkEvaluator(terrain, propagation).evaluate(a, b, 10)
    strict_link = LinkEvaluator(terrain, strict_settings()).evaluate(a, b, 10)
    assert propagation_link.diffraction_loss_db > 0
    assert propagation_link.valid
    assert not strict_link.valid


def test_multi_client_mesh_keeps_manual_router_and_exact_count_alternatives() -> None:
    terrain = ArrayTerrain(np.zeros((101, 101)), resolution_m=100)
    a = Site("A", 0, 0, kind=SiteKind.ENDPOINT_A, antenna_height_m=20)
    b = Site("B", 10_000, 0, kind=SiteKind.ENDPOINT_B, antenna_height_m=20)
    c = Site("C3", 5_000, 8_000, kind=SiteKind.CLIENT, antenna_height_m=20)
    manual = Site(
        "M1",
        5_000,
        3_000,
        kind=SiteKind.ROUTER,
        antenna_height_m=20,
        origin=SiteOrigin.MANUAL,
        required=True,
        locked=True,
    )
    optional = Site(
        "N1", 5_000, 5_000, kind=SiteKind.CANDIDATE, antenna_height_m=20
    )
    candidates = [a, b, c, manual, optional]
    settings = candidate_settings()
    settings.maximum_solution_routers = 2

    result = RouteOptimizer(terrain, strict_settings(), settings).optimize(
        a,
        b,
        candidates=candidates,
        clients=[a, b, c],
        required_routers=[manual],
    )

    assert result.found
    assert [solution.router_count for solution in result.alternatives] == [1, 2]
    assert all("M1" in solution.router_ids for solution in result.alternatives)
    assert set(result.active_solution.client_ids) == {"A", "B", "C3"}  # type: ignore[union-attr]
    assert len(result.active_solution.links) == 6  # type: ignore[union-attr]


def test_selected_mesh_finally_checks_all_node_pairs() -> None:
    terrain = ArrayTerrain(np.zeros((3, 101)), resolution_m=100)
    a = Site("A", 0, 100, kind=SiteKind.ENDPOINT_A, antenna_height_m=20)
    b = Site("B", 10_000, 100, kind=SiteKind.ENDPOINT_B, antenna_height_m=20)
    routers = [
        Site(
            f"N{index}",
            index * 2_500,
            100,
            kind=SiteKind.CANDIDATE,
            antenna_height_m=20,
        )
        for index in range(1, 4)
    ]
    settings = candidate_settings()
    settings.maximum_solution_routers = 3
    settings.maximum_neighbors_per_site = 1

    result = RouteOptimizer(terrain, strict_settings(), settings).optimize(
        a, b, candidates=[a, *routers, b]
    )

    three_router_solution = next(
        solution for solution in result.alternatives if solution.router_count == 3
    )
    assert len(three_router_solution.sites) == 5
    assert len(three_router_solution.links) == 10
