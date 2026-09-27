from __future__ import annotations

import logging
from abc import ABC, abstractmethod
from collections.abc import Iterable
from dataclasses import dataclass
from pathlib import Path

import numpy as np

logger = logging.getLogger(__name__)


class TerrainSource(ABC):
    """Metric-coordinate terrain interface used by the RF engine."""

    crs: str
    resolution_m: float
    bounds: tuple[float, float, float, float]
    has_surface: bool

    @abstractmethod
    def sample(self, x: np.ndarray, y: np.ndarray, surface: bool = False) -> np.ndarray:
        raise NotImplementedError


@dataclass(slots=True)
class ArrayTerrain(TerrainSource):
    """In-memory regularly spaced terrain, primarily useful for tests and examples."""

    dtm: np.ndarray
    origin_x: float = 0.0
    origin_y: float = 0.0
    resolution_m: float = 10.0
    dom: np.ndarray | None = None
    crs: str = "EPSG:25833"

    def __post_init__(self) -> None:
        self.dtm = np.asarray(self.dtm, dtype=float)
        if self.dtm.ndim != 2:
            raise ValueError("DTM must be a 2-D array")
        if self.dom is not None:
            self.dom = np.asarray(self.dom, dtype=float)
            if self.dom.shape != self.dtm.shape:
                raise ValueError("DOM and DTM must have identical shapes")
        rows, cols = self.dtm.shape
        self.bounds = (
            self.origin_x,
            self.origin_y,
            self.origin_x + (cols - 1) * self.resolution_m,
            self.origin_y + (rows - 1) * self.resolution_m,
        )
        self.has_surface = self.dom is not None

    def sample(self, x: np.ndarray, y: np.ndarray, surface: bool = False) -> np.ndarray:
        data = self.dom if surface and self.dom is not None else self.dtm
        xs = np.asarray(x, dtype=float)
        ys = np.asarray(y, dtype=float)
        col = (xs - self.origin_x) / self.resolution_m
        row = (ys - self.origin_y) / self.resolution_m
        c0 = np.floor(col).astype(int)
        r0 = np.floor(row).astype(int)
        valid = (col >= 0) & (row >= 0) & (col <= data.shape[1] - 1) & (row <= data.shape[0] - 1)
        c0 = np.clip(c0, 0, data.shape[1] - 1)
        r0 = np.clip(r0, 0, data.shape[0] - 1)
        c1 = np.clip(c0 + 1, 0, data.shape[1] - 1)
        r1 = np.clip(r0 + 1, 0, data.shape[0] - 1)
        result = np.full(xs.shape, np.nan, dtype=float)
        if not np.any(valid):
            return result
        dx = col[valid] - c0[valid]
        dy = row[valid] - r0[valid]
        result[valid] = (
            data[r0[valid], c0[valid]] * (1 - dx) * (1 - dy)
            + data[r0[valid], c1[valid]] * dx * (1 - dy)
            + data[r1[valid], c0[valid]] * (1 - dx) * dy
            + data[r1[valid], c1[valid]] * dx * dy
        )
        return result


class RasterTerrain(TerrainSource):
    """Windowed, on-demand GeoTIFF sampler with optional DTM/DOM mosaics."""

    SAMPLE_WINDOW_SIDE = 256

    def __init__(
        self, dtm_paths: Iterable[str | Path], dom_paths: Iterable[str | Path] = ()
    ) -> None:
        try:
            import rasterio
            from rasterio.merge import merge
        except ImportError as exc:
            raise RuntimeError("Rasterio is required to load GeoTIFF terrain") from exc

        self._rasterio = rasterio
        self._dtm = [rasterio.open(str(path)) for path in dtm_paths]
        self._dom = [rasterio.open(str(path)) for path in dom_paths]
        if not self._dtm:
            raise ValueError("At least one DTM GeoTIFF is required")
        crs = self._dtm[0].crs
        if not crs or any(ds.crs != crs for ds in [*self._dtm, *self._dom]):
            self.close()
            raise ValueError("All terrain rasters must have the same defined CRS")
        self.crs = crs.to_string()
        self.resolution_m = float(min(abs(self._dtm[0].res[0]), abs(self._dtm[0].res[1])))
        all_bounds = [ds.bounds for ds in self._dtm]
        self.bounds = (
            min(b.left for b in all_bounds),
            min(b.bottom for b in all_bounds),
            max(b.right for b in all_bounds),
            max(b.top for b in all_bounds),
        )
        self.has_surface = bool(self._dom)
        self._merge = merge
        logger.info(
            "Loaded %d DTM and %d DOM tiles in %s", len(self._dtm), len(self._dom), self.crs
        )

    def sample(self, x: np.ndarray, y: np.ndarray, surface: bool = False) -> np.ndarray:
        from rasterio.transform import rowcol
        from rasterio.windows import Window

        datasets = self._dom if surface and self._dom else self._dtm
        xs, ys = np.asarray(x, dtype=float), np.asarray(y, dtype=float)
        if xs.ndim != 1 or ys.shape != xs.shape:
            raise ValueError("Terrain sampling requires equally sized one-dimensional coordinates")
        output = np.full(xs.size, np.nan, dtype=float)
        remaining = np.isfinite(xs) & np.isfinite(ys)
        side = self.SAMPLE_WINDOW_SIDE
        for dataset in datasets:
            bounds = dataset.bounds
            indices = np.flatnonzero(
                remaining & (xs >= bounds.left) & (xs <= bounds.right)
                & (ys >= bounds.bottom) & (ys <= bounds.top)
            )
            if indices.size == 0:
                continue
            rows, columns = rowcol(dataset.transform, xs[indices], ys[indices])
            rows, columns = np.asarray(rows), np.asarray(columns)
            in_grid = (rows >= 0) & (rows < dataset.height) & (columns >= 0) & (columns < dataset.width)
            indices, rows, columns = indices[in_grid], rows[in_grid], columns[in_grid]
            if indices.size == 0:
                continue
            # Read only touched, bounded windows. A long diagonal must not load
            # the whole rectangular extent of its profile into a Python array.
            window_columns = (dataset.width + side - 1) // side
            keys = (rows // side) * window_columns + columns // side
            order = np.argsort(keys, kind="stable")
            groups = np.split(order, np.flatnonzero(np.diff(keys[order])) + 1)
            for group in groups:
                first = group[0]
                row_start = int(rows[first] // side) * side
                column_start = int(columns[first] // side) * side
                window = Window(
                    column_start, row_start,
                    min(side, dataset.width - column_start),
                    min(side, dataset.height - row_start),
                )
                block = dataset.read(1, window=window, masked=True)
                sampled = block[rows[group] - row_start, columns[group] - column_start]
                valid = ~np.ma.getmaskarray(sampled)
                destinations = indices[group][valid]
                output[destinations] = np.asarray(sampled)[valid]
                remaining[destinations] = False
        return output

    @property
    def dtm_paths(self) -> list[str]:
        return [dataset.name for dataset in self._dtm]

    @property
    def dom_paths(self) -> list[str]:
        return [dataset.name for dataset in self._dom]

    def close(self) -> None:
        for dataset in getattr(self, "_dtm", []):
            dataset.close()
        for dataset in getattr(self, "_dom", []):
            dataset.close()

    def __enter__(self) -> RasterTerrain:
        return self

    def __exit__(self, *_: object) -> None:
        self.close()
