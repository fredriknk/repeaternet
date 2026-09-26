from __future__ import annotations

import itertools
import math
from collections.abc import Callable, Iterable, Sequence
from dataclasses import dataclass

import networkx as nx

from rf_router_planner.models.link import LinkResult
from rf_router_planner.models.network import NetworkSolution
from rf_router_planner.models.settings import CandidateSettings, OptimizationPriority
from rf_router_planner.models.site import Site, SiteKind

CLIENT_KINDS = frozenset({SiteKind.ENDPOINT_A, SiteKind.ENDPOINT_B, SiteKind.CLIENT})
_EXACT_COMBINATION_LIMIT = 100_000
_HEURISTIC_BEAM_WIDTH = 64


@dataclass(frozen=True, slots=True)
class _EvaluatedTopology:
    solution: NetworkSolution
    score: tuple[float, ...]
    router_key: tuple[str, ...]


def _valid_link_graph(sites: Sequence[Site], links: Sequence[LinkResult]) -> nx.Graph:
    graph = nx.Graph()
    enabled = {site.id: site for site in sorted(sites, key=lambda s: s.id) if site.enabled}
    for site in enabled.values():
        graph.add_node(site.id, site=site)
    for link in sorted(links, key=lambda edge: tuple(sorted((edge.source_id, edge.target_id)))):
        if not link.valid or link.source_id not in enabled or link.target_id not in enabled:
            continue
        if link.source_id == link.target_id:
            continue
        if graph.has_edge(link.source_id, link.target_id):
            previous: LinkResult = graph.edges[link.source_id, link.target_id]["link"]
            if previous.worst_margin_db >= link.worst_margin_db:
                continue
        graph.add_edge(
            link.source_id,
            link.target_id,
            link=link,
            margin=link.worst_margin_db,
            distance=link.distance_m,
        )
    return graph


def _widest_path_margin(graph: nx.Graph, source: str, target: str) -> float:
    """Return the greatest possible bottleneck margin between two nodes."""
    best: dict[str, float] = {source: float("inf")}
    remaining = set(graph.nodes)
    while remaining:
        node = max(remaining, key=lambda item: (best.get(item, float("-inf")), item))
        remaining.remove(node)
        if node not in best:
            break
        if node == target:
            return best[node]
        for neighbor, attributes in graph[node].items():
            if neighbor not in remaining:
                continue
            possible = min(best[node], float(attributes["margin"]))
            if possible > best.get(neighbor, float("-inf")):
                best[neighbor] = possible
    return float("-inf")


def _representative_paths(
    graph: nx.Graph,
    clients: Sequence[Site],
    cutoff: int,
) -> tuple[dict[tuple[str, str], list[list[str]]], int]:
    paths_by_pair: dict[tuple[str, str], list[list[str]]] = {}
    minimum_count = math.inf
    for left, right in itertools.combinations(clients, 2):
        paths = list(nx.node_disjoint_paths(graph, left.id, right.id, cutoff=cutoff))
        paths.sort(
            key=lambda path: (
                len(path),
                -min(
                    float(graph.edges[a, b]["margin"]) for a, b in zip(path, path[1:], strict=False)
                ),
                tuple(path),
            )
        )
        paths_by_pair[(left.id, right.id)] = paths
        minimum_count = min(minimum_count, len(paths))
    return paths_by_pair, int(minimum_count if minimum_count != math.inf else 0)


