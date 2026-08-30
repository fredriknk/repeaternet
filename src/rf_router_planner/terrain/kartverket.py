from __future__ import annotations

import hashlib
import json
import logging
import math
from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path

from shapely.geometry import LineString, box

logger = logging.getLogger(__name__)


@dataclass(frozen=True, slots=True)
class WCSService:
    crs: str
    dtm_endpoint: str
    dom_endpoint: str
    dtm_coverage: str
    dom_coverage: str


Bounds = tuple[float, float, float, float]


@dataclass(frozen=True, slots=True)
class RouteCorridor:
    points: tuple[tuple[float, float], ...]
    width_m: float

    def __post_init__(self) -> None:
        if len(self.points) < 2:
            raise ValueError("A route corridor requires at least two points")
        if self.width_m <= 0:
            raise ValueError("Corridor width must be positive")


Corridor = tuple[float, float, float, float, float] | RouteCorridor


@dataclass(frozen=True, slots=True)
class DownloadTile:
    bounds: Bounds
    width_pixels: int
    height_pixels: int

    @property
    def pixel_count(self) -> int:
        return self.width_pixels * self.height_pixels

    @property
    def area_km2(self) -> float:
        min_x, min_y, max_x, max_y = self.bounds
        return (max_x - min_x) * (max_y - min_y) / 1_000_000.0


@dataclass(frozen=True, slots=True)
class DownloadPlan:
    requested_bounds: Bounds
    corridor: Corridor | None
    requested_resolution_m: float
    effective_resolution_m: float
    tiles: tuple[DownloadTile, ...]
    auto_resolution: bool
    pixel_budget: int

    @property
    def pixel_count(self) -> int:
        return sum(tile.pixel_count for tile in self.tiles)

    @property
    def download_area_km2(self) -> float:
        return sum(tile.area_km2 for tile in self.tiles)

    @property
    def estimated_raw_mib_per_product(self) -> float:
        return self.pixel_count * 4 / (1024 * 1024)

    def summary(self, product_count: int = 1) -> str:
        resolution_note = f"{self.effective_resolution_m:g} m"
        if self.effective_resolution_m != self.requested_resolution_m:
            resolution_note += f" (requested {self.requested_resolution_m:g} m)"
        return (
            f"{len(self.tiles)} tile(s), {self.download_area_km2:,.0f} km² downloaded, "
            f"{resolution_note}, {self.pixel_count / 1_000_000:.1f} million pixels per product, "
            f"approximately {self.estimated_raw_mib_per_product * product_count:,.0f} MiB raw"
        )


def load_services(path: str | Path) -> dict[str, WCSService]:
    data = json.loads(Path(path).read_text(encoding="utf-8"))
    return {key: WCSService(key, **value) for key, value in data["services"].items()}


