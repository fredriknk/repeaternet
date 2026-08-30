import numpy as np

from rf_router_planner.models.settings import CandidateSettings, RFSettings, ValidationMode
from rf_router_planner.models.site import Site, SiteKind
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
