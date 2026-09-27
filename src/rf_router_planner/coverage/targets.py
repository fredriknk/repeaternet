"""Bounded GeoJSON target validation and metric sampling utilities."""

from __future__ import annotations

import math
import secrets
from typing import Any

from shapely.geometry import LineString, shape

MAX_TARGETS = 500
MAX_TARGET_VERTICES = 50_000
MAX_TARGET_IMPORT_BYTES = 5 * 1024 * 1024
MAX_LINE_SAMPLES = 20_000


def normalize_target_collection(value: Any) -> dict[str, Any]:
    """Validate and normalize a bounded Point/Polygon/LineString collection."""
    if not isinstance(value, dict) or value.get("type") != "FeatureCollection":
        raise ValueError("Import a GeoJSON FeatureCollection")
    features = value.get("features")
    if not isinstance(features, list) or len(features) > MAX_TARGETS:
        raise ValueError(f"A target collection must contain between 0 and {MAX_TARGETS} features")
    normalized: list[dict[str, Any]] = []
    seen_ids: set[str] = set()
    vertices = 0
    for feature in features:
        if not isinstance(feature, dict) or feature.get("type") != "Feature":
            raise ValueError("Every target must be a GeoJSON Feature")
        geometry = feature.get("geometry")
        properties = feature.get("properties") or {}
        if not isinstance(geometry, dict) or not isinstance(properties, dict):
            raise ValueError("Each target needs geometry and a properties object")
        geometry_type = geometry.get("type")
        if geometry_type not in {"Point", "Polygon", "LineString"}:
            raise ValueError("Targets must be Point, Polygon, or LineString geometries")
        if geometry_type == "Polygon":
            _validate_polygon_rings(geometry.get("coordinates"))
        vertices += _count_vertices(geometry.get("coordinates"))
        if vertices > MAX_TARGET_VERTICES:
            raise ValueError(f"Target collection exceeds {MAX_TARGET_VERTICES} vertices")
        _validate_coordinates(geometry.get("coordinates"))
        try:
            parsed_geometry = shape(geometry)
        except (TypeError, ValueError) as exc:
            raise ValueError("Target geometry is malformed") from exc
        if parsed_geometry.is_empty or not parsed_geometry.is_valid:
            raise ValueError("Target geometry must be nonempty and valid")
        if geometry_type == "Polygon" and (parsed_geometry.area <= 0 or not parsed_geometry.exterior.is_closed):
            raise ValueError("Polygon rings must be closed and enclose an area")
        if geometry_type == "LineString" and (parsed_geometry.length <= 0 or len(parsed_geometry.coords) < 2):
            raise ValueError("Road or route targets need at least two distinct vertices")
        identifier = feature.get("id", properties.get("id"))
        if identifier is None:
            identifier = "T-" + secrets.token_hex(8)
        if not isinstance(identifier, str) or not identifier or len(identifier) > 64:
            raise ValueError("Target IDs must be 1–64 characters")
        if identifier in seen_ids:
            raise ValueError("Target IDs must be unique")
        seen_ids.add(identifier)
        name = properties.get("name", f"Target {len(normalized) + 1}")
        if not isinstance(name, str) or not name.strip() or len(name.strip()) > 120:
            raise ValueError("Target names must be 1–120 characters")
        margin = properties.get("minimum_margin_db", 0.0)
        if (
            not isinstance(margin, (int, float))
            or isinstance(margin, bool)
            or not math.isfinite(margin)
            or not -100 <= margin <= 100
        ):
            raise ValueError("Minimum margin must be between -100 and 100 dB")
        reference = properties.get("reference_id")
        if reference is not None and (not isinstance(reference, str) or len(reference) > 64):
            raise ValueError("Reference router IDs must be at most 64 characters")
        normalized.append(
            {
                "type": "Feature",
                "id": identifier,
                "geometry": {"type": geometry_type, "coordinates": geometry["coordinates"]},
                "properties": {
                    "name": name.strip(),
                    "minimum_margin_db": float(margin),
                    "reference_id": reference,
                },
            }
        )
    return {"type": "FeatureCollection", "features": normalized}


def line_sample_positions(length_m: float, spacing_m: float) -> list[tuple[float, float]]:
    """Return distance along a line and represented length around each sample."""
    if not math.isfinite(length_m) or length_m <= 0:
        raise ValueError("Road target length must be positive")
    if not math.isfinite(spacing_m) or spacing_m <= 0:
        raise ValueError("Road sample spacing must be positive")
    if math.ceil(length_m / spacing_m) + 1 > MAX_LINE_SAMPLES:
        raise ValueError(
            f"Road target exceeds the {MAX_LINE_SAMPLES:,}-sample limit; increase sample spacing"
        )
    distances = [0.0]
    next_distance = spacing_m
    while next_distance < length_m:
        distances.append(next_distance)
        next_distance += spacing_m
    distances.append(length_m)
    samples = []
    for index, distance in enumerate(distances):
        left = 0.0 if index == 0 else (distances[index - 1] + distance) / 2
        right = length_m if index == len(distances) - 1 else (distance + distances[index + 1]) / 2
        samples.append((distance, right - left))
    return samples


def projected_line_samples(line: LineString, spacing_m: float) -> list[tuple[float, float, float]]:
    """Return projected sample X/Y and represented segment length in metres."""
    return [
        (float(point.x), float(point.y), weight)
        for distance, weight in line_sample_positions(line.length, spacing_m)
        if (point := line.interpolate(distance)) is not None
    ]


def _count_vertices(value: Any) -> int:
    if isinstance(value, list):
        if len(value) >= 2 and all(isinstance(item, (int, float)) for item in value[:2]):
            return 1
        return sum(_count_vertices(item) for item in value)
    return 0


def _validate_coordinates(value: Any) -> None:
    if isinstance(value, list):
        if len(value) >= 2 and all(isinstance(item, (int, float)) for item in value[:2]):
            longitude, latitude = value[:2]
            if (
                isinstance(longitude, bool)
                or isinstance(latitude, bool)
                or not math.isfinite(longitude)
                or not math.isfinite(latitude)
                or not -180 <= longitude <= 180
                or not -90 <= latitude <= 90
            ):
                raise ValueError("Target coordinates must be finite WGS84 longitude/latitude")
            return
        for item in value:
            _validate_coordinates(item)
        return
    raise ValueError("Target coordinates are malformed")


def _validate_polygon_rings(value: Any) -> None:
    if not isinstance(value, list) or not value:
        raise ValueError("Polygon coordinates need at least one closed ring")
    for ring in value:
        if not isinstance(ring, list) or len(ring) < 4 or ring[0] != ring[-1]:
            raise ValueError("Polygon rings must contain at least four positions and be closed")
