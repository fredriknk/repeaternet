from __future__ import annotations

import pytest
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
from rf_router_planner.models.site import Site


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
