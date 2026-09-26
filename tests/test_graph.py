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


def test_later_bottleneck_must_not_discard_better_fresnel_prefix():
    sites = [Site(name, i, 0) for i, name in enumerate(["A", "X", "Y", "M", "B"])]
    links = [link("A", "X", 20), link("X", "M", 20), link("A", "Y", 10), link("Y", "M", 10), link("M", "B", 5)]
    links[0].minimum_fresnel_clearance_ratio = 0.2
    graph = build_graph(sites, links)
    assert lexicographic_minimum_hop_path(graph, "A", "B") == ["A", "Y", "M", "B"]


def test_path_objectives_match_exhaustive_small_graph_oracle():
    import random

    import networkx as nx

    from rf_router_planner.optimization.graph import maximum_reliability_path

    randomizer = random.Random(20260926)
    for _ in range(40):
        names = ["A", "B", "C", "D", "E", "F"]
        sites = [Site(n, i, 0, antenna_height_m=randomizer.randint(1, 20), site_quality=randomizer.random()) for i, n in enumerate(names)]
        edges = []
        for i, a in enumerate(names):
            for b in names[i + 1:]:
                if randomizer.random() < 0.55:
                    edge = link(a, b, randomizer.randint(1, 4), randomizer.randint(1, 10))
                    edge.minimum_fresnel_clearance_ratio = randomizer.choice([0.2, 0.6, 1.0])
                    edges.append(edge)
        graph = build_graph(sites, edges)
        paths = list(nx.all_simple_paths(graph, "A", "F"))
        def quality(path, graph=graph):
            links = [graph.edges[a, b]["link"] for a, b in zip(path, path[1:], strict=False)]
            return (min(edge.worst_margin_db for edge in links), min(edge.minimum_fresnel_clearance_ratio for edge in links), -sum(graph.nodes[n]["site"].antenna_height_m for n in path[1:-1]), sum(graph.nodes[n]["site"].site_quality for n in path[1:]), -sum(edge.distance_m for edge in links))
        for algorithm, reliability in [(lexicographic_minimum_hop_path, False), (maximum_reliability_path, True)]:
            if not paths:
                assert algorithm(graph, "A", "F") is None
                continue
            def score(path, reliability=reliability):
                q = quality(path)
                return (q[0], -len(path), *q[1:]) if reliability else (-len(path), *q)
            expected = max(sorted(paths, reverse=True), key=score)
            assert algorithm(graph, "A", "F") == expected
