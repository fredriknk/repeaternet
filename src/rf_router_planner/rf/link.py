from __future__ import annotations

import math


def free_space_path_loss_db(distance_m: float, frequency_mhz: float) -> float:
    """Free-space path loss using distance in metres and frequency in MHz."""
    if distance_m <= 0 or frequency_mhz <= 0:
        raise ValueError("Distance and frequency must be positive")
    return 32.44 + 20.0 * math.log10(distance_m / 1000.0) + 20.0 * math.log10(frequency_mhz)


def received_power_dbm(
    tx_power_dbm: float,
    tx_gain_dbi: float,
    tx_feed_loss_db: float,
    propagation_loss_db: float,
    rx_gain_dbi: float,
    rx_feed_loss_db: float,
    miscellaneous_loss_db: float = 0.0,
) -> float:
    return (
        tx_power_dbm
        + tx_gain_dbi
        - tx_feed_loss_db
        - propagation_loss_db
        + rx_gain_dbi
        - rx_feed_loss_db
        - miscellaneous_loss_db
    )
