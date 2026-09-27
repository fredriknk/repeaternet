"""Fresh-process synthetic or local-GeoTIFF mesh-coverage benchmarks."""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import platform
import statistics
import subprocess
import sys
import time
from collections import Counter
from dataclasses import asdict
from pathlib import Path
from typing import Any

import numpy as np

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "src"))


def peak_working_set_mib() -> float | None:
    if sys.platform == "win32":
        import ctypes
        from ctypes import wintypes

        class MemoryCounters(ctypes.Structure):
            _fields_ = [
                ("cb", wintypes.DWORD),
                ("PageFaultCount", wintypes.DWORD),
                ("PeakWorkingSetSize", ctypes.c_size_t),
                ("WorkingSetSize", ctypes.c_size_t),
                ("QuotaPeakPagedPoolUsage", ctypes.c_size_t),
                ("QuotaPagedPoolUsage", ctypes.c_size_t),
                ("QuotaPeakNonPagedPoolUsage", ctypes.c_size_t),
                ("QuotaNonPagedPoolUsage", ctypes.c_size_t),
                ("PagefileUsage", ctypes.c_size_t),
                ("PeakPagefileUsage", ctypes.c_size_t),
            ]

        counters = MemoryCounters()
        counters.cb = ctypes.sizeof(counters)
        psapi = ctypes.WinDLL("Psapi.dll")
        process = ctypes.windll.kernel32
        process.GetCurrentProcess.restype = wintypes.HANDLE
        get_memory = psapi.GetProcessMemoryInfo
        get_memory.argtypes = (wintypes.HANDLE, ctypes.POINTER(MemoryCounters), wintypes.DWORD)
        get_memory.restype = wintypes.BOOL
        if not get_memory(process.GetCurrentProcess(), ctypes.byref(counters), counters.cb):
            return None
        return counters.PeakWorkingSetSize / (1024 * 1024)
    try:
        import resource

        peak = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
        return peak / (1024 * 1024 if sys.platform == "darwin" else 1024)
    except (ImportError, AttributeError):
        return None


class MeteredTerrain:
    def __init__(self, terrain: Any) -> None:
        self.terrain = terrain
        self.crs = terrain.crs
        self.resolution_m = terrain.resolution_m
        self.bounds = terrain.bounds
        self.has_surface = terrain.has_surface
        self.sample_calls = 0
        self.sample_points = 0
        self.sample_bytes = 0
        self.sample_nodata_points = 0

    def sample(self, x: np.ndarray, y: np.ndarray, surface: bool = False) -> np.ndarray:
        result = self.terrain.sample(x, y, surface=surface)
        self.sample_calls += 1
        self.sample_points += int(np.asarray(x).size)
        self.sample_bytes += int(result.nbytes)
        self.sample_nodata_points += int((~np.isfinite(result)).sum())
        return result


def _parse_ints(value: str) -> list[int]:
    values = [int(part) for part in value.split(",")]
    if not values or any(item < 1 for item in values):
        raise argparse.ArgumentTypeError("values must be positive comma-separated integers")
    return values


def _parse_scenarios(value: str) -> list[str]:
    choices = {"flat", "ridge", "valley", "nodata"}
    scenarios = value.split(",")
    if not scenarios or any(item not in choices for item in scenarios):
        raise argparse.ArgumentTypeError(f"scenarios must be selected from {sorted(choices)}")
    return scenarios


