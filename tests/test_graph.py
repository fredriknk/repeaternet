from rf_router_planner.models.link import DirectionResult, LinkResult
from rf_router_planner.models.site import Site
from rf_router_planner.optimization.graph import build_graph, lexicographic_minimum_hop_path


def link(a: str, b: str, margin: float, distance: float = 1000) -> LinkResult:
    direction = DirectionResult(
        a, b, distance, 0, 0, 0, 0, 0, 0, 0, -130, margin + 10, margin, 0, 0, True
    )
    reverse = DirectionResult(
        b, a, distance, 0, 0, 0, 0, 0, 0, 0, -130, margin + 10, margin, 0, 0, True
    )
    return LinkResult(a, b, distance, direction, reverse, True, True, True, 5, 1, 500, 5, 0)


def test_fewer_routers_beats_shorter_geographic_path() -> None:
    sites = [Site(name, index, 0) for index, name in enumerate(["A", "X", "Y", "Z", "B"])]
    links = [
        link("A", "X", 5, 10_000),
        link("X", "B", 5, 10_000),
        link("A", "Y", 20, 1_000),
        link("Y", "Z", 20, 1_000),
        link("Z", "B", 20, 1_000),
    ]
    graph = build_graph(sites, links)
    assert lexicographic_minimum_hop_path(graph, "A", "B") == ["A", "X", "B"]


def test_equal_hops_choose_greater_bottleneck_margin() -> None:
    sites = [Site(name, index, 0) for index, name in enumerate(["A", "weak", "strong", "B"])]
    links = [
        link("A", "weak", 4),
        link("weak", "B", 4),
        link("A", "strong", 12),
        link("strong", "B", 10),
    ]
    graph = build_graph(sites, links)
    assert lexicographic_minimum_hop_path(graph, "A", "B") == ["A", "strong", "B"]
