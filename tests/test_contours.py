from pathlib import Path

import numpy as np

from rf_router_planner.terrain.contours import contour_geojson
from rf_router_planner.terrain.raster import RasterTerrain


def _write_test_dtm(path: Path) -> None:
    import rasterio
    from rasterio.transform import from_origin

    data = np.add.outer(np.arange(10), np.arange(10)).astype(np.float32) * 5
    with rasterio.open(
        path,
        "w",
        driver="GTiff",
        height=data.shape[0],
        width=data.shape[1],
        count=1,
        dtype=data.dtype,
        crs="EPSG:25832",
        transform=from_origin(590_000, 6_620_100, 10, 10),
    ) as dataset:
        dataset.write(data, 1)


def test_contours_are_generated_as_wgs84_geojson(tmp_path: Path) -> None:
    path = tmp_path / "dtm.tif"
    _write_test_dtm(path)

    with RasterTerrain([path]) as terrain:
        geojson = contour_geojson(
            terrain,
            interval_m=20,
            max_grid_cells=1_000,
            max_output_points=1_000,
        )

    assert geojson["type"] == "FeatureCollection"
    features = geojson["features"]
    assert features
    assert {feature["properties"]["elevation_m"] for feature in features} >= {20.0, 40.0}
    first_coordinate = features[0]["geometry"]["coordinates"][0][0]
    longitude, latitude = first_coordinate
    assert 9 < longitude < 12
    assert 59 < latitude < 61
