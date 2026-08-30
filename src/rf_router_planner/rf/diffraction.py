from __future__ import annotations

import math
from dataclasses import dataclass

import numpy as np

from .fresnel import SPEED_OF_LIGHT_M_S


def knife_edge_loss_db(v: float) -> float:
    """ITU-R P.526 single knife-edge approximation."""
    if v <= -0.78:
        return 0.0
    return 6.9 + 20.0 * math.log10(math.sqrt((v - 0.1) ** 2 + 1.0) + v - 0.1)


def knife_edge_v(
    height_above_los_m: float, d1_m: float, d2_m: float, frequency_mhz: float
) -> float:
    if d1_m <= 0 or d2_m <= 0:
        return -math.inf
    wavelength = SPEED_OF_LIGHT_M_S / (frequency_mhz * 1_000_000.0)
    return height_above_los_m * math.sqrt(2.0 * (d1_m + d2_m) / (wavelength * d1_m * d2_m))


@dataclass(frozen=True, slots=True)
class DiffractionResult:
    loss_db: float
    obstacles: list[dict[str, float]]


def deygout_loss_db(
    distances_m: np.ndarray,
    obstruction_elevations_m: np.ndarray,
    los_elevations_m: np.ndarray,
    frequency_mhz: float,
    maximum_depth: int = 8,
) -> DiffractionResult:
    """Recursive Deygout multiple-knife-edge calculation.

    The dominant edge is the point with maximum Fresnel-Kirchhoff v. Secondary
    edges on each side are recursively evaluated against their local ray.
    """
    d = np.asarray(distances_m, dtype=float)
    surface = np.asarray(obstruction_elevations_m, dtype=float)
    los = np.asarray(los_elevations_m, dtype=float)
    if len(d) < 3:
        return DiffractionResult(0.0, [])

    def solve(
        lo: int, hi: int, ray_lo: float, ray_hi: float, depth: int
    ) -> tuple[float, list[dict[str, float]]]:
        if hi - lo < 2 or depth >= maximum_depth:
            return 0.0, []
        segment_distance = d[hi] - d[lo]
        local_ray = ray_lo + (ray_hi - ray_lo) * (d[lo + 1 : hi] - d[lo]) / segment_distance
        heights = surface[lo + 1 : hi] - local_ray
        d1 = d[lo + 1 : hi] - d[lo]
        d2 = d[hi] - d[lo + 1 : hi]
        wavelength = SPEED_OF_LIGHT_M_S / (frequency_mhz * 1_000_000.0)
        v_values = heights * np.sqrt(2.0 * segment_distance / (wavelength * d1 * d2))
        relative = int(np.argmax(v_values))
        idx = lo + 1 + relative
        v = float(v_values[relative])
        primary = knife_edge_loss_db(v)
        if primary <= 0:
            return 0.0, []
        obstacle = {
            "distance_m": float(d[idx]),
            "elevation_m": float(surface[idx]),
            "v": v,
            "loss_db": primary,
        }
        left_loss, left_obstacles = solve(lo, idx, ray_lo, float(surface[idx]), depth + 1)
        right_loss, right_obstacles = solve(idx, hi, float(surface[idx]), ray_hi, depth + 1)
        return primary + left_loss + right_loss, [obstacle, *left_obstacles, *right_obstacles]

    loss, obstacles = solve(0, len(d) - 1, float(los[0]), float(los[-1]), 0)
    return DiffractionResult(loss, obstacles)
