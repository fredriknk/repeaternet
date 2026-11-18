from unittest.mock import Mock

import numpy as np
import pytest
import requests
from pyproj import Transformer
from shapely.geometry import LineString

from rf_router_planner.models.settings import CandidateSettings, RFSettings, RoadAccessClasses
from rf_router_planner.models.site import Site, SiteKind, SiteOrigin
from rf_router_planner.optimization.optimizer import RouteOptimizer
from rf_router_planner.terrain.candidate_sites import generate_candidates
from rf_router_planner.terrain.raster import ArrayTerrain
from rf_router_planner.terrain.roads import RoadAccessIndex, load_road_access_index


def test_road_access_index_distances():
    index = RoadAccessIndex([LineString([(0, 0), (2000, 0)])], 1000)
    xs = np.array([1000, 1000, 1000, 1000, 2500, 2600])
    ys = np.array([0, 999, 1000, 1001, 0, 900])

    np.testing.assert_array_equal(index.allowed(xs, ys), [True, True, True, False, True, False])
    np.testing.assert_array_equal(
        RoadAccessIndex([], 1000).allowed(xs, ys), np.zeros(xs.size, dtype=bool)
    )


def test_overpass_parses_projects_and_caches_response(tmp_path, monkeypatch):
    endpoint = "https://overpass.example/api/interpreter"
    monkeypatch.setenv("RF_PLANNER_OVERPASS_URL", endpoint)
    response = Mock()
    response.json.return_value = {
        "elements": [
            {
                "type": "way",
                "id": 123,
                "tags": {"highway": "residential"},
                "geometry": [{"lon": 15, "lat": 60}, {"lon": 15, "lat": 60.01}],
            },
        ],
    }
    post = Mock(return_value=response)
    monkeypatch.setattr(requests, "post", post)
    crs = "EPSG:25833"
    x, y = Transformer.from_crs(4326, crs, always_xy=True).transform(15, 60.005)
    bounds = (x - 300, y - 300, x + 300, y + 300)
    distance = 100
    first = load_road_access_index(bounds, crs, RoadAccessClasses.PUBLIC_ROADS, distance, tmp_path)

    assert post.call_args.args == (endpoint,)
    query = post.call_args.kwargs["data"]["data"]
    assert query.startswith('[out:json][timeout:45];way["highway"~"^(')
    assert "motorway_link" in query and "service" in query and "track" not in query
    assert query.endswith(");out geom;")
    south, west, north, east = map(
        float, query.removesuffix(");out geom;").rsplit("(", 1)[1].split(",")
    )
    reverse = Transformer.from_crs(crs, 4326, always_xy=True)
    expanded_west, expanded_south, expanded_east, expanded_north = reverse.transform_bounds(
        x - 400, y - 400, x + 400, y + 400
    )
    assert south <= expanded_south and west <= expanded_west
    assert north >= expanded_north and east >= expanded_east

    cached = load_road_access_index(bounds, crs, RoadAccessClasses.PUBLIC_ROADS, distance, tmp_path)
    post.assert_called_once()
    for index in (first, cached):
        np.testing.assert_array_equal(
            index.allowed(np.array([x + 99, x + 101]), np.array([y, y])), [True, False]
        )

    load_road_access_index(bounds, crs, RoadAccessClasses.ALL_DRIVABLE, distance, tmp_path)
    assert post.call_count == 2
    assert "|track)$" in post.call_args.kwargs["data"]["data"]


@pytest.mark.parametrize(
    ("failure", "message"),
    [("http", "HTTP 503"), ("json", "invalid JSON"), ("remark", "timed out")],
)
def test_overpass_failure_is_reported_and_not_cached(tmp_path, monkeypatch, failure, message):
    response = Mock()
    if failure == "http":
        failed_response = requests.Response()
        failed_response.status_code = 503
        response.raise_for_status.side_effect = requests.HTTPError(response=failed_response)
    elif failure == "json":
        response.json.side_effect = ValueError("Malformed payload")
    else:
        response.json.return_value = {"elements": [], "remark": "Query timed out"}
    monkeypatch.setattr(requests, "post", Mock(return_value=response))

    with pytest.raises(RuntimeError, match=message):
        load_road_access_index(
            (500000, 6650000, 501000, 6651000),
            "EPSG:25833",
            RoadAccessClasses.ALL_DRIVABLE,
            1000,
            tmp_path,
        )
    assert not list(tmp_path.iterdir())


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
