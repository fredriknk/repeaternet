from __future__ import annotations

import json
from collections.abc import Callable
from pathlib import Path

from rf_router_planner.optimization.optimizer import OptimizationResult


def export_route_geojson(
    result: OptimizationResult,
    path: str | Path,
    to_lon_lat: Callable[[float, float], tuple[float, float]] | None = None,
) -> None:
    def coordinate(site):  # type: ignore[no-untyped-def]
        if site.longitude is not None and site.latitude is not None:
            return [site.longitude, site.latitude]
        if to_lon_lat:
            lon, lat = to_lon_lat(site.x, site.y)
            return [lon, lat]
        return [site.x, site.y]

    features: list[dict[str, object]] = []
    sites = result.active_solution.sites if result.active_solution else result.route
    for site in sites:
        features.append(
            {
                "type": "Feature",
                "geometry": {"type": "Point", "coordinates": coordinate(site)},
                "properties": {
                    "id": site.id,
                    "kind": site.kind.value,
                    "dtm_elevation_m": site.ground_elevation_m,
                    "dom_elevation_m": site.surface_elevation_m,
                    "antenna_height_m": site.antenna_height_m,
                    "antenna_absolute_elevation_m": site.antenna_absolute_elevation_m,
                    "terrain_slope": site.terrain_slope,
                    "site_quality": site.site_quality,
                },
            }
        )
    by_id = {site.id: site for site in sites}
    for link in result.links:
        features.append(
            {
                "type": "Feature",
                "geometry": {
                    "type": "LineString",
                    "coordinates": [
                        coordinate(by_id[link.source_id]),
                        coordinate(by_id[link.target_id]),
                    ],
                },
                "properties": {
                    "from": link.source_id,
                    "to": link.target_id,
                    "distance_m": link.distance_m,
                    "worst_margin_db": link.worst_margin_db,
                    "forward_margin_db": link.forward.usable_margin_db,
                    "reverse_margin_db": link.reverse.usable_margin_db,
                    "fresnel_clearance_percent": link.minimum_fresnel_clearance_ratio * 100,
                    "diffraction_loss_db": link.diffraction_loss_db,
                    "los_clear": link.los_clear,
                    "valid": link.valid,
                },
            }
        )
    document = {"type": "FeatureCollection", "features": features}
    Path(path).write_text(json.dumps(document, indent=2), encoding="utf-8")