def _evaluate_router_subset(
    full_graph: nx.Graph,
    clients: Sequence[Site],
    routers: Sequence[Site],
    requested_paths: int,
    priority: OptimizationPriority = OptimizationPriority.MAXIMUM_RELIABILITY,
) -> _EvaluatedTopology | None:
    selected_ids = [*(client.id for client in clients), *(router.id for router in routers)]
    graph = full_graph.subgraph(selected_ids).copy()
    if len(graph) != len(selected_ids) or not nx.is_connected(graph):
        return None

    client_paths, achieved_paths = _representative_paths(graph, clients, max(1, requested_paths))
    pair_margins = [
        _widest_path_margin(graph, left.id, right.id)
        for left, right in itertools.combinations(clients, 2)
    ]
    minimum_pair_margin = min(pair_margins, default=float("-inf"))
    margins = [float(attributes["margin"]) for _, _, attributes in graph.edges(data=True)]
    mean_margin = sum(margins) / len(margins) if margins else float("-inf")
    minimum_degree = min((graph.degree[node] for node in graph), default=0)
    site_quality = sum(router.site_quality for router in routers)
    total_distance = sum(
        float(attributes["distance"]) for _, _, attributes in graph.edges(data=True)
    )
    quality_score = (
        minimum_pair_margin,
        float(minimum_degree),
        mean_margin,
        site_quality,
        -total_distance,
    )
    score: tuple[float, ...]
    if priority == OptimizationPriority.MAXIMUM_RELIABILITY:
        score = (float(achieved_paths), *quality_score)
    elif priority == OptimizationPriority.MINIMUM_INFRASTRUCTURE:
        score = (-sum(100 + router.antenna_height_m for router in routers), *quality_score)
    else:
        score = (
            minimum_pair_margin,
            min(
                attributes["link"].minimum_fresnel_clearance_ratio
                for _, _, attributes in graph.edges(data=True)
            ),
            -sum(router.antenna_height_m for router in routers),
            site_quality,
            -total_distance,
        )
    selected_links: list[LinkResult] = []
    for left, right in sorted((min(a, b), max(a, b)) for a, b in graph.edges):
        selected_links.append(graph.edges[left, right]["link"])
    router_ids = [router.id for router in routers]
    count = len(router_ids)
    solution = NetworkSolution(
        name=f"{count} router" + ("s" if count != 1 else ""),
        sites=[*clients, *routers],
        links=selected_links,
        client_ids=[client.id for client in clients],
        router_ids=router_ids,
        client_paths=client_paths,
        requested_path_count=requested_paths,
        achieved_path_count=achieved_paths,
    )
    return _EvaluatedTopology(solution, score, tuple(sorted(router_ids)))


def _partial_subset_score(
    graph: nx.Graph,
    clients: Sequence[Site],
    routers: Sequence[Site],
) -> tuple[float, ...]:
    """A deterministic beam-search score for otherwise intractable pools."""
    selected_ids = [*(client.id for client in clients), *(router.id for router in routers)]
    adjacency: dict[str, set[str]] = {site_id: set() for site_id in selected_ids}
    margins: list[float] = []
    for left, right in itertools.combinations(selected_ids, 2):
        attributes = graph.get_edge_data(left, right)
        if attributes is None:
            continue
        adjacency[left].add(right)
        adjacency[right].add(left)
        margins.append(float(attributes["margin"]))

    components: list[set[str]] = []
    unseen = set(selected_ids)
    while unseen:
        pending = [unseen.pop()]
        component: set[str] = set()
        while pending:
            node = pending.pop()
            component.add(node)
            new_nodes = adjacency[node] & unseen
            unseen.difference_update(new_nodes)
            pending.extend(new_nodes)
        components.append(component)
    client_ids = {client.id for client in clients}
    connected_client_pairs = 0
    largest_client_component = 0
    for component in components:
        client_count = len(component & client_ids)
        largest_client_component = max(largest_client_component, client_count)
        connected_client_pairs += client_count * (client_count - 1) // 2
    selected_edge_count = len(margins)
    boundary_reach = sum(graph.degree[node] for node in selected_ids) - 2 * selected_edge_count
    return (
        float(connected_client_pairs),
        float(largest_client_component),
        float(selected_edge_count),
        float(boundary_reach),
        sum(margins),
        sum(router.site_quality for router in routers),
    )


def _router_combinations(
    required: Sequence[Site],
    optional: Sequence[Site],
    count: int,
    heuristic_layers: dict[int, list[tuple[Site, ...]]],
) -> Iterable[tuple[Site, ...]]:
    optional_count = count - len(required)
    if optional_count < 0 or optional_count > len(optional):
        return ()
    combination_count = math.comb(len(optional), optional_count)
    if combination_count <= _EXACT_COMBINATION_LIMIT:
        return (
            (*required, *selection)
            for selection in itertools.combinations(optional, optional_count)
        )
    return heuristic_layers.get(optional_count, ())


