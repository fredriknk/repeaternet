from __future__ import annotations

from rf_router_planner.models.link import DirectionResult, LinkResult
from rf_router_planner.models.network import NetworkSolution
from rf_router_planner.models.settings import CandidateSettings, OptimizationPriority
from rf_router_planner.models.site import Site, SiteKind, SiteOrigin
from rf_router_planner.optimization.optimizer import OptimizationResult
from rf_router_planner.optimization.topology import (
    select_active_solution_index,
    solve_topologies,
)
from rf_router_planner.project import Project, load_project, save_project


def link(left: str, right: str, margin: float = 10.0) -> LinkResult:
    forward = DirectionResult(
        left, right, 1_000, 0, 0, 0, 0, 0, 0, -100, -130, margin, margin, 0, 0, True
    )
    reverse = DirectionResult(
        right, left, 1_000, 0, 0, 0, 0, 0, 0, -100, -130, margin, margin, 0, 0, True
    )
    return LinkResult(left, right, 1_000, forward, reverse, True, True, True, 10, 1, 500, 5, 0)


def client(site_id: str, x: float) -> Site:
    return Site(site_id, x, 0, kind=SiteKind.CLIENT)


def router(
    site_id: str,
    x: float,
    *,
    required: bool = False,
    origin: SiteOrigin = SiteOrigin.OPTIMIZED,
) -> Site:
    return Site(
        site_id,
        x,
        0,
        kind=SiteKind.CANDIDATE,
        required=required,
        origin=origin,
    )


def test_maximum_reliability_chooses_small_redundant_topology_not_dense_chain() -> None:
    a, b = client("A", 0), client("B", 10)
    redundant_left, redundant_right = router("redundant-left", 4), router("redundant-right", 6)
    chain = [router(f"chain-{index}", float(index + 1)) for index in range(3)]
    links = [
        link("A", "redundant-left", 5),
        link("redundant-left", "B", 5),
        link("A", "redundant-right", 5),
        link("redundant-right", "B", 5),
        link("A", "chain-0", 30),
        link("chain-0", "chain-1", 30),
        link("chain-1", "chain-2", 30),
        link("chain-2", "B", 30),
    ]
    settings = CandidateSettings(
        maximum_solution_routers=3,
        reliability_paths=2,
        priority=OptimizationPriority.MAXIMUM_RELIABILITY,
    )

    alternatives = solve_topologies(
        [a, b, redundant_left, redundant_right, *chain], links, settings
    )
    active = alternatives[
        select_active_solution_index(alternatives, OptimizationPriority.MAXIMUM_RELIABILITY)
    ]

    assert active.router_count == 2
    assert set(active.router_ids) == {"redundant-left", "redundant-right"}
    assert active.achieved_path_count == 2
    assert active.resilient


def test_returns_best_solution_for_each_exact_router_count() -> None:
    a, b = client("A", 0), client("B", 10)
    routers = [router(f"R{index}", float(index)) for index in range(1, 4)]
    links = [
        edge for candidate in routers for edge in (link("A", candidate.id), link(candidate.id, "B"))
    ]
    links.extend(link(left.id, right.id) for left, right in zip(routers, routers[1:], strict=False))
    links.append(link("R1", "R3"))
    settings = CandidateSettings(maximum_solution_routers=3)

    alternatives = solve_topologies([a, b, *routers], links, settings)

    assert [solution.router_count for solution in alternatives] == [1, 2, 3]
    assert all(len(solution.router_ids) == solution.router_count for solution in alternatives)
    assert len(alternatives[1].links) == 5
    assert len(alternatives[2].links) == 9


def test_three_clients_are_all_connected_and_have_pair_paths() -> None:
    clients = [client("A", 0), client("B", 10), client("C", 20)]
    middle = router("middle", 10)
    links = [link(item.id, middle.id) for item in clients]

    alternatives = solve_topologies(
        [*clients, middle],
        links,
        CandidateSettings(maximum_solution_routers=1),
    )

    assert len(alternatives) == 1
    solution = alternatives[0]
    assert solution.client_ids == ["A", "B", "C"]
    assert set(solution.client_paths) == {("A", "B"), ("A", "C"), ("B", "C")}
    assert all(paths for paths in solution.client_paths.values())
    assert {frozenset((edge.source_id, edge.target_id)) for edge in solution.links} == {
        frozenset(("A", "middle")),
        frozenset(("B", "middle")),
        frozenset(("C", "middle")),
    }


def test_required_manual_router_is_in_every_alternative() -> None:
    a, b = client("A", 0), client("B", 10)
    manual = router("manual", 4, required=True, origin=SiteOrigin.MANUAL)
    optional = router("optional", 6)
    links = [
        link("A", "manual", 3),
        link("manual", "B", 3),
        link("A", "optional", 30),
        link("optional", "B", 30),
    ]

    alternatives = solve_topologies(
        [a, b, manual, optional],
        links,
        CandidateSettings(maximum_solution_routers=2),
    )

    assert [solution.router_count for solution in alternatives] == [1, 2]
    assert all("manual" in solution.router_ids for solution in alternatives)
    assert alternatives[0].router_ids == ["manual"]


