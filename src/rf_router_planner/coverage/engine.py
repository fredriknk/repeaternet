"""Bounded, deterministic radio-coverage sampling over terrain."""

from __future__ import annotations

import math
from collections.abc import Callable
from dataclasses import replace

import numpy as np
from pyproj import Transformer

from rf_router_planner.models.coverage import (
    ClientRadioProfile,
    CoverageCell,
    CoverageGrid,
    CoverageMode,
    CoverageSettings,
    CoverageSourceResult,
    CoverageState,
)
from rf_router_planner.models.settings import RFSettings, ValidationMode
from rf_router_planner.models.site import Site, SiteKind
from rf_router_planner.rf.propagation import LinkEvaluator
from rf_router_planner.terrain.raster import TerrainSource

Progress = Callable[[int, int], None]
ChunkCallback = Callable[[list[CoverageCell]], None]
Cancel = Callable[[], bool]


def make_grid(
    sources: list[Site],
    terrain: TerrainSource,
    settings: CoverageSettings,
    requested_bounds: tuple[float, float, float, float] | None = None,
) -> tuple[CoverageGrid, np.ndarray, np.ndarray, np.ndarray, np.ndarray]:
    """Build cell centres in terrain CRS and their WGS84 render coordinates."""
    if not sources:
        raise ValueError("Select at least one mesh router to calculate coverage")
    if settings.maximum_cells < 1 or settings.maximum_sources < 1:
        raise ValueError("Coverage limits must be positive")
    if requested_bounds is None:
        left = min(site.x for site in sources) - settings.area_buffer_m
        bottom = min(site.y for site in sources) - settings.area_buffer_m
        right = max(site.x for site in sources) + settings.area_buffer_m
        top = max(site.y for site in sources) + settings.area_buffer_m
    else:
        left, bottom, right, top = requested_bounds
    if not all(math.isfinite(v) for v in (left, bottom, right, top)) or left >= right or bottom >= top:
        raise ValueError("Coverage area bounds must be finite and have positive width and height")

    cell_size = settings.cell_size_m
    if not math.isfinite(cell_size) or cell_size <= 0:
        raise ValueError("Coverage cell size must be positive")
    maximum_cells = settings.maximum_cells
    while True:
        columns = max(1, math.ceil((right - left) / cell_size))
        rows = max(1, math.ceil((top - bottom) / cell_size))
        if rows * columns <= maximum_cells:
            break
        cell_size *= math.sqrt((rows * columns) / maximum_cells) * 1.001

    xs = left + (np.arange(columns, dtype=float) + 0.5) * cell_size
    ys = top - (np.arange(rows, dtype=float) + 0.5) * cell_size
    grid_x, grid_y = np.meshgrid(xs, ys)
    flat_x = grid_x.ravel()
    flat_y = grid_y.ravel()
    reverse = Transformer.from_crs(terrain.crs, 4326, always_xy=True)
    flat_lon, flat_lat = reverse.transform(flat_x, flat_y)
    grid = CoverageGrid(
        (left, bottom, right, top),
        settings.cell_size_m,
        cell_size,
        rows,
        columns,
        rows * columns,
        surface_available=terrain.has_surface,
    )
    return (
        grid,
        flat_x,
        flat_y,
        np.asarray(flat_lat, dtype=float),
        np.asarray(flat_lon, dtype=float),
    )


