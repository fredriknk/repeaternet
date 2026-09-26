"""Reproducible size sweep for the candidate screening and topology search."""

from __future__ import annotations

import argparse
import json
import os
import platform
import random
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


def run_case(count: int, seed: int, topology: str) -> dict[str, Any]:
    from rf_router_planner.models.settings import (
        CandidateSettings,
        OptimizationPriority,
        RFSettings,
    )
    from rf_router_planner.models.site import Site, SiteKind
    from rf_router_planner.optimization.optimizer import RouteOptimizer
    from rf_router_planner.terrain.raster import ArrayTerrain

    rng = random.Random(seed)
    terrain = ArrayTerrain(np.zeros((401, 1001), dtype=np.float32), resolution_m=25.0)
    rf = RFSettings()
    if topology == "mesh":
        from rf_router_planner.models.settings import ValidationMode

        rf.validation_mode = ValidationMode.STRICT_LOS
        rf.required_fresnel_clearance = 0.6
        rf.fade_margin_db = 0.0
        rf.endpoint_a.height_agl_m = 3.0
        rf.endpoint_b.height_agl_m = 3.0
        rf.router.height_agl_m = 100.0
    else:
        rf.endpoint_a.height_agl_m = 100.0
        rf.endpoint_b.height_agl_m = 100.0
    settings = CandidateSettings(
        maximum_candidates=count,
        maximum_neighbors_per_site=16,
        maximum_solution_routers=4 if topology == "mesh" else 6,
        reliability_paths=2,
        priority=OptimizationPriority.MAXIMUM_RELIABILITY
        if topology == "mesh"
        else OptimizationPriority.MINIMUM_ROUTERS,
        refine_radius_m=0.0,
        parallel_workers=1,
    )
    endpoint_height = rf.endpoint_a.height_agl_m
    endpoint_a = Site(
        "A", 500.0, 5_000.0, kind=SiteKind.ENDPOINT_A, antenna_height_m=endpoint_height
    )
    endpoint_b = Site(
        "B", 24_500.0, 5_000.0, kind=SiteKind.ENDPOINT_B, antenna_height_m=endpoint_height
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
                ((8_000.0, 3_500.0), (16_000.0, 3_500.0), (8_000.0, 6_500.0), (16_000.0, 6_500.0))
            )
        )
    candidates.extend(
        Site(
            f"C{index:04d}",
            rng.uniform(500.0, 24_500.0),
            rng.uniform(0.0, 10_000.0),
            antenna_height_m=rf.router.height_agl_m,
        )
        for index in range(len(candidates), count)
    )
    optimizer = RouteOptimizer(terrain, rf, settings)
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
    result = optimizer.optimize(
        endpoint_a,
        endpoint_b,
        candidates=[endpoint_a, *candidates, endpoint_b],
        progress=progress,
    )
    elapsed = time.perf_counter() - started
    if active_phase is not None:
        phase_seconds[active_phase] = (
            phase_seconds.get(active_phase, 0.0) + time.perf_counter() - last_progress
        )
    solution = result.active_solution
    return {
        "candidates": count,
        "topology": topology,
        "seconds": round(elapsed, 3),
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
        "diagnostics": result.diagnostics,
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
    parser.add_argument("--timeout-seconds", type=int, default=180)
    parser.add_argument("--topology", choices=("direct", "mesh"), default="direct")
    parser.add_argument("--seed", type=int, default=20260926)
    parser.add_argument("--worker-count", type=int, help=argparse.SUPPRESS)
    parser.add_argument("--worker-seed", type=int, default=20260926, help=argparse.SUPPRESS)
    parser.add_argument(
        "--worker-topology", choices=("direct", "mesh"), default="direct", help=argparse.SUPPRESS
    )
    args = parser.parse_args()
    if args.repetitions < 1 or any(size < 2 for size in args.sizes):
        parser.error("sizes must be at least 2 and repetitions must be positive")
    if args.topology == "mesh" and any(size < 4 for size in args.sizes):
        parser.error("mesh sizes must be at least 4 to include the four relay anchors")
    if args.worker_count is not None:
        print(json.dumps(run_case(args.worker_count, args.worker_seed, args.worker_topology)))
        return
    print(
        f"RF planner size benchmark | {platform.platform()} | Python {platform.python_version()} | {os.cpu_count()} logical CPUs"
    )
    if args.topology == "direct":
        print("Flat 25 m ArrayTerrain (25 x 10 km); randomized sites; clear direct route.")
    else:
        print(
            "Flat 25 m ArrayTerrain (25 x 10 km); four required relay anchors plus random sites; strict LOS, 2-path reliability."
        )
        print(
            "Mesh mode fixes the four-relay solution; candidate scaling measures screening, not subset search."
        )
    print("One process per measurement; reported times cover RouteOptimizer.optimize.")
    print("RSS is child process peak working set.")
    for size in args.sizes:
        runs = []
        for repetition in range(args.repetitions):
            try:
                completed = subprocess.run(
                    [
                        sys.executable,
                        str(Path(__file__).resolve()),
                        "--worker-count",
                        str(size),
                        "--worker-seed",
                        str(args.seed + repetition),
                        "--worker-topology",
                        args.topology,
                    ],
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
        rss_values = [
            row["peak_rss_mib"] for row in runs if row["peak_rss_mib"] is not None
        ]
        median_rss = statistics.median(rss_values) if rss_values else None
        link_counts = [row["screened_links"] for row in runs]
        links = (
            str(link_counts[0])
            if min(link_counts) == max(link_counts)
            else f"{min(link_counts)}-{max(link_counts)}"
        )
        print(
            f"{size:5d} candidates | median {seconds:8.3f} s | "
            f"peak RSS {median_rss if median_rss is not None else 'n/a':>8} MiB | "
            f"valid screened links {links:>11} | "
            f"routers {runs[-1]['routers']} | found {runs[-1]['found']}"
        )
        for row in runs:
            print("  " + json.dumps(row, sort_keys=True))


if __name__ == "__main__":
    main()
