from __future__ import annotations

import heapq

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
    # score=(bottleneck margin, worst Fresnel ratio, -mast total, site quality, -distance)
    best: dict[str, tuple[tuple[float, float, float, float, float], list[str]]] = {
        source: ((float("inf"), float("inf"), 0.0, 0.0, 0.0), [source])
    }
    for depth in range(target_hops):
        for node in [n for n, value in distances.items() if value == depth and n in best]:
            score, path = best[node]
            for neighbor in graph.neighbors(node):
                if distances.get(neighbor) != depth + 1:
                    continue
                link: LinkResult = graph.edges[node, neighbor]["link"]
                site: Site = graph.nodes[neighbor]["site"]
                next_score = (
                    min(score[0], link.worst_margin_db),
                    min(score[1], link.minimum_fresnel_clearance_ratio),
                    score[2] - (site.antenna_height_m if neighbor != target else 0.0),
                    score[3] + site.site_quality,
                    score[4] - link.distance_m,
                )
                if neighbor not in best or next_score > best[neighbor][0]:
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
    best: dict[str, tuple[float, int, float]] = {source: (float("inf"), 0, float("inf"))}
    paths: dict[str, list[str]] = {source: [source]}
    queue: list[tuple[float, int, float, str]] = [(-float("inf"), 0, -float("inf"), source)]
    while queue:
        _negative_margin, _hops, _negative_fresnel, node = heapq.heappop(queue)
        if node == target:
            return paths[node]
        margin, negative_hops, fresnel = best[node]
        for neighbor in graph.neighbors(node):
            link: LinkResult = graph.edges[node, neighbor]["link"]
            score = (
                min(margin, link.worst_margin_db),
                negative_hops - 1,
                min(fresnel, link.minimum_fresnel_clearance_ratio),
            )
            if neighbor not in best or score > best[neighbor]:
                best[neighbor] = score
                paths[neighbor] = [*paths[node], neighbor]
                heapq.heappush(queue, (-score[0], -score[1], -score[2], neighbor))
    return None


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
