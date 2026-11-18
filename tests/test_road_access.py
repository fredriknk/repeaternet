import math
from threading import Event
from unittest.mock import Mock

import numpy as np
import pytest
import requests
from pyproj import Transformer
from shapely.geometry import LineString, box

from rf_router_planner.models.network import NetworkSolution
from rf_router_planner.models.settings import CandidateSettings, RFSettings, RoadAccessClasses
from rf_router_planner.models.site import Site, SiteKind, SiteOrigin
from rf_router_planner.optimization.optimizer import OptimizationResult, RouteOptimizer
from rf_router_planner.terrain.candidate_sites import generate_candidates
from rf_router_planner.terrain.raster import ArrayTerrain
from rf_router_planner.terrain.roads import RoadAccessIndex, RoadLoadCancelled, TiledRoadAccess
from rf_router_planner.web import result_payload

CRS = "EPSG:25833"
FORWARD = Transformer.from_crs(4326, CRS, always_xy=True)
REVERSE = Transformer.from_crs(CRS, 4326, always_xy=True)


def overpass_ways(monkeypatch, ways=()):
    """Select full ways intersecting the query box, as Overpass does."""
    ways = [LineString(way) for way in ways]

    def request(endpoint, *, data, headers, timeout):
        south, west, north, east = map(
            float, data["data"].split(");out ", 1)[0].rsplit("(", 1)[1].split(",")
        )
        bounds = box(west, south, east, north)
        return Mock(json=lambda: {
            "elements": [
                {
                    "type": "way",
                    "id": index,
                    "nodes": list(range(len(way.coords))),
                    "geometry": [{"lon": lon, "lat": lat} for lon, lat in way.coords],
                }
                for index, way in enumerate(ways)
                if way.intersects(bounds)
            ]
        })

    post = Mock(side_effect=request)
    monkeypatch.setattr(requests, "post", post)
    return post


def road_access(tmp_path, distance=100, classes=RoadAccessClasses.PUBLIC_ROADS, **kwargs):
    return TiledRoadAccess(CRS, classes, distance, tmp_path, **kwargs)


def projected_points(longitudes, latitude=60.05):
    return FORWARD.transform(np.asarray(longitudes), np.full(len(longitudes), latitude))


def test_road_access_index_distances():
    index = RoadAccessIndex([LineString([(0, 0), (2000, 0)])], 1000)
    xs = np.array([1000, 1000, 1000, 1000, 2500, 2600])
    ys = np.array([0, 999, 1000, 1001, 0, 900])

    np.testing.assert_array_equal(index.allowed(xs, ys), [True, True, True, False, True, False])
    np.testing.assert_array_equal(
        RoadAccessIndex([], 1000).allowed(xs, ys), np.zeros(xs.size, dtype=bool)
    )


def test_overpass_skel_geometry_and_per_tile_cache(tmp_path, monkeypatch):
    endpoint = "https://overpass.example/api/interpreter"
    monkeypatch.setenv("RF_PLANNER_OVERPASS_URL", endpoint)
    post = overpass_ways(monkeypatch, [[(15.1, 60.01), (15.1, 60.09)]])
    xs, ys = projected_points([15.1, 15.3])
    progress = Mock()
    first = road_access(tmp_path, progress=progress).allowed(xs, ys)
    np.testing.assert_array_equal(first, [True, False])
    assert post.call_count == 2
    assert [call.args for call in progress.call_args_list] == [
        ("Loading road network", 0, 2),
        ("Loading road network", 1, 2),
        ("Loading road network", 2, 2),
    ]
    call = post.call_args_list[0]
    assert call.args == (endpoint,)
    assert call.kwargs["headers"] == {"User-Agent": "RepeaterNet (road access planner)"}
    assert call.kwargs["timeout"][1] > 180
    query = call.kwargs["data"]["data"]
    assert query.startswith('[out:json][timeout:180];way["highway"~"^(')
    assert "motorway_link" in query and "service" in query and "track" not in query
    assert query.endswith("(60.0,15.0,60.1,15.2);out skel geom;")
    np.testing.assert_array_equal(road_access(tmp_path).allowed(xs, ys), first)
    assert post.call_count == 2

    road_access(tmp_path).allowed(*projected_points([15.3, 15.5]))
    assert post.call_count == 3
    assert "(60.0,15.4,60.1,15.6)" in post.call_args.kwargs["data"]["data"]
    assert len(list(tmp_path.iterdir())) == 3

    road_access(tmp_path, classes=RoadAccessClasses.ALL_DRIVABLE).allowed(xs[:1], ys[:1])
    assert post.call_count == 4
    assert "|track)$" in post.call_args.kwargs["data"]["data"]
    monkeypatch.setenv("RF_PLANNER_OVERPASS_URL", "https://other.example/api/interpreter")
    road_access(tmp_path).allowed(xs[:1], ys[:1])
    assert post.call_count == 5


