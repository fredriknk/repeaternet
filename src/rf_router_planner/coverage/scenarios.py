"""Pure scenario analysis over completed coverage and certified route snapshots."""

from __future__ import annotations

from collections import deque
from collections.abc import Iterable
from typing import Any


def analyze_node_failures(
    cells: Iterable[dict[str, Any]],
    router_ids: list[str],
    source_ids: list[str],
    links: list[dict[str, Any]],
    failed_ids: list[str],
    reference_id: str,
) -> dict[str, Any]:
    """Remove failed routers, then report local and reference-connected coverage.

    Coverage overlap is computed from each cell's same-source two-way validity;
    mesh reachability is computed separately from certified router-to-router links.
    """
    routers = set(router_ids)
    sources = set(source_ids) & routers
    failed = set(failed_ids)
    if len(failed) != len(failed_ids) or not failed <= sources:
        raise ValueError("Failed router IDs must be unique selected coverage sources")
    if reference_id not in sources:
        raise ValueError("Reference router must be an enabled coverage source")

    def build_components(disabled: set[str]) -> tuple[dict[str, set[str]], list[list[str]], dict[str, int]]:
        graph: dict[str, set[str]] = {
            router_id: set() for router_id in routers - disabled
        }
        for link in links:
            left, right = link.get("source_id"), link.get("target_id")
            if (
                link.get("valid")
                and left in graph
                and right in graph
                and left != right
            ):
                graph[left].add(right)
                graph[right].add(left)

        components: list[list[str]] = []
        component_for: dict[str, int] = {}
        for root in sorted(graph):
            if root in component_for:
                continue
            component_index = len(components)
            component = []
            pending = deque([root])
            component_for[root] = component_index
            while pending:
                current = pending.popleft()
                component.append(current)
                for neighbor in sorted(graph[current]):
                    if neighbor not in component_for:
                        component_for[neighbor] = component_index
                        pending.append(neighbor)
            components.append(sorted(component))
        return graph, components, component_for

    baseline_graph, _baseline_components, baseline_component_for = build_components(set())
    graph, components, component_for = build_components(failed)

    reference_component = component_for.get(reference_id)
    connected_sources = (
        set(components[reference_component]) & sources if reference_component is not None else set()
    )
    baseline_reference_component = baseline_component_for.get(reference_id)
    baseline_connected_sources = (
        set(_baseline_components[baseline_reference_component]) & sources
        if baseline_reference_component is not None
        else set()
    )
    rows: list[dict[str, Any]] = []
    lost_local = 0
    lost_connected = 0
    newly_uncovered = 0
    unknown = 0
    outside_area = 0
    for cell in cells:
        source_rows = cell.get("sources", [])
        usable = {
            item.get("source_id")
            for item in source_rows
            if item.get("valid_two_way") and item.get("source_id") in sources
        }
        usable_after = usable - failed
        connected_after = usable_after & connected_sources
        baseline_local = bool(usable)
        baseline_connected = bool(usable & baseline_connected_sources)
        local_after = bool(usable_after)
        connected = bool(connected_after)
        state = cell.get("state")
        if state == "outside_area":
            outside_area += 1
        elif state in {"unknown_terrain", "not_evaluated"}:
            unknown += 1
        else:
            lost_local += int(baseline_local and not local_after)
            lost_connected += int(baseline_connected and not connected)
            newly_uncovered += int(baseline_local and not local_after)
        rows.append(
            {
                "index": cell.get("index"),
                "state": state,
                "local_source_count": len(usable_after),
                "reference_source_count": len(connected_after),
                "locally_covered": local_after,
                "reference_connected": connected,
            }
        )

    isolated = sorted(
        router_id for router_id in graph if not graph[router_id] and len(graph) > 1
    )
    disconnected = sorted(sources - connected_sources - failed)
    return {
        "reference_id": reference_id,
        "reference_available": reference_component is not None,
        "baseline_reference_available": baseline_reference_component is not None,
        "failed_ids": sorted(failed),
        "connected_source_ids": sorted(connected_sources),
        "disconnected_source_ids": disconnected,
        "components": components,
        "isolated_router_ids": isolated,
        "baseline_connected_source_ids": sorted(baseline_connected_sources),
        "unknown_cells": unknown,
        "outside_area_cells": outside_area,
        "lost_local_cells": lost_local,
        "lost_reference_connected_cells": lost_connected,
        "newly_uncovered_cells": newly_uncovered,
        "cells": rows,
    }
