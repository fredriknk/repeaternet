"""OpenStreetMap road geometry from Overpass and projected distance queries."""

from __future__ import annotations

import hashlib
import json
import math
import os
from collections.abc import Sequence
from pathlib import Path
from tempfile import NamedTemporaryFile

import numpy as np
import requests
from pyproj import Transformer
from shapely import STRtree, points
from shapely.geometry import LineString

from rf_router_planner.models.settings import RoadAccessClasses

_OVERPASS_URL = "https://overpass-api.de/api/interpreter"
_PUBLIC_ROAD_TYPES = (
    "motorway",
    "trunk",
    "primary",
    "secondary",
    "tertiary",
    "unclassified",
    "residential",
    "service",
    "living_street",
)
_PUBLIC_ROAD_TAGS = (*_PUBLIC_ROAD_TYPES, *(f"{tag}_link" for tag in _PUBLIC_ROAD_TYPES))
_HIGHWAY_TAGS = {
    RoadAccessClasses.PUBLIC_ROADS: _PUBLIC_ROAD_TAGS,
    RoadAccessClasses.ALL_DRIVABLE: (*_PUBLIC_ROAD_TAGS, "track"),
}


class RoadAccessIndex:
    """Query the distance to the nearest road in projected metre coordinates."""

    def __init__(self, lines: Sequence[LineString], max_distance_m: float) -> None:
        self._tree = STRtree(lines)
        self._max_distance_m = max_distance_m

    def allowed(self, xs: np.ndarray, ys: np.ndarray) -> np.ndarray:
        allowed = np.zeros(xs.size, dtype=bool)
        matches = self._tree.query_nearest(
            points(xs.ravel(), ys.ravel()), max_distance=self._max_distance_m, all_matches=False
        )
        allowed[matches[0]] = True
        return allowed.reshape(xs.shape)


def _projected_lines(payload: object, forward: Transformer) -> list[LineString]:
    if not isinstance(payload, dict) or not isinstance(payload.get("elements"), list):
        raise RuntimeError("Could not load road network: invalid Overpass response")
    if payload.get("remark"):
        raise RuntimeError(f"Could not load road network: Overpass reported {payload['remark']}")

    lines = []
    try:
        for way in payload["elements"]:
            if way["type"] != "way":
                continue
            coordinates = np.asarray(
                [(point["lon"], point["lat"]) for point in way["geometry"]], dtype=float
            )
            if len(coordinates) < 2 or not np.isfinite(coordinates).all():
                raise ValueError("Invalid road coordinates")
            xs, ys = forward.transform(coordinates[:, 0], coordinates[:, 1])
            projected = np.column_stack((xs, ys))
            if not np.isfinite(projected).all():
                raise ValueError("Invalid projected road coordinates")
            lines.append(LineString(projected))
    except (KeyError, TypeError, ValueError) as exc:
        raise RuntimeError(
            "Could not load road network: invalid road geometry in Overpass response"
        ) from exc
    return lines


def load_road_access_index(
    bounds: tuple[float, float, float, float],
    terrain_crs: str,
    road_classes: RoadAccessClasses,
    max_distance_m: float,
    cache_directory: Path,
) -> RoadAccessIndex:
    """Fetch ways around projected search bounds, including roads just outside them."""
    min_x, min_y, max_x, max_y = bounds
    reverse = Transformer.from_crs(terrain_crs, 4326, always_xy=True)
    west, south, east, north = reverse.transform_bounds(
        min_x - max_distance_m,
        min_y - max_distance_m,
        max_x + max_distance_m,
        max_y + max_distance_m,
    )
    # Round outward for the cache and query so rounding never shrinks the fetch area.
    scale = 100_000
    bbox = (
        math.floor(south * scale) / scale,
        math.floor(west * scale) / scale,
        math.ceil(north * scale) / scale,
        math.ceil(east * scale) / scale,
    )
    tags = "|".join(_HIGHWAY_TAGS[road_classes])
    query = (
        f'[out:json][timeout:45];way["highway"~"^({tags})$"]({",".join(map(str, bbox))});out geom;'
    )
    endpoint = os.environ.get("RF_PLANNER_OVERPASS_URL", _OVERPASS_URL)
    digest = hashlib.sha256(f"{endpoint}\n{query}".encode()).hexdigest()
    cache_path = cache_directory / f"overpass-{digest}.json"
    forward = Transformer.from_crs(4326, terrain_crs, always_xy=True)
    if cache_path.exists():
        try:
            payload = json.loads(cache_path.read_text(encoding="utf-8"))
        except (OSError, ValueError) as exc:
            raise RuntimeError("Could not load road network: unreadable Overpass cache") from exc
        return RoadAccessIndex(_projected_lines(payload, forward), max_distance_m)

    try:
        response = requests.post(
            endpoint,
            data={"data": query},
            headers={"User-Agent": "RepeaterNet (road access planner)"},
            timeout=(10, 60),
        )
        response.raise_for_status()
    except requests.RequestException as exc:
        status = exc.response.status_code if exc.response is not None else None
        detail = f"HTTP {status}" if status is not None else "request failed"
        raise RuntimeError(f"Could not load road network from Overpass ({detail})") from exc
    try:
        payload = response.json()
    except ValueError as exc:
        raise RuntimeError("Could not load road network: Overpass returned invalid JSON") from exc
    lines = _projected_lines(payload, forward)

    cache_directory.mkdir(parents=True, exist_ok=True)
    with NamedTemporaryFile(
        mode="w", encoding="utf-8", dir=cache_directory, delete_on_close=False
    ) as temporary:
        json.dump(payload, temporary, separators=(",", ":"))
        temporary.close()
        Path(temporary.name).replace(cache_path)
    return RoadAccessIndex(lines, max_distance_m)
