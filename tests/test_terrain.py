import numpy as np
import pytest

from rf_router_planner.models.site import Site
from rf_router_planner.terrain.raster import ArrayTerrain
from rf_router_planner.terrain.sampling import sample_profile


def test_array_terrain_bilinear_interpolation_and_edges() -> None:
    terrain = ArrayTerrain(np.array([[0.0, 10.0], [20.0, 30.0]]), resolution_m=10)
    assert terrain.sample(np.array([5.0]), np.array([5.0]))[0] == pytest.approx(15)
    assert terrain.sample(np.array([10.0]), np.array([10.0]))[0] == pytest.approx(30)


def test_profile_uses_dom_as_obstruction_but_dtm_as_ground() -> None:
    dtm = np.zeros((11, 101))
    dom = dtm.copy()
    dom[:, 50] = 20
    terrain = ArrayTerrain(dtm, resolution_m=10, dom=dom)
    source = Site("A", 0, 50, ground_elevation_m=0, antenna_height_m=10)
    target = Site("B", 1000, 50, ground_elevation_m=0, antenna_height_m=10)
    profile = sample_profile(terrain, source, target, 869.5, sample_step_m=10)
    assert profile.surface_available
    assert profile.surface_elevation_m[50] == 20
    assert profile.dtm_elevation_m[50] == 0
    assert profile.clearance_m[50] < 0


def test_profile_falls_back_explicitly_to_dtm() -> None:
    terrain = ArrayTerrain(np.zeros((3, 11)), resolution_m=10)
    profile = sample_profile(
        terrain,
        Site("A", 0, 10, antenna_height_m=10),
        Site("B", 100, 10, antenna_height_m=10),
        869.5,
    )
    assert not profile.surface_available
    np.testing.assert_array_equal(profile.surface_elevation_m, profile.dtm_elevation_m)
