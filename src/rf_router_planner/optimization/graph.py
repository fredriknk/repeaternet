from __future__ import annotations

import networkx as nx

from rf_router_planner.models.link import LinkResult
from rf_router_planner.models.settings import OptimizationPriority
from rf_router_planner.models.site import Site


def build_graph(sites: list[Site], links: list[LinkResult]) -> nx.Graph:
    graph = nx.Graph()
    for site in sites:
        graph.add_node(site.id, site=site)
    for link in links:
        if link.valid:
            graph.add_edge(link.source_id, link.target_id, link=link)
    return graph


def lexicographic_minimum_hop_path(graph: nx.Graph, source: str, target: str) -> list[str] | None:
    """Minimum hops, then maximum bottleneck margin and documented tie-breakers."""
    if source not in graph or target not in graph:
        return None
    distances = nx.single_source_shortest_path_length(graph, source)
    if target not in distances:
        return None
    target_hops = distances[target]
    if source == target:
        return [source]
    remaining = nx.single_source_shortest_path_length(graph, target)
    dag = nx.DiGraph()
    for node in sorted(distances):
        for neighbor in sorted(graph.neighbors(node)):
            if distances.get(neighbor) == distances[node] + 1 and distances[node] + 1 + remaining.get(neighbor, target_hops + 1) == target_hops:
                dag.add_edge(node, neighbor, link=graph.edges[node, neighbor]["link"])
    # Solve the bottleneck criteria one at a time. A single lexicographic label
    # at an intermediate node is unsafe: a later weak edge can erase its lead,
    # making a previously discarded path better on the next criterion.
    for attribute in ("worst_margin_db", "minimum_fresnel_clearance_ratio"):
        widest = {source: float("inf")}
        for node in sorted(dag, key=lambda n: (distances[n], n)):
            if node not in widest:
                continue
            for neighbor in dag.successors(node):
                value = min(widest[node], getattr(dag.edges[node, neighbor]["link"], attribute))
                widest[neighbor] = max(widest.get(neighbor, -float("inf")), value)
        threshold = widest[target]
        dag.remove_edges_from([(u, v) for u, v, data in dag.edges(data=True) if getattr(data["link"], attribute) < threshold])
    best: dict[str, tuple[tuple[float, float, float], list[str]]] = {
        source: ((0.0, 0.0, 0.0), [source])
    }
    for depth in range(target_hops):
        for node in sorted(n for n, value in distances.items() if value == depth and n in best):
            score, path = best[node]
            for neighbor in sorted(dag.successors(node)):
                if distances.get(neighbor) != depth + 1:
                    continue
                link: LinkResult = graph.edges[node, neighbor]["link"]
                site: Site = graph.nodes[neighbor]["site"]
                next_score = (
                    score[0] - (site.antenna_height_m if neighbor != target else 0.0),
                    score[1] + site.site_quality,
                    score[2] - link.distance_m,
                )
                if neighbor not in best or next_score > best[neighbor][0] or (next_score == best[neighbor][0] and [*path, neighbor] < best[neighbor][1]):
                    best[neighbor] = (next_score, [*path, neighbor])
    return best.get(target, ((), None))[1]


def minimum_infrastructure_path(graph: nx.Graph, source: str, target: str) -> list[str] | None:
    """Minimize router installations plus mast metres, then RF distance."""

    def cost(u: str, v: str, attributes: dict[str, object]) -> float:
        site: Site = graph.nodes[v]["site"]
        link: LinkResult = attributes["link"]  # type: ignore[assignment]
        router_cost = 100.0 + site.antenna_height_m if v != target else 0.0
        return router_cost + link.distance_m / 1_000_000.0

    try:
        return nx.dijkstra_path(graph, source, target, weight=cost)
    except nx.NetworkXNoPath:
        return None


def maximum_reliability_path(graph: nx.Graph, source: str, target: str) -> list[str] | None:
    """Widest path by worst RF margin, with fewer hops and Fresnel as tie-breakers."""
    if source not in graph or target not in graph:
        return None
    if source == target:
        return [source]
    weighted = nx.Graph()
    weighted.add_nodes_from(graph)
    weighted.add_edges_from((u, v, {"weight": data["link"].worst_margin_db}) for u, v, data in graph.edges(data=True))
    tree = nx.maximum_spanning_tree(weighted)
    if not nx.has_path(tree, source, target):
        return None
    path = nx.shortest_path(tree, source, target)
    threshold = min(tree.edges[a, b]["weight"] for a, b in zip(path, path[1:], strict=False))
    eligible = nx.subgraph_view(graph, filter_edge=lambda a, b: graph.edges[a, b]["link"].worst_margin_db >= threshold)
    return lexicographic_minimum_hop_path(eligible, source, target)


def select_path(
    graph: nx.Graph,
    source: str,
    target: str,
    priority: OptimizationPriority,
) -> list[str] | None:
    if priority == OptimizationPriority.MINIMUM_INFRASTRUCTURE:
        return minimum_infrastructure_path(graph, source, target)
    if priority == OptimizationPriority.MAXIMUM_RELIABILITY:
        return maximum_reliability_path(graph, source, target)
    return lexicographic_minimum_hop_path(graph, source, target)