def _heuristic_router_layers(
    graph: nx.Graph,
    clients: Sequence[Site],
    required: Sequence[Site],
    optional: Sequence[Site],
    maximum_optional_count: int,
    cancelled: Callable[[], bool] = lambda: False,
) -> dict[int, list[tuple[Site, ...]]]:
    """Build every beam depth once and reuse it for exact-count alternatives."""
    if maximum_optional_count <= 0:
        return {}

    # An exhaustive Steiner-node search is exponential.  Keep the result
    # deterministic and practical for hundreds of terrain candidates by
    # retaining the best partial subsets at every depth.  First reduce the
    # expansion pool using client adjacency, graph degree, and full-graph
    # shortest paths.  The latter protects low-degree bridge sites from being
    # discarded merely because they do not hear a client directly.
    client_ids = {client.id for client in clients}
    by_id = {site.id: site for site in optional}
    protected: set[str] = set()
    for left, right in itertools.combinations(clients, 2):
        if cancelled():
            return {}
        try:
            protected.update(nx.shortest_path(graph, left.id, right.id)[1:-1])
        except nx.NetworkXNoPath:
            continue

    def static_score(site: Site) -> tuple[float, ...]:
        neighbors = list(graph[site.id].items())
        client_neighbors = sum(neighbor in client_ids for neighbor, _ in neighbors)
        margins = [float(attributes["margin"]) for _, attributes in neighbors]
        return (
            float(client_neighbors),
            float(len(neighbors)),
            max(margins, default=float("-inf")),
            sum(margins),
            site.site_quality,
        )

    target_pool_size = min(len(optional), max(64, maximum_optional_count * 24, len(clients) * 16))
    ranked_optional = sorted(optional, key=lambda site: site.id)
    ranked_optional.sort(key=static_score, reverse=True)
    pool_ids = set(protected)
    for site in ranked_optional:
        if len(pool_ids) >= target_pool_size:
            break
        pool_ids.add(site.id)
    search_pool = [by_id[site_id] for site_id in sorted(pool_ids) if site_id in by_id]

    beam: list[tuple[Site, ...]] = [tuple(required)]
    layers: dict[int, list[tuple[Site, ...]]] = {}
    required_ids = {site.id for site in required}
    for depth in range(1, maximum_optional_count + 1):
        if cancelled():
            return {}
        expanded: dict[tuple[str, ...], tuple[Site, ...]] = {}
        for selection in beam:
            selected_ids = {site.id for site in selection}
            for router in search_pool:
                if cancelled():
                    return {}
                if router.id in selected_ids or router.id in required_ids:
                    continue
                trial = (*selection, router)
                key = tuple(sorted(site.id for site in trial))
                expanded[key] = trial
        ranked = sorted(expanded.items(), key=lambda item: item[0])
        ranked.sort(
            key=lambda item: _partial_subset_score(graph, clients, item[1]),
            reverse=True,
        )
        beam = [selection for _, selection in ranked[:_HEURISTIC_BEAM_WIDTH]]
        layers[depth] = beam
        if not beam:
            break
    return layers


