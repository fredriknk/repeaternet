from __future__ import annotations

import itertools
import logging
import time
from collections.abc import Callable
from dataclasses import dataclass, field, replace

import numpy as np

from rf_router_planner.models.link import LinkResult
from rf_router_planner.models.network import NetworkSolution
from rf_router_planner.models.settings import CandidateSettings, OptimizationPriority, RFSettings
from rf_router_planner.models.site import Site, SiteKind
from rf_router_planner.rf.propagation import LinkEvaluator
from rf_router_planner.terrain.candidate_sites import generate_candidates
from rf_router_planner.terrain.raster import TerrainSource

from .graph import build_graph, select_path
from .parallel import evaluate_link_pairs
from .topology import select_active_solution_index, solve_topologies

logger = logging.getLogger(__name__)


@dataclass(slots=True)
class OptimizationResult:
    route: list[Site]
    links: list[LinkResult]
    candidates: list[Site]
    all_valid_links: list[LinkResult]
    diagnostics: list[str] = field(default_factory=list)
    elapsed_seconds: float = 0.0
    alternatives: list[NetworkSolution] = field(default_factory=list)
    active_solution_index: int = 0

    @property
    def found(self) -> bool:
        return bool(self.route or self.alternatives)

    @property
    def router_count(self) -> int:
        if self.active_solution is not None:
            return self.active_solution.router_count
        return max(0, len(self.route) - 2)

    @property
    def active_solution(self) -> NetworkSolution | None:
        if not self.alternatives:
            return None
        if not 0 <= self.active_solution_index < len(self.alternatives):
            return None
        return self.alternatives[self.active_solution_index]

    def select_solution(self, index: int) -> NetworkSolution:
        """Activate an alternative while keeping legacy route fields useful."""
        if not 0 <= index < len(self.alternatives):
            raise IndexError("Solution index out of range")
        self.active_solution_index = index
        solution = self.alternatives[index]
        self.route = solution.primary_route
        self.links = list(solution.links)
        return solution


ProgressCallback = Callable[[str, int, int], None]
CancelCallback = Callable[[], bool]


def screening_pair_indices(
    sites: list[Site],
    neighbor_limit: int,
    terminal_indices: set[int] | None = None,
) -> list[tuple[int, int]]:
    """Return a bounded, spatially connected set of candidate link indices.

    A direct endpoint pair and every endpoint-to-candidate pair are retained.
    Intermediate sites also get their nearest neighbors.  Endpoint completeness
    matters because the geographically nearest or globally highest sites are
    not necessarily the ones visible from a low endpoint.
    """
    if len(sites) < 2:
        return []
    limit = max(1, neighbor_limit)
    terminals = terminal_indices or {0, len(sites) - 1}
    allowed: set[tuple[int, int]] = {
        (min(left, right), max(left, right))
        for left, right in itertools.combinations(sorted(terminals), 2)
    }
    for terminal in terminals:
        for candidate in range(len(sites)):
            if candidate != terminal:
                allowed.add((min(terminal, candidate), max(terminal, candidate)))
    for i, source in enumerate(sites):
        nearest = sorted(
            ((source.distance_to(target), j) for j, target in enumerate(sites) if j != i),
            key=lambda item: item[0],
        )
        for _, j in nearest[:limit]:
            allowed.add((min(i, j), max(i, j)))
    return sorted(allowed)


