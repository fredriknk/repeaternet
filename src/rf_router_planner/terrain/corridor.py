"""The route search corridor: a band around the A-B line with square ends.

The corridor reaches ``half_width_m`` to each side of the route line and the
same distance beyond each endpoint, at full width.  Candidate generation,
terrain downloads, MeshCore lookups and the default coverage area all use this
one shape.
"""

from __future__ import annotations

import math
from collections.abc import Sequence

import numpy as np
from shapely.geometry import LineString, MultiPoint, Point, box
from shapely.geometry.base import BaseGeometry


def inside_corridor(
    xs: np.ndarray,
    ys: np.ndarray,
    start: tuple[float, float],
    end: tuple[float, float],
    half_width_m: float,
) -> np.ndarray:
    """Vectorized membership test for the square-ended corridor (projected metres)."""
    xs = np.asarray(xs, dtype=float)
    ys = np.asarray(ys, dtype=float)
    ax, ay = start
    bx, by = end
    dx, dy = bx - ax, by - ay
    length = math.hypot(dx, dy)
    if length == 0:
        return (np.abs(xs - ax) <= half_width_m) & (np.abs(ys - ay) <= half_width_m)
    ux, uy = dx / length, dy / length
    along = (xs - ax) * ux + (ys - ay) * uy
    across = -(xs - ax) * uy + (ys - ay) * ux
    return (
        (along >= -half_width_m)
        & (along <= length + half_width_m)
        & (np.abs(across) <= half_width_m)
    )


def corridor_geometry(
    points: Sequence[tuple[float, float]],
    half_width_m: float,
    extra_points: Sequence[tuple[float, float]] = (),
) -> BaseGeometry:
    """Polygon of the corridor along ``points``; extra sites get a round buffer."""
    if half_width_m <= 0:
        raise ValueError("Corridor width must be positive")
    if len(points) >= 2 and LineString(points).length > 0:
        shape: BaseGeometry = LineString(points).buffer(
            half_width_m, cap_style="square", join_style="mitre"
        )
    else:
        x, y = points[0]
        shape = box(x - half_width_m, y - half_width_m, x + half_width_m, y + half_width_m)
    if extra_points:
        shape = shape.union(MultiPoint(list(extra_points)).buffer(half_width_m))
    return shape


def corridor_bounds(
    points: Sequence[tuple[float, float]],
    half_width_m: float,
    extra_points: Sequence[tuple[float, float]] = (),
) -> tuple[float, float, float, float]:
    """Axis-aligned bounds that contain the whole corridor, including rotated corners."""
    left, bottom, right, top = corridor_geometry(points, half_width_m, extra_points).bounds
    return float(left), float(bottom), float(right), float(top)


def site_buffer(x: float, y: float, radius_m: float) -> BaseGeometry:
    return Point(x, y).buffer(radius_m)
