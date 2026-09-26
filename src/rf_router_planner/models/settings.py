from __future__ import annotations

import os
from dataclasses import asdict, dataclass, field
from enum import Enum
from pathlib import Path
from typing import Any


def default_cache_directory() -> str:
    root = Path(os.environ.get("LOCALAPPDATA", Path.home() / ".cache"))
    return str(root / "RF Router Planner" / "terrain-cache")


class ValidationMode(str, Enum):
    PROPAGATION = "rf_propagation"
    STRICT_LOS = "strict_los"


class DiffractionModel(str, Enum):
    BULLINGTON = "bullington"
    DEYGOUT = "deygout"


class OptimizationPriority(str, Enum):
    MINIMUM_ROUTERS = "minimum_routers"
    FEWEST_NEW_INSTALLATIONS = "fewest_new_installations"
    MINIMUM_INFRASTRUCTURE = "minimum_infrastructure"
    MAXIMUM_RELIABILITY = "maximum_reliability"


class InfrastructurePolicy(str, Enum):
    PROPOSED_ONLY = "proposed_only"
    EXISTING_ONLY = "existing_only"
    EXISTING_AND_PROPOSED = "existing_and_proposed"


@dataclass(slots=True)
class LoRaSettings:
    enabled: bool = False
    bandwidth_hz: float = 125_000.0
    spreading_factor: int = 12
    coding_rate: str = "4/5"
    noise_figure_db: float = 6.0
    manual_sensitivity_override: bool = True
    snr_thresholds_db: dict[int, float] = field(
        default_factory=lambda: {7: -7.5, 8: -10.0, 9: -12.5, 10: -15.0, 11: -17.5, 12: -20.0}
    )

    def sensitivity_dbm(self) -> float:
        if self.spreading_factor not in self.snr_thresholds_db:
            raise ValueError("LoRa spreading factor must be SF7 through SF12")
        import math

        thermal_noise = -174.0 + 10.0 * math.log10(self.bandwidth_hz)
        return thermal_noise + self.noise_figure_db + self.snr_thresholds_db[self.spreading_factor]


@dataclass(slots=True)
class AntennaSettings:
    gain_dbi: float = 2.15
    feed_loss_db: float = 0.0
    height_agl_m: float = 3.0
    pattern_csv: str | None = None


@dataclass(slots=True)
class RFSettings:
    frequency_mhz: float = 869.5
    tx_power_dbm: float = 22.0
    receiver_sensitivity_dbm: float = -130.0
    fade_margin_db: float = 10.0
    miscellaneous_loss_db: float = 0.0
    k_factor: float = 4.0 / 3.0
    required_fresnel_clearance: float = 0.60
    validation_mode: ValidationMode = ValidationMode.PROPAGATION
    endpoint_a: AntennaSettings = field(default_factory=AntennaSettings)
    endpoint_b: AntennaSettings = field(default_factory=AntennaSettings)
    router: AntennaSettings = field(default_factory=AntennaSettings)
    lora: LoRaSettings = field(default_factory=LoRaSettings)
    clutter_loss_enabled: bool = False
    clutter_loss_db_per_m: float = 0.0
    diffraction_model: DiffractionModel = DiffractionModel.BULLINGTON

    @property
    def effective_sensitivity_dbm(self) -> float:
        if self.lora.enabled and not self.lora.manual_sensitivity_override:
            return self.lora.sensitivity_dbm()
        return self.receiver_sensitivity_dbm

    @classmethod
    def eu868_meshcore(cls) -> RFSettings:
        return cls()


@dataclass(slots=True)
class TerrainSettings:
    dtm_paths: list[str] = field(default_factory=list)
    dom_paths: list[str] = field(default_factory=list)
    cache_directory: str = field(default_factory=default_cache_directory)
    requested_resolution_m: float = 10.0
    auto_resolution: bool = True
    maximum_total_pixels: int = 50_000_000
    maximum_download_area_km2: float = 400.0
    maximum_pixels_per_tile: int = 4_000_000
    maximum_download_tiles: int = 256
    detail_resolution_m: float = 10.0
    detail_corridor_width_m: float = 500.0
    minimum_sample_step_m: float = 20.0


@dataclass(slots=True)
class CandidateSettings:
    infrastructure_policy: InfrastructurePolicy = InfrastructurePolicy.EXISTING_AND_PROPOSED
    corridor_width_m: float = 10_000.0
    unrestricted_bounding_area: bool = False
    grid_spacing_m: float = 1_000.0
    cell_size_m: float = 2_000.0
    candidates_per_cell: int = 3
    maximum_candidates: int = 800
    maximum_neighbors_per_site: int = 16
    maximum_solution_routers: int = 6
    reliability_paths: int = 2
    parallel_workers: int = 0
    coarse_sample_step_m: float = 200.0
    medium_sample_step_m: float = 50.0
    final_sample_step_m: float = 10.0
    maximum_link_distance_m: float | None = None
    refine_radius_m: float = 250.0
    refine_step_m: float = 50.0
    optimize_heights: bool = False
    minimum_router_height_m: float = 2.0
    maximum_router_height_m: float = 10.0
    router_height_step_m: float = 1.0
    priority: OptimizationPriority = OptimizationPriority.MINIMUM_ROUTERS


def dataclass_to_dict(value: Any) -> Any:
    """JSON-compatible recursive dataclass conversion with enum values."""
    if isinstance(value, Enum):
        return value.value
    if hasattr(value, "__dataclass_fields__"):
        return {k: dataclass_to_dict(v) for k, v in asdict(value).items()}
    if isinstance(value, dict):
        return {k: dataclass_to_dict(v) for k, v in value.items()}
    if isinstance(value, (list, tuple)):
        return [dataclass_to_dict(v) for v in value]
    return value
