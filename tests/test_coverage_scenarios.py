from __future__ import annotations

import pytest

from rf_router_planner.coverage.scenarios import analyze_node_failures


def router(source_id: str, valid: bool = True) -> dict[str, object]:
    return {"source_id": source_id, "valid_two_way": valid}


def cell(index: int, *sources: dict[str, object], state: str = "covered") -> dict[str, object]:
    return {"index": index, "state": state, "sources": list(sources)}


def test_bridge_failure_loses_reference_coverage_but_retains_local_coverage() -> None:
    result = analyze_node_failures(
        [cell(0, router("A")), cell(1, router("B")), cell(2, router("C"))],
        ["A", "B", "C"],
        ["A", "B", "C"],
        [
            {"source_id": "A", "target_id": "B", "valid": True},
            {"source_id": "B", "target_id": "C", "valid": True},
        ],
        ["B"],
        "A",
    )

    assert result["connected_source_ids"] == ["A"]
    assert result["disconnected_source_ids"] == ["C"]
    assert result["lost_local_cells"] == 1
    assert result["lost_reference_connected_cells"] == 2
    assert result["cells"][2]["locally_covered"] is True
    assert result["cells"][2]["reference_connected"] is False


def test_ring_failure_keeps_remaining_sources_connected_and_overlap_is_not_graph() -> None:
    ring_links = [
        {"source_id": "A", "target_id": "B", "valid": True},
        {"source_id": "B", "target_id": "C", "valid": True},
        {"source_id": "C", "target_id": "A", "valid": True},
    ]
    result = analyze_node_failures(
        [cell(0, router("A"), router("B"), router("C"))],
        ["A", "B", "C"],
        ["A", "B", "C"],
        ring_links,
        ["B"],
        "A",
    )
    assert result["connected_source_ids"] == ["A", "C"]
    assert result["lost_local_cells"] == 0
    assert result["lost_reference_connected_cells"] == 0

    isolated = analyze_node_failures(
        [cell(0, router("A"), router("C"))],
        ["A", "C"],
        ["A", "C"],
        [],
        [],
        "A",
    )
    assert isolated["cells"][0]["locally_covered"] is True
    assert isolated["cells"][0]["reference_connected"] is True
    assert isolated["disconnected_source_ids"] == ["C"]


def test_failed_reference_and_unknown_cells_are_reported_without_false_claims() -> None:
    with pytest.raises(ValueError, match="unique selected coverage sources"):
        analyze_node_failures([], ["A"], ["A"], [], ["A", "A"], "A")
    result = analyze_node_failures(
        [cell(0, router("A"), state="unknown_terrain")],
        ["A"],
        ["A"],
        [],
        [],
        "A",
    )
    assert result["unknown_cells"] == 1
    assert result["lost_local_cells"] == 0


def test_outside_area_cells_are_not_counted_as_unknown_or_lost_coverage() -> None:
    result = analyze_node_failures(
        [cell(0, router("A"), state="outside_area")],
        ["A"],
        ["A"],
        [],
        [],
        "A",
    )
    assert result["outside_area_cells"] == 1
    assert result["unknown_cells"] == 0
    assert result["lost_local_cells"] == 0