def test_tile_buffer_loads_road_across_edge(tmp_path, monkeypatch):
    post = overpass_ways(monkeypatch, [[(15.1999, 60.04), (15.1999, 60.06)]])
    allowed = road_access(tmp_path, distance=50).allowed(*projected_points([15.2001]))

    np.testing.assert_array_equal(allowed, [True])
    assert [call.kwargs["data"]["data"].split("]")[-1] for call in post.call_args_list] == [
        "(60.0,15.0,60.1,15.2);out skel geom;",
        "(60.0,15.2,60.1,15.4);out skel geom;",
    ]


def test_diagonal_candidate_search_loads_fewer_tiles_with_identical_results(tmp_path, monkeypatch):
    ax, ay = 420000, 6650000
    bx, by = 520000, 6750000
    terrain = ArrayTerrain(np.zeros((105, 105)), ax - 2000, ay - 2000, 1000)
    a = Site("A", ax, ay, kind=SiteKind.ENDPOINT_A)
    b = Site("B", bx, by, kind=SiteKind.ENDPOINT_B)
    settings = CandidateSettings(corridor_width_m=1500, grid_spacing_m=1000, refine_radius_m=0)
    roads = [
        [REVERSE.transform(ax + shift, ay), REVERSE.transform(bx + shift, by)]
        for shift in (150, 950, 20000)
    ]
    post = overpass_ways(monkeypatch, roads)
    distance = 400
    tiled = generate_candidates(
        terrain, a, b, settings, site_filter=road_access(tmp_path, distance).allowed
    )
    padding = settings.corridor_width_m + distance
    west, south, east, north = REVERSE.transform_bounds(
        ax - padding, ay - padding, bx + padding, by + padding
    )
    legacy_index = RoadAccessIndex(
        [
            LineString([FORWARD.transform(*point) for point in road])
            for road in roads
            if LineString(road).intersects(box(west, south, east, north))
        ],
        distance,
    )
    whole_rectangle = generate_candidates(terrain, a, b, settings, site_filter=legacy_index.allowed)
    assert tiled == whole_rectangle
    assert len(tiled) > 2
    rectangle_tiles = (math.floor(north * 10) - math.floor(south * 10) + 1) * (
        math.floor(east * 5) - math.floor(west * 5) + 1
    )
    assert (post.call_count, rectangle_tiles) == (28, 121)


def test_nodata_and_excluded_points_do_not_load_tiles(tmp_path, monkeypatch):
    post = overpass_ways(monkeypatch)
    middle_x, y = FORWARD.transform(15.3, 60.05)
    # Three east-west tiles: finite terrain, sea, then excluded terrain.
    x = middle_x - 11000
    terrain = ArrayTerrain(
        np.tile([0, 0, np.nan, np.nan, 0, 0], (2, 1)), x, y, 5000
    )
    a = Site("A", x, y, kind=SiteKind.ENDPOINT_A)
    b = Site("B", x + 25000, y, kind=SiteKind.ENDPOINT_B)
    settings = CandidateSettings(
        corridor_width_m=0, grid_spacing_m=5000, refine_radius_m=0,
        unrestricted_bounding_area=True,
    )
    exclusions = [[
        (x + 18000, y - 100), (x + 26000, y - 100),
        (x + 26000, y + 100), (x + 18000, y + 100),
    ]]
    assert generate_candidates(
        terrain, a, b, settings, exclusions=exclusions, site_filter=road_access(tmp_path).allowed
    ) == [a, b]
    post.assert_called_once()
    assert "(60.0,15.0,60.1,15.2)" in post.call_args.kwargs["data"]["data"]


def http_failure(status, retry_after=None):
    response = requests.Response()
    response.status_code = status
    if retry_after is not None:
        response.headers["Retry-After"] = str(retry_after)
    return requests.HTTPError(response=response)


def test_overpass_retries_connection_error_and_honours_retry_after(tmp_path, monkeypatch):
    response = Mock(json=lambda: {"elements": []})
    post = Mock(side_effect=[requests.ConnectionError(), http_failure(429, 3), response])
    monkeypatch.setattr(requests, "post", post)
    cancel = Event()
    monkeypatch.setattr(cancel, "wait", Mock(return_value=False))

    road_access(tmp_path, cancel=cancel).allowed(*projected_points([15.1]))

    assert post.call_count == 3
    assert [call.args for call in cancel.wait.call_args_list] == [(1,), (3,)]
    assert len(list(tmp_path.iterdir())) == 1