def run_one(
    source_count: int,
    requested_cells: int,
    scenario: str,
    cache_entries: int = 50_000,
    dtm_path: str | None = None,
    profile_step_m: float | None = None,
) -> dict[str, Any]:
    from pyproj import CRS

    from rf_router_planner.terrain.raster import ArrayTerrain, RasterTerrain

    if dtm_path is not None:
        path = Path(dtm_path).resolve()
        with path.open("rb") as handle:
            checksum = hashlib.file_digest(handle, "sha256").hexdigest()
        with RasterTerrain([path]) as raster:
            crs = CRS.from_user_input(raster.crs)
            if not crs.is_projected or any(
                axis.unit_conversion_factor != 1.0 for axis in crs.axis_info[:2]
            ):
                raise ValueError("Benchmark raster must have a projected metre CRS")
            left, bottom, right, top = raster.bounds
            width = min(right - left, top - bottom) - 2 * raster.resolution_m
            if width <= 0:
                raise ValueError("Benchmark raster must span more than two pixels per axis")
            centre_x, centre_y = (left + right) / 2, (bottom + top) / 2
            bounds = (
                centre_x - width / 2, centre_y - width / 2,
                centre_x + width / 2, centre_y + width / 2,
            )
            return measure_coverage(
                raster, bounds, source_count, requested_cells, "local_raster", cache_entries,
                profile_step_m or raster.resolution_m,
                {"kind": "local_geotiff", "path": str(path), "sha256": checksum,
                 "file_bytes": path.stat().st_size, "crs": raster.crs,
                 "raster_bounds_m": raster.bounds, "resolution_m": raster.resolution_m,
                 "surface_available": False},
            )

    side_cells = math.ceil(math.sqrt(requested_cells))
    cell_size = 1_000.0
    padding = 1_000.0
    width = side_cells * cell_size
    left = 400_000.0
    bottom = 6_500_000.0
    bounds = (left, bottom, left + width, bottom + width)
    terrain_resolution = 200.0
    terrain_span = width + 2 * padding
    columns = math.ceil(terrain_span / terrain_resolution) + 1
    rows = math.ceil(terrain_span / terrain_resolution) + 1
    values = np.zeros((rows, columns), dtype=np.float32)
    if scenario in {"ridge", "valley", "nodata"}:
        terrain_column = min(columns - 2, round((width / 2 + padding) / terrain_resolution))
        terrain_slice = slice(max(1, terrain_column - 1), terrain_column + 2)
        if scenario == "ridge":
            values[:, terrain_slice] = 180.0
        elif scenario == "valley":
            values[:, terrain_slice] = -80.0
        else:
            values[:, terrain_slice] = np.nan
    terrain = ArrayTerrain(
        values,
        origin_x=left - padding,
        origin_y=bottom - padding,
        resolution_m=terrain_resolution,
    )
    return measure_coverage(
        terrain, bounds, source_count, requested_cells, scenario, cache_entries,
        profile_step_m or 200.0,
        {"kind": "synthetic_array", "sha256": hashlib.sha256(values.tobytes()).hexdigest(),
         "crs": terrain.crs, "resolution_m": terrain_resolution},
    )


