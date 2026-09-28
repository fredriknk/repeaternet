from __future__ import annotations

import pytest

from rf_router_planner.coverage.compare import compare_coverage_cells


def cell(index: int, longitude: float, *source_values: tuple[str, bool, float]) -> dict[str, object]:
    return {
        "index": index,
        "latitude": 60.0,
        "longitude": longitude,
        "state": "covered" if source_values else "uncovered",
        "sources": [
            {"source_id": source_id, "valid_two_way": valid, "two_way_margin_db": margin}
            for source_id, valid, margin in source_values
        ],
    }


def test_identical_grids_have_zero_change_and_margins_are_comparable() -> None:
    rows = [cell(0, 10.0, ("A", True, 7.0)), cell(1, 10.01)]
    compared = compare_coverage_cells(rows, rows, {"A"}, {"A"})
    assert compared["counts"]["gained_local"] == 0
    assert compared["counts"]["lost_local"] == 0
    assert compared["counts"]["retained_local"] == 1
    assert compared["cells"][0]["margin_delta_db"] == 0.0


def test_gains_losses_reverse_when_baseline_and_scenario_are_swapped() -> None:
    baseline = [cell(0, 10.0), cell(1, 10.01, ("A", True, 5.0))]
    scenario = [cell(0, 10.0, ("B", True, 10.0)), cell(1, 10.01)]
    forward = compare_coverage_cells(baseline, scenario, set(), {"B"})["counts"]
    reverse = compare_coverage_cells(scenario, baseline, {"B"}, set())["counts"]
    assert forward["gained_local"] == reverse["lost_local"] == 1
    assert forward["lost_local"] == reverse["gained_local"] == 1


def test_unknown_cells_are_excluded_and_misaligned_grids_are_rejected() -> None:
    unknown = {**cell(0, 10.0), "state": "unknown_terrain"}
    result = compare_coverage_cells([unknown], [unknown], set(), set())
    assert result["counts"]["unknown"] == 1
    assert result["counts"]["evaluated_cells"] == 0
    assert result["counts"]["baseline_coverage_percent"] is None
    with pytest.raises(ValueError, match="sample coordinates"):
        compare_coverage_cells([cell(0, 10.0)], [cell(0, 10.1)], set(), set())


def test_outside_area_cells_are_separate_from_unknown_and_coverage_deltas() -> None:
    outside = {**cell(0, 10.0, ("A", True, 12.0)), "state": "outside_area"}
    result = compare_coverage_cells([outside], [outside], {"A"}, {"A"})
    assert result["counts"]["outside_area"] == 1
    assert result["counts"]["unknown"] == 0
    assert result["counts"]["evaluated_cells"] == 0
    assert result["counts"]["baseline_coverage_percent"] is None
    assert result["cells"][0]["local_change"] == "outside_area"


def test_unresolved_cells_do_not_become_coverage_gains_or_losses() -> None:
    unresolved = {**cell(0, 10.0), "state": "unresolved"}

    result = compare_coverage_cells([unresolved], [cell(0, 10.0, ("A", True, 8.0))], set(), {"A"})

    assert result["counts"]["unresolved"] == 1
    assert result["counts"]["gained_local"] == 0
    assert result["counts"]["lost_local"] == 0
    assert result["counts"]["evaluated_cells"] == 0
    assert result["cells"][0]["local_change"] == "unresolved"
