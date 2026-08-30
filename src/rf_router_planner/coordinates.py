from __future__ import annotations

import math


def norway_utm_epsg(longitude: float) -> int:
    """Choose one of the UTM zones commonly used for mainland Norway/Svalbard."""
    zone = int(math.floor((longitude + 180.0) / 6.0) + 1)
    if zone <= 32:
        return 25832
    if zone <= 34:
        return 25833
    return 25835


def transformer_pair(latitude: float, longitude: float):  # type: ignore[no-untyped-def]
    try:
        from pyproj import Transformer
    except ImportError as exc:
        raise RuntimeError("PyProj is required for latitude/longitude transformation") from exc
    epsg = norway_utm_epsg(longitude)
    forward = Transformer.from_crs("EPSG:4326", f"EPSG:{epsg}", always_xy=True)
    reverse = Transformer.from_crs(f"EPSG:{epsg}", "EPSG:4326", always_xy=True)
    return epsg, forward, reverse
