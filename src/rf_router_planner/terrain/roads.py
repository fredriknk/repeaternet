"""OpenStreetMap road geometry from Overpass and projected distance queries."""

from __future__ import annotations

import hashlib
import json
import math
import os
from collections.abc import Callable, Sequence
from dataclasses import dataclass
from datetime import UTC, datetime
from email.utils import parsedate_to_datetime
from pathlib import Path
from tempfile import NamedTemporaryFile
from threading import Event, Lock

import numpy as np
import requests
from pyproj import Transformer
from shapely import STRtree, buffer, get_parts, points, polygons, union_all
from shapely.geometry import LineString

from rf_router_planner.models.network import NetworkSolution
from rf_router_planner.models.settings import RoadAccessClasses
from rf_router_planner.models.site import SiteKind

_OVERPASS_URL = "https://overpass-api.de/api/interpreter"
# 0.1° latitude × 0.2° longitude is about 11 × 11 km at 60° N.
_LAT_TILES_PER_DEGREE = 10
_LON_TILES_PER_DEGREE = 5
_TILE_QUERY_VERSION = 1
_OVERPASS_LOCK = Lock()
_MAX_ATTEMPTS = 4
_RETRYABLE_STATUS = {429, 502, 503, 504}
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

    def nearby(
        self, locations: dict[str, tuple[float, float]], radius: float
    ) -> tuple[dict[str, float | None], list[LineString]]:
        distances: dict[str, float | None] = dict.fromkeys(locations)
        if not locations:
            return distances, []
        site_ids = list(locations)
        centers = points(list(locations.values()))
        matches, nearest = self._tree.query_nearest(
            centers, max_distance=radius, return_distance=True, all_matches=False
        )
        for index, distance in zip(matches[0], nearest, strict=True):
            distances[site_ids[index]] = float(distance)

        # Clip each road against the union so neighbouring repeaters share the
        # same stretch of road within the solution.
        area = union_all(buffer(centers, radius, quad_segs=32))
        lines = []
        for index in sorted(self._tree.query(area, predicate="intersects")):
            clipped = self._tree.geometries[index].intersection(area)
            for part in get_parts(clipped):
                if isinstance(part, LineString) and part.length > 0:
                    lines.append(part.simplify(7).normalize())
        return distances, lines


@dataclass(slots=True)
class RoadOverlay:
    polylines: list[tuple[tuple[float, float], ...]]
    polyline_indices: list[list[int]]
    nearest_distances_m: list[dict[str, float | None]]


class RoadLoadCancelled(RuntimeError):
    """The route job stopped while resolving road tiles."""


Tile = tuple[int, int]


def _tiles_for_points(
    xs: np.ndarray, ys: np.ndarray, radius: float, forward: Transformer, reverse: Transformer
) -> list[Tile]:
    if not xs.size:
        return []

    # Densify tile edges before projecting. A metre of extra clearance covers
    # interpolation error along these short segments in Norwegian metre CRSs.
    radius += 1
    west, south, east, north = reverse.transform_bounds(
        xs.min() - radius, ys.min() - radius, xs.max() + radius, ys.max() + radius
    )
    longitude_ranges = [(west, east)] if west <= east else [(west, 180), (-180, east)]
    tiles = [
        (row, col)
        for row in range(
            max(-900, math.floor(south * _LAT_TILES_PER_DEGREE)),
            min(900, math.floor(north * _LAT_TILES_PER_DEGREE) + 1),
        )
        for lo, hi in longitude_ranges
        for col in range(
            max(-900, math.floor(lo * _LON_TILES_PER_DEGREE)),
            min(900, math.floor(hi * _LON_TILES_PER_DEGREE) + 1),
        )
    ]
    if not tiles:
        return []
    edges = np.linspace(0, 1, 20, endpoint=False)
    unit_lat = np.concatenate((np.zeros_like(edges), edges, np.ones_like(edges), 1 - edges))
    unit_lon = np.concatenate((edges, np.ones_like(edges), 1 - edges, np.zeros_like(edges)))
    rows, cols = np.array(tiles).T
    lon = (cols[:, None] + unit_lon) / _LON_TILES_PER_DEGREE
    lat = (rows[:, None] + unit_lat) / _LAT_TILES_PER_DEGREE
    tile_x, tile_y = forward.transform(lon, lat)
    tile_tree = STRtree(polygons(np.stack((tile_x, tile_y), axis=-1)))
    # Batch the point/tile join to bound memory on dense search grids.
    selected: set[int] = set()
    for start in range(0, xs.size, 16384):
        batch = slice(start, start + 16384)
        hits = tile_tree.query(points(xs[batch], ys[batch]), predicate="dwithin", distance=radius)
        selected.update(np.unique(hits[1]))
    return sorted(tiles[index] for index in selected)


