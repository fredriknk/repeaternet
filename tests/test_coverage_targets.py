from __future__ import annotations

import numpy as np
import pytest
from pyproj import Transformer
from shapely.geometry import LineString

from rf_router_planner.coverage.target_assessment import assess_targets
from rf_router_planner.coverage.targets import (
    MAX_TARGET_VERTICES,
    MAX_TARGETS,
    line_sample_positions,
    normalize_target_collection,
    projected_line_samples,
)
from rf_router_planner.models.coverage import CoverageSettings
from rf_router_planner.models.settings import CandidateSettings, RFSettings
from rf_router_planner.models.site import Site, SiteKind
from rf_router_planner.terrain.raster import ArrayTerrain


def feature(geometry_type: str, coordinates, *, name: str = "Target", properties=None):
    return {
        "type": "Feature",
        "geometry": {"type": geometry_type, "coordinates": coordinates},
        "properties": {"name": name, **(properties or {})},
    }


def collection(*features):
    return {"type": "FeatureCollection", "features": list(features)}


def test_target_collection_normalizes_point_polygon_and_road() -> None:
    normalized = normalize_target_collection(
        collection(
            feature("Point", [10.0, 60.0]),
            feature(
                "Polygon",
                [[[10.0, 60.0], [10.1, 60.0], [10.1, 60.1], [10.0, 60.0]]],
                name="Park",
            ),
            feature("LineString", [[10.0, 60.0], [10.1, 60.1]], name="Road"),
        )
    )
    assert len(normalized["features"]) == 3
    assert all(item["id"].startswith("T-") for item in normalized["features"])
    assert normalized["features"][1]["properties"]["name"] == "Park"


def test_target_collection_rejects_bad_ids_geometry_coordinates_and_margin() -> None:
    duplicate = feature("Point", [10.0, 60.0])
    duplicate["id"] = "same"
    second = feature("Point", [10.1, 60.1])
    second["id"] = "same"
    with pytest.raises(ValueError, match="unique"):
        normalize_target_collection(collection(duplicate, second))
    with pytest.raises(ValueError, match="WGS84"):
        normalize_target_collection(collection(feature("Point", [181.0, 60.0])))
    with pytest.raises(ValueError, match="closed"):
        normalize_target_collection(
            collection(feature("Polygon", [[[10, 60], [11, 60], [11, 61], [10, 61]]]))
        )
    with pytest.raises(ValueError, match="margin"):
        normalize_target_collection(
            collection(feature("Point", [10, 60], properties={"minimum_margin_db": 101}))
        )


def test_target_import_and_vertex_caps_are_enforced() -> None:
    too_many = [feature("Point", [10.0, 60.0]) for _ in range(MAX_TARGETS + 1)]
    with pytest.raises(ValueError, match="500"):
        normalize_target_collection(collection(*too_many))
    many_vertices = [[10.0, 60.0] for _ in range(MAX_TARGET_VERTICES + 1)]
    with pytest.raises(ValueError, match="vertices"):
        normalize_target_collection(collection(feature("LineString", many_vertices)))


def test_road_samples_are_evenly_spaced_and_length_weights_sum_to_road_length() -> None:
    samples = line_sample_positions(1_050.0, 400.0)
    assert [distance for distance, _ in samples] == [0, 400, 800, 1050]
    assert sum(weight for _, weight in samples) == pytest.approx(1_050.0)


def test_projected_road_sampling_rejects_unbounded_work() -> None:
    with pytest.raises(ValueError, match="sample limit"):
        projected_line_samples(LineString([(0, 0), (1_000_000, 0)]), 25)


def test_target_assessment_rejects_excessive_exact_router_evaluations() -> None:
    points = collection(
        *(feature("Point", [10.0, 60.0]) for _ in range(MAX_TARGETS))
    )
    normalized = normalize_target_collection(points)
    sources = [Site(f"R-{index}", 0, 0) for index in range(101)]
    with pytest.raises(ValueError, match="50,000"):
        assess_targets(
            None,  # Exact evaluation is never entered beyond the preflight cap.
            RFSettings(),
            CandidateSettings(),
            CoverageSettings(),
            sources,
            normalized,
            lambda: (),
            terrain_crs="EPSG:25833",
            grid_bounds=(0, 0, 100, 100),
            effective_cell_size_m=100,
            requested_cells=1,
        )


def test_drawn_analysis_area_reports_points_outside_without_rf_evaluation() -> None:
    reverse = Transformer.from_crs(25833, 4326, always_xy=True)
    corners = [
        reverse.transform(x, y)
        for x, y in [(0, 0), (100, 0), (100, 100), (0, 100), (0, 0)]
    ]
    point = list(reverse.transform(150, 50))
    settings = CoverageSettings(
        area_mode="drawn",
        area_polygon_wgs84=[[lon, lat] for lon, lat in corners],
    )
    targets = normalize_target_collection(collection(feature("Point", point)))

    report = assess_targets(
        ArrayTerrain([[0.0, 0.0], [0.0, 0.0]], resolution_m=100.0),
        RFSettings(),
        CandidateSettings(),
        settings,
        [],
        targets,
        lambda: (),
        terrain_crs="EPSG:25833",
        grid_bounds=(0, 0, 200, 100),
        effective_cell_size_m=100,
        requested_cells=2,
    )

    assert report["targets"][0]["state"] == "outside_analysed_area"
    assert report["targets"][0]["evaluated_samples"] == 0


