"""Measure production replay validation of large, synthetic persisted cell files.

The files use the production JSONL shape but intentionally contain generated
scalar results. This isolates parsing/validation/manifest lookup from RF work.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import platform
import subprocess
import sys
import tempfile
import time
from dataclasses import asdict
from pathlib import Path
from typing import Any

from benchmark_mesh_coverage import peak_working_set_mib, summarize

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "src"))


def run_case(source_count: int, requested_cells: int) -> dict[str, Any]:
    from pyproj import Transformer

    from rf_router_planner.models.coverage import (
        CoverageCell,
        CoverageSettings,
        CoverageSourceResult,
        CoverageState,
    )
    from rf_router_planner.web import (
        COVERAGE_MODEL_VERSION,
        _coverage_reuse_fingerprint,
        _find_completed_coverage_reuse,
    )

    side = math.isqrt(min(requested_cells, 250_000 // source_count))
    cells = side * side
    job_id = "0" * 32
    source_ids = [f"R{i:02d}" for i in range(source_count)]
    settings = asdict(CoverageSettings(maximum_cells=requested_cells))
    job = {
        "job_id": job_id, "state": "complete", "model_version": COVERAGE_MODEL_VERSION,
        "project_id": "replay-benchmark", "route_fingerprint": "synthetic-route",
        "terrain_fingerprint": [], "terrain_crs": "EPSG:25833",
        "settings_fingerprint": hashlib.sha256(json.dumps(settings, sort_keys=True).encode()).hexdigest(),
        "radio_fingerprint": "synthetic-radio", "settings": settings,
        "source_ids": source_ids,
        "source_sites": [{"id": identity, "x": 400000 + 1000 * index, "y": 6500000,
                          "antenna_height_m": 50} for index, identity in enumerate(source_ids)],
        "source_height_overrides": {}, "alternative_id": "reference", "snapshot_version": 1,
        "grid_bounds_projected": [400000, 6500000, 400000 + side * 1000, 6500000 + side * 1000],
        "rows": side, "columns": side, "requested_cells": cells,
        "effective_cell_size_m": 1000, "result_file": f"coverage-results/{job_id}.jsonl",
    }
    source_results = [CoverageSourceResult(identity, 20 - i / 64, 17 - i / 64,
                      17 - i / 64, True, True, True) for i, identity in enumerate(source_ids)]
    reverse = Transformer.from_crs(25833, 4326, always_xy=True)
    fingerprint = _coverage_reuse_fingerprint(job)
    assert fingerprint is not None

    with tempfile.TemporaryDirectory(prefix="coverage-replay-") as scratch:
        directory = Path(scratch)
        result_path = directory / job["result_file"]
        result_path.parent.mkdir()
        with result_path.open("w", encoding="utf-8", newline="\n") as handle:
            for first in range(0, cells, 128):
                chunk = []
                for index in range(first, min(first + 128, cells)):
                    row, column = divmod(index, side)
                    x = 400000 + (column + 0.5) * 1000
                    y = 6500000 + (side - row - 0.5) * 1000
                    longitude, latitude = reverse.transform(x, y)
                    chunk.append(asdict(CoverageCell(
                        index, x, y, latitude, longitude, CoverageState.COVERED,
                        source_count, 0, 17.0, source_ids[0], source_results,
                    )))
                handle.write(json.dumps(chunk, separators=(",", ":"), allow_nan=False) + "\n")
        file_bytes = result_path.stat().st_size
        assert file_bytes < 100 * 1024 * 1024, "Fixture exceeds the production result limit"
        with result_path.open("rb") as handle:
            file_sha256 = hashlib.file_digest(handle, "sha256").hexdigest()

        # The matching manifest is deliberately last in the 50-entry lookup.
        archive = directory / "coverage-jobs"
        archive.mkdir()
        for index in range(50):
            manifest = {**job, "job_id": f"{index:032x}"}
            if index:
                manifest["route_fingerprint"] = f"other-route-{index}"
            path = archive / f"{index:032x}.json"
            path.write_text(json.dumps(manifest), encoding="utf-8")
            os.utime(path, (1700000000 + index, 1700000000 + index))

        started = time.perf_counter()
        current = _find_completed_coverage_reuse(directory, fingerprint, job)
        current_seconds = time.perf_counter() - started
        assert current is not None and current["job_id"] == job_id
        started = time.perf_counter()
        archived = _find_completed_coverage_reuse(directory, fingerprint)
        archive_seconds = time.perf_counter() - started
        assert archived is not None and archived["job_id"] == job_id

        # Corruption at EOF must be rejected after scanning the entire file.
        with result_path.open("a", encoding="utf-8") as handle:
            handle.write(json.dumps([{"index": cells, "state": "covered"}]) + "\n")
        started = time.perf_counter()
        corrupt = _find_completed_coverage_reuse(directory, fingerprint)
        corruption_seconds = time.perf_counter() - started
        assert corrupt is None, "Replay accepted an out-of-grid trailing cell"

    return {
        "source_count": source_count, "requested_cells": requested_cells,
        "grid_cells": cells, "cell_source_pairs": cells * source_count,
        "result_file_bytes": file_bytes, "result_file_sha256": file_sha256,
        "manifest_count": 50, "current_job_validation_seconds": current_seconds,
        "archive_lookup_validation_seconds": archive_seconds,
        "corrupt_file_rejection_seconds": corruption_seconds,
        "corrupt_file_rejected": True, "peak_working_set_mib": peak_working_set_mib(),
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--sources", type=int, choices=range(1, 65), default=8)
    parser.add_argument("--cells", type=int, default=16384)
    parser.add_argument("--repeats", type=int, default=3)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--_child", action="store_true", help=argparse.SUPPRESS)
    args = parser.parse_args()
    if not 1 <= args.cells <= 16384 or args.repeats < 1:
        parser.error("--cells must be 1..16384 and --repeats must be positive")
    if args._child:
        print(json.dumps(run_case(args.sources, args.cells)))
        return
    runs = []
    for _ in range(args.repeats):
        child = subprocess.run([
            sys.executable, str(Path(__file__).resolve()), "--_child",
            "--sources", str(args.sources), "--cells", str(args.cells),
        ], check=True, capture_output=True, text=True)
        runs.append(json.loads(child.stdout))
    report = {
        "benchmark": "production replay validation, generated scalar JSONL fixtures",
        "host": {"platform": platform.platform(), "python": platform.python_version(),
                 "cpu_count": os.cpu_count()},
        "measurement_notes": [
            "Fixture generation and hashing are outside the timed validation calls.",
            "OS/storage caches are warm after fixture creation; no RF or HTTP latency is measured.",
            "Memory is the child high-water working set, including imports and fixture generation.",
            "Current-job and 50-manifest archive lookup use the production integrity validator.",
        ],
        "repetition_count": len(runs),
        "summary": {key: summarize([run[key] for run in runs]) for key in (
            "current_job_validation_seconds", "archive_lookup_validation_seconds",
            "corrupt_file_rejection_seconds", "peak_working_set_mib",
        )},
        "repetitions": runs,
    }
    output = json.dumps(report, indent=2, sort_keys=True)
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(output + "\n", encoding="utf-8")
    print(output)


if __name__ == "__main__":
    main()