def _projected_lines(payload: object, forward: Transformer) -> list[LineString]:
    if not isinstance(payload, dict) or not isinstance(payload.get("elements"), list):
        raise RuntimeError("invalid Overpass response")
    if payload.get("remark"):
        raise RuntimeError(f"Overpass reported {payload['remark']}")

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
        raise RuntimeError("invalid road geometry in Overpass response") from exc
    return lines


def _retry_delay(response: requests.Response | None, attempt: int) -> float:
    delay = float(2**attempt)
    if response is not None and (retry_after := response.headers.get("Retry-After")):
        try:
            return max(delay, int(retry_after))
        except ValueError:
            try:
                date = parsedate_to_datetime(retry_after).astimezone(UTC)
                return max(delay, (date - datetime.now(UTC)).total_seconds())
            except (TypeError, ValueError, OverflowError):
                pass
    return delay


def _request_tile(endpoint: str, query: str, cancel: Event) -> object:
    for attempt in range(_MAX_ATTEMPTS):
        _check_cancelled(cancel)
        try:
            response = requests.post(
                endpoint,
                data={"data": query},
                headers={"User-Agent": "RepeaterNet (road access planner)"},
                timeout=(10, 195),
            )
            response.raise_for_status()
        except requests.RequestException as exc:
            _check_cancelled(cancel)
            status = exc.response.status_code if exc.response is not None else None
            retryable = status in _RETRYABLE_STATUS or isinstance(
                exc, (requests.ConnectionError, requests.Timeout)
            )
            if not retryable or attempt == _MAX_ATTEMPTS - 1:
                detail = f"HTTP {status}" if status is not None else "connection or request failed"
                raise RuntimeError(f"Overpass {detail}") from exc
            if cancel.wait(_retry_delay(exc.response, attempt)):
                raise RoadLoadCancelled("Road network loading cancelled") from exc
        else:
            try:
                return response.json()
            except ValueError as exc:
                raise RuntimeError("Overpass returned invalid JSON") from exc
    raise AssertionError("Overpass request attempts exhausted")


def _check_cancelled(cancel: Event) -> None:
    if cancel.is_set():
        raise RoadLoadCancelled("Road network loading cancelled")


def _load_tile(
    tile: Tile,
    endpoint: str,
    road_classes: RoadAccessClasses,
    cache_directory: Path,
    forward: Transformer,
    cancel: Event,
) -> list[LineString]:
    row, col = tile
    tags = "|".join(_HIGHWAY_TAGS[road_classes])
    key = (endpoint, tags, tile, _TILE_QUERY_VERSION)
    digest = hashlib.sha256(json.dumps(key).encode()).hexdigest()
    cache_path = cache_directory / f"overpass-tile-{digest}.json"
    if cache_path.exists():
        try:
            payload = json.loads(cache_path.read_text(encoding="utf-8"))
            return _projected_lines(payload, forward)
        except (OSError, ValueError, RuntimeError):
            # Recover a damaged cache entry with a fresh validated response.
            pass

    south, north = row / _LAT_TILES_PER_DEGREE, (row + 1) / _LAT_TILES_PER_DEGREE
    west, east = col / _LON_TILES_PER_DEGREE, (col + 1) / _LON_TILES_PER_DEGREE
    query = (
        f'[out:json][timeout:180];way["highway"~"^({tags})$"]'
        f"({south},{west},{north},{east});out skel geom;"
    )
    payload = _request_tile(endpoint, query, cancel)
    lines = _projected_lines(payload, forward)

    cache_directory.mkdir(parents=True, exist_ok=True)
    with NamedTemporaryFile(
        mode="w", encoding="utf-8", dir=cache_directory, delete=False
    ) as temporary:
        temporary_path = Path(temporary.name)
        try:
            json.dump(payload, temporary, separators=(",", ":"))
            temporary.close()
            temporary_path.replace(cache_path)
        finally:
            temporary.close()
            temporary_path.unlink(missing_ok=True)
    return lines


