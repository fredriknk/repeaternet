from __future__ import annotations

from dataclasses import dataclass

import numpy as np

from rf_router_planner.models.site import Site
from rf_router_planner.rf.curvature import earth_bulge_m
from rf_router_planner.rf.fresnel import first_fresnel_radius_m

from .raster import TerrainSource


@dataclass(slots=True)
class TerrainProfile:
    distances_m: np.ndarray
    x: np.ndarray
    y: np.ndarray
    dtm_elevation_m: np.ndarray
    surface_elevation_m: np.ndarray
    earth_bulge_m: np.ndarray
    effective_obstruction_m: np.ndarray
    los_elevation_m: np.ndarray
    fresnel_radius_m: np.ndarray
    clearance_m: np.ndarray
    fresnel_clearance_ratio: np.ndarray
    surface_available: bool


def sample_profile(
    terrain: TerrainSource,
    source: Site,
    target: Site,
    frequency_mhz: float,
    k_factor: float = 4.0 / 3.0,
    sample_step_m: float | None = None,
) -> TerrainProfile:
    distance = source.distance_to(target)
    if distance <= 0:
        raise ValueError("Link endpoints must not have identical coordinates")
    step = max(float(sample_step_m or terrain.resolution_m), terrain.resolution_m)
    count = max(3, int(np.ceil(distance / step)) + 1)
    distances = np.linspace(0.0, distance, count)
    fraction = distances / distance
    xs = source.x + fraction * (target.x - source.x)
    ys = source.y + fraction * (target.y - source.y)
    dtm = terrain.sample(xs, ys, surface=False)
    if np.any(~np.isfinite(dtm)):
        raise ValueError("Terrain data does not cover the complete link")
    surface_available = terrain.has_surface
    surface = terrain.sample(xs, ys, surface=True) if surface_available else dtm.copy()
    invalid_surface = ~np.isfinite(surface)
    surface[invalid_surface] = dtm[invalid_surface]
    bulge = np.asarray(earth_bulge_m(distances, distance, k_factor), dtype=float)
    effective_obstruction = surface + bulge
    los = source.antenna_absolute_elevation_m + fraction * (
        target.antenna_absolute_elevation_m - source.antenna_absolute_elevation_m
    )
    fresnel = np.asarray(
        first_fresnel_radius_m(distances, distance - distances, frequency_mhz), dtype=float
    )
    clearance = los - effective_obstruction
    ratios = np.divide(
        clearance, fresnel, out=np.full_like(clearance, np.inf), where=fresnel > 1e-9
    )
    return TerrainProfile(
        distances,
        xs,
        ys,
        dtm,
        surface,
        bulge,
        effective_obstruction,
        los,
        fresnel,
        clearance,
        ratios,
        surface_available,
    )
