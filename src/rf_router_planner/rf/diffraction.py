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


def bullington_loss_db(
    distances_m: np.ndarray,
    obstruction_elevations_m: np.ndarray,
    los_elevations_m: np.ndarray,
    frequency_mhz: float,
) -> DiffractionResult:
    """P.526-16 §4.5.1 Bullington component (not full delta-Bullington).

    Inputs use metres and already include effective-Earth bulge. Applying
    curvature again here would double-count it. The virtual edge is the
    intersection of the two horizon rays, or the maximum-v point for LOS.
    """
    d, surface, los = (
        np.asarray(values, dtype=float)
        for values in (distances_m, obstruction_elevations_m, los_elevations_m)
    )
    if (
        d.ndim != 1 or surface.shape != d.shape or los.shape != d.shape
        or len(d) < 2 or not all(np.all(np.isfinite(a)) for a in (d, surface, los))
        or np.any(np.diff(d) <= 0)
        or not math.isfinite(frequency_mhz) or frequency_mhz <= 0
    ):
        raise ValueError("Diffraction requires matching finite profiles, increasing distances and positive frequency")
    if len(d) == 2:
        return DiffractionResult(0.0, [])
    d = d - d[0]
    length = float(d[-1])
    x = d[1:-1]
    tx, rx = float(los[0]), float(los[-1])
    wavelength = SPEED_OF_LIGHT_M_S / (frequency_mhz * 1e6)
    slope_tx = float(np.max((surface[1:-1] - tx) / x))
    direct_slope = (rx - tx) / length
    if slope_tx < direct_slope:
        ray = tx + direct_slope * x
        v_values = (surface[1:-1] - ray) * np.sqrt(2 * length / (wavelength * x * (length - x)))
        index = int(np.argmax(v_values))
        edge_distance = float(x[index])
        edge_height = float(surface[index + 1])
        v = float(v_values[index])
    else:
        slope_rx = float(np.max((surface[1:-1] - rx) / (length - x)))
        # A profile exactly tangent to LOS has v=0 and no unique intersection.
        denominator = slope_tx + slope_rx
        edge_distance = (rx - tx + slope_rx * length) / denominator if denominator > 0 else length / 2
        edge_distance = float(np.clip(edge_distance, x[0], x[-1]))
        edge_height = tx + slope_tx * edge_distance
        height = edge_height - (tx + direct_slope * edge_distance)
        v = knife_edge_v(height, edge_distance, length - edge_distance, frequency_mhz)
    knife_loss = knife_edge_loss_db(v)
    loss = knife_loss + (-math.expm1(-knife_loss / 6)) * (10 + 0.02 * length / 1000)
    obstacles = [] if loss == 0 else [{
        "distance_m": edge_distance,
        "elevation_m": edge_height,
        "v": v,
        "loss_db": loss,
    }]
    return DiffractionResult(loss, obstacles)


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