def measure_coverage(
    raw_terrain: Any,
    bounds: tuple[float, float, float, float],
    source_count: int,
    requested_cells: int,
    scenario: str,
    cache_entries: int,
    profile_step_m: float,
    fixture: dict[str, Any],
) -> dict[str, Any]:
    from rf_router_planner.coverage.engine import calculate_coverage
    from rf_router_planner.models.coverage import CoverageSettings
    from rf_router_planner.models.settings import CandidateSettings, RFSettings
    from rf_router_planner.models.site import Site, SiteKind
    from rf_router_planner.optimization.cache import LinkMetricsCache

    terrain = MeteredTerrain(raw_terrain)
    left, bottom, right, _top = bounds
    width = right - left
    side_cells = math.ceil(math.sqrt(requested_cells))
    cell_size = width / side_cells
    requested_cells_actual = side_cells**2
    sources = [
        Site(
            f"R{index:02d}",
            left + width * (index + 1) / (source_count + 1),
            bottom + width / 2,
            kind=SiteKind.ROUTER,
            antenna_height_m=50.0,
        )
        for index in range(source_count)
    ]
    settings = CoverageSettings(
        cell_size_m=cell_size,
        area_buffer_m=0.0,
        maximum_cells=requested_cells_actual,
        maximum_evaluations=250_000,
        profile_step_m=profile_step_m,
        maximum_profile_samples=4_096,
    )
    rf = RFSettings(fade_margin_db=0.0)
    cold_result_hash = hashlib.sha256()
    warm_result_hash = hashlib.sha256()
    state_counts: Counter[str] = Counter()
    first_chunk_seconds: float | None = None
    response_bytes = 0
    unknown_source_evaluations = 0
    rf_calls = 0
    cancel_rf_calls = 0
    warm_rf_calls = 0
    measurement_phase = "cold"
    cancel_requested_at: float | None = None
    cache = LinkMetricsCache(max_entries=cache_entries)

    from rf_router_planner.rf.propagation import LinkEvaluator

    original_evaluate = LinkEvaluator.evaluate

    def counted_evaluate(self, *args, **kwargs):
        nonlocal cancel_rf_calls, rf_calls, warm_rf_calls, cancel_requested_at
        if measurement_phase == "cancel":
            cancel_rf_calls += 1
        elif measurement_phase == "warm":
            warm_rf_calls += 1
        else:
            rf_calls += 1
        try:
            return original_evaluate(self, *args, **kwargs)
        finally:
            if measurement_phase == "cancel" and cancel_rf_calls >= cancel_threshold:
                cancel_requested_at = time.perf_counter()

    from unittest.mock import patch

    started = time.perf_counter()

    def stream_chunk(cells: list[Any]) -> None:
        nonlocal first_chunk_seconds, response_bytes, unknown_source_evaluations
        if first_chunk_seconds is None:
            first_chunk_seconds = time.perf_counter() - started
        unknown_source_evaluations += sum(cell.unknown_sources for cell in cells)
        encoded = json.dumps(
            [asdict(cell) for cell in cells], separators=(",", ":"), allow_nan=False,
        ).encode()
        response_bytes += len(encoded)
        cold_result_hash.update(encoded)
        state_counts.update(cell.state.value for cell in cells)

    with patch.object(LinkEvaluator, "evaluate", counted_evaluate):
        grid = calculate_coverage(
            terrain,
            rf,
            CandidateSettings(),
            sources,
            settings,
            requested_bounds=bounds,
            on_chunk=stream_chunk,
            chunk_size=128,
            evaluation_cache=cache,
            cache_namespace="benchmark",
        )
    elapsed = time.perf_counter() - started
    cold_cache_stats = cache.stats("benchmark")
    cold_peak_working_set = peak_working_set_mib()
    primary_terrain_metrics = (
        terrain.sample_calls,
        terrain.sample_points,
        terrain.sample_bytes,
        terrain.sample_nodata_points,
    )

    measurement_phase = "warm"
    warm_first_chunk: float | None = None
    warm_started = time.perf_counter()

    def warm_chunk(cells: list[Any]) -> None:
        nonlocal warm_first_chunk
        if warm_first_chunk is None:
            warm_first_chunk = time.perf_counter() - warm_started
        warm_result_hash.update(json.dumps(
            [asdict(cell) for cell in cells], separators=(",", ":"), allow_nan=False,
        ).encode())

    with patch.object(LinkEvaluator, "evaluate", counted_evaluate):
        warm_grid = calculate_coverage(
            terrain,
            rf,
            CandidateSettings(),
            sources,
            settings,
            requested_bounds=bounds,
            on_chunk=warm_chunk,
            evaluation_cache=cache,
            cache_namespace="benchmark",
        )
    warm_elapsed = time.perf_counter() - warm_started
    warm_cache_stats = cache.stats("benchmark")
    if warm_result_hash.digest() != cold_result_hash.digest():
        raise AssertionError("Cold and warm coverage cells differ")
    before_cancel_metrics = (
        terrain.sample_calls, terrain.sample_points, terrain.sample_bytes,
        terrain.sample_nodata_points,
    )

    # A repeatable cooperative-cancel probe records how much work is completed
    # before the engine observes cancellation at its bounded pair checks.
    measurement_phase = "cancel"
    cancel_threshold = max(1, source_count * 2)
    cancel_started = time.perf_counter()
    with patch.object(LinkEvaluator, "evaluate", counted_evaluate):
        cancelled_grid = calculate_coverage(
            terrain,
            rf,
            CandidateSettings(),
            sources,
            settings,
            requested_bounds=bounds,
            cancelled=lambda: cancel_requested_at is not None,
            evaluation_cache=cache,
            cache_namespace="benchmark-cancel",
        )
    cancel_finished = time.perf_counter()
    if cancel_requested_at is None:
        raise AssertionError("Fixture did not reach the cancellation probe's RF threshold")
    cancellation_seconds = cancel_finished - cancel_requested_at
    measurement_phase = "cold"
    cancel_terrain_metrics = (
        terrain.sample_calls - before_cancel_metrics[0],
        terrain.sample_points - before_cancel_metrics[1],
        terrain.sample_bytes - before_cancel_metrics[2],
        terrain.sample_nodata_points - before_cancel_metrics[3],
    )
    return {
        "source_count": source_count,
        "requested_cells": requested_cells,
        "grid_cells": grid.requested_cells,
        "effective_cell_size_m": grid.effective_cell_size_m,
        "scenario": scenario,
        "requested_bounds_m": bounds,
        "requested_area_km2": round(width * width / 1_000_000, 3),
        "completed_cells": grid.completed_cells,
        "evaluated_cells": grid.evaluated_cells,
        "unknown_cells": grid.unknown_cells,
        "unknown_source_evaluations": unknown_source_evaluations,
        "rf_evaluations": rf_calls,
        "cold_cache_hits": cold_cache_stats["hits"],
        "cold_cache_misses": cold_cache_stats["misses"],
        "warm_run": {
            "elapsed_seconds": warm_elapsed,
            "first_chunk_seconds": warm_first_chunk,
            "pair_requests": warm_rf_calls,
            "cache_hits": warm_cache_stats["hits"] - cold_cache_stats["hits"],
            "cache_misses": warm_cache_stats["misses"] - cold_cache_stats["misses"],
            "completed_cells": warm_grid.completed_cells,
        },
        "cache_capacity": cache.max_entries,
        "terrain_fixture_sha256": fixture["sha256"],
        "fixture": fixture,
        "profile_step_m": profile_step_m,
        "maximum_profile_samples": settings.maximum_profile_samples,
        "sources_projected": [
            {"id": source.id, "x": source.x, "y": source.y,
             "height_agl_m": source.antenna_height_m} for source in sources
        ],
        "state_counts": dict(state_counts),
        "cold_warm_results_identical": True,
        "result_sha256": cold_result_hash.hexdigest(),
        "radio_profile": {
            "frequency_mhz": rf.frequency_mhz,
            "validation_mode": rf.validation_mode.value,
            "fade_margin_db": rf.fade_margin_db,
            "router_height_agl_m": 50.0,
            "client_height_agl_m": settings.client.height_agl_m,
            "client_tx_power_dbm": settings.client.tx_power_dbm,
            "client_sensitivity_dbm": settings.client.sensitivity_dbm,
        },
        "terrain_reads": primary_terrain_metrics[0],
        "terrain_sample_points": primary_terrain_metrics[1],
        "terrain_bytes_returned": primary_terrain_metrics[2],
        "terrain_nodata_points": primary_terrain_metrics[3],
        "first_chunk_seconds": first_chunk_seconds,
        "elapsed_seconds": elapsed,
        "serialized_result_bytes_estimate": response_bytes,
        "cold_peak_working_set_mib": cold_peak_working_set,
        "peak_working_set_mib": peak_working_set_mib(),
        "cancellation_probe": {
            "requested_after_rf_calls": cancel_threshold,
            "rf_evaluations_before_observed": cancel_rf_calls,
            "completed_cells_before_observed": cancelled_grid.completed_cells,
            "acknowledgement_seconds": cancellation_seconds,
            "probe_total_seconds": cancel_finished - cancel_started,
            "terrain_reads": cancel_terrain_metrics[0],
            "terrain_sample_points": cancel_terrain_metrics[1],
            "terrain_bytes_returned": cancel_terrain_metrics[2],
            "terrain_nodata_points": cancel_terrain_metrics[3],
        },
    }


