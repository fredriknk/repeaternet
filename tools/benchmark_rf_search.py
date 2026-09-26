"""Reproducible size sweep for the candidate screening and topology search."""

from __future__ import annotations

import argparse
import json
import math
import os
import platform
import random
import statistics
import subprocess
import sys
import time
from pathlib import Path
from typing import Any
from unittest.mock import patch

import numpy as np

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "src"))


def peak_working_set_mib() -> float | None:
    """Return this process's peak resident working set where the OS exposes it."""
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


def summarize_metric(rows: list[dict[str, Any]], key: str) -> dict[str, float]:
    values = [float(row[key]) for row in rows if row.get(key) is not None]
    if not values:
        return {}
    return {
        "median": round(statistics.median(values), 2),
        "min": round(min(values), 2),
        "max": round(max(values), 2),
    }


def run_case(
    count: int, seed: int, topology: str, distance_km: float, warm_rerun: bool = False
) -> dict[str, Any]:
    from rf_router_planner.models.settings import (
        CandidateSettings,
        OptimizationPriority,
        RFSettings,
    )
    from rf_router_planner.models.site import Site, SiteKind
    from rf_router_planner.optimization.cache import LinkMetricsCache
    from rf_router_planner.optimization.optimizer import RouteOptimizer
    from rf_router_planner.terrain.raster import ArrayTerrain

    rng = random.Random(seed)
    distance_m = distance_km * 1_000.0
    terrain_columns = math.ceil((distance_m + 1_000.0) / 25.0) + 1
    terrain = ArrayTerrain(np.zeros((401, terrain_columns), dtype=np.float32), resolution_m=25.0)
    rf = RFSettings()
    if topology in {"mesh", "long"}:
        from rf_router_planner.models.settings import ValidationMode

        rf.validation_mode = ValidationMode.STRICT_LOS
        rf.required_fresnel_clearance = 0.6
        rf.fade_margin_db = 0.0
        endpoint_height = 10.0 if topology == "long" else 3.0
        rf.endpoint_a.height_agl_m = endpoint_height
        rf.endpoint_b.height_agl_m = endpoint_height
        rf.router.height_agl_m = 100.0
    else:
        rf.endpoint_a.height_agl_m = 100.0
        rf.endpoint_b.height_agl_m = 100.0
    long_relay_count = max(1, math.ceil(distance_m / 22_000.0) - 1)
    settings = CandidateSettings(
        maximum_candidates=count,
        maximum_neighbors_per_site=16,
        maximum_solution_routers=(
            long_relay_count if topology == "long" else 4 if topology == "mesh" else 6
        ),
        reliability_paths=1 if topology == "long" else 2,
        priority=(
            OptimizationPriority.MINIMUM_ROUTERS
            if topology in {"direct", "long"}
            else OptimizationPriority.MAXIMUM_RELIABILITY
        ),
        refine_radius_m=0.0,
        parallel_workers=1,
    )
    endpoint_height = rf.endpoint_a.height_agl_m
    endpoint_a = Site(
        "A", 500.0, 5_000.0, kind=SiteKind.ENDPOINT_A, antenna_height_m=endpoint_height
    )
    endpoint_b = Site(
        "B",
        500.0 + distance_m,
        5_000.0,
        kind=SiteKind.ENDPOINT_B,
        antenna_height_m=endpoint_height,
    )
    candidates = []
    if topology == "mesh":
        # Anchor two disjoint relay corridors so success does not depend on a
        # random sparse sample happening to span the full 24 km.
        candidates.extend(
            Site(
                f"C{index:04d}",
                x,
                y,
                required=True,
                antenna_height_m=rf.router.height_agl_m,
            )
            for index, (x, y) in enumerate(
                (
                    (500.0 + distance_m * (7_500 / 24_000), 3_500.0),
                    (500.0 + distance_m * (15_500 / 24_000), 3_500.0),
                    (500.0 + distance_m * (7_500 / 24_000), 6_500.0),
                    (500.0 + distance_m * (15_500 / 24_000), 6_500.0),
                )
            )
        )
    elif topology == "long":
        # Required relays ensure a long, strict-LOS route exists independently
        # of the randomized distractors; this measures scale, not subset choice.
        candidates.extend(
            Site(
                f"C{index - 1:04d}",
                500.0 + distance_m * index / (long_relay_count + 1),
                5_000.0,
                required=True,
                antenna_height_m=rf.router.height_agl_m,
            )
            for index in range(1, long_relay_count + 1)
        )
    candidates.extend(
        Site(
            f"C{index:04d}",
            rng.uniform(500.0, 500.0 + distance_m),
            rng.uniform(0.0, 10_000.0),
            antenna_height_m=rf.router.height_agl_m,
        )
        for index in range(len(candidates), count)
    )
    cache = LinkMetricsCache() if warm_rerun else None
    optimizer = RouteOptimizer(terrain, rf, settings, evaluation_cache=cache)
    required_routers = (
        [site for site in candidates if site.required] if topology == "long" else None
    )
    counters = {"rf_evaluations": 0, "optimistic_checks": 0, "terrain_sampled_points": 0}
    original_evaluate = optimizer.evaluator.evaluate
    original_optimistic = optimizer.evaluator.optimistic_margin_db
    original_sample = terrain.sample

    def evaluate(*args, **kwargs):
        before = cache.stats("default")["misses"] if cache is not None else 0
        link = original_evaluate(*args, **kwargs)
        if cache is None or kwargs.get("include_profile", True):
            counters["rf_evaluations"] += 1
        elif cache.stats("default")["misses"] > before:
            counters["rf_evaluations"] += 1
        return link

    def optimistic(*args, **kwargs):
        counters["optimistic_checks"] += 1
        return original_optimistic(*args, **kwargs)

    def sample(_terrain, x, y, *args, **kwargs):
        counters["terrain_sampled_points"] += int(np.size(x))
        return original_sample(x, y, *args, **kwargs)

    optimizer.evaluator.evaluate = evaluate
    optimizer.evaluator.optimistic_margin_db = optimistic
    with patch.object(type(terrain), "sample", sample):
        return _run_optimization_case(
            optimizer,
            terrain,
            rf,
            settings,
            endpoint_a,
            endpoint_b,
            candidates,
            required_routers,
            counters,
            count,
            topology,
            distance_km,
            warm_rerun,
        )


