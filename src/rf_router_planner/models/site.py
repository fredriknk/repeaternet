from __future__ import annotations

from dataclasses import dataclass
from enum import Enum


class SiteKind(str, Enum):
    ENDPOINT_A = "endpoint_a"
    ENDPOINT_B = "endpoint_b"
    CLIENT = "client"
    ROUTER = "router"
    CANDIDATE = "candidate"


class SiteOrigin(str, Enum):
    """How a site entered the project.

    ``OPTIMIZED`` is the backward-compatible default for generated candidate
    sites.  Manual and known-network sites are kept distinct so the UI can
    render them differently without overloading ``SiteKind``.
    """

    MANUAL = "manual"
    KNOWN = "known"
    OPTIMIZED = "optimized"


class HeightReference(str, Enum):
    GROUND_DTM = "ground_dtm"
    SURFACE_DOM = "surface_dom"


@dataclass(slots=True)
class Site:
    id: str
    x: float
    y: float
    latitude: float | None = None
    longitude: float | None = None
    kind: SiteKind = SiteKind.CANDIDATE
    ground_elevation_m: float = 0.0
    surface_elevation_m: float | None = None
    antenna_height_m: float = 3.0
    height_reference: HeightReference = HeightReference.GROUND_DTM
    terrain_slope: float = 0.0
    site_quality: float = 0.0
    locked: bool = False
    origin: SiteOrigin = SiteOrigin.OPTIMIZED
    required: bool = False
    enabled: bool = True
    height_override: bool = False

    @property
    def obstruction_height_m(self) -> float | None:
        if self.surface_elevation_m is None:
            return None
        return max(0.0, self.surface_elevation_m - self.ground_elevation_m)

    @property
    def antenna_absolute_elevation_m(self) -> float:
        base = self.ground_elevation_m
        if (
            self.height_reference == HeightReference.SURFACE_DOM
            and self.surface_elevation_m is not None
        ):
            base = self.surface_elevation_m
        return base + self.antenna_height_m

    def distance_to(self, other: Site) -> float:
        import math

        return math.hypot(other.x - self.x, other.y - self.y)
