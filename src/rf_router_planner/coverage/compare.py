"""Reference comparisons of compatible, immutable coverage result grids."""

from __future__ import annotations

import math
from collections.abc import Iterable
from typing import Any


def compare_coverage_cells(
    baseline: list[dict[str, Any]],
    scenario: list[dict[str, Any]],
    baseline_connected_sources: set[str],
    scenario_connected_sources: set[str],
    *,
    coordinate_tolerance: float = 1e-6,
) -> dict[str, Any]:
    """Compare two-way local and reference-connected coverage on aligned cells."""
    if len(baseline) != len(scenario):
        raise ValueError("Coverage grids contain different cell counts")
    return compare_coverage_streams(
        baseline,
        scenario,
        baseline_connected_sources,
        scenario_connected_sources,
        expected_count=len(baseline),
        coordinate_tolerance=coordinate_tolerance,
    )


def compare_coverage_streams(
    baseline: Iterable[dict[str, Any]],
    scenario: Iterable[dict[str, Any]],
    baseline_connected_sources: set[str],
    scenario_connected_sources: set[str],
    *,
    expected_count: int,
    coordinate_tolerance: float = 1e-6,
) -> dict[str, Any]:
    """Compare JSONL cell streams with bounded input memory."""
    if expected_count < 0:
        raise ValueError("Expected cell count cannot be negative")
    rows: list[dict[str, Any]] = []
    counts: dict[str, Any] = {
        "gained_local": 0,
        "lost_local": 0,
        "retained_local": 0,
        "unchanged_uncovered_local": 0,
        "gained_reference_connected": 0,
        "lost_reference_connected": 0,
        "retained_reference_connected": 0,
        "unchanged_disconnected": 0,
        "unknown": 0,
        "unresolved": 0,
        "outside_area": 0,
        "margin_comparable": 0,
    }

    def usable_ids(cell: dict[str, Any]) -> set[str]:
        return {
            str(source.get("source_id"))
            for source in cell.get("sources", [])
            if source.get("valid_two_way") and source.get("source_id") is not None
        }

    left_rows, right_rows = iter(baseline), iter(scenario)
    for _ in range(expected_count):
        try:
            left, right = next(left_rows), next(right_rows)
        except StopIteration as exc:
            raise ValueError("Coverage grids contain different cell counts") from exc
        if left.get("index") != right.get("index"):
            raise ValueError("Coverage grids have different cell ordering")
        for key in ("latitude", "longitude"):
            a, b = left.get(key), right.get(key)
            if (
                not isinstance(a, (int, float))
                or not isinstance(b, (int, float))
                or not math.isfinite(a)
                or not math.isfinite(b)
                or abs(a - b) > coordinate_tolerance
            ):
                raise ValueError("Coverage grids do not share the same sample coordinates")
        left_state, right_state = left.get("state"), right.get("state")
        if left_state == "outside_area" or right_state == "outside_area":
            counts["outside_area"] += 1
            rows.append(
                {
                    "index": left["index"],
                    "latitude": left["latitude"],
                    "longitude": left["longitude"],
                    "local_change": "outside_area",
                    "reference_connected_change": "outside_area",
                    "baseline_source_count": 0,
                    "scenario_source_count": 0,
                    "margin_delta_db": None,
                }
            )
            continue
        unknown = left_state in {"unknown_terrain", "not_evaluated"} or right_state in {
            "unknown_terrain",
            "not_evaluated",
        }
        unresolved = left_state == "unresolved" or right_state == "unresolved"
        if unresolved:
            counts["unresolved"] += 1
            local_change, connected_change = "unresolved", "unresolved"
            margin_delta = None
        elif unknown:
            counts["unknown"] += 1
            local_change, connected_change = "unknown", "unknown"
            margin_delta = None
        else:
            left_sources, right_sources = usable_ids(left), usable_ids(right)
            left_local, right_local = bool(left_sources), bool(right_sources)
            left_connected = bool(left_sources & baseline_connected_sources)
            right_connected = bool(right_sources & scenario_connected_sources)
            local_change = _change(left_local, right_local)
            connected_change = _change(left_connected, right_connected)
            if local_change == "none":
                counts["unchanged_uncovered_local"] += 1
            else:
                counts[f"{local_change}_local"] += 1
            if connected_change == "none":
                counts["unchanged_disconnected"] += 1
            else:
                counts[f"{connected_change}_reference_connected"] += 1
            left_margin = _best_margin(left)
            right_margin = _best_margin(right)
            margin_delta = None
            if left_margin is not None and right_margin is not None:
                margin_delta = right_margin - left_margin
                counts["margin_comparable"] += 1
        rows.append(
            {
                "index": left["index"],
                "latitude": left["latitude"],
                "longitude": left["longitude"],
                "local_change": local_change,
                "reference_connected_change": connected_change,
                "baseline_source_count": len(usable_ids(left)),
                "scenario_source_count": len(usable_ids(right)),
                "margin_delta_db": margin_delta,
            }
        )
    try:
        next(left_rows)
        raise ValueError("Coverage grids contain different cell counts")
    except StopIteration:
        pass
    try:
        next(right_rows)
        raise ValueError("Coverage grids contain different cell counts")
    except StopIteration:
        pass
    known = len(rows) - counts["unknown"] - counts["unresolved"] - counts["outside_area"]
    baseline_covered = counts["gained_local"] + counts["retained_local"]
    scenario_covered = counts["lost_local"] + counts["retained_local"]
    counts.update(
        evaluated_cells=known,
        outside_area_cells=counts["outside_area"],
        baseline_covered_cells=baseline_covered,
        scenario_covered_cells=scenario_covered,
        baseline_coverage_percent=(100.0 * baseline_covered / known if known else None),
        scenario_coverage_percent=(100.0 * scenario_covered / known if known else None),
    )
    return {"counts": counts, "cells": rows}


def _change(baseline: bool, scenario: bool) -> str:
    if baseline and scenario:
        return "retained"
    if scenario:
        return "gained"
    if baseline:
        return "lost"
    return "none"


def _best_margin(cell: dict[str, Any]) -> float | None:
    values = [
        source.get("two_way_margin_db")
        for source in cell.get("sources", [])
        if source.get("valid_two_way") and isinstance(source.get("two_way_margin_db"), (int, float))
    ]
    return float(max(values)) if values else None