def test_overpass_persistent_gateway_timeout_reports_failed_tile(tmp_path, monkeypatch):
    post = Mock(side_effect=http_failure(504))
    monkeypatch.setattr(requests, "post", post)
    cancel = Event()
    monkeypatch.setattr(cancel, "wait", Mock(return_value=False))

    with pytest.raises(RuntimeError, match=r"1 of 2 tiles failed.*HTTP 504"):
        road_access(tmp_path, cancel=cancel).allowed(*projected_points([15.1, 15.3]))
    assert post.call_count == 4
    assert [call.args for call in cancel.wait.call_args_list] == [(1,), (2,), (4,)]
    assert not list(tmp_path.iterdir())


@pytest.mark.parametrize(
    ("failure", "message"),
    [("http", "HTTP 400"), ("json", "invalid JSON"), ("remark", "timed out")],
)
def test_overpass_failure_is_reported_and_not_cached(tmp_path, monkeypatch, failure, message):
    response = Mock()
    if failure == "http":
        response.raise_for_status.side_effect = http_failure(400)
    elif failure == "json":
        response.json.side_effect = ValueError("Malformed payload")
    else:
        response.json.return_value = {"elements": [], "remark": "Query timed out"}
    monkeypatch.setattr(requests, "post", Mock(return_value=response))

    with pytest.raises(RuntimeError, match=message):
        road_access(tmp_path).allowed(*projected_points([15.1]))
    assert not list(tmp_path.iterdir())


def test_cancellation_stops_between_tiles(tmp_path, monkeypatch):
    post = overpass_ways(monkeypatch)
    cancel = Event()

    def progress(stage, done, total):
        if done == 1:
            cancel.set()

    access = road_access(tmp_path, cancel=cancel, progress=progress)
    with pytest.raises(RoadLoadCancelled, match="cancelled"):
        access.allowed(*projected_points([15.1, 15.3]))
    post.assert_called_once()
    assert len(list(tmp_path.iterdir())) == 1


def test_payload_clips_roads_and_shares_geometry_across_repeaters_and_alternatives(
    tmp_path, monkeypatch
):
    x, y = FORWARD.transform(15.1, 60.05)
    road = [(x - 2000, y), (x, y), (x + 2000, y)]
    other_road = [(x + 20000, y), (x + 24000, y)]
    post = overpass_ways(
        monkeypatch, [[REVERSE.transform(*point) for point in way] for way in (road, other_road)]
    )
    a = Site("A", x - 1000, y, kind=SiteKind.ENDPOINT_A)
    b = Site("B", x + 1000, y, kind=SiteKind.ENDPOINT_B)
    r = Site("R", x - 50, y + 50)
    s = Site("S", x + 50, y + 50, kind=SiteKind.ROUTER)
    distant = Site("D", x, y + 800, kind=SiteKind.ROUTER)
    t = Site("T", x + 22000, y + 75, kind=SiteKind.ROUTER)
    alternatives = [
        NetworkSolution("two routers", [a, r, s, b], [], ["A", "B"], ["R", "S"]),
        NetworkSolution("three routers", [a, r, s, distant, b], [], ["A", "B"], ["R", "S", "D"]),
        NetworkSolution("one router", [a, t, b], [], ["A", "B"], ["T"]),
    ]
    result = OptimizationResult([a, r, s, b], [], [], [], alternatives=alternatives)
    old_payload = result_payload(result, None, 0, 0)
    assert "road_polylines" not in old_payload
    assert "nearest_road_distance_m" not in old_payload["alternatives"][0]

    access = road_access(tmp_path)
    assert access.allowed(np.array([r.x]), np.array([r.y])).item()
    post.assert_called_once()
    result.road_overlay = access.overlay(alternatives)
    assert post.call_count == 2
    payload = result_payload(result, None, 0, 0)
    assert len(payload["road_polylines"]) == 2
    polyline = payload["road_polylines"][0]
    assert len(polyline) == 2
    xs, ys = FORWARD.transform(*np.asarray(polyline).T[::-1])
    np.testing.assert_allclose(sorted(xs), [x - 191.4, x + 191.4], atol=0.15)
    np.testing.assert_allclose(ys, [y, y], atol=0.1)
    first, second, third = payload["alternatives"]
    assert first["road_polyline_indices"] == second["road_polyline_indices"] == [0]
    assert first["nearest_road_distance_m"] == pytest.approx({"R": 50, "S": 50})
    assert second["nearest_road_distance_m"]["D"] is None
    assert set(second["nearest_road_distance_m"]) == {"R", "S", "D"}
    assert third["road_polyline_indices"] == [1]
    assert third["nearest_road_distance_m"] == pytest.approx({"T": 75})