def _run_optimization_case(
    optimizer,
    terrain,
    rf,
    settings,
    endpoint_a,
    endpoint_b,
    candidates,
    required_routers,
    counters,
    count,
    topology,
    distance_km,
    warm_rerun,
) -> dict[str, Any]:
    phase_seconds: dict[str, float] = {}
    active_phase: str | None = None
    last_progress = time.perf_counter()

    def progress(stage: str, _done: int, _total: int) -> None:
        nonlocal active_phase, last_progress
        now = time.perf_counter()
        if active_phase is not None:
            phase_seconds[active_phase] = phase_seconds.get(active_phase, 0.0) + now - last_progress
        active_phase = stage
        last_progress = now

    started = time.perf_counter()
    first_certified_seconds: float | None = None

    def solution_progress(_result, search_complete: bool) -> None:
        nonlocal first_certified_seconds
        if first_certified_seconds is None and _result.found:
            first_certified_seconds = time.perf_counter() - started

    optimize_args = (
        endpoint_a,
        endpoint_b,
    )
    optimize_kwargs = {
        "candidates": [endpoint_a, *candidates, endpoint_b],
        "required_routers": required_routers,
        "progress": progress,
        "solution_progress": solution_progress,
    }
    result = optimizer.optimize(*optimize_args, **optimize_kwargs)
    elapsed = time.perf_counter() - started
    cold_first_certified = first_certified_seconds
    if active_phase is not None:
        phase_seconds[active_phase] = (
            phase_seconds.get(active_phase, 0.0) + time.perf_counter() - last_progress
        )
        active_phase = None
    cold_rf_evaluations = counters["rf_evaluations"]
    cold_terrain_points = counters["terrain_sampled_points"]
    warm_elapsed: float | None = None
    warm_first_certified: float | None = None
    if warm_rerun:
        started = time.perf_counter()
        first_certified_seconds = None
        active_phase = None
        last_progress = started
        result = optimizer.optimize(*optimize_args, **optimize_kwargs)
        warm_elapsed = time.perf_counter() - started
        warm_first_certified = first_certified_seconds
    if active_phase is not None:
        phase_seconds[active_phase] = (
            phase_seconds.get(active_phase, 0.0) + time.perf_counter() - last_progress
        )
    solution = result.active_solution
    return {
        "candidates": count,
        "topology": topology,
        "distance_km": distance_km,
        "seconds": round(elapsed, 3),
        "warm_rerun_seconds": round(warm_elapsed, 3) if warm_elapsed is not None else None,
        "warm_rerun_speedup": (
            round(elapsed / warm_elapsed, 2) if warm_elapsed and warm_elapsed > 0 else None
        ),
        "warm_rf_evaluations": counters["rf_evaluations"] - cold_rf_evaluations,
        "warm_terrain_sampled_points": counters["terrain_sampled_points"] - cold_terrain_points,
        "peak_rss_mib": round(memory, 1)
        if (memory := peak_working_set_mib()) is not None
        else None,
        "found": result.found,
        "routers": result.router_count,
        "achieved_paths": solution.achieved_path_count if solution else 0,
        "resilient": solution.resilient if solution else False,
        "solution_sites": len(solution.sites) if solution else 0,
        "selected_links": len(solution.links) if solution else 0,
        "screened_links": len(result.all_valid_links),
        "rf_evaluations": counters["rf_evaluations"],
        "rf_cache_hits": optimizer.evaluator.cache_stats["hits"]
        if optimizer.evaluator.cache_stats
        else 0,
        "rf_cache_misses": optimizer.evaluator.cache_stats["misses"]
        if optimizer.evaluator.cache_stats
        else counters["rf_evaluations"],
        "optimistic_checks": counters["optimistic_checks"],
        "terrain_sampled_points": counters["terrain_sampled_points"],
        "terrain_io_bytes": 0,
        "terrain_kind": "in-memory ArrayTerrain; no file I/O",
        "worker_count": settings.parallel_workers,
        "time_to_first_certified_route_seconds": (
            round(cold_first_certified, 3) if cold_first_certified is not None else None
        ),
        "first_route_metric_status": "certified solution-progress callback",
        "warm_time_to_first_certified_route_seconds": (
            round(warm_first_certified, 3) if warm_first_certified is not None else None
        ),
        "diagnostics": result.diagnostics,
        "route_ids": [site.id for site in result.route],
        "route_margins_db": [link.worst_margin_db for link in result.links],
        "alternative_router_ids": [solution.router_ids for solution in result.alternatives],
        "endpoint_a_links": sum(
            link.source_id == endpoint_a.id or link.target_id == endpoint_a.id
            for link in result.all_valid_links
        ),
        "endpoint_b_links": sum(
            link.source_id == endpoint_b.id or link.target_id == endpoint_b.id
            for link in result.all_valid_links
        ),
        "search_mode": next(
            (
                line
                for line in (solution.diagnostics if solution else [])
                if "search" in line.lower()
            ),
            "unreported",
        ),
        "slowest_phases_s": {
            key: round(value, 3)
            for key, value in sorted(phase_seconds.items(), key=lambda item: item[1], reverse=True)[
                :4
            ]
        },
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--sizes", nargs="+", type=int, default=[200, 800, 2_000])
    parser.add_argument("--repetitions", type=int, default=1)
    parser.add_argument("--summary-only", action="store_true")
    parser.add_argument(
        "--warm-rerun",
        action="store_true",
        help="Run one identical optimization again in the same process/cache.",
    )
    parser.add_argument("--timeout-seconds", type=int, default=180)
    parser.add_argument("--topology", choices=("direct", "mesh", "long"), default="direct")
    parser.add_argument("--distance-km", type=float, default=24.0)
    parser.add_argument("--seed", type=int, default=20260926)
    parser.add_argument("--worker-count", type=int, help=argparse.SUPPRESS)
    parser.add_argument("--worker-seed", type=int, default=20260926, help=argparse.SUPPRESS)
    parser.add_argument(
        "--worker-topology",
        choices=("direct", "mesh", "long"),
        default="direct",
        help=argparse.SUPPRESS,
    )
    parser.add_argument("--worker-distance-km", type=float, default=24.0, help=argparse.SUPPRESS)
    args = parser.parse_args()
    if args.repetitions < 1 or any(size < 2 for size in args.sizes):
        parser.error("sizes must be at least 2 and repetitions must be positive")
    if args.distance_km <= 0:
        parser.error("distance must be positive")
    if args.topology == "mesh" and any(size < 4 for size in args.sizes):
        parser.error("mesh sizes must be at least 4 to include the four relay anchors")
    if args.topology == "long":
        relay_count = max(1, math.ceil(args.distance_km * 1_000 / 22_000) - 1)
        if any(size < relay_count for size in args.sizes):
            parser.error(f"candidate counts must be at least {relay_count} for this distance")
    if args.worker_count is not None:
        print(
            json.dumps(
                run_case(
                    args.worker_count,
                    args.worker_seed,
                    args.worker_topology,
                    args.worker_distance_km,
                    args.warm_rerun,
                )
            )
        )
        return
    print(
        f"RF planner size benchmark | {platform.platform()} | Python {platform.python_version()} | {os.cpu_count()} logical CPUs"
    )
    print(
        f"Flat 25 m ArrayTerrain ({args.distance_km:g} km x 10 km); "
        "times cover RouteOptimizer.optimize, one process per measurement."
    )
    if args.warm_rerun:
        print("Each measurement includes one cold run followed by an identical warm-cache rerun.")
    if args.topology == "direct":
        print("Randomized sites; 100 m endpoint antennas; clear direct route.")
    else:
        if args.topology == "mesh":
            case_description = (
                "Four required relay anchors; strict LOS, 2-path reliability. "
                "Candidate scaling measures screening, not subset search."
            )
        else:
            relay_count = max(1, math.ceil(args.distance_km * 1_000 / 22_000) - 1)
            case_description = (
                f"{relay_count} required chain relays; strict LOS, one path. "
                "Candidate scaling measures screening, not subset search."
            )
        print(case_description)
    print("RSS is child process peak working set.")
    for size in args.sizes:
        runs = []
        for repetition in range(args.repetitions):
            try:
                command = [
                    sys.executable,
                    str(Path(__file__).resolve()),
                    "--worker-count",
                    str(size),
                    "--worker-seed",
                    str(args.seed + repetition),
                    "--worker-topology",
                    args.topology,
                    "--worker-distance-km",
                    str(args.distance_km),
                ]
                if args.warm_rerun:
                    command.append("--warm-rerun")
                completed = subprocess.run(
                    command,
                    check=True,
                    capture_output=True,
                    text=True,
                    cwd=ROOT,
                    timeout=args.timeout_seconds,
                )
            except subprocess.TimeoutExpired:
                print(f"{size:5d} candidates | timed out after {args.timeout_seconds}s")
                runs = []
                break
            runs.append(json.loads(completed.stdout))
        if not runs:
            continue
        seconds = statistics.median(row["seconds"] for row in runs)
        time_range = (min(row["seconds"] for row in runs), max(row["seconds"] for row in runs))
        rss_values = [row["peak_rss_mib"] for row in runs if row["peak_rss_mib"] is not None]
        median_rss = statistics.median(rss_values) if rss_values else None
        rss_range = (min(rss_values), max(rss_values)) if rss_values else None
        rss_range_label = (
            f"{rss_range[0]:.1f}-{rss_range[1]:.1f}" if rss_range is not None else "n/a"
        )
        link_counts = [row["screened_links"] for row in runs]
        links = (
            str(link_counts[0])
            if min(link_counts) == max(link_counts)
            else f"{min(link_counts)}-{max(link_counts)}"
        )
        print(
            f"{size:5d} candidates | median {seconds:8.3f} s "
            f"(range {time_range[0]:.3f}-{time_range[1]:.3f}) | "
            f"peak RSS {median_rss if median_rss is not None else 'n/a':>8} MiB "
            f"(range {rss_range_label}) | "
            f"valid screened links {links:>11} | "
            f"routers {runs[-1]['routers']} | found {runs[-1]['found']}"
        )
        if args.summary_only:
            print(
                "  measurements "
                + json.dumps(
                    {
                        key: summarize_metric(runs, key)
                        for key in (
                            "rf_evaluations",
                            "rf_cache_hits",
                            "rf_cache_misses",
                            "optimistic_checks",
                            "terrain_sampled_points",
                            "time_to_first_certified_route_seconds",
                            "warm_rerun_seconds",
                            "warm_rerun_speedup",
                            "warm_rf_evaluations",
                            "warm_terrain_sampled_points",
                            "warm_time_to_first_certified_route_seconds",
                        )
                    },
                    sort_keys=True,
                )
            )
            if len({tuple(row["route_ids"]) for row in runs}) != 1:
                print("  warning: selected routes vary across repetitions")
        else:
            for row in runs:
                print("  " + json.dumps(row, sort_keys=True))


if __name__ == "__main__":
    main()