def test_optimization_result_additions_preserve_positional_construction() -> None:
    a, b = client("A", 0), client("B", 10)
    old_style = OptimizationResult([a, b], [], [], [], ["legacy"], 1.5)
    solution = NetworkSolution(
        "one router",
        [a, router("R1", 5), b],
        [],
        ["A", "B"],
        ["R1"],
        {("A", "B"): [["A", "R1", "B"]]},
    )
    old_style.alternatives = [solution]

    selected = old_style.select_solution(0)

    assert selected is solution
    assert old_style.router_count == 1
    assert [site.id for site in old_style.route] == ["A", "R1", "B"]


def test_site_origin_and_topology_settings_round_trip(tmp_path) -> None:
    a, b = client("A", 0), client("B", 10)
    known = router("known", 5, required=True, origin=SiteOrigin.KNOWN)
    project = Project(endpoint_a=a, endpoint_b=b, selected_routers=[known])
    project.candidate_settings.maximum_solution_routers = 4
    project.candidate_settings.reliability_paths = 3
    path = tmp_path / "network.rfplan.json"

    save_project(project, path)
    loaded = load_project(path)

    assert loaded.selected_routers[0].origin == SiteOrigin.KNOWN
    assert loaded.selected_routers[0].required
    assert loaded.selected_routers[0].enabled
    assert loaded.candidate_settings.maximum_solution_routers == 4
    assert loaded.candidate_settings.reliability_paths == 3


def test_infrastructure_can_prefer_two_short_masts_over_one_tall_mast():
    a, b = client("A", 0), client("B", 10)
    tall, left, right = router("tall", 5), router("left", 3), router("right", 7)
    tall.antenna_height_m = 200
    left.antenna_height_m = right.antenna_height_m = 3
    links = [
        link("A", "tall"),
        link("tall", "B"),
        link("A", "left"),
        link("left", "right"),
        link("right", "B"),
    ]
    settings = CandidateSettings(
        maximum_solution_routers=2, priority=OptimizationPriority.MINIMUM_INFRASTRUCTURE
    )
    alternatives = solve_topologies([a, b, tall, left, right], links, settings)
    selected = alternatives[select_active_solution_index(alternatives, settings.priority)]
    assert set(selected.router_ids) == {"left", "right"}
    assert alternatives[
        select_active_solution_index(alternatives, OptimizationPriority.MINIMUM_ROUTERS)
    ].router_ids == ["tall"]


def test_topology_selection_is_stable_under_input_reversal():
    a, b = client("A", 0), client("B", 10)
    x, y = router("X", 4), router("Y", 6)
    sites = [a, b, x, y]
    edges = [link("A", "X"), link("X", "B"), link("A", "Y"), link("Y", "B")]
    settings = CandidateSettings(maximum_solution_routers=1)
    forward = solve_topologies(sites, edges, settings)
    reverse = solve_topologies(sites[::-1], edges[::-1], settings)
    assert forward[0].router_ids == reverse[0].router_ids == ["X"]
    assert forward[0].client_paths == reverse[0].client_paths


def test_disconnected_candidates_are_pruned_before_subset_enumeration(monkeypatch):
    import rf_router_planner.optimization.topology as topology

    a, b = client("A", 0), client("B", 10)
    bridge = router("bridge", 5)
    unrelated = [router(f"island-{i}", 100 + i) for i in range(40)]
    calls = []
    original = topology._evaluate_router_subset

    def observe(graph, clients, routers, paths, priority):
        calls.append([s.id for s in routers])
        return original(graph, clients, routers, paths, priority)

    monkeypatch.setattr(topology, "_evaluate_router_subset", observe)
    solutions = solve_topologies(
        [a, b, bridge, *unrelated],
        [link("A", "bridge"), link("bridge", "B")],
        CandidateSettings(maximum_solution_routers=6),
    )
    assert calls == [[], ["bridge"]]
    assert solutions[0].router_ids == ["bridge"]
    assert "Exhaustive" in solutions[0].diagnostics[0]


def test_topology_cancellation_discards_partial_alternatives():
    a, b = client("A", 0), client("B", 10)
    routers = [router(f"R{i}", i) for i in range(20)]
    edges = [
        link("A", "B"),
        *[edge for r in routers for edge in (link("A", r.id), link(r.id, "B"))],
    ]
    checks = 0

    def cancelled():
        nonlocal checks
        checks += 1
        return checks >= 8

    assert (
        solve_topologies(
            [a, b, *routers],
            edges,
            CandidateSettings(maximum_solution_routers=2),
            cancelled=cancelled,
        )
        == []
    )
    assert checks == 8


def test_heuristic_search_is_labeled_and_cancellable(monkeypatch):
    import rf_router_planner.optimization.topology as topology

    monkeypatch.setattr(topology, "_EXACT_COMBINATION_LIMIT", 1)
    a, b = client("A", 0), client("B", 10)
    routers = [router(f"R{i}", i) for i in range(5)]
    edges = [edge for r in routers for edge in (link("A", r.id), link(r.id, "B"))]
    solutions = solve_topologies(
        [a, b, *routers], edges, CandidateSettings(maximum_solution_routers=2)
    )
    assert all("Heuristic" in s.diagnostics[0] for s in solutions)
    checks = 0

    def cancelled():
        nonlocal checks
        checks += 1
        return checks >= 6

    assert (
        solve_topologies(
            [a, b, *routers],
            edges,
            CandidateSettings(maximum_solution_routers=2),
            cancelled=cancelled,
        )
        == []
    )
    assert checks < 10
