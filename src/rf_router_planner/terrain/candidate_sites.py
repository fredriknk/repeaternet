from __future__ import annotations

import math
from collections import defaultdict
from collections.abc import Iterable

import numpy as np

from rf_router_planner.models.settings import CandidateSettings
from rf_router_planner.models.site import Site, SiteKind

from .raster import TerrainSource


def _distance_to_segment(
    x: np.ndarray, y: np.ndarray, ax: float, ay: float, bx: float, by: float
) -> np.ndarray:
    dx, dy = bx - ax, by - ay
    length_squared = dx * dx + dy * dy
    if length_squared == 0:
        return np.hypot(x - ax, y - ay)
    t = np.clip(((x - ax) * dx + (y - ay) * dy) / length_squared, 0.0, 1.0)
    return np.hypot(x - (ax + t * dx), y - (ay + t * dy))


def _inside_polygon(x: float, y: float, polygon: list[tuple[float, float]]) -> bool:
    inside = False
    j = len(polygon) - 1
    for i, (xi, yi) in enumerate(polygon):
        xj, yj = polygon[j]
        if (yi > y) != (yj > y) and x < (xj - xi) * (y - yi) / (yj - yi) + xi:
            inside = not inside
        j = i
    return inside


def generate_candidates(
    terrain: TerrainSource,
    endpoint_a: Site,
    endpoint_b: Site,
    settings: CandidateSettings,
    exclusions: Iterable[list[tuple[float, float]]] = (),
) -> list[Site]:
    """Generate and spatially thin grid/high-ground candidates in an A-B corridor."""
    padding = settings.corridor_width_m
    min_x = max(terrain.bounds[0], min(endpoint_a.x, endpoint_b.x) - padding)
    min_y = max(terrain.bounds[1], min(endpoint_a.y, endpoint_b.y) - padding)
    max_x = min(terrain.bounds[2], max(endpoint_a.x, endpoint_b.x) + padding)
    max_y = min(terrain.bounds[3], max(endpoint_a.y, endpoint_b.y) + padding)
    # Sample densely enough that the later local-refinement radius can actually
    # reach useful high ground.  A single grid whose points are 1 km apart can
    # miss a summit by more than 700 m, so a 250 m refinement radius would never
    # be able to recover it after route selection.  Cell thinning below still
    # bounds the number of RF candidates exposed to the optimizer.
    search_spacing = settings.grid_spacing_m
    if settings.refine_radius_m > 0:
        search_spacing = min(search_spacing, settings.refine_radius_m)
    spacing = max(search_spacing, terrain.resolution_m)
    xs_1d = np.arange(min_x, max_x + spacing * 0.5, spacing)
    ys_1d = np.arange(min_y, max_y + spacing * 0.5, spacing)
    if not len(xs_1d) or not len(ys_1d):
        return [endpoint_a, endpoint_b]
    grid_x, grid_y = np.meshgrid(xs_1d, ys_1d)
    xs, ys = grid_x.ravel(), grid_y.ravel()
    if not settings.unrestricted_bounding_area:
        mask = (
            _distance_to_segment(xs, ys, endpoint_a.x, endpoint_a.y, endpoint_b.x, endpoint_b.y)
            <= settings.corridor_width_m
        )
        xs, ys = xs[mask], ys[mask]
    dtm = terrain.sample(xs, ys)
    dom = terrain.sample(xs, ys, surface=True) if terrain.has_surface else dtm.copy()
    valid = np.isfinite(dtm)
    xs, ys, dtm, dom = xs[valid], ys[valid], dtm[valid], dom[valid]
    if not len(xs):
        return [endpoint_a, endpoint_b]

    delta = max(terrain.resolution_m, spacing / 4.0)
    east = terrain.sample(xs + delta, ys)
    west = terrain.sample(xs - delta, ys)
    north = terrain.sample(xs, ys + delta)
    south = terrain.sample(xs, ys - delta)
    slope = np.hypot((east - west) / (2 * delta), (north - south) / (2 * delta))
    slope[~np.isfinite(slope)] = 1.0
    obstruction = np.maximum(0.0, np.nan_to_num(dom - dtm, nan=0.0))
    elevation_span = max(float(np.ptp(dtm)), 1.0)
    elevation_score = (dtm - float(np.min(dtm))) / elevation_span
    # High terrain helps, but slope and surface obstructions reduce installation quality.
    quality = (
        elevation_score - np.clip(slope, 0, 1) * 0.35 - np.clip(obstruction / 20.0, 0, 1) * 0.25
    )

    polygons = list(exclusions)
    cells: dict[tuple[int, int], list[int]] = defaultdict(list)
    for index, (x, y) in enumerate(zip(xs, ys, strict=True)):
        if any(_inside_polygon(float(x), float(y), polygon) for polygon in polygons):
            continue
        cell = (math.floor(x / settings.cell_size_m), math.floor(y / settings.cell_size_m))
        cells[cell].append(index)

    retained: list[int] = []
    for indices in cells.values():
        indices.sort(key=lambda i: (float(quality[i]), float(dtm[i])), reverse=True)
        retained.extend(indices[: settings.candidates_per_cell])
    retained.sort(key=lambda i: float(quality[i]), reverse=True)
    retained = retained[: settings.maximum_candidates]
    candidates = [endpoint_a]
    for number, index in enumerate(retained, 1):
        candidates.append(
            Site(
                f"C{number}",
                float(xs[index]),
                float(ys[index]),
                kind=SiteKind.CANDIDATE,
                ground_elevation_m=float(dtm[index]),
                surface_elevation_m=float(dom[index]) if terrain.has_surface else None,
                antenna_height_m=settings.minimum_router_height_m
                if settings.optimize_heights
                else 3.0,
                terrain_slope=float(slope[index]),
                site_quality=float(quality[index]),
            )
        )
    candidates.append(endpoint_b)
    return candidates
