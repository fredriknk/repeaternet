from __future__ import annotations

import numpy as np
import pytest
import rasterio
from affine import Affine
from rasterio.transform import from_origin, xy

from rf_router_planner.terrain.raster import RasterTerrain


def write_raster(path, values, transform=None, *, nodata=-9999):
    with rasterio.open(
        path, "w", driver="GTiff", width=values.shape[1], height=values.shape[0],
        count=1, dtype=values.dtype, crs="EPSG:25833", nodata=nodata,
        transform=transform if transform is not None else from_origin(500000, 6651000, 10, 10),
    ) as dataset:
        dataset.write(values, 1)
    return path


@pytest.mark.parametrize("rotated", [False, True])
def test_windowed_sampling_matches_rasterio_nearest_pixel_and_masks(tmp_path, rotated):
    rng = np.random.default_rng(442)
    values = rng.normal(200, 50, size=(530, 620)).astype(np.float32)
    values[20:110, 40:190] = -9999
    transform = from_origin(500000, 6651000, 10, 10)
    if rotated:
        transform = transform * Affine.rotation(21)
    path = write_raster(tmp_path / "terrain.tif", values, transform)
    rows, columns = rng.integers(0, 530, 5000), rng.integers(0, 620, 5000)
    xs, ys = xy(transform, rows, columns)
    xs, ys = np.asarray(xs), np.asarray(ys)
    # Fractional positions within the pixels verify floor rather than rounding.
    xs += transform.a * 0.2 + transform.b * 0.1
    ys += transform.d * 0.2 + transform.e * 0.1
    with rasterio.open(path) as reference:
        expected = np.ma.vstack(list(reference.sample(zip(xs, ys, strict=True), masked=True)))[:, 0]
    with RasterTerrain([path]) as terrain:
        actual = terrain.sample(xs, ys)
    np.testing.assert_array_equal(actual, expected.astype(float).filled(np.nan))


def test_mosaic_uses_first_unmasked_value_and_fills_gaps_from_next_tile(tmp_path):
    first = np.array([[1, -9999], [3, 4]], dtype=np.float32)
    second = np.array([[10, 20], [30, 40]], dtype=np.float32)
    first_path = write_raster(tmp_path / "first.tif", first)
    second_path = write_raster(tmp_path / "second.tif", second)
    xs = np.array([500005, 500015, 500005, 500015])
    ys = np.array([6650995, 6650995, 6650985, 6650985])
    with RasterTerrain([first_path, second_path]) as terrain:
        np.testing.assert_array_equal(terrain.sample(xs, ys), [1, 20, 3, 4])
        np.testing.assert_array_equal(terrain.sample(xs, ys, surface=True), [1, 20, 3, 4])
    with RasterTerrain([second_path, first_path]) as terrain:
        np.testing.assert_array_equal(terrain.sample(xs, ys), [10, 20, 30, 40])


def test_surface_raster_mask_stays_unknown_for_profile_fallback(tmp_path):
    ground = write_raster(tmp_path / "ground.tif", np.full((2, 2), 10, dtype=np.float32))
    surface = write_raster(tmp_path / "surface.tif", np.array([[20, -9999], [30, 40]], dtype=np.float32))
    with RasterTerrain([ground], [surface]) as terrain:
        xs, ys = np.array([500005, 500015]), np.array([6650995, 6650995])
        np.testing.assert_array_equal(terrain.sample(xs, ys), [10, 10])
        np.testing.assert_array_equal(terrain.sample(xs, ys, surface=True), [20, np.nan])


def test_edges_nonfinite_coordinates_and_empty_input_remain_unknown(tmp_path):
    path = write_raster(tmp_path / "terrain.tif", np.full((2, 2), 7, dtype=np.uint16), nodata=None)
    with RasterTerrain([path]) as terrain:
        xs = np.array([500000, 500020, 500005, 499999, np.nan, np.inf])
        ys = np.array([6651000, 6650995, 6650980, 6650995, 6650995, 6650995])
        np.testing.assert_array_equal(terrain.sample(xs, ys), [7, np.nan, np.nan, np.nan, np.nan, np.nan])
        assert terrain.sample(np.array([]), np.array([])).size == 0
        with pytest.raises(ValueError, match="equally sized"):
            terrain.sample(np.array([1, 2]), np.array([1]))


def test_long_sparse_samples_read_only_bounded_windows_in_original_order(tmp_path):
    values = np.arange(513 * 1025, dtype=np.float32).reshape(513, 1025)
    path = write_raster(tmp_path / "terrain.tif", values)
    windows = []

    class MeteredDataset:
        def __init__(self, dataset):
            self.dataset = dataset

        def __getattr__(self, name):
            return getattr(self.dataset, name)

        def read(self, *args, window, **kwargs):
            windows.append(window)
            assert window.width <= 256 and window.height <= 256
            return self.dataset.read(*args, window=window, **kwargs)

        def sample(self, *args, **kwargs):
            pytest.fail("Windowed sampler must not perform per-point reads")

    with RasterTerrain([path]) as terrain:
        terrain._dtm = [MeteredDataset(terrain._dtm[0])]
        rows, columns = np.array([512, 0, 260, 0, 512]), np.array([1024, 0, 800, 0, 1024])
        xs, ys = xy(terrain._dtm[0].transform, rows, columns)
        np.testing.assert_array_equal(terrain.sample(np.asarray(xs), np.asarray(ys)), values[rows, columns])
    assert len(windows) == 3
    assert any(window.width == 1 and window.height == 1 for window in windows)
