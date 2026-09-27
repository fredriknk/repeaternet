"""Exact, profile-free client evaluation for point and road targets."""

from __future__ import annotations

import math
from typing import Any

import numpy as np
from pyproj import Transformer

from rf_router_planner.models.coverage import CoverageSettings
from rf_router_planner.models.settings import CandidateSettings, RFSettings, ValidationMode
from rf_router_planner.models.site import Site, SiteKind
from rf_router_planner.rf.propagation import LinkEvaluator
from rf_router_planner.terrain.raster import TerrainSource


def evaluate_client_point(
    terrain: TerrainSource,
    rf_settings: RFSettings,
    candidate_settings: CandidateSettings,
    sources: list[Site],
    coverage_settings: CoverageSettings,
    latitude: float,
    longitude: float,
    evaluator: LinkEvaluator,
) -> dict[str, Any]:
    """Evaluate exact WGS84 coordinates with C4 client budgets and validation."""
    forward = Transformer.from_crs(4326, terrain.crs, always_xy=True)
    x, y = forward.transform(longitude, latitude)
    ground = float(terrain.sample(np.array([x]), np.array([y]))[0])
    if not math.isfinite(ground):
        return {
            "state": "unknown_terrain",
            "latitude": latitude,
            "longitude": longitude,
            "sources": [],
            "message": "No ground-elevation data at this target; coverage is unknown.",
        }
    surface_elevation = None
    if terrain.has_surface:
        value = float(terrain.sample(np.array([x]), np.array([y]), surface=True)[0])
        surface_elevation = value if math.isfinite(value) else None
    client = coverage_settings.client
    target = Site(
        "coverage-target-client",
        float(x),
        float(y),
        latitude,
        longitude,
        SiteKind.CLIENT,
        ground_elevation_m=ground,
        surface_elevation_m=surface_elevation,
        antenna_height_m=client.height_agl_m,
    )
    structural_validation = rf_settings.validation_mode != ValidationMode.STRICT_LOS
    source_results: list[dict[str, Any]] = []
    for source in sources:
        distance = source.distance_to(target)
        sample_step = max(
            candidate_settings.final_sample_step_m,
            distance / max(1, coverage_settings.maximum_profile_samples - 1),
        )
        try:
            link = evaluator.evaluate(
                source,
                target,
                sample_step,
                include_profile=False,
                target_radio=client.as_radio_budget(),
            )
        except ValueError:
            source_results.append(
                {
                    "source_id": source.id,
                    "distance_m": distance,
                    "downlink_margin_db": None,
                    "uplink_margin_db": None,
                    "two_way_margin_db": None,
                    "valid_downlink": False,
                    "valid_uplink": False,
                    "valid_two_way": False,
                    "rejection": "unknown_terrain",
                }
            )
            continue
        structural_ok = structural_validation or (link.los_clear and link.fresnel_clear)
        downlink, uplink = link.forward.usable_margin_db, link.reverse.usable_margin_db
        down_valid = structural_ok and link.forward.valid
        up_valid = structural_ok and link.reverse.valid
        two_way_valid = down_valid and up_valid
        source_results.append(
            {
                "source_id": source.id,
                "distance_m": distance,
                "downlink_margin_db": downlink,
                "uplink_margin_db": uplink,
                "two_way_margin_db": min(downlink, uplink),
                "valid_downlink": down_valid,
                "valid_uplink": up_valid,
                "valid_two_way": two_way_valid,
                "rejection": (
                    None
                    if two_way_valid
                    else "LOS/Fresnel validation"
                    if not structural_ok
                    else "radio margin"
                ),
            }
        )
    source_results.sort(
        key=lambda row: (
            not row["valid_two_way"],
            -float(row["two_way_margin_db"])
            if row["two_way_margin_db"] is not None
            else math.inf,
            row["source_id"],
        )
    )
    if source_results and all(row["rejection"] == "unknown_terrain" for row in source_results):
        state = "unknown_terrain"
    else:
        state = "evaluated"
    return {
        "state": state,
        "latitude": latitude,
        "longitude": longitude,
        "ground_elevation_m": ground,
        "surface_elevation_m": surface_elevation,
        "surface_available": terrain.has_surface,
        "client": {"height_agl_m": client.height_agl_m},
        "sources": source_results,
    }
