import csv
import json

import pytest

from rf_router_planner.coordinates import transformer_pair
from rf_router_planner.export import export_route_csv, export_route_geojson
from rf_router_planner.models.link import DirectionResult, LinkResult
from rf_router_planner.models.site import Site, SiteKind
from rf_router_planner.optimization.optimizer import OptimizationResult


def test_coordinate_round_trip() -> None:
    _epsg, forward, reverse = transformer_pair(59.9139, 10.7522)
    x, y = forward.transform(10.7522, 59.9139)
    longitude, latitude = reverse.transform(x, y)
    assert latitude == pytest.approx(59.9139, abs=1e-7)
    assert longitude == pytest.approx(10.7522, abs=1e-7)


def test_csv_and_geojson_exports(tmp_path) -> None:
    a = Site("A", 0, 0, 60.0, 10.0, SiteKind.ENDPOINT_A)
    b = Site("B", 1000, 0, 60.0, 10.02, SiteKind.ENDPOINT_B)
    forward = DirectionResult("A", "B", 1000, 91, 0, 0, 91, 2, 2, -65, -130, 65, 55, 0, 0, True)
    reverse = DirectionResult("B", "A", 1000, 91, 0, 0, 91, 2, 2, -65, -130, 65, 55, 0, 0, True)
    link = LinkResult("A", "B", 1000, forward, reverse, True, True, True, 5, 1, 500, 9, 0)
    result = OptimizationResult([a, b], [link], [a, b], [link])
    csv_path = tmp_path / "route.csv"
    geojson_path = tmp_path / "route.geojson"
    export_route_csv(result, csv_path)
    export_route_geojson(result, geojson_path)
    with csv_path.open(newline="", encoding="utf-8") as handle:
        rows = list(csv.DictReader(handle))
    document = json.loads(geojson_path.read_text(encoding="utf-8"))
    assert rows[0]["worst_margin_db"] == "55.00"
    assert len(document["features"]) == 3
    assert document["features"][-1]["geometry"]["type"] == "LineString"
