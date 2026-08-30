from __future__ import annotations

import csv
from dataclasses import dataclass
from pathlib import Path

import numpy as np


class AntennaPattern:
    def gain_at(self, elevation_deg: float, azimuth_deg: float | None = None) -> float:
        raise NotImplementedError


@dataclass(frozen=True, slots=True)
class ConstantGain(AntennaPattern):
    gain_dbi: float

    def gain_at(self, elevation_deg: float, azimuth_deg: float | None = None) -> float:
        return self.gain_dbi


@dataclass(frozen=True, slots=True)
class ElevationPattern(AntennaPattern):
    elevation_deg: np.ndarray
    gain_dbi: np.ndarray

    def __post_init__(self) -> None:
        if len(self.elevation_deg) < 2 or len(self.elevation_deg) != len(self.gain_dbi):
            raise ValueError("An elevation pattern needs at least two matching samples")
        if np.any(np.diff(self.elevation_deg) <= 0):
            raise ValueError("Pattern elevation angles must be strictly increasing")

    def gain_at(self, elevation_deg: float, azimuth_deg: float | None = None) -> float:
        return float(np.interp(elevation_deg, self.elevation_deg, self.gain_dbi))

    @classmethod
    def from_csv(cls, path: str | Path) -> ElevationPattern:
        angles: list[float] = []
        gains: list[float] = []
        with Path(path).open(newline="", encoding="utf-8-sig") as handle:
            reader = csv.DictReader(handle)
            if not reader.fieldnames or not {"elevation_deg", "gain_dbi"}.issubset(
                reader.fieldnames
            ):
                raise ValueError("CSV must contain elevation_deg and gain_dbi columns")
            for row in reader:
                angles.append(float(row["elevation_deg"]))
                gains.append(float(row["gain_dbi"]))
        order = np.argsort(angles)
        return cls(np.asarray(angles)[order], np.asarray(gains)[order])


def elevation_angle_deg(
    horizontal_distance_m: float, tx_elevation_m: float, rx_elevation_m: float
) -> float:
    import math

    if horizontal_distance_m <= 0:
        raise ValueError("Horizontal distance must be positive")
    return math.degrees(math.atan2(rx_elevation_m - tx_elevation_m, horizontal_distance_m))
