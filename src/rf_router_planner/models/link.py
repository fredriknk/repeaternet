from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any


@dataclass(slots=True)
class DirectionResult:
    source_id: str
    target_id: str
    distance_m: float
    fspl_db: float
    diffraction_loss_db: float
    clutter_loss_db: float
    total_path_loss_db: float
    tx_gain_dbi: float
    rx_gain_dbi: float
    received_power_dbm: float
    sensitivity_dbm: float
    raw_margin_db: float
    usable_margin_db: float
    departure_angle_deg: float
    arrival_angle_deg: float
    valid: bool
    tx_power_dbm: float = 0.0
    tx_feed_loss_db: float = 0.0
    rx_feed_loss_db: float = 0.0
    miscellaneous_loss_db: float = 0.0


@dataclass(slots=True)
class LinkResult:
    source_id: str
    target_id: str
    distance_m: float
    forward: DirectionResult
    reverse: DirectionResult
    valid: bool
    los_clear: bool
    fresnel_clear: bool
    minimum_clearance_m: float
    minimum_fresnel_clearance_ratio: float
    minimum_clearance_distance_m: float
    maximum_fresnel_radius_m: float
    diffraction_loss_db: float
    dominant_obstacles: list[dict[str, float]] = field(default_factory=list)
    profile: Any | None = None

    @property
    def worst_margin_db(self) -> float:
        return min(self.forward.usable_margin_db, self.reverse.usable_margin_db)
