from __future__ import annotations

import numpy as np

SPEED_OF_LIGHT_M_S = 299_792_458.0


def first_fresnel_radius_m(
    distance_from_tx_m: float | np.ndarray,
    distance_to_rx_m: float | np.ndarray,
    frequency_mhz: float,
) -> float | np.ndarray:
    if frequency_mhz <= 0:
        raise ValueError("Frequency must be positive")
    d1 = np.asarray(distance_from_tx_m, dtype=float)
    d2 = np.asarray(distance_to_rx_m, dtype=float)
    if np.any(d1 < 0) or np.any(d2 < 0):
        raise ValueError("Distances cannot be negative")
    denominator = d1 + d2
    wavelength = SPEED_OF_LIGHT_M_S / (frequency_mhz * 1_000_000.0)
    radius = np.sqrt(
        np.divide(
            wavelength * d1 * d2, denominator, out=np.zeros_like(d1 + d2), where=denominator > 0
        )
    )
    return float(radius) if radius.ndim == 0 else radius