def solve_topologies(
    sites: Sequence[Site],
    links: Sequence[LinkResult],
    settings: CandidateSettings,
    priority: OptimizationPriority | None = None,
    cancelled: Callable[[], bool] | None = None,
) -> list[NetworkSolution]:
    """Find the best feasible mesh for each exact router count.

    Every enabled endpoint/client is mandatory.  Enabled non-client sites are
    router candidates, and candidates marked ``required`` occur in every
    solution.  Small candidate sets are solved exactly; large terrain-derived
    sets use a bounded deterministic beam search.
    """
    is_cancelled = cancelled or (lambda: False)
    if is_cancelled():
        return []
    enabled_sites = sorted((site for site in sites if site.enabled), key=lambda s: s.id)
    ids = [site.id for site in enabled_sites]
    if len(ids) != len(set(ids)):
        raise ValueError("Site IDs must be unique")
    disabled_required = [site.id for site in sites if site.required and not site.enabled]
    if disabled_required:
        raise ValueError("Required sites cannot be disabled: " + ", ".join(disabled_required))
    clients = [site for site in enabled_sites if site.kind in CLIENT_KINDS]
    if len(clients) < 2:
        return []
    router_candidates = [site for site in enabled_sites if site.kind not in CLIENT_KINDS]
    required = [site for site in router_candidates if site.required]
    optional = [site for site in router_candidates if not site.required]
    maximum = max(0, settings.maximum_solution_routers)
    if len(required) > maximum:
        raise ValueError(
            f"{len(required)} routers are required, but the solution limit is {maximum}"
        )

    graph = _valid_link_graph(enabled_sites, links)
    component = nx.node_connected_component(graph, clients[0].id)
    if any(site.id not in component for site in [*clients, *required]):
        return []
    optional = [site for site in optional if site.id in component]
    router_candidates = [*required, *optional]
    graph = graph.subgraph(component).copy()
    requested_paths = max(1, settings.reliability_paths)
    effective_priority = priority or settings.priority
    alternatives: list[NetworkSolution] = []
    maximum_router_count = min(maximum, len(router_candidates))
    maximum_optional_count = maximum_router_count - len(required)
    needs_heuristic = any(
        math.comb(len(optional), optional_count) > _EXACT_COMBINATION_LIMIT
        for optional_count in range(maximum_optional_count + 1)
    )
    heuristic_layers = (
        _heuristic_router_layers(
            graph, clients, required, optional, maximum_optional_count, is_cancelled
        )
        if needs_heuristic
        else {}
    )
    for router_count in range(len(required), maximum_router_count + 1):
        if is_cancelled():
            return []
        best: _EvaluatedTopology | None = None
        for routers in _router_combinations(required, optional, router_count, heuristic_layers):
            if is_cancelled():
                return []
            evaluated = _evaluate_router_subset(
                graph, clients, routers, requested_paths, effective_priority
            )
            if evaluated is None:
                continue
            if (
                best is None
                or evaluated.score > best.score
                or (evaluated.score == best.score and evaluated.router_key < best.router_key)
            ):
                best = evaluated
        if best is not None:
            combinations = math.comb(len(optional), router_count - len(required))
            best.solution.diagnostics.append(
                f"Exhaustive subset search within screened graph ({combinations} subsets)."
                if combinations <= _EXACT_COMBINATION_LIMIT
                else f"Heuristic subset search within screened graph (beam width {_HEURISTIC_BEAM_WIDTH}); global optimum not guaranteed."
            )
            alternatives.append(best.solution)

    # Keep exact-count alternatives stable for dropdowns.  Selection policy is
    # exposed separately because changing list order makes "2 routers" jump.
    if effective_priority == OptimizationPriority.MAXIMUM_RELIABILITY:
        active = select_active_solution_index(alternatives, effective_priority)
        if alternatives and active >= 0 and not alternatives[active].resilient:
            alternatives[active].diagnostics.append(
                f"No topology provides {requested_paths} independent client paths; "
                f"best available provides {alternatives[active].achieved_path_count}."
            )
    return alternatives


def select_active_solution_index(
    alternatives: Sequence[NetworkSolution],
    priority: OptimizationPriority,
) -> int:
    """Return the preferred alternative without changing dropdown ordering."""
    if not alternatives:
        return -1
    if priority == OptimizationPriority.MINIMUM_INFRASTRUCTURE:

        def infrastructure_score(index: int) -> tuple[float, float, tuple[str, ...]]:
            solution = alternatives[index]
            routers = set(solution.router_ids)
            cost = sum(100 + site.antenna_height_m for site in solution.sites if site.id in routers)
            return cost, -solution.minimum_margin_db, tuple(sorted(routers))

        return min(range(len(alternatives)), key=infrastructure_score)
    if priority != OptimizationPriority.MAXIMUM_RELIABILITY:
        return min(range(len(alternatives)), key=lambda index: alternatives[index].router_count)

    requested = max(solution.requested_path_count for solution in alternatives)
    resilient = [
        (index, solution)
        for index, solution in enumerate(alternatives)
        if solution.achieved_path_count >= requested
    ]
    if resilient:
        return min(
            resilient,
            key=lambda item: (
                item[1].router_count,
                -item[1].minimum_margin_db,
                tuple(item[1].router_ids),
            ),
        )[0]
    return min(
        range(len(alternatives)),
        key=lambda index: (
            -alternatives[index].achieved_path_count,
            alternatives[index].router_count,
            -alternatives[index].minimum_margin_db,
            tuple(alternatives[index].router_ids),
        ),
    )
