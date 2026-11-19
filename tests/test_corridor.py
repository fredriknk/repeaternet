import numpy as np

from rf_router_planner.models.settings import CandidateSettings
from rf_router_planner.models.site import Site, SiteKind
from rf_router_planner.terrain.candidate_sites import generate_candidates
from rf_router_planner.terrain.corridor import corridor_geometry, inside_corridor
from rf_router_planner.terrain.kartverket import KartverketProvider, RouteCorridor
from rf_router_planner.terrain.raster import ArrayTerrain
from rf_router_planner.web import within_corridor_latlon

A, B, W = (0.0, 0.0), (10_000.0, 0.0), 2_000.0


def test_corridor_extends_full_width_beyond_both_endpoints() -> None:
    xs = np.array([5_000, -1_900, -1_900, 11_900, -2_100, 5_000, 12_100])
    ys = np.array([1_900, 1_900, -1_900, 1_900, 0, 2_100, 0])
    assert inside_corridor(xs, ys, A, B, W).tolist() == [
        True,
        True,
        True,
        True,
        False,
        False,
        False,
    ]


def test_corridor_is_rotation_invariant() -> None:
    angle = np.radians(37)
    rotate = np.array([[np.cos(angle), -np.sin(angle)], [np.sin(angle), np.cos(angle)]])
    corner = rotate @ np.array([-1_900.0, 1_900.0])
    b = tuple(rotate @ np.array(B))
    assert inside_corridor(np.array([corner[0]]), np.array([corner[1]]), A, b, W)[0]
    assert corridor_geometry([A, b], W).contains(corridor_geometry([A, b], W).centroid)


def test_candidates_include_full_width_corners_behind_endpoints() -> None:
    terrain = ArrayTerrain(np.zeros((61, 181)), origin_x=-4_000, origin_y=-3_000, resolution_m=100)
    a = Site("A", *A, kind=SiteKind.ENDPOINT_A)
    b = Site("B", *B, kind=SiteKind.ENDPOINT_B)
    settings = CandidateSettings(
        corridor_width_m=W,
        grid_spacing_m=500,
        cell_size_m=500,
        candidates_per_cell=1,
        maximum_candidates=2_000,
        refine_radius_m=0,
    )
    sites = generate_candidates(terrain, a, b, settings)[1:-1]
    assert any(site.x < -1_000 and abs(site.y) > 1_000 for site in sites)
    assert all(-W <= site.x <= B[0] + W and abs(site.y) <= W for site in sites)


def test_terrain_download_corridor_uses_square_ends() -> None:
    shape = KartverketProvider._corridor_shape(RouteCorridor((A, B), W))
    assert shape.bounds == (-W, -W, B[0] + W, W)


def test_meshcore_lookup_uses_square_ends() -> None:
    start, end = (60.0, 10.0), (60.0, 10.2)
    behind_corner = (60.0 + 1_900 / 111_320, 10.0 - 1_900 / (111_320 * np.cos(np.radians(60))))
    assert within_corridor_latlon(behind_corner, start, end, W)
    assert not within_corridor_latlon((60.0 + 2_100 / 111_320, 10.1), start, end, W)