def test_candidate_filter_runs_before_quality_scoring_and_thinning():
    terrain = ArrayTerrain(np.tile([0, 55, 0, 100, 0], (3, 1)), resolution_m=10)
    a = Site("A", 0, 10, kind=SiteKind.ENDPOINT_A)
    b = Site("B", 40, 10, kind=SiteKind.ENDPOINT_B)
    settings = CandidateSettings(
        corridor_width_m=100,
        grid_spacing_m=10,
        cell_size_m=100,
        candidates_per_cell=1,
        refine_radius_m=0,
    )

    def near_road(xs, ys):
        return (xs >= 10) & (xs <= 20)

    unfiltered = generate_candidates(terrain, a, b, settings)
    filtered = generate_candidates(terrain, a, b, settings, site_filter=near_road)
    assert unfiltered[1].x == 30
    assert len(filtered) == 3 and filtered[1].x == 10
    assert all(near_road(site.x, site.y) for site in filtered[1:-1])
    assert filtered[0] is a and filtered[-1] is b
    assert generate_candidates(
        terrain, a, b, settings, site_filter=lambda xs, ys: np.zeros(xs.shape, dtype=bool)
    ) == [a, b]


@pytest.mark.parametrize("network_mode", [False, True])
def test_optimizer_keeps_fixed_sites_with_road_access(network_mode):
    terrain = ArrayTerrain(np.zeros((9, 21)), resolution_m=10)
    a = Site("A", 0, 10, kind=SiteKind.ENDPOINT_A)
    b = Site("B", 200, 10, kind=SiteKind.ENDPOINT_B)
    client = Site("Client", 100, 10, kind=SiteKind.CLIENT)
    known = Site("Known", 90, 10, kind=SiteKind.ROUTER, origin=SiteOrigin.KNOWN, locked=True)
    manual = Site("Manual", 110, 10, kind=SiteKind.ROUTER, origin=SiteOrigin.MANUAL, locked=True)
    required = Site("Required", 150, 10, kind=SiteKind.ROUTER, required=True, locked=True)
    settings = CandidateSettings(
        corridor_width_m=50,
        grid_spacing_m=20,
        cell_size_m=50,
        refine_radius_m=0,
        maximum_candidates=4,
        maximum_solution_routers=2,
    )

    def near_road(xs, ys):
        return ys >= 40

    result = RouteOptimizer(terrain, RFSettings(), settings, site_filter=near_road).optimize(
        a,
        b,
        clients=[a, b, client] if network_mode else None,
        required_routers=[required] if network_mode else None,
        optional_routers=[known, manual],
    )

    assert result.found
    ids = {site.id for site in result.candidates}
    assert {a.id, b.id, known.id, manual.id} <= ids
    if network_mode:
        assert {client.id, required.id} <= ids
    generated = [site for site in result.candidates if site.kind == SiteKind.CANDIDATE]
    assert generated and all(near_road(site.x, site.y) for site in generated)
    assert any(report.startswith("Road access filter kept ") for report in result.diagnostics)


def test_no_mesh_diagnostics_report_filtered_grid():
    terrain = ArrayTerrain(np.zeros((3, 21)), resolution_m=10)
    a = Site("A", 0, 10, kind=SiteKind.ENDPOINT_A)
    b = Site("B", 200, 10, kind=SiteKind.ENDPOINT_B)
    settings = CandidateSettings(grid_spacing_m=10, refine_radius_m=0, maximum_link_distance_m=50)
    result = RouteOptimizer(
        terrain, RFSettings(), settings, site_filter=lambda xs, ys: np.zeros(xs.shape, dtype=bool)
    ).optimize(a, b)

    assert not result.found
    assert result.candidates == [a, b]
    assert "Road access filter kept 0 of 63 grid points." in result.diagnostics


def test_refinement_stays_near_road():
    terrain = ArrayTerrain(np.zeros((3, 21)), resolution_m=10)
    a = Site("A", 0, 10, kind=SiteKind.ENDPOINT_A)
    b = Site("B", 200, 10, kind=SiteKind.ENDPOINT_B)
    site = Site("Candidate", 100, 10)
    settings = CandidateSettings(refine_radius_m=10, refine_step_m=10)

    def evaluate(source, target, step):
        trial = source if source.id == site.id else target
        return Mock(
            valid=True, worst_margin_db=-abs(trial.x - 90), minimum_fresnel_clearance_ratio=1
        )

    optimizer = RouteOptimizer(terrain, RFSettings(), settings)
    optimizer.evaluator = Mock(evaluate=evaluate)
    assert optimizer._refine_site(a, site, b).x == 90

    optimizer = RouteOptimizer(
        terrain, RFSettings(), settings, site_filter=lambda xs, ys: xs >= 100
    )
    optimizer.evaluator = Mock(evaluate=evaluate)
    assert optimizer._refine_site(a, site, b).x == 100
