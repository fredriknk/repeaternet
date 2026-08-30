from __future__ import annotations

import logging
import time
from collections.abc import Callable
from dataclasses import dataclass, field, replace

import numpy as np

from rf_router_planner.models.link import LinkResult
from rf_router_planner.models.settings import CandidateSettings, OptimizationPriority, RFSettings
from rf_router_planner.models.site import Site, SiteKind
from rf_router_planner.rf.propagation import LinkEvaluator
from rf_router_planner.terrain.candidate_sites import generate_candidates
from rf_router_planner.terrain.raster import TerrainSource

from .graph import build_graph, select_path

logger = logging.getLogger(__name__)


@dataclass(slots=True)
class OptimizationResult:
    route: list[Site]
    links: list[LinkResult]
    candidates: list[Site]
    all_valid_links: list[LinkResult]
    diagnostics: list[str] = field(default_factory=list)
    elapsed_seconds: float = 0.0

    @property
    def found(self) -> bool:
        return bool(self.route)

    @property
    def router_count(self) -> int:
        return max(0, len(self.route) - 2)


ProgressCallback = Callable[[str, int, int], None]
CancelCallback = Callable[[], bool]


def screening_pair_indices(sites: list[Site], neighbor_limit: int) -> list[tuple[int, int]]:
    """Return a bounded, spatially connected set of candidate link indices.

    A direct endpoint pair and every endpoint-to-candidate pair are retained.
    Intermediate sites also get their nearest neighbors.  Endpoint completeness
    matters because the geographically nearest or globally highest sites are
    not necessarily the ones visible from a low endpoint.
    """
    if len(sites) < 2:
        return []
    limit = max(1, neighbor_limit)
    last = len(sites) - 1
    allowed: set[tuple[int, int]] = {(0, last)}
    for candidate in range(1, last):
        allowed.add((0, candidate))
        allowed.add((candidate, last))
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
        """Return a final-resolution link after cheap-to-expensive validation."""
        maximum_distance = self.candidate_settings.maximum_link_distance_m
        if maximum_distance is not None and source.distance_to(target) > maximum_distance:
            return None
        if self.evaluator.optimistic_margin_db(source, target) < 0.0:
            return None
        link: LinkResult | None = None
        sample_steps = dict.fromkeys(
            (
                self.candidate_settings.coarse_sample_step_m,
                self.candidate_settings.medium_sample_step_m,
                self.candidate_settings.final_sample_step_m,
            )
        )
        try:
            for sample_step_m in sample_steps:
                link = self.evaluator.evaluate(source, target, sample_step_m)
                if not link.valid:
                    return None
        except ValueError:
            return None
        return link

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
    ) -> OptimizationResult:
        started = time.perf_counter()
        notify = progress or (lambda _stage, _done, _total: None)
        is_cancelled = cancelled or (lambda: False)
        notify("Generating candidates", 0, 1)
        sites = candidates or generate_candidates(
            self.terrain, endpoint_a, endpoint_b, self.candidate_settings, exclusions or []
        )
        for site in sites:
            if site.kind not in {SiteKind.ENDPOINT_A, SiteKind.ENDPOINT_B}:
                site.antenna_height_m = (
                    self.candidate_settings.maximum_router_height_m
                    if self.candidate_settings.optimize_heights
                    else self.rf_settings.router.height_agl_m
                )
        notify(f"{len(sites) - 2} candidate sites", 1, 1)
        if self.candidate_settings.priority != OptimizationPriority.MAXIMUM_RELIABILITY:
            low_hop = self._find_low_hop_route(sites, notify, is_cancelled)
            if is_cancelled():
                return OptimizationResult([], [], sites, [], ["Optimization cancelled"])
            if low_hop is not None:
                low_hop_route, validated_links = low_hop
                if self.candidate_settings.optimize_heights:
                    self._reduce_mast_heights(low_hop_route)
                for index, site in enumerate(low_hop_route[1:-1], 1):
                    site.kind = SiteKind.ROUTER
                    site.id = f"R{index}"
                low_hop_links = [
                    self.evaluator.evaluate(
                        source,
                        target,
                        self.candidate_settings.final_sample_step_m,
                    )
                    for source, target in zip(
                        low_hop_route, low_hop_route[1:], strict=False
                    )
                ]
                if all(link.valid for link in low_hop_links):
                    logger.info(
                        "Selected exact low-hop route %s in %.2fs",
                        " -> ".join(site.id for site in low_hop_route),
                        time.perf_counter() - started,
                    )
                    return OptimizationResult(
                        low_hop_route,
                        low_hop_links,
                        sites,
                        validated_links,
                        [],
                        time.perf_counter() - started,
                    )
        plausible: list[tuple[Site, Site]] = []
        neighbor_limit = max(1, self.candidate_settings.maximum_neighbors_per_site)
        allowed_pairs = screening_pair_indices(sites, neighbor_limit)
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
        for index, (source, target) in enumerate(plausible, 1):
            if is_cancelled():
                return OptimizationResult([], [], sites, links, ["Optimization cancelled"])
            try:
                link = self.evaluator.evaluate(
                    source, target, self.candidate_settings.coarse_sample_step_m
                )
            except ValueError:
                continue
            (links if link.valid else failures).append(link)
            if index % 20 == 0 or index == len(plausible):
                notify("Evaluating RF links", index, len(plausible))
        notify("Building graph", len(links), len(plausible))
        graph = build_graph(sites, links)
        notify("Finding minimum router path", 0, 1)
        path_ids = select_path(
            graph, endpoint_a.id, endpoint_b.id, self.candidate_settings.priority
        )
        # Medium and final validation both remove false-positive edges and
        # continue searching.  A single aliased coarse path must not turn into
        # a false "no route" result while alternatives remain in the graph.
        route: list[Site] | None = None
        route_links: list[LinkResult] = []
        while path_ids:
            invalid_edges: list[tuple[str, str]] = []
            notify("Medium-resolution RF validation", 0, len(path_ids) - 1)
            for number, (source_id, target_id) in enumerate(
                zip(path_ids, path_ids[1:], strict=False), 1
            ):
                source = graph.nodes[source_id]["site"]
                target = graph.nodes[target_id]["site"]
                medium = self.evaluator.evaluate(
                    source, target, self.candidate_settings.medium_sample_step_m
                )
                if not medium.valid:
                    invalid_edges.append((source_id, target_id))
                else:
                    graph.edges[source_id, target_id]["link"] = medium
                notify("Medium-resolution RF validation", number, len(path_ids) - 1)
            if not invalid_edges:
                base_route = [graph.nodes[site_id]["site"] for site_id in path_ids]
                base_link_results: list[LinkResult | None] = []
                notify("Final-resolution RF validation", 0, len(base_route) - 1)
                for number, (source, target) in enumerate(
                    zip(base_route, base_route[1:], strict=False), 1
                ):
                    try:
                        final_link = self.evaluator.evaluate(
                            source,
                            target,
                            self.candidate_settings.final_sample_step_m,
                        )
                    except ValueError:
                        final_link = None
                    base_link_results.append(final_link)
                    notify("Final-resolution RF validation", number, len(base_route) - 1)
                if all(
                    link is not None and link.valid for link in base_link_results
                ):
                    route = base_route
                    route_links = [
                        link for link in base_link_results if link is not None
                    ]
                    break

                # A medium-valid seed can still be rescued by the configured
                # local search.  Work on copies so rejected attempts do not
                # mutate graph candidates or mast heights.
                refined_route = [
                    base_route[0],
                    *(replace(site) for site in base_route[1:-1]),
                    base_route[-1],
                ]
                notify("Refining router locations", 0, max(1, len(refined_route) - 2))
                for index in range(1, len(refined_route) - 1):
                    refined_route[index] = self._refine_site(
                        refined_route[index - 1],
                        refined_route[index],
                        refined_route[index + 1],
                    )
                    notify("Refining router locations", index, len(refined_route) - 2)
                if self.candidate_settings.optimize_heights:
                    self._reduce_mast_heights(refined_route)
                refined_links = [
                    self.evaluator.evaluate(
                        source,
                        target,
                        self.candidate_settings.final_sample_step_m,
                    )
                    for source, target in zip(
                        refined_route, refined_route[1:], strict=False
                    )
                ]
                if all(link.valid for link in refined_links):
                    route, route_links = refined_route, refined_links
                    break

                invalid_edges = [
                    edge
                    for edge, link in zip(
                        zip(path_ids, path_ids[1:], strict=False),
                        base_link_results,
                        strict=True,
                    )
                    if link is None or not link.valid
                ]
            graph.remove_edges_from(invalid_edges)
            path_ids = select_path(
                graph, endpoint_a.id, endpoint_b.id, self.candidate_settings.priority
            )
        if route is None:
            diagnostics = ["No valid route found with the current constraints."]
            if failures:
                nearest = max(failures, key=lambda item: item.worst_margin_db)
                diagnostics.extend(
                    [
                        f"Best near-valid link: {nearest.source_id} → {nearest.target_id}",
                        f"Predicted usable margin: {nearest.worst_margin_db:.1f} dB",
                        "Try increasing router height or candidate density, widening the corridor, or reducing the fade margin.",
                    ]
                )
            return OptimizationResult(
                [], [], sites, links, diagnostics, time.perf_counter() - started
            )
        for index, site in enumerate(route[1:-1], 1):
            site.kind = SiteKind.ROUTER
            site.id = f"R{index}"
        # Re-evaluate route links after stable router IDs and at final resolution.
        notify("Validating final solution", 0, len(route) - 1)
        route_links = []
        for index, (source, target) in enumerate(zip(route, route[1:], strict=False), 1):
            route_links.append(
                self.evaluator.evaluate(source, target, self.candidate_settings.final_sample_step_m)
            )
            notify("Validating final solution", index, len(route) - 1)
        if not all(link.valid for link in route_links):
            return OptimizationResult(
                [],
                route_links,
                sites,
                links,
                ["The selected route changed during finalization and is no longer valid."],
                time.perf_counter() - started,
            )
        logger.info(
            "Selected route %s in %.2fs (%d valid edges)",
            " -> ".join(site.id for site in route),
            time.perf_counter() - started,
            len(links),
        )
        return OptimizationResult(
            route, route_links, sites, links, [], time.perf_counter() - started
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
                left = self.evaluator.evaluate(
                    route[index - 1], site, settings.final_sample_step_m
                )
                right = self.evaluator.evaluate(
                    site, route[index + 1], settings.final_sample_step_m
                )
                if left.valid and right.valid:
                    break
            else:
                site.antenna_height_m = original