def summarize(values: list[float]) -> dict[str, float]:
    return {
        "median": round(statistics.median(values), 4),
        "min": round(min(values), 4),
        "max": round(max(values), 4),
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--sources", type=_parse_ints, default=[2])
    parser.add_argument("--cells", type=_parse_ints, default=[256])
    parser.add_argument("--scenarios", type=_parse_scenarios, default=["flat"])
    parser.add_argument("--repeats", type=int, default=3)
    parser.add_argument("--cache-entries", type=int, default=50_000)
    parser.add_argument("--dtm", type=Path, help="Local metric-CRS GeoTIFF; no downloads")
    parser.add_argument("--profile-step-m", type=float, help="Default: 200 m synthetic, raster pixel size otherwise")
    parser.add_argument("--output", type=Path, help="Also save the JSON measurement artifact")
    parser.add_argument("--summary-only", action="store_true")
    parser.add_argument("--_case", help=argparse.SUPPRESS)
    args = parser.parse_args()
    if args.repeats < 1:
        parser.error("--repeats must be positive")
    if args.cache_entries < 1:
        parser.error("--cache-entries must be positive")
    if args.profile_step_m is not None and (
        not math.isfinite(args.profile_step_m) or args.profile_step_m <= 0
    ):
        parser.error("--profile-step-m must be finite and positive")
    if args.dtm is not None and args.scenarios != ["flat"]:
        parser.error("--dtm replaces the synthetic fixture; omit --scenarios")
    if args._case:
        case = json.loads(args._case)
        print(
            json.dumps(
                run_one(**case), sort_keys=True
            )
        )
        return

    reports: list[dict[str, Any]] = []
    for source_count in args.sources:
        for requested_cells in args.cells:
            for scenario in args.scenarios:
                runs = []
                for _ in range(args.repeats):
                    child = subprocess.run(
                        [
                            sys.executable,
                            str(Path(__file__).resolve()),
                            "--_case",
                            json.dumps(
                                {"source_count": source_count, "requested_cells": requested_cells,
                                 "scenario": scenario, "cache_entries": args.cache_entries,
                                 "dtm_path": str(args.dtm.resolve()) if args.dtm else None,
                                 "profile_step_m": args.profile_step_m}
                            ),
                        ],
                        check=True,
                        capture_output=True,
                        text=True,
                    )
                    runs.append(json.loads(child.stdout.strip()))
                report = {
                    "scenario": runs[0]["scenario"],
                    "fixture": runs[0]["fixture"],
                    "radio_profile": runs[0]["radio_profile"],
                    "profile_step_m": runs[0]["profile_step_m"],
                    "requested_bounds_m": runs[0]["requested_bounds_m"],
                    "grid_cells": runs[0]["grid_cells"],
                    "state_counts": runs[0]["state_counts"],
                    "result_sha256": runs[0]["result_sha256"],
                    "cold_warm_results_identical": all(run["cold_warm_results_identical"] for run in runs),
                    "source_count": source_count,
                    "requested_cells": requested_cells,
                    "repetition_count": len(runs),
                    "elapsed_seconds": summarize([run["elapsed_seconds"] for run in runs]),
                    "first_chunk_seconds": summarize(
                        [run["first_chunk_seconds"] for run in runs if run["first_chunk_seconds"]]
                    ),
                    "peak_working_set_mib": summarize(
                        [run["peak_working_set_mib"] for run in runs if run["peak_working_set_mib"]]
                    ),
                    "cold_peak_working_set_mib": summarize(
                        [
                            run["cold_peak_working_set_mib"]
                            for run in runs
                            if run["cold_peak_working_set_mib"]
                        ]
                    ),
                    "unknown_cells": summarize([run["unknown_cells"] for run in runs]),
                    "unknown_source_evaluations": summarize(
                        [run["unknown_source_evaluations"] for run in runs]
                    ),
                    "rf_evaluations": summarize([run["rf_evaluations"] for run in runs]),
                    "serialized_result_bytes_estimate": summarize(
                        [run["serialized_result_bytes_estimate"] for run in runs]
                    ),
                    "cancellation_acknowledgement_seconds": summarize(
                        [run["cancellation_probe"]["acknowledgement_seconds"] for run in runs]
                    ),
                    "warm_elapsed_seconds": summarize(
                        [run["warm_run"]["elapsed_seconds"] for run in runs]
                    ),
                    "warm_cache_hits": summarize(
                        [run["warm_run"]["cache_hits"] for run in runs]
                    ),
                    "warm_cache_misses": summarize(
                        [run["warm_run"]["cache_misses"] for run in runs]
                    ),
                }
                if not args.summary_only:
                    report["repetitions"] = runs
                reports.append(report)
    output = json.dumps(
            {
                "benchmark": "local raster mesh coverage" if args.dtm else "synthetic area mesh coverage",
                "measurement_notes": [
                    "Cold means a fresh application cache, not flushed OS or storage caches.",
                    "terrain_reads counts sampling calls; terrain_bytes_returned is sampled array bytes, not disk I/O.",
                    "Peak memory is the child process high-water working set, including setup and all passes.",
                    "Cancellation acknowledgement starts after the requested RF evaluation returns; no server scheduler is measured.",
                ],
                "host": {
                    "platform": platform.platform(),
                    "python": platform.python_version(),
                    "cpu_count": os.cpu_count(),
                },
                "cache_warm_rerun": "measured against a separate bounded coverage cache per child process",
                "results": reports,
            },
            indent=2,
            sort_keys=True,
        )
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(output + "\n", encoding="utf-8")
    print(output)


if __name__ == "__main__":
    main()
