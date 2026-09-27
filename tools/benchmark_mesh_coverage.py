"""Fresh-process synthetic benchmarks for the area mesh-coverage engine."""

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


def run_one(source_count: int, requested_cells: int, scenario: str) -> dict[str, Any]:
    from rf_router_planner.coverage.engine import calculate_coverage
    from rf_router_planner.models.coverage import CoverageSettings
    from rf_router_planner.models.settings import CandidateSettings, RFSettings
    from rf_router_planner.models.site import Site, SiteKind
    from rf_router_planner.optimization.cache import LinkMetricsCache
    from rf_router_planner.terrain.raster import ArrayTerrain

    side_cells = math.ceil(math.sqrt(requested_cells))
    requested_cells_actual = side_cells**2
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
    terrain = MeteredTerrain(
        ArrayTerrain(
            values,
            origin_x=left - padding,
            origin_y=bottom - padding,
            resolution_m=terrain_resolution,
        )
    )
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
        profile_step_m=200.0,
        maximum_profile_samples=4_096,
    )
    rf = RFSettings(fade_margin_db=0.0)
    terrain_sha256 = hashlib.sha256(values.tobytes()).hexdigest()
    first_chunk_seconds: float | None = None
    response_bytes = 0
    unknown_source_evaluations = 0
    rf_calls = 0
    cancel_rf_calls = 0
    warm_rf_calls = 0
    measurement_phase = "cold"
    cache = LinkMetricsCache(max_entries=50_000)

    from rf_router_planner.rf.propagation import LinkEvaluator

    original_evaluate = LinkEvaluator.evaluate

    def counted_evaluate(self, *args, **kwargs):
        nonlocal cancel_rf_calls, rf_calls, warm_rf_calls
        if measurement_phase == "cancel":
            cancel_rf_calls += 1
        elif measurement_phase == "warm":
            warm_rf_calls += 1
        else:
            rf_calls += 1
        return original_evaluate(self, *args, **kwargs)

    from unittest.mock import patch

    started = time.perf_counter()

    def stream_chunk(cells: list[Any]) -> None:
        nonlocal first_chunk_seconds, response_bytes, unknown_source_evaluations
        if first_chunk_seconds is None:
            first_chunk_seconds = time.perf_counter() - started
        unknown_source_evaluations += sum(cell.unknown_sources for cell in cells)
        response_bytes += len(
            json.dumps(
                [
                    {
                        "index": cell.index,
                        "state": cell.state.value,
                        "source_count": cell.source_count,
                        "unknown_sources": cell.unknown_sources,
                        "best_margin_db": cell.best_margin_db,
                        "best_source_id": cell.best_source_id,
                        "sources": [
                            {
                                "source_id": source.source_id,
                                "downlink_margin_db": source.downlink_margin_db,
                                "uplink_margin_db": source.uplink_margin_db,
                                "two_way_margin_db": source.two_way_margin_db,
                                "valid_downlink": source.valid_downlink,
                                "valid_uplink": source.valid_uplink,
                                "valid_two_way": source.valid_two_way,
                                "rejection": source.rejection,
                            }
                            for source in cell.sources
                        ],
                    }
                    for cell in cells
                ],
                separators=(",", ":"),
            ).encode()
        )

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

    def warm_chunk(_cells: list[Any]) -> None:
        nonlocal warm_first_chunk
        if warm_first_chunk is None:
            warm_first_chunk = time.perf_counter() - warm_started

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
            cancelled=lambda: cancel_rf_calls >= cancel_threshold,
            evaluation_cache=cache,
            cache_namespace="benchmark-cancel",
        )
    cancellation_seconds = time.perf_counter() - cancel_started
    measurement_phase = "cold"
    cancel_terrain_metrics = (
        terrain.sample_calls - primary_terrain_metrics[0],
        terrain.sample_points - primary_terrain_metrics[1],
        terrain.sample_bytes - primary_terrain_metrics[2],
        terrain.sample_nodata_points - primary_terrain_metrics[3],
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
        "terrain_fixture_sha256": terrain_sha256,
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
    parser.add_argument("--summary-only", action="store_true")
    parser.add_argument("--_case", help=argparse.SUPPRESS)
    args = parser.parse_args()
    if args.repeats < 1:
        parser.error("--repeats must be positive")
    if args._case:
        source_count, cells, scenario = json.loads(args._case)
        print(json.dumps(run_one(source_count, cells, scenario), sort_keys=True))
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
                            json.dumps([source_count, requested_cells, scenario]),
                        ],
                        check=True,
                        capture_output=True,
                        text=True,
                    )
                    runs.append(json.loads(child.stdout.strip()))
                report = {
                    "scenario": scenario,
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
    print(
        json.dumps(
            {
                "benchmark": "synthetic area mesh coverage",
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
    )


if __name__ == "__main__":
    main()