class TiledRoadAccess:
    """Resolve road data lazily from the actual projected points under consideration."""

    def __init__(
        self,
        terrain_crs: str,
        road_classes: RoadAccessClasses,
        max_distance_m: float,
        cache_directory: Path,
        *,
        progress: Callable[[str, int, int], None] | None = None,
        cancel: Event | None = None,
    ) -> None:
        self._forward = Transformer.from_crs(4326, terrain_crs, always_xy=True)
        self._reverse = Transformer.from_crs(terrain_crs, 4326, always_xy=True)
        self._endpoint = os.environ.get("RF_PLANNER_OVERPASS_URL", _OVERPASS_URL)
        self._road_classes = road_classes
        self._max_distance_m = max_distance_m
        self._cache_directory = cache_directory
        self._progress = progress or (lambda _stage, _done, _total: None)
        self._cancel = cancel if cancel is not None else Event()
        self._loaded: set[Tile] = set()
        self._lines: dict[bytes, LineString] = {}
        self._index: RoadAccessIndex | None = None

    @property
    def _road_index(self) -> RoadAccessIndex:
        if self._index is None:
            self._index = RoadAccessIndex(list(self._lines.values()), self._max_distance_m)
        return self._index

    def _load_for_points(self, xs: np.ndarray, ys: np.ndarray, radius: float) -> None:
        _check_cancelled(self._cancel)
        tiles = _tiles_for_points(xs, ys, radius, self._forward, self._reverse)
        if all(tile in self._loaded for tile in tiles):
            return

        stage = "Loading road network"
        self._progress(stage, 0, len(tiles))
        for done, tile in enumerate(tiles, 1):
            _check_cancelled(self._cancel)
            if tile not in self._loaded:
                # Jobs can run in parallel, but share a single Overpass slot
                # and can reuse a tile already downloaded by another job.
                try:
                    with _OVERPASS_LOCK:
                        _check_cancelled(self._cancel)
                        lines = _load_tile(
                            tile, self._endpoint, self._road_classes, self._cache_directory,
                            self._forward, self._cancel,
                        )
                except RoadLoadCancelled:
                    raise
                except (RuntimeError, OSError) as exc:
                    raise RuntimeError(
                        f"Could not load road network: 1 of {len(tiles)} tiles failed "
                        f"(tile {tile}: {exc})"
                    ) from exc
                # Overpass returns full ways, so adjacent tiles can contain the
                # same geometry. Keep one copy in the shared spatial index.
                self._lines.update((line.wkb, line) for line in lines)
                self._loaded.add(tile)
                self._index = None
            self._progress(stage, done, len(tiles))
        _check_cancelled(self._cancel)

    def allowed(self, xs: np.ndarray, ys: np.ndarray) -> np.ndarray:
        self._load_for_points(xs.ravel(), ys.ravel(), self._max_distance_m)
        return self._road_index.allowed(xs, ys)

    def overlay(self, alternatives: Sequence[NetworkSolution]) -> RoadOverlay:
        routers = [
            {
                site.id: (site.x, site.y)
                for site in solution.sites
                if site.id in solution.router_ids
                and site.kind not in {SiteKind.ENDPOINT_A, SiteKind.ENDPOINT_B, SiteKind.CLIENT}
            }
            for solution in alternatives
        ]
        coordinates = {xy for locations in routers for xy in locations.values()}
        radius = self._max_distance_m * 1.5
        if coordinates:
            xs, ys = np.array(sorted(coordinates)).T
            self._load_for_points(xs, ys, radius)

        polylines: dict[tuple[tuple[float, float], ...], int] = {}
        polyline_indices = []
        nearest_distances = []
        for locations in routers:
            _check_cancelled(self._cancel)
            distances, lines = self._road_index.nearby(locations, radius)
            nearest_distances.append(distances)
            indices = set()
            for line in lines:
                xs, ys = np.asarray(line.coords).T
                lon, lat = self._reverse.transform(xs, ys)
                polyline = tuple(
                    zip(np.round(lat, 6).tolist(), np.round(lon, 6).tolist(), strict=True)
                )
                if len(set(polyline)) > 1:
                    indices.add(polylines.setdefault(polyline, len(polylines)))
            polyline_indices.append(sorted(indices))
        return RoadOverlay(list(polylines), polyline_indices, nearest_distances)