def test_exact_point_target_reports_profile_cap_as_unresolved() -> None:
    reverse = Transformer.from_crs(25833, 4326, always_xy=True)
    longitude, latitude = reverse.transform(4_000.0, 2_500.0)
    targets = normalize_target_collection(
        collection(feature("Point", [longitude, latitude]))
    )
    settings = CoverageSettings(maximum_profile_samples=8)

    report = assess_targets(
        ArrayTerrain(np.zeros((51, 51)), resolution_m=100.0),
        RFSettings(),
        CandidateSettings(final_sample_step_m=100.0),
        settings,
        [Site("R-1", 0.0, 2_500.0, kind=SiteKind.ROUTER, antenna_height_m=50.0)],
        targets,
        lambda: (),
        terrain_crs="EPSG:25833",
        grid_bounds=(0, 0, 5_000, 5_000),
        effective_cell_size_m=100,
        requested_cells=1,
    )

    result = report["targets"][0]
    assert result["state"] == "unresolved"
    assert result["unresolved_sources"] == 1
    assert "profile step" in result["message"]


def test_polygon_target_keeps_unresolved_area_separate_from_failed_area() -> None:
    reverse = Transformer.from_crs(25833, 4326, always_xy=True)
    ring = [
        list(reverse.transform(x, y))
        for x, y in [(0, 0), (100, 0), (100, 100), (0, 100), (0, 0)]
    ]
    targets = normalize_target_collection(
        collection(feature("Polygon", [ring], name="Capped area"))
    )
    grid_cell = {
        "index": 0,
        "x": 50.0,
        "y": 50.0,
        "state": "unresolved",
        "sources": [
            {
                "source_id": "R-1",
                "valid_two_way": False,
                "rejection": "profile_sample_limit",
            }
        ],
    }

    report = assess_targets(
        ArrayTerrain([[0.0, 0.0], [0.0, 0.0]], resolution_m=100.0),
        RFSettings(),
        CandidateSettings(),
        CoverageSettings(),
        [Site("R-1", 0, 0, kind=SiteKind.ROUTER)],
        targets,
        lambda: [grid_cell],
        terrain_crs="EPSG:25833",
        grid_bounds=(0, 0, 100, 100),
        effective_cell_size_m=100,
        requested_cells=1,
    )

    result = report["targets"][0]
    assert result["state"] == "unresolved"
    assert result["unresolved_area_m2"] == pytest.approx(10_000)
    assert result["failed_area_m2"] == 0


def test_polygon_target_reports_area_beyond_drawn_analysis_boundary() -> None:
    reverse = Transformer.from_crs(25833, 4326, always_xy=True)

    def ring(points):
        return [list(reverse.transform(x, y)) for x, y in points]

    analysis_ring = ring([(0, 0), (100, 0), (100, 100), (0, 100), (0, 0)])
    target_ring = ring([(50, 25), (150, 25), (150, 75), (50, 75), (50, 25)])
    settings = CoverageSettings(
        area_mode="drawn",
        area_polygon_wgs84=analysis_ring,
    )
    targets = normalize_target_collection(
        collection(feature("Polygon", [target_ring]))
    )
    samples = [
        {
            "index": 0,
            "x": 75.0,
            "y": 50.0,
            "state": "covered",
            "sources": [
                {
                    "source_id": "R-1",
                    "valid_two_way": True,
                    "two_way_margin_db": 8.0,
                }
            ],
        }
    ]

    report = assess_targets(
        ArrayTerrain([[0.0, 0.0], [0.0, 0.0]], resolution_m=100.0),
        RFSettings(),
        CandidateSettings(),
        settings,
        [Site("R-1", 50.0, 50.0)],
        targets,
        lambda: samples,
        terrain_crs="EPSG:25833",
        grid_bounds=(0, 0, 200, 100),
        effective_cell_size_m=50,
        requested_cells=8,
    )

    result = report["targets"][0]
    assert result["state"] == "unknown"
    assert result["outside_analysed_area_m2"] == pytest.approx(2_500, rel=0.01)


def test_polygon_target_assessment_rejects_excessive_grid_scans() -> None:
    target = normalize_target_collection(
        collection(feature("Polygon", [[[10, 60], [10.1, 60], [10.1, 60.1], [10, 60]]]))
    )
    with pytest.raises(ValueError, match="grid-scan limit"):
        assess_targets(
            None,
            RFSettings(),
            CandidateSettings(),
            CoverageSettings(),
            [],
            target,
            lambda: (),
            terrain_crs="EPSG:25833",
            grid_bounds=(0, 0, 100, 100),
            effective_cell_size_m=100,
            requested_cells=250_001,
        )