def calculate_coverage(
    terrain: TerrainSource,
    rf_settings: RFSettings,
    candidate_settings,
    sources: list[Site],
    coverage_settings: CoverageSettings,
    *,
    requested_bounds: tuple[float, float, float, float] | None = None,
    progress: Progress | None = None,
    cancelled: Cancel | None = None,
    on_chunk: ChunkCallback | None = None,
    chunk_size: int = 128,
) -> CoverageGrid:
    """Evaluate best valid source links at bounded cell centres.

    When ``on_chunk`` is supplied, cells are streamed to its callback and are
    not retained in the returned grid. Terrain samples with nodata remain
    explicitly unknown.
    """
    if len(sources) > coverage_settings.maximum_sources:
        raise ValueError(
            f"Select no more than {coverage_settings.maximum_sources} sources for coverage"
        )
    if chunk_size < 1:
        raise ValueError("Coverage chunk size must be positive")
    if coverage_settings.maximum_evaluations < 1:
        raise ValueError("Coverage evaluation limit must be positive")
    cell_limit = min(
        coverage_settings.maximum_cells,
        coverage_settings.maximum_evaluations // len(sources),
    )
    if cell_limit < 1:
        raise ValueError("Coverage settings allow no source evaluations")
    effective_settings = replace(coverage_settings, maximum_cells=cell_limit)
    report = progress or (lambda _done, _total: None)
    is_cancelled = cancelled or (lambda: False)
    grid, xs, ys, latitudes, longitudes = make_grid(
        sources, terrain, effective_settings, requested_bounds
    )
    dtm = terrain.sample(xs, ys, surface=False)
    grid.terrain_available_cells = int(np.isfinite(dtm).sum())
    evaluator = LinkEvaluator(terrain, rf_settings)
    client_profile: ClientRadioProfile = coverage_settings.client
    client_radio = client_profile.as_radio_budget()
    structural_validation = rf_settings.validation_mode != ValidationMode.STRICT_LOS
    pending: list[CoverageCell] = []

    for index, (x, y, latitude, longitude, ground) in enumerate(
        zip(xs, ys, latitudes, longitudes, dtm, strict=True)
    ):
        if is_cancelled():
            break
        cell = CoverageCell(index, float(x), float(y), float(latitude), float(longitude))
        if not math.isfinite(float(ground)):
            cell.state = CoverageState.UNKNOWN_TERRAIN
        else:
            target = Site(
                f"coverage-cell-{index}",
                float(x),
                float(y),
                float(latitude),
                float(longitude),
                SiteKind.CLIENT,
                ground_elevation_m=float(ground),
                antenna_height_m=client_profile.height_agl_m,
            )
            best_margin = -math.inf
            terrain_profile_failed = False
            for source in sources:
                if is_cancelled():
                    break
                try:
                    distance = source.distance_to(target)
                    profile_step = max(
                        coverage_settings.profile_step_m,
                        distance / max(1, coverage_settings.maximum_profile_samples - 1),
                    )
                    link = evaluator.evaluate(
                        source,
                        target,
                        profile_step,
                        include_profile=False,
                        target_radio=client_radio,
                    )
                except ValueError:
                    terrain_profile_failed = True
                    continue
                structural_ok = structural_validation or (link.los_clear and link.fresnel_clear)
                down_margin = link.forward.usable_margin_db
                up_margin = link.reverse.usable_margin_db
                down_valid = structural_ok and link.forward.valid
                up_valid = structural_ok and link.reverse.valid
                two_way_valid = down_valid and up_valid
                source_result = CoverageSourceResult(
                    source.id,
                    down_margin,
                    up_margin,
                    min(down_margin, up_margin),
                    down_valid,
                    up_valid,
                    two_way_valid,
                    None if structural_ok else "validation",
                )
                if coverage_settings.mode == CoverageMode.DOWNLINK:
                    margin, valid = down_margin, down_valid
                elif coverage_settings.mode == CoverageMode.UPLINK:
                    margin, valid = up_margin, up_valid
                else:
                    margin, valid = min(down_margin, up_margin), two_way_valid
                if valid:
                    cell.source_count += 1
                    if margin > best_margin:
                        best_margin = margin
                        cell.best_source_id = source.id
                cell.sources.append(source_result)

            if cell.source_count:
                cell.best_margin_db = best_margin
                cell.state = CoverageState.COVERED
                grid.evaluated_cells += 1
            elif terrain_profile_failed:
                cell.state = CoverageState.UNKNOWN_TERRAIN
                grid.unknown_cells += 1
            else:
                cell.state = CoverageState.UNCOVERED
                grid.evaluated_cells += 1
        grid.completed_cells += 1
        pending.append(cell)
        if len(pending) >= chunk_size:
            if on_chunk:
                on_chunk(pending)
            else:
                grid.cells.extend(pending)
            pending = []
        if grid.completed_cells % 20 == 0 or grid.completed_cells == grid.requested_cells:
            report(grid.completed_cells, grid.requested_cells)

    if pending:
        if on_chunk:
            on_chunk(pending)
        else:
            grid.cells.extend(pending)
    return grid