class RouteOptimizer:
    def __init__(
        self, terrain: TerrainSource, rf_settings: RFSettings, candidate_settings: CandidateSettings
    ) -> None:
        self.terrain = terrain
        self.rf_settings = rf_settings
        self.candidate_settings = candidate_settings
        self.evaluator = LinkEvaluator(terrain, rf_settings)

    def _progressively_validate_link(self, source: Site, target: Site) -> LinkResult | None:
        """Certify at final resolution after lossless budget/distance screening.

        Coarser terrain is not a bound on diffraction: a coarse rejection cannot
        safely exclude a final-valid link.
        """
        maximum_distance = self.candidate_settings.maximum_link_distance_m
        if maximum_distance is not None and source.distance_to(target) > maximum_distance:
            return None
        if self.evaluator.optimistic_margin_db(source, target) < 0.0:
            return None
        try:
            link = self.evaluator.evaluate(
                source, target, self.candidate_settings.final_sample_step_m
            )
        except ValueError:
            return None
        return link if link.valid else None

    def _find_low_hop_route(
        self,
        sites: list[Site],
        notify: ProgressCallback,
        is_cancelled: CancelCallback,
    ) -> tuple[list[Site], list[LinkResult]] | None:
        """Exhaustively check final-valid routes with at most two repeaters.

        Nearest-neighbor graphs are useful for local connectivity, but they can
        never prove a minimum-repeater result: the important summit-to-summit
        edge is often deliberately long.  Endpoint reachability keeps this
        exact low-hop search small on obstructed terrain.
        """
        if len(sites) < 2:
            return None
        if is_cancelled():
            return None
        endpoint_a, endpoint_b = sites[0], sites[-1]
        direct = self._progressively_validate_link(endpoint_a, endpoint_b)
        if direct is not None:
            return [endpoint_a, endpoint_b], [direct]

        candidates = sites[1:-1]
        endpoint_links: list[LinkResult] = []
        from_a: dict[str, Site] = {}
        to_b: dict[str, Site] = {}
        notify("Checking endpoint-visible candidates", 0, len(candidates))
        for number, candidate in enumerate(candidates, 1):
            if is_cancelled():
                return None
            left_link = self._progressively_validate_link(endpoint_a, candidate)
            if left_link is not None:
                endpoint_links.append(left_link)
                from_a[candidate.id] = candidate
            right_link = self._progressively_validate_link(candidate, endpoint_b)
            if right_link is not None:
                endpoint_links.append(right_link)
                to_b[candidate.id] = candidate
            if number % 10 == 0 or number == len(candidates):
                notify("Checking endpoint-visible candidates", number, len(candidates))

        endpoint_graph = build_graph(sites, endpoint_links)
        path_ids = select_path(
            endpoint_graph,
            endpoint_a.id,
            endpoint_b.id,
            self.candidate_settings.priority,
        )
        if path_ids:
            route = [endpoint_graph.nodes[site_id]["site"] for site_id in path_ids]
            route_links = [
                endpoint_graph.edges[source_id, target_id]["link"]
                for source_id, target_id in zip(path_ids, path_ids[1:], strict=False)
            ]
            return route, route_links

        frontier_pairs: list[tuple[Site, Site]] = []
        seen_pairs: set[tuple[str, str]] = set()
        for left_site in from_a.values():
            for right_site in to_b.values():
                if left_site.id == right_site.id:
                    continue
                key = (
                    (left_site.id, right_site.id)
                    if left_site.id < right_site.id
                    else (right_site.id, left_site.id)
                )
                if key in seen_pairs:
                    continue
                seen_pairs.add(key)
                frontier_pairs.append((left_site, right_site))

        cross_links: list[LinkResult] = []
        notify("Checking two-repeater summit links", 0, len(frontier_pairs))
        for number, (left_site, right_site) in enumerate(frontier_pairs, 1):
            if is_cancelled():
                return None
            link = self._progressively_validate_link(left_site, right_site)
            if link is not None:
                cross_links.append(link)
            if number % 10 == 0 or number == len(frontier_pairs):
                notify("Checking two-repeater summit links", number, len(frontier_pairs))

        final_links = [*endpoint_links, *cross_links]
        graph = build_graph(sites, final_links)
        path_ids = select_path(
            graph, endpoint_a.id, endpoint_b.id, self.candidate_settings.priority
        )
        if not path_ids or len(path_ids) > 4:
            return None
        route = [graph.nodes[site_id]["site"] for site_id in path_ids]
        route_links = [
            graph.edges[source_id, target_id]["link"]
            for source_id, target_id in zip(path_ids, path_ids[1:], strict=False)
        ]
        return route, route_links

    def optimize(
        self,
        endpoint_a: Site,
        endpoint_b: Site,
        exclusions: list[list[tuple[float, float]]] | None = None,
        progress: ProgressCallback | None = None,
        cancelled: CancelCallback | None = None,
        candidates: list[Site] | None = None,
        clients: list[Site] | None = None,
        required_routers: list[Site] | None = None,
    ) -> OptimizationResult:
        started = time.perf_counter()
        notify = progress or (lambda _stage, _done, _total: None)
        is_cancelled = cancelled or (lambda: False)
        if is_cancelled():
            return OptimizationResult([], [], [], [], ["Optimization cancelled"])
        if any(site.required and not site.enabled for site in (required_routers or [])):
            raise ValueError("Required routers cannot be disabled")
        notify("Generating candidates", 0, 1)
        network_clients = clients or [endpoint_a, endpoint_b]
        mandatory = [site for site in (required_routers or []) if site.enabled]
        network_mode = len(network_clients) > 2 or bool(mandatory)
        if candidates is not None:
            sites = candidates
        elif network_mode:
            min_x = min(site.x for site in [*network_clients, *mandatory])
            min_y = min(site.y for site in [*network_clients, *mandatory])
            max_x = max(site.x for site in [*network_clients, *mandatory])
            max_y = max(site.y for site in [*network_clients, *mandatory])
            anchor_a = replace(network_clients[0], x=min_x, y=min_y)
            anchor_b = replace(network_clients[-1], x=max_x, y=max_y)
            candidate_settings = replace(self.candidate_settings, unrestricted_bounding_area=True)
            generated = generate_candidates(
                self.terrain, anchor_a, anchor_b, candidate_settings, exclusions or []
            )[1:-1]
            occupied = {site.id for site in [*network_clients, *mandatory]}
            for index, site in enumerate(generated, 1):
                site.id = f"N{index}"
                while site.id in occupied:
                    site.id = "N" + site.id
                occupied.add(site.id)
            sites = [*network_clients, *mandatory, *generated]
        else:
            sites = generate_candidates(
                self.terrain, endpoint_a, endpoint_b, self.candidate_settings, exclusions or []
            )
        for site in sites:
            if site.kind not in {
                SiteKind.ENDPOINT_A,
                SiteKind.ENDPOINT_B,
                SiteKind.CLIENT,
            }:
                site.antenna_height_m = (
                    self.candidate_settings.maximum_router_height_m
                    if self.candidate_settings.optimize_heights
                    else self.rf_settings.router.height_agl_m
                )
        candidate_count = sum(site.kind == SiteKind.CANDIDATE for site in sites)
        notify(f"{candidate_count} candidate sites", 1, 1)
        plausible: list[tuple[Site, Site]] = []
        neighbor_limit = max(1, self.candidate_settings.maximum_neighbors_per_site)
        terminal_indices = {
            index
            for index, site in enumerate(sites)
            if site.kind in {SiteKind.ENDPOINT_A, SiteKind.ENDPOINT_B, SiteKind.CLIENT}
            or site.required
        }
        allowed_pairs = screening_pair_indices(sites, neighbor_limit, terminal_indices)
        total_pairs = len(allowed_pairs)
        notify("Screening possible links", 0, total_pairs)
        maximum_distance = self.candidate_settings.maximum_link_distance_m
        for pair_number, (i, j) in enumerate(allowed_pairs, 1):
            if is_cancelled():
                return OptimizationResult([], [], sites, [], ["Optimization cancelled"])
            source, target = sites[i], sites[j]
            distance = source.distance_to(target)
            if maximum_distance is not None and distance > maximum_distance:
                continue
            if self.evaluator.optimistic_margin_db(source, target) >= 0.0:
                plausible.append((source, target))
            if pair_number % 20 == 0 or pair_number == total_pairs:
                notify("Screening possible links", pair_number, total_pairs)

        links: list[LinkResult] = []
        failures: list[LinkResult] = []
        notify("Evaluating RF links", 0, len(plausible))
        evaluated = evaluate_link_pairs(
            self.terrain,
            self.rf_settings,
            plausible,
            self.candidate_settings.coarse_sample_step_m,
            workers=self.candidate_settings.parallel_workers,
            progress=lambda done, total: notify("Evaluating RF links", done, total),
            cancelled=is_cancelled,
        )
        if is_cancelled():
            return OptimizationResult([], [], sites, links, ["Optimization cancelled"])
        links.extend(link for link in evaluated if link.valid)
        failures.extend(link for link in evaluated if not link.valid)
        link_by_key = {frozenset((link.source_id, link.target_id)): link for link in links}
        validated_keys: set[frozenset[str]] = set()
        by_id = {site.id: site for site in sites}
        # Coarse failures can be aliasing/model-resolution effects, not proofs.
        for failed in failures:
            if is_cancelled():
                return OptimizationResult([], [], sites, [], ["Optimization cancelled"])
            key = frozenset((failed.source_id, failed.target_id))
            final = self._progressively_validate_link(
                by_id[failed.source_id], by_id[failed.target_id]
            )
            validated_keys.add(key)
            if final is not None:
                link_by_key[key] = final
        if (
            not network_mode
            and self.candidate_settings.priority == OptimizationPriority.MINIMUM_ROUTERS
        ):
            ordered = [
                endpoint_a,
                *(
                    site
                    for site in sites
                    if site.id not in {endpoint_a.id, endpoint_b.id} and site.enabled
                ),
                endpoint_b,
            ]
            low_hop = self._find_low_hop_route(ordered, notify, is_cancelled)
            if (
                low_hop is not None
                and len(low_hop[0]) - 2 <= self.candidate_settings.maximum_solution_routers
            ):
                for link in low_hop[1]:
                    key = frozenset((link.source_id, link.target_id))
                    link_by_key[key] = link
                    validated_keys.add(key)
        alternatives: list[NetworkSolution] = []
        # Every nonterminal round certifies at least one previously unseen pair.
        # This terminates on a finite pool without returning coarse-only edges.
        while True:
            if is_cancelled():
                return OptimizationResult([], [], sites, [], ["Optimization cancelled"])
            alternatives = solve_topologies(
                sites,
                list(link_by_key.values()),
                self.candidate_settings,
                cancelled=is_cancelled,
            )
            if not alternatives:
                break
            # The coarse graph is deliberately sparse.  Once a subset has been
            # selected, test every node pair so the displayed mesh contains all
            # RF links that can actually communicate, not only search edges.
            selected_edges = {
                frozenset((left.id, right.id))
                for solution in alternatives
                for left, right in itertools.combinations(solution.sites, 2)
            }
            pending = sorted(
                selected_edges - validated_keys,
                key=lambda key: tuple(sorted(key)),
            )
            if not pending:
                break
            notify("Validating solution alternatives", 0, len(pending))
            by_id = {site.id: site for site in sites}
            for number, key in enumerate(pending, 1):
                if is_cancelled():
                    return OptimizationResult([], [], sites, [], ["Optimization cancelled"])
                left_id, right_id = sorted(key)
                final = None
                try:
                    left, right = by_id[left_id], by_id[right_id]
                    too_far = (
                        maximum_distance is not None and left.distance_to(right) > maximum_distance
                    )
                    if not too_far and self.evaluator.optimistic_margin_db(left, right) >= 0:
                        candidate = self.evaluator.evaluate(
                            left,
                            right,
                            self.candidate_settings.final_sample_step_m,
                        )
                        final = candidate if candidate.valid else None
                except ValueError:
                    final = None
                validated_keys.add(key)
                if final is None:
                    link_by_key.pop(key, None)
                else:
                    link_by_key[key] = final
                notify("Validating solution alternatives", number, len(pending))
        if is_cancelled():
            return OptimizationResult([], [], sites, [], ["Optimization cancelled"])
        alternatives = solve_topologies(
            sites,
            list(link_by_key.values()),
            self.candidate_settings,
            cancelled=is_cancelled,
        )
        if is_cancelled():
            return OptimizationResult([], [], sites, [], ["Optimization cancelled"])
        if alternatives:
            active_index = select_active_solution_index(
                alternatives, self.candidate_settings.priority
            )
            active = alternatives[active_index]
            logger.info(
                "Selected %s with %d independent path(s) in %.2fs",
                active.name,
                active.achieved_path_count,
                time.perf_counter() - started,
            )
            return OptimizationResult(
                active.primary_route,
                list(active.links),
                sites,
                list(link_by_key.values()),
                list(active.diagnostics),
                time.perf_counter() - started,
                alternatives,
                active_index,
            )
        if (
            not network_mode
            and self.candidate_settings.priority != OptimizationPriority.MAXIMUM_RELIABILITY
        ):
            notify("Checking long summit alternatives", 0, 1)
            low_hop = self._find_low_hop_route(sites, notify, is_cancelled)
            if is_cancelled():
                return OptimizationResult([], [], sites, [], ["Optimization cancelled"])
            if low_hop is not None:
                fallback_route, fallback_links = low_hop
                router_ids = [site.id for site in fallback_route[1:-1]]
                if len(router_ids) <= self.candidate_settings.maximum_solution_routers:
                    solution = NetworkSolution(
                        name=f"{len(router_ids)} router" + ("s" if len(router_ids) != 1 else ""),
                        sites=list(fallback_route),
                        links=list(fallback_links),
                        client_ids=[endpoint_a.id, endpoint_b.id],
                        router_ids=router_ids,
                        client_paths={
                            (endpoint_a.id, endpoint_b.id): [[site.id for site in fallback_route]]
                        },
                        requested_path_count=1,
                        achieved_path_count=1,
                        diagnostics=[
                            "Long summit links were used after the local mesh graph was disconnected."
                        ],
                    )
                    return OptimizationResult(
                        fallback_route,
                        list(fallback_links),
                        sites,
                        [*link_by_key.values(), *fallback_links],
                        list(solution.diagnostics),
                        time.perf_counter() - started,
                        [solution],
                        0,
                    )
        required_names = ", ".join(site.id for site in mandatory)
        diagnostics = ["No connected mesh was found within the configured router-count limit."]
        if required_names:
            diagnostics.append(f"Required routers checked: {required_names}")
        diagnostics.append(
            "Try showing more router-count solutions, widening the corridor, or increasing candidate density."
        )
        return OptimizationResult(
            [],
            [],
            sites,
            list(link_by_key.values()),
            diagnostics,
            time.perf_counter() - started,
        )

    def _refine_site(self, previous: Site, site: Site, following: Site) -> Site:
        radius = self.candidate_settings.refine_radius_m
        step = max(self.candidate_settings.refine_step_m, self.terrain.resolution_m)
        offsets = np.arange(-radius, radius + step * 0.5, step)
        best = site
        best_score = (-float("inf"), -float("inf"))
        for dx in offsets:
            for dy in offsets:
                if dx * dx + dy * dy > radius * radius:
                    continue
                x, y = site.x + float(dx), site.y + float(dy)
                ground = float(self.terrain.sample(np.array([x]), np.array([y]))[0])
                if not np.isfinite(ground):
                    continue
                surface = None
                if self.terrain.has_surface:
                    sampled = float(
                        self.terrain.sample(np.array([x]), np.array([y]), surface=True)[0]
                    )
                    surface = sampled if np.isfinite(sampled) else None
                trial = replace(
                    site, x=x, y=y, ground_elevation_m=ground, surface_elevation_m=surface
                )
                try:
                    left = self.evaluator.evaluate(
                        previous, trial, self.candidate_settings.medium_sample_step_m
                    )
                    right = self.evaluator.evaluate(
                        trial, following, self.candidate_settings.medium_sample_step_m
                    )
                except ValueError:
                    continue
                if not left.valid or not right.valid:
                    continue
                score = (
                    min(left.worst_margin_db, right.worst_margin_db),
                    min(
                        left.minimum_fresnel_clearance_ratio, right.minimum_fresnel_clearance_ratio
                    ),
                )
                if score > best_score:
                    best, best_score = trial, score
        return best

    def _reduce_mast_heights(self, route: list[Site]) -> None:
        settings = self.candidate_settings
        heights = np.arange(
            settings.minimum_router_height_m,
            settings.maximum_router_height_m + settings.router_height_step_m * 0.5,
            settings.router_height_step_m,
        )
        for index in range(1, len(route) - 1):
            site = route[index]
            original = site.antenna_height_m
            for height in heights:
                site.antenna_height_m = float(height)
                left = self.evaluator.evaluate(route[index - 1], site, settings.final_sample_step_m)
                right = self.evaluator.evaluate(
                    site, route[index + 1], settings.final_sample_step_m
                )
                if left.valid and right.valid:
                    break
            else:
                site.antenna_height_m = original
