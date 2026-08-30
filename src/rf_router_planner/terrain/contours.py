from __future__ import annotations

import math
from collections import defaultdict
from typing import Any

import numpy as np

from .raster import RasterTerrain


def contour_geojson(
    terrain: RasterTerrain,
    interval_m: float = 20.0,
    *,
    max_grid_cells: int = 750_000,
    max_output_points: int = 150_000,
) -> dict[str, Any]:
    """Generate a display-sized WGS84 contour overlay from loaded DTM tiles.

    The DTM mosaic is downsampled only when needed to keep both contouring and
    Leaflet rendering bounded.  This is a visual overlay; RF calculations keep
    using the original raster resolution.
    """
    if interval_m <= 0:
        raise ValueError("Contour interval must be greater than zero")
    if max_grid_cells <= 0 or max_output_points <= 0:
        raise ValueError("Contour size limits must be greater than zero")

    width_m = terrain.bounds[2] - terrain.bounds[0]
    height_m = terrain.bounds[3] - terrain.bounds[1]
    native_cells = max(1.0, width_m * height_m / terrain.resolution_m**2)
    scale = max(1.0, math.sqrt(native_cells / max_grid_cells))
    display_resolution_m = terrain.resolution_m * scale

    from rasterio.enums import Resampling

    mosaic, transform = terrain._merge(
        terrain._dtm,
        res=display_resolution_m,
        resampling=Resampling.bilinear,
        masked=True,
    )
    elevations = np.ma.masked_invalid(mosaic[0])
    finite = elevations.compressed()
    if not len(finite):
        return {"type": "FeatureCollection", "features": []}

    first_level = math.ceil(float(np.min(finite)) / interval_m) * interval_m
    last_level = math.floor(float(np.max(finite)) / interval_m) * interval_m
    if first_level > last_level:
        return {"type": "FeatureCollection", "features": []}
    levels = np.arange(first_level, last_level + interval_m * 0.5, interval_m)

    rows, columns = elevations.shape
    xs = transform.c + (np.arange(columns) + 0.5) * transform.a
    ys = transform.f + (np.arange(rows) + 0.5) * transform.e

    from matplotlib.figure import Figure
    from shapely.geometry import LineString

    figure = Figure(figsize=(1, 1))
    axes = figure.subplots()
    contour_set = axes.contour(xs, ys, elevations, levels=levels)
    projected_by_level: dict[float, list[np.ndarray]] = defaultdict(list)
    total_points = 0
    simplify_tolerance = max(terrain.resolution_m, display_resolution_m * 0.6)
    for level, segments in zip(contour_set.levels, contour_set.allsegs, strict=True):
        for segment in segments:
            if len(segment) < 2:
                continue
            simplified = LineString(segment).simplify(simplify_tolerance)
            if simplified.is_empty or simplified.geom_type != "LineString":
                continue
            coordinates = np.asarray(simplified.coords, dtype=float)
            if len(coordinates) < 2:
                continue
            projected_by_level[float(level)].append(coordinates)
            total_points += len(coordinates)
    figure.clear()

    decimation = max(1, math.ceil(total_points / max_output_points))
    from pyproj import Transformer

    to_wgs84 = Transformer.from_crs(terrain.crs, "EPSG:4326", always_xy=True)
    features: list[dict[str, Any]] = []
    for level, segments in projected_by_level.items():
        lines: list[list[list[float]]] = []
        for segment in segments:
            sampled = segment[::decimation]
            if not np.array_equal(sampled[-1], segment[-1]):
                sampled = np.vstack((sampled, segment[-1]))
            if len(sampled) < 2:
                continue
            longitudes, latitudes = to_wgs84.transform(sampled[:, 0], sampled[:, 1])
            lines.append(
                [
                    [round(float(longitude), 7), round(float(latitude), 7)]
                    for longitude, latitude in zip(longitudes, latitudes, strict=True)
                ]
            )
        if lines:
            rounded_level = round(level, 3)
            features.append(
                {
                    "type": "Feature",
                    "properties": {
                        "elevation_m": rounded_level,
                        "index": math.isclose(level % 100.0, 0.0, abs_tol=1e-6),
                    },
                    "geometry": {"type": "MultiLineString", "coordinates": lines},
                }
            )
    return {"type": "FeatureCollection", "features": features}
