from __future__ import annotations

import numpy as np

EARTH_MEAN_RADIUS_M = 6_371_000.0


def earth_bulge_m(
    distance_from_start_m: float | np.ndarray,
    total_distance_m: float,
    k_factor: float = 4.0 / 3.0,
) -> float | np.ndarray:
    """Earth bulge relative to the endpoint chord using an effective Earth radius."""
    if total_distance_m <= 0 or k_factor <= 0:
        raise ValueError("Distance and k-factor must be positive")
    d = np.asarray(distance_from_start_m, dtype=float)
    bulge = d * (total_distance_m - d) / (2.0 * EARTH_MEAN_RADIUS_M * k_factor)
    return float(bulge) if bulge.ndim == 0 else bulge
