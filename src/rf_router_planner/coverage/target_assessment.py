"""Bounded assessments for saved point, road and polygon coverage targets."""

from __future__ import annotations

import math
from collections.abc import Callable, Iterable
from typing import Any

from pyproj import Transformer
from shapely.geometry import Point, box, shape
from shapely.ops import transform as transform_geometry

from rf_router_planner.coverage.target_evaluation import evaluate_client_point
from rf_router_planner.coverage.targets import projected_line_samples
from rf_router_planner.models.coverage import CoverageSettings
from rf_router_planner.models.settings import CandidateSettings, RFSettings
from rf_router_planner.models.site import Site
from rf_router_planner.rf.propagation import LinkEvaluator
from rf_router_planner.terrain.raster import TerrainSource

MAX_TARGET_POINT_EVALUATIONS = 50_000
MAX_TARGET_POLYGON_FEATURES = 32
MAX_TARGET_POLYGON_CELL_SCANS = 250_000


def assess_targets(
    terrain: TerrainSource,
    rf_settings: RFSettings,
    candidate_settings: CandidateSettings,
    coverage_settings: CoverageSettings,
    sources: list[Site],
    targets: dict[str, Any],
    cells: Callable[[], Iterable[dict[str, Any]]],
    *,
    terrain_crs: str,
    grid_bounds: tuple[float, float, float, float],
    effective_cell_size_m: float,
    requested_cells: int,
    connected_sources_by_reference: dict[str, set[str]] | None = None,
    road_spacing_m: float = 100.0,
) -> dict[str, Any]:
    """Assess a normalized GeoJSON collection against an immutable coverage job.

    Point and road links use exact endpoint evaluation. Polygon estimates consume
    persisted grid results; partial cell footprints without a sample centre inside
    the polygon are classified as unknown.
    """
    if not 25 <= road_spacing_m <= 5_000 or not math.isfinite(road_spacing_m):
        raise ValueError("Road sample spacing must be between 25 and 5,000 m")
    features = targets.get("features", [])
    polygons = [item for item in features if item["geometry"]["type"] == "Polygon"]
    if len(polygons) > MAX_TARGET_POLYGON_FEATURES:
        raise ValueError(f"Assess at most {MAX_TARGET_POLYGON_FEATURES} polygon targets at once")
    if requested_cells * len(polygons) > MAX_TARGET_POLYGON_CELL_SCANS:
        raise ValueError("Polygon assessment exceeds the bounded grid-scan limit; assess fewer areas at a time")

    forward = Transformer.from_crs(4326, terrain_crs, always_xy=True)
    point_and_road_samples = 0
    for item in features:
        if item["geometry"]["type"] == "Point":
            point_and_road_samples += 1
        elif item["geometry"]["type"] == "LineString":
            projected = transform_geometry(forward.transform, shape(item["geometry"]))
            point_and_road_samples += len(projected_line_samples(projected, road_spacing_m))
    pair_evaluations = point_and_road_samples * len(sources)
    if pair_evaluations > MAX_TARGET_POINT_EVALUATIONS:
        raise ValueError(
            f"Point and road targets need {pair_evaluations:,} router evaluations; "
            f"the limit is {MAX_TARGET_POINT_EVALUATIONS:,}. Increase road spacing or assess fewer targets."
        )

    evaluator = LinkEvaluator(terrain, rf_settings)
    reverse = Transformer.from_crs(terrain_crs, 4326, always_xy=True)
    all_sources = {item.id for item in sources}
    reports: list[dict[str, Any]] = []
    for feature in features:
        geometry_type = feature["geometry"]["type"]
        properties = feature["properties"]
        minimum_margin = float(properties.get("minimum_margin_db", 0.0))
        reference_id = properties.get("reference_id")
        allowed_ids = (
            connected_sources_by_reference[reference_id]
            if reference_id is not None and connected_sources_by_reference is not None
            else all_sources
        )
        report = {
            "id": feature["id"],
            "name": properties["name"],
            "geometry_type": geometry_type,
            "minimum_margin_db": minimum_margin,
            "reference_id": reference_id,
            "state": "unknown",
            "message": "",
        }

        if geometry_type == "Point":
            longitude, latitude = feature["geometry"]["coordinates"][:2]
            evaluated = evaluate_client_point(
                terrain,
                rf_settings,
                candidate_settings,
                sources,
                coverage_settings,
                latitude,
                longitude,
                evaluator,
            )
            rows = [row for row in evaluated["sources"] if row["source_id"] in allowed_ids]
            passing = [
                row
                for row in rows
                if row["valid_two_way"]
                and row["two_way_margin_db"] is not None
                and row["two_way_margin_db"] >= minimum_margin
            ]
            unknown = evaluated["state"] == "unknown_terrain" or any(
                row.get("rejection") == "unknown_terrain" for row in rows
            )
            point_margins = [
                float(row["two_way_margin_db"])
                for row in rows
                if row["valid_two_way"] and row["two_way_margin_db"] is not None
            ]
            report.update(
                state="pass" if passing else "unknown" if unknown else "fail",
                evaluated_samples=int(evaluated["state"] != "unknown_terrain"),
                passing_sources=[row["source_id"] for row in passing],
                best_two_way_margin_db=max(point_margins) if point_margins else None,
                latitude=latitude,
                longitude=longitude,
                message=(
                    "At least one selected source meets the two-way target margin."
                    if passing
                    else "Terrain or a selected source link is unknown at this location."
                    if unknown
                    else "No selected source meets the two-way target margin."
                ),
            )
        elif geometry_type == "LineString":
            line = transform_geometry(forward.transform, shape(feature["geometry"]))
            road_samples = projected_line_samples(line, road_spacing_m)
            total_length = float(line.length)
            covered_length = unknown_length = failed_length = 0.0
            road_margins: list[float] = []
            for x, y, represented_length in road_samples:
                longitude, latitude = reverse.transform(x, y)
                evaluated = evaluate_client_point(
                    terrain,
                    rf_settings,
                    candidate_settings,
                    sources,
                    coverage_settings,
                    latitude,
                    longitude,
                    evaluator,
                )
                rows = [row for row in evaluated["sources"] if row["source_id"] in allowed_ids]
                passing = [
                    row
                    for row in rows
                    if row["valid_two_way"]
                    and row["two_way_margin_db"] is not None
                    and row["two_way_margin_db"] >= minimum_margin
                ]
                unknown = evaluated["state"] == "unknown_terrain" or any(
                    row.get("rejection") == "unknown_terrain" for row in rows
                )
                if passing:
                    covered_length += represented_length
                    road_margins.extend(float(row["two_way_margin_db"]) for row in passing)
                elif unknown:
                    unknown_length += represented_length
                else:
                    failed_length += represented_length
            tolerance = max(1e-6, total_length * 1e-9)
            state = (
                "pass"
                if unknown_length <= tolerance and failed_length <= tolerance
                else "unknown"
                if unknown_length > tolerance
                else "fail"
            )
            report.update(
                state=state,
                total_length_m=total_length,
                covered_length_m=covered_length,
                failed_length_m=failed_length,
                unknown_length_m=unknown_length,
                sample_spacing_m=road_spacing_m,
                sample_count=len(road_samples),
                worst_passing_margin_db=min(road_margins) if road_margins else None,
                message=(
                    "Every sampled road segment meets the two-way target margin."
                    if state == "pass"
                    else "Some road length has missing terrain or an unknown source link."
                    if state == "unknown"
                    else "Some sampled road length does not meet the two-way target margin."
                ),
            )
        else:
            geometry = shape(feature["geometry"])
            projected = transform_geometry(forward.transform, geometry)
            requested_area = float(projected.area)
            grid_polygon = box(*grid_bounds)
            evaluated_area = covered_area = failed_area = unknown_area = 0.0
            area_sample_count = 0
            min_x, min_y, max_x, max_y = grid_bounds
            half = effective_cell_size_m / 2
            for cell in cells():
                x, y = float(cell["x"]), float(cell["y"])
                cell_bounds = box(
                    max(min_x, x - half),
                    max(min_y, y - half),
                    min(max_x, x + half),
                    min(max_y, y + half),
                )
                overlap = projected.intersection(cell_bounds)
                area = float(overlap.area)
                if area <= 0:
                    continue
                sample_point = Point(x, y)
                if not projected.covers(sample_point):
                    unknown_area += area
                    continue
                area_sample_count += 1
                rows = [
                    row for row in cell.get("sources", [])
                    if row.get("source_id") in allowed_ids
                ]
                passing = [
                    row
                    for row in rows
                    if row.get("valid_two_way")
                    and row.get("two_way_margin_db") is not None
                    and row["two_way_margin_db"] >= minimum_margin
                ]
                unknown = cell.get("state") in {"unknown_terrain", "not_evaluated"} or any(
                    row.get("rejection") == "unknown_terrain" for row in rows
                )
                if passing:
                    evaluated_area += area
                    covered_area += area
                elif unknown:
                    unknown_area += area
                else:
                    evaluated_area += area
                    failed_area += area
            represented_area = evaluated_area + unknown_area
            remainder = max(0.0, requested_area - represented_area)
            unknown_area += remainder
            tolerance = max(1e-3, requested_area * 1e-9)
            state = (
                "outside_analysed_area"
                if not projected.intersects(grid_polygon)
                else "needs_finer_sampling"
                if area_sample_count == 0
                else "pass"
                if unknown_area <= tolerance and failed_area <= tolerance
                else "unknown"
                if unknown_area > tolerance
                else "fail"
            )
            report.update(
                state=state,
                requested_area_m2=requested_area,
                evaluated_area_m2=evaluated_area,
                covered_area_m2=covered_area,
                failed_area_m2=failed_area,
                unknown_area_m2=unknown_area,
                grid_cell_size_m=effective_cell_size_m,
                sampled_cells=area_sample_count,
                message=(
                    "No coverage-grid sample falls inside this area; calculate a finer grid."
                    if state == "needs_finer_sampling"
                    else "Target area is outside the analysed coverage grid."
                    if state == "outside_analysed_area"
                    else "All represented area meets the two-way target margin at this grid resolution."
                    if state == "pass"
                    else "Some target area is unsampled or has unknown terrain/source links."
                    if state == "unknown"
                    else "Some sampled target area does not meet the two-way target margin."
                ),
            )
        reports.append(report)

    return {
        "targets": reports,
        "assumptions": {
            "coverage_job_id": None,
            "model": "two-way terrain-based RF link prediction",
            "minimum_margin_db": "per target",
            "road_sample_spacing_m": road_spacing_m,
            "polygon_grid_cell_size_m": effective_cell_size_m,
            "reference_connected_source_ids": {
                key: sorted(value)
                for key, value in (connected_sources_by_reference or {}).items()
            },
            "terrain_surface_available": terrain.has_surface,
            "warning": "Predictions are not measurements or guarantees of packet delivery; polygon results represent grid samples only.",
            "router_evaluations": pair_evaluations,
        },
    }