class KartverketProvider:
    """Configurable WCS download provider with a content-addressed disk cache.

    Service URLs and coverage identifiers intentionally live in JSON configuration,
    because Kartverket can rename or replace published services. Large jobs are
    split into aligned corridor tiles; the area setting limits each request, not
    the total planning area.
    """

    def __init__(
        self,
        services: dict[str, WCSService],
        cache_directory: str | Path,
        maximum_area_km2: float = 400.0,
        maximum_pixels_per_tile: int = 4_000_000,
        maximum_tiles: int = 256,
    ) -> None:
        self.services = services
        self.cache_directory = Path(cache_directory)
        self.maximum_tile_area_km2 = maximum_area_km2
        self.maximum_pixels_per_tile = maximum_pixels_per_tile
        self.maximum_tiles = maximum_tiles

    @property
    def maximum_area_km2(self) -> float:
        """Backward-compatible name for the per-request tile area."""
        return self.maximum_tile_area_km2

    def plan_download(
        self,
        bounds: Bounds,
        requested_resolution_m: float,
        *,
        corridor: Corridor | None = None,
        auto_resolution: bool = True,
        maximum_total_pixels: int = 50_000_000,
    ) -> DownloadPlan:
        """Create a deterministic, cache-friendly WCS tile plan.

        Tiles are aligned to a global grid and filtered against the true buffered
        route corridor. In automatic mode the resolution is increased only as
        much as needed to remain under the total pixel budget.
        """
        min_x, min_y, max_x, max_y = bounds
        if max_x <= min_x or max_y <= min_y:
            raise ValueError("Download bounds must have positive width and height")
        if requested_resolution_m <= 0:
            raise ValueError("Requested resolution must be positive")
        if maximum_total_pixels <= 0:
            raise ValueError("Maximum total pixels must be positive")
        if self.maximum_tile_area_km2 <= 0 or self.maximum_pixels_per_tile <= 0:
            raise ValueError("Tile area and pixel limits must be positive")

        resolution = requested_resolution_m
        if auto_resolution:
            target_area = self._target_area_m2(bounds, corridor)
            minimum_resolution = math.sqrt(target_area / maximum_total_pixels)
            resolution = self._nice_resolution(max(resolution, minimum_resolution))

        for _attempt in range(12):
            tiles = self._build_tiles(bounds, resolution, corridor)
            pixels = sum(tile.pixel_count for tile in tiles)
            if not auto_resolution or pixels <= maximum_total_pixels:
                break
            resolution = self._nice_resolution(
                resolution * math.sqrt(pixels / maximum_total_pixels)
            )
        else:  # pragma: no cover - convergence guard
            raise RuntimeError("Could not create a download plan within the pixel budget")

        if not tiles:
            raise ValueError("No WCS tiles intersect the requested route corridor")
        if len(tiles) > self.maximum_tiles:
            message = (
                f"The download requires {len(tiles)} tiles; the configured limit is "
                f"{self.maximum_tiles}. "
            )
            if auto_resolution:
                message += "Increase the tile limit or reduce the corridor width."
            else:
                message += "Enable automatic resolution or request a coarser raster."
            raise ValueError(message)
        return DownloadPlan(
            bounds,
            corridor,
            requested_resolution_m,
            resolution,
            tuple(tiles),
            auto_resolution,
            maximum_total_pixels,
        )

    def _build_tiles(
        self, bounds: Bounds, resolution_m: float, corridor: Corridor | None
    ) -> list[DownloadTile]:
        min_x, min_y, max_x, max_y = bounds
        side_from_area = math.sqrt(self.maximum_tile_area_km2 * 1_000_000.0)
        side_from_pixels = math.sqrt(self.maximum_pixels_per_tile) * resolution_m
        tile_side = max(resolution_m, min(side_from_area, side_from_pixels))
        start_x = math.floor(min_x / tile_side) * tile_side
        start_y = math.floor(min_y / tile_side) * tile_side
        corridor_shape = self._corridor_shape(corridor) if corridor else None
        tiles: list[DownloadTile] = []
        y = start_y
        while y < max_y:
            x = start_x
            while x < max_x:
                tile_bounds = (x, y, x + tile_side, y + tile_side)
                if corridor_shape is None or corridor_shape.intersects(box(*tile_bounds)):
                    width_pixels = max(1, math.ceil(tile_side / resolution_m))
                    height_pixels = max(1, math.ceil(tile_side / resolution_m))
                    tiles.append(DownloadTile(tile_bounds, width_pixels, height_pixels))
                x += tile_side
            y += tile_side
        return tiles

    @staticmethod
    def _target_area_m2(bounds: Bounds, corridor: Corridor | None) -> float:
        min_x, min_y, max_x, max_y = bounds
        bounding_area = (max_x - min_x) * (max_y - min_y)
        if not corridor:
            return bounding_area
        corridor_area = KartverketProvider._corridor_shape(corridor).area
        return min(bounding_area, corridor_area)

    @staticmethod
    def _corridor_shape(corridor: Corridor):  # type: ignore[no-untyped-def]
        if isinstance(corridor, RouteCorridor):
            return LineString(corridor.points).buffer(corridor.width_m)
        ax, ay, bx, by, width = corridor
        if width <= 0:
            raise ValueError("Corridor width must be positive")
        return LineString([(ax, ay), (bx, by)]).buffer(width)

    @staticmethod
    def _nice_resolution(minimum_m: float) -> float:
        choices = (1, 2, 5, 10, 15, 20, 25, 30, 50, 75, 100, 150, 200, 250, 500, 1000)
        for choice in choices:
            if choice >= minimum_m - 1e-9:
                return float(choice)
        magnitude = 10 ** math.floor(math.log10(minimum_m))
        return math.ceil(minimum_m / magnitude) * magnitude

    def fetch_plan(
        self,
        product: str,
        crs: str,
        plan: DownloadPlan,
        progress: Callable[[int, int, bool], None] | None = None,
    ) -> list[Path]:
        paths: list[Path] = []
        total = len(plan.tiles)
        for index, tile in enumerate(plan.tiles, 1):
            destination = self.cache_path(product, crs, tile.bounds, plan.effective_resolution_m)
            cached = destination.exists()
            if progress:
                progress(index, total, cached)
            paths.append(self.fetch(product, crs, tile.bounds, plan.effective_resolution_m))
        return paths

    def cache_path(self, product: str, crs: str, bounds: Bounds, resolution_m: float) -> Path:
        endpoint, params = self._request(product, crs, bounds, resolution_m)
        digest = hashlib.sha256(repr((endpoint, sorted(params.items()))).encode()).hexdigest()[:20]
        return self.cache_directory / f"kartverket_{product.lower()}_{digest}.tif"

    def cached_tile_count(self, product: str, crs: str, plan: DownloadPlan) -> int:
        return sum(
            self.cache_path(product, crs, tile.bounds, plan.effective_resolution_m).exists()
            for tile in plan.tiles
        )

    def fetch(
        self,
        product: str,
        crs: str,
        bounds: Bounds,
        resolution_m: float,
        progress: Callable[[int, int], None] | None = None,
    ) -> Path:
        if product.lower() not in {"dtm", "dom"}:
            raise ValueError("Product must be DTM or DOM")
        if crs not in self.services:
            raise ValueError(f"No configured Kartverket WCS service for {crs}")
        min_x, min_y, max_x, max_y = bounds
        area_km2 = (max_x - min_x) * (max_y - min_y) / 1_000_000.0
        if area_km2 <= 0 or area_km2 > self.maximum_tile_area_km2 + 1e-6:
            raise ValueError(
                f"Tile area is {area_km2:.1f} km²; configured per-tile maximum is "
                f"{self.maximum_tile_area_km2:.1f} km²"
            )
        endpoint, params = self._request(product, crs, bounds, resolution_m)
        self.cache_directory.mkdir(parents=True, exist_ok=True)
        destination = self.cache_path(product, crs, bounds, resolution_m)
        if destination.exists():
            logger.info("Kartverket cache hit: %s", destination)
            return destination
        try:
            import requests
        except ImportError as exc:
            raise RuntimeError("Requests is required for online Kartverket downloads") from exc
        logger.info("Requesting Kartverket %s coverage from %s", product.upper(), endpoint)
        with requests.get(endpoint, params=params, stream=True, timeout=(15, 300)) as response:
            response.raise_for_status()
            total = int(response.headers.get("content-length", 0))
            downloaded = 0
            temporary = destination.with_suffix(".part")
            with temporary.open("wb") as handle:
                for chunk in response.iter_content(1024 * 1024):
                    handle.write(chunk)
                    downloaded += len(chunk)
                    if progress:
                        progress(downloaded, total)
            temporary.replace(destination)
        return destination

    def _request(
        self, product: str, crs: str, bounds: Bounds, resolution_m: float
    ) -> tuple[str, dict[str, str | int]]:
        if product.lower() not in {"dtm", "dom"}:
            raise ValueError("Product must be DTM or DOM")
        if crs not in self.services:
            raise ValueError(f"No configured Kartverket WCS service for {crs}")
        if resolution_m <= 0:
            raise ValueError("Resolution must be positive")
        min_x, min_y, max_x, max_y = bounds
        service = self.services[crs]
        is_dtm = product.lower() == "dtm"
        coverage = service.dtm_coverage if is_dtm else service.dom_coverage
        endpoint = service.dtm_endpoint if is_dtm else service.dom_endpoint
        params: dict[str, str | int] = {
            "service": "WCS",
            "version": "1.0.0",
            "request": "GetCoverage",
            "coverage": coverage,
            "format": "GeoTIFF",
            "crs": crs,
            "bbox": f"{min_x},{min_y},{max_x},{max_y}",
            "width": max(1, math.ceil((max_x - min_x) / resolution_m)),
            "height": max(1, math.ceil((max_y - min_y) / resolution_m)),
        }
        return endpoint, params
