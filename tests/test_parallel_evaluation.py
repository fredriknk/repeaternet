from pathlib import Path

import numpy as np

from rf_router_planner.models.settings import RFSettings
from rf_router_planner.models.site import Site, SiteKind
from rf_router_planner.optimization.parallel import evaluate_link_pairs
from rf_router_planner.terrain.raster import RasterTerrain


def _write_flat_dtm(path: Path) -> None:
    import rasterio
    from rasterio.transform import from_origin

    data = np.zeros((100, 100), dtype=np.float32)
    with rasterio.open(
        path,
        "w",
        driver="GTiff",
        height=100,
        width=100,
        count=1,
        dtype=data.dtype,
        crs="EPSG:25832",
        transform=from_origin(0, 1_000, 10, 10),
    ) as dataset:
        dataset.write(data, 1)


def test_process_and_sequential_rf_evaluation_are_equivalent(tmp_path: Path) -> None:
    path = tmp_path / "flat.tif"
    _write_flat_dtm(path)
    a = Site("A", 100, 500, kind=SiteKind.ENDPOINT_A, antenna_height_m=20)
    targets = [
        Site(f"C{index}", 300 + index % 50, 300 + index % 40, antenna_height_m=20)
        for index in range(205)
    ]
    pairs = [(a, target) for target in targets]
    settings = RFSettings(receiver_sensitivity_dbm=-150, fade_margin_db=0)
    with RasterTerrain([path]) as terrain:
        sequential = evaluate_link_pairs(terrain, settings, pairs, 20, workers=1)
        parallel = evaluate_link_pairs(terrain, settings, pairs, 20, workers=2)
    assert len(parallel) == len(sequential) == len(pairs)
    assert [link.source_id for link in parallel] == [link.source_id for link in sequential]
    assert [link.target_id for link in parallel] == [link.target_id for link in sequential]
    assert [link.worst_margin_db for link in parallel] == [
        link.worst_margin_db for link in sequential
    ]
