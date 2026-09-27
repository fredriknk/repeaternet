"""Typed settings and compact results for predicted mesh coverage."""

from __future__ import annotations

from dataclasses import dataclass, field
from enum import Enum

from .settings import AntennaSettings


class CoverageMode(str, Enum):
    TWO_WAY = "two_way"
    DOWNLINK = "downlink"
    UPLINK = "uplink"


class CoverageState(str, Enum):
    NOT_EVALUATED = "not_evaluated"
    UNKNOWN_TERRAIN = "unknown_terrain"
    COVERED = "covered"
    UNCOVERED = "uncovered"


@dataclass(slots=True)
class ClientRadioProfile:
    """Editable example radio assumptions for a handheld client."""

    name: str = "Handheld · 1.5 m"
    height_agl_m: float = 1.5
    tx_power_dbm: float = 20.0
    gain_dbi: float = 2.15
    feed_loss_db: float = 0.0
    sensitivity_dbm: float = -130.0
    miscellaneous_loss_db: float = 0.0

    def as_radio_budget(self) -> RadioBudget:
        return RadioBudget(
            antenna=AntennaSettings(
                gain_dbi=self.gain_dbi,
                feed_loss_db=self.feed_loss_db,
                height_agl_m=self.height_agl_m,
            ),
            tx_power_dbm=self.tx_power_dbm,
            sensitivity_dbm=self.sensitivity_dbm,
            miscellaneous_loss_db=self.miscellaneous_loss_db,
        )


@dataclass(slots=True)
class RadioBudget:
    """Per-endpoint radio values used for asymmetric link directions."""

    antenna: AntennaSettings
    tx_power_dbm: float
    sensitivity_dbm: float
    miscellaneous_loss_db: float = 0.0


@dataclass(slots=True)
class CoverageSettings:
    mode: CoverageMode = CoverageMode.TWO_WAY
    cell_size_m: float = 1_000.0
    area_buffer_m: float = 5_000.0
    profile_step_m: float = 200.0
    maximum_cells: int = 4_096
    maximum_sources: int = 64
    include_endpoints: bool = False
    client: ClientRadioProfile = field(default_factory=ClientRadioProfile)


@dataclass(slots=True)
class CoverageSourceResult:
    source_id: str
    downlink_margin_db: float | None
    uplink_margin_db: float | None
    two_way_margin_db: float | None
    valid_downlink: bool
    valid_uplink: bool
    valid_two_way: bool
    rejection: str | None = None


@dataclass(slots=True)
class CoverageCell:
    index: int
    x: float
    y: float
    latitude: float
    longitude: float
    state: CoverageState = CoverageState.NOT_EVALUATED
    source_count: int = 0
    best_margin_db: float | None = None
    best_source_id: str | None = None
    sources: list[CoverageSourceResult] = field(default_factory=list)


@dataclass(slots=True)
class CoverageGrid:
    bounds: tuple[float, float, float, float]
    requested_cell_size_m: float
    effective_cell_size_m: float
    rows: int
    columns: int
    requested_cells: int
    terrain_available_cells: int = 0
    evaluated_cells: int = 0
    completed_cells: int = 0
    cells: list[CoverageCell] = field(default_factory=list)
    surface_available: bool = False
