import itertools
import random
import tracemalloc
import weakref
from types import SimpleNamespace

import numpy as np
import pytest

from rf_router_planner.models.link import DirectionResult, LinkResult
from rf_router_planner.models.settings import CandidateSettings, OptimizationPriority, RFSettings
from rf_router_planner.models.site import Site, SiteKind
from rf_router_planner.optimization.graph import build_graph, select_path
from rf_router_planner.optimization.optimizer import RouteOptimizer, screening_pair_indices
from rf_router_planner.terrain.raster import ArrayTerrain


class ChainOnlyEvaluator:
    def __init__(self, valid_edges: set[frozenset[tuple[float, float]]]) -> None:
        self.valid_edges = valid_edges

    @staticmethod
    def _edge(source: Site, target: Site) -> frozenset[tuple[float, float]]:
        return frozenset(((source.x, source.y), (target.x, target.y)))

    def optimistic_margin_db(self, source: Site, target: Site) -> float:
        return 20.0 if self._edge(source, target) in self.valid_edges else -20.0

    @staticmethod
    def _result(source: Site, target: Site, valid: bool, margin: float) -> LinkResult:
        distance = source.distance_to(target)
        forward = DirectionResult(
            source.id,
            target.id,
            distance,
            0,
            0,
            0,
            0,
            0,
            0,
            -100,
            -130,
            margin,
            margin,
            0,
            0,
            valid,
        )
        reverse = DirectionResult(
            target.id,
            source.id,
            distance,
            0,
            0,
            0,
            0,
            0,
            0,
            -100,
            -130,
            margin,
            margin,
            0,
            0,
            valid,
        )
        return LinkResult(
            source.id,
            target.id,
            distance,
            forward,
            reverse,
            valid,
            valid,
            valid,
            10,
            1,
            distance / 2,
            1,
            0,
        )

    def evaluate(self, source: Site, target: Site, _sample_step_m: float) -> LinkResult:
        valid = self._edge(source, target) in self.valid_edges
        return self._result(source, target, valid, 20.0 if valid else -20.0)


class ResolutionAwareEvaluator(ChainOnlyEvaluator):
    def __init__(
        self,
        edge_margins: dict[frozenset[tuple[float, float]], float],
        final_invalid_edges: set[frozenset[tuple[float, float]]],
        final_sample_step_m: float,
    ) -> None:
        super().__init__(set(edge_margins))
        self.edge_margins = edge_margins
        self.final_invalid_edges = final_invalid_edges
        self.final_sample_step_m = final_sample_step_m
        self.calls: list[tuple[frozenset[tuple[float, float]], float]] = []

    def evaluate(self, source: Site, target: Site, sample_step_m: float) -> LinkResult:
        edge = self._edge(source, target)
        self.calls.append((edge, sample_step_m))
        valid = edge in self.edge_margins and not (
            sample_step_m == self.final_sample_step_m and edge in self.final_invalid_edges
        )
        return self._result(source, target, valid, self.edge_margins.get(edge, -20.0))


def test_screening_pairs_are_bounded_and_keep_direct_endpoint_link() -> None:
    sites = [Site("A", 0, 0, kind=SiteKind.ENDPOINT_A)]
    sites.extend(
        Site(f"C{index}", index * 1000, (index % 5) * 1000, site_quality=index / 100)
        for index in range(1, 101)
    )
    sites.append(Site("B", 102_000, 0, kind=SiteKind.ENDPOINT_B))
    pairs = screening_pair_indices(sites, 8)
    assert (0, len(sites) - 1) in pairs
    endpoint_pairs = 2 * (len(sites) - 2) + 1
    assert len(pairs) <= len(sites) * 8 + endpoint_pairs
    assert len(pairs) < len(sites) * (len(sites) - 1) // 5


def test_all_candidates_are_considered_from_both_endpoints() -> None:
    sites = [Site("A", 0, 0, kind=SiteKind.ENDPOINT_A)]
    sites.extend(Site(f"C{i}", i, 0, site_quality=float(i)) for i in range(1, 20))
    sites.append(Site("B", 20, 0, kind=SiteKind.ENDPOINT_B))
    pairs = screening_pair_indices(sites, 3)
    best = len(sites) - 2
    assert (0, best) in pairs
    assert (best, len(sites) - 1) in pairs


def test_spatial_neighbor_index_matches_exhaustive_distance_ordering() -> None:
    rng = random.Random(20260927)
    sites = [
        Site("A", 0, 0, kind=SiteKind.ENDPOINT_A),
        Site("duplicate-1", 100, 100),
        Site("duplicate-2", 100, 100),
        Site("north", 100, 200),
        Site("south", 100, 0),
        Site("east", 200, 100),
        Site("west", 0, 100),
    ]
    sites.extend(
        Site(f"random-{index}", rng.uniform(-1000, 1000), rng.uniform(-1000, 1000))
        for index in range(80)
    )
    sites.append(Site("B", 2000, 0, kind=SiteKind.ENDPOINT_B))

    for neighbor_limit in (1, 2, 5, 12, len(sites) - 1):
        for terminals in (None, set(), {0, 17, len(sites) - 1}):
            allowed = {
                (min(left, right), max(left, right))
                for left, right in itertools.combinations(
                    sorted(terminals or {0, len(sites) - 1}), 2
                )
            }
            active_terminals = terminals or {0, len(sites) - 1}
            for terminal in active_terminals:
                for candidate in range(len(sites)):
                    if candidate != terminal:
                        allowed.add((min(terminal, candidate), max(terminal, candidate)))
            for i, source in enumerate(sites):
                nearest = sorted(
                    (source.distance_to(target), j) for j, target in enumerate(sites) if j != i
                )
                for _, j in nearest[:neighbor_limit]:
                    allowed.add((min(i, j), max(i, j)))

            assert screening_pair_indices(sites, neighbor_limit, terminals) == sorted(allowed)


def test_optimizer_expands_beyond_nearest_neighbors_for_two_router_chain() -> None:
    endpoint_a = Site("A", 0, 0, kind=SiteKind.ENDPOINT_A)
    left_router = Site("left-router", 30, 0)
    right_router = Site("right-router", 70, 0)
    endpoint_b = Site("B", 100, 0, kind=SiteKind.ENDPOINT_B)
    sites = [
        endpoint_a,
        Site("near-a", 1, 0),
        left_router,
        Site("near-left", 30, 1),
        Site("quality-decoy", 50, 20, site_quality=100),
        Site("near-right", 70, 1),
        right_router,
        Site("near-b", 99, 0),
        endpoint_b,
    ]
    initial_pairs = set(screening_pair_indices(sites, neighbor_limit=1))
    left_index = sites.index(left_router)
    right_index = sites.index(right_router)

    def outside_nearest_neighbor_cap(source: Site, target: Site) -> bool:
        nearest = min(
            (candidate for candidate in sites if candidate is not source),
            key=source.distance_to,
        )
        reverse_nearest = min(
            (candidate for candidate in sites if candidate is not target),
            key=target.distance_to,
        )
        return nearest is not target and reverse_nearest is not source

    assert outside_nearest_neighbor_cap(endpoint_a, left_router)
    assert outside_nearest_neighbor_cap(left_router, right_router)
    assert outside_nearest_neighbor_cap(right_router, endpoint_b)
    assert (0, left_index) in initial_pairs
    assert (left_index, right_index) not in initial_pairs
    assert (right_index, len(sites) - 1) in initial_pairs

    valid_edges = {
        frozenset(((endpoint_a.x, endpoint_a.y), (left_router.x, left_router.y))),
        frozenset(((left_router.x, left_router.y), (right_router.x, right_router.y))),
        frozenset(((right_router.x, right_router.y), (endpoint_b.x, endpoint_b.y))),
    }
    settings = CandidateSettings(
        maximum_neighbors_per_site=1,
        refine_radius_m=0,
        refine_step_m=1,
        coarse_sample_step_m=1,
        medium_sample_step_m=1,
        final_sample_step_m=1,
    )
    optimizer = RouteOptimizer(
        ArrayTerrain(np.zeros((21, 101)), resolution_m=1), RFSettings(), settings
    )
    optimizer.evaluator = ChainOnlyEvaluator(valid_edges)  # type: ignore[assignment]

    result = optimizer.optimize(endpoint_a, endpoint_b, candidates=sites)

    assert result.found
    assert result.router_count == 2
    assert [(site.x, site.y) for site in result.route] == [
        (0, 0),
        (30, 0),
        (70, 0),
        (100, 0),
    ]


def test_optimizer_retries_after_final_resolution_invalidates_selected_path() -> None:
    endpoint_a = Site("A", 0, 0, kind=SiteKind.ENDPOINT_A)
    unreliable = Site("unreliable", 50, 0)
    reliable = Site("reliable", 50, 10)
    endpoint_b = Site("B", 100, 0, kind=SiteKind.ENDPOINT_B)
    sites = [endpoint_a, unreliable, reliable, endpoint_b]

    edge = ChainOnlyEvaluator._edge
    unreliable_right = edge(unreliable, endpoint_b)
    edge_margins = {
        edge(endpoint_a, unreliable): 30.0,
        unreliable_right: 30.0,
        edge(endpoint_a, reliable): 10.0,
        edge(reliable, endpoint_b): 10.0,
    }
    evaluator = ResolutionAwareEvaluator(
        edge_margins,
        final_invalid_edges={unreliable_right},
        final_sample_step_m=1,
    )
    settings = CandidateSettings(
        maximum_neighbors_per_site=3,
        coarse_sample_step_m=100,
        medium_sample_step_m=10,
        final_sample_step_m=1,
        refine_radius_m=0,
        refine_step_m=1,
        priority=OptimizationPriority.MAXIMUM_RELIABILITY,
    )
    optimizer = RouteOptimizer(
        ArrayTerrain(np.zeros((11, 101)), resolution_m=1), RFSettings(), settings
    )
    optimizer.evaluator = evaluator  # type: ignore[assignment]

    result = optimizer.optimize(endpoint_a, endpoint_b, candidates=sites)

    assert (unreliable_right, settings.final_sample_step_m) in evaluator.calls
    assert result.found
    assert result.router_count == 1
    assert [(site.x, site.y) for site in result.route] == [(0, 0), (50, 10), (100, 0)]


def mock_batch(monkeypatch, evaluator):
    def evaluate(_terrain, _rf, pairs, step, **_kwargs):
        return [evaluator.evaluate(a, b, step) for a, b in pairs]

    monkeypatch.setattr("rf_router_planner.optimization.optimizer.evaluate_link_pairs", evaluate)


def test_low_hop_search_beats_an_existing_sparse_three_router_route(monkeypatch):
    a, b = Site("A", 0, 0, kind=SiteKind.ENDPOINT_A), Site("B", 100, 0, kind=SiteKind.ENDPOINT_B)
    x, y, z = Site("X", 10, 0), Site("Y", 20, 0), Site("Z", 30, 0)
    left, right = Site("L", 0, 30), Site("R", 100, 30)
    sites = [a, x, y, z, left, Site("LD", 0, 31), right, Site("RD", 100, 31), b]
    edge = ChainOnlyEvaluator._edge
    valid = {
        edge(u, v)
        for u, v in [(a, x), (x, y), (y, z), (z, b), (a, left), (left, right), (right, b)]
    }
    assert (sites.index(left), sites.index(right)) not in screening_pair_indices(sites, 1)
    evaluator = ChainOnlyEvaluator(valid)
    mock_batch(monkeypatch, evaluator)
    optimizer = RouteOptimizer(
        ArrayTerrain(np.zeros((40, 110)), resolution_m=1),
        RFSettings(),
        CandidateSettings(maximum_neighbors_per_site=1, maximum_solution_routers=3),
    )
    optimizer.evaluator = evaluator
    result = optimizer.optimize(a, b, candidates=sites)
    assert result.router_count == 2
    assert [s.id for s in result.route] == ["A", "L", "R", "B"]


def test_validation_continues_beyond_old_round_cap(monkeypatch):
    a, b = Site("A", 0, 0, kind=SiteKind.ENDPOINT_A), Site("B", 100, 0, kind=SiteKind.ENDPOINT_B)
    routers = [Site(f"R{i:02}", 50, i + 1) for i in range(30)]
    edge = ChainOnlyEvaluator._edge
    margins = {edge(u, v): 100 - i for i, r in enumerate(routers) for u, v in [(a, r), (r, b)]}
    evaluator = ResolutionAwareEvaluator(margins, {edge(r, b) for r in routers[:-1]}, 1)
    mock_batch(monkeypatch, evaluator)
    optimizer = RouteOptimizer(
        ArrayTerrain(np.zeros((40, 110)), resolution_m=1),
        RFSettings(),
        CandidateSettings(
            maximum_solution_routers=1,
            final_sample_step_m=1,
            priority=OptimizationPriority.MAXIMUM_RELIABILITY,
        ),
    )
    optimizer.evaluator = evaluator
    result = optimizer.optimize(a, b, candidates=[a, *routers, b])
    assert result.found
    assert result.route[1].id == "R29"
    assert all(
        (
            edge(
                next(s for s in result.candidates if s.id == link.source_id),
                next(s for s in result.candidates if s.id == link.target_id),
            ),
            1,
        )
        in evaluator.calls
        for link in result.links
    )


def test_final_valid_link_is_not_rejected_by_coarse_classification():
    a, b = Site("A", 0, 0), Site("B", 100, 0)

    class CoarseRejects(ChainOnlyEvaluator):
        def evaluate(self, source, target, step):
            return self._result(source, target, step == 1, 10)

    optimizer = RouteOptimizer(
        ArrayTerrain(np.zeros((3, 101)), resolution_m=1),
        RFSettings(),
        CandidateSettings(final_sample_step_m=1),
    )
    optimizer.evaluator = CoarseRejects({ChainOnlyEvaluator._edge(a, b)})
    assert optimizer._progressively_validate_link(a, b).valid


def test_cancellation_during_final_validation_returns_no_route(monkeypatch):
    a, b = Site("A", 0, 0, kind=SiteKind.ENDPOINT_A), Site("B", 100, 0, kind=SiteKind.ENDPOINT_B)
    evaluator = ChainOnlyEvaluator({ChainOnlyEvaluator._edge(a, b)})
    mock_batch(monkeypatch, evaluator)
    optimizer = RouteOptimizer(
        ArrayTerrain(np.zeros((3, 101)), resolution_m=1),
        RFSettings(),
        CandidateSettings(priority=OptimizationPriority.MAXIMUM_RELIABILITY),
    )
    optimizer.evaluator = evaluator
    stopped = False

    def progress(stage, done, total):
        nonlocal stopped
        if stage == "Validating solution alternatives":
            stopped = True

    result = optimizer.optimize(
        a, b, candidates=[a, b], progress=progress, cancelled=lambda: stopped
    )
    assert not result.found
    assert "cancelled" in result.diagnostics[0]


def test_pre_cancelled_search_skips_candidate_generation(monkeypatch):
    def fail(*args, **kwargs):
        raise AssertionError("Candidate generation should not run")

    monkeypatch.setattr("rf_router_planner.optimization.optimizer.generate_candidates", fail)
    optimizer = RouteOptimizer(
        ArrayTerrain(np.zeros((3, 101)), resolution_m=1), RFSettings(), CandidateSettings()
    )
    result = optimizer.optimize(Site("A", 0, 0), Site("B", 100, 0), cancelled=lambda: True)
    assert not result.found
    assert result.diagnostics == ["Optimization cancelled"]


def test_dense_low_hop_search_releases_profiles_and_matches_reference():
    a = Site("A", 0, 0, kind=SiteKind.ENDPOINT_A)
    b = Site("B", 100, 0, kind=SiteKind.ENDPOINT_B)
    left = [Site(f"L{i:02}", 25, i) for i in range(20)]
    right = [Site(f"R{i:02}", 75, i) for i in range(20)]
    pairs = [(a, s) for s in left] + [(s, b) for s in right]
    pairs += [(u, v) for u in left for v in right]
    live = weakref.WeakValueDictionary()
    peak = 0

    class WithProfiles(ChainOnlyEvaluator):
        def evaluate(self, source, target, step):
            nonlocal peak
            result = super().evaluate(source, target, step)
            samples = np.zeros(8192)
            live[id(samples)] = samples
            peak = max(peak, len(live))
            result.profile = SimpleNamespace(samples=samples)
            return result

    evaluator = WithProfiles({ChainOnlyEvaluator._edge(u, v) for u, v in pairs})
    sites = [a, *left, *right, b]
    optimizer = RouteOptimizer(
        ArrayTerrain(np.zeros((30, 110)), resolution_m=1), RFSettings(), CandidateSettings()
    )
    optimizer.evaluator = evaluator
    result = optimizer._find_low_hop_route(sites, lambda *_: None, lambda: False)
    reference = build_graph(sites, [ChainOnlyEvaluator._result(u, v, True, 20) for u, v in pairs])
    assert result is not None
    assert [s.id for s in result[0]] == select_path(
        reference, "A", "B", OptimizationPriority.MINIMUM_ROUTERS
    )
    assert peak <= 2  # Bound live payloads independently of hundreds of accepted links.
    assert not live
    assert all(link.profile is None for link in result[1])


def test_large_frontier_cancels_before_quadratic_pair_allocation():
    a = Site("A", 0, 0, kind=SiteKind.ENDPOINT_A)
    b = Site("B", 100, 0, kind=SiteKind.ENDPOINT_B)
    left = [Site(f"L{i}", 25, i) for i in range(400)]
    right = [Site(f"R{i}", 75, i) for i in range(400)]
    evaluator = ChainOnlyEvaluator(
        {
            ChainOnlyEvaluator._edge(u, v)
            for u, v in [(a, s) for s in left] + [(s, b) for s in right]
        }
    )
    optimizer = RouteOptimizer(
        ArrayTerrain(np.zeros((2, 2)), resolution_m=1), RFSettings(), CandidateSettings()
    )
    optimizer.evaluator = evaluator
    stopped = False

    def progress(stage, done, total):
        nonlocal stopped
        if stage == "Checking two-repeater summit links":
            assert total == 160_000
            stopped = True

    tracemalloc.start()
    try:
        result = optimizer._find_low_hop_route([a, *left, *right, b], progress, lambda: stopped)
        _, peak = tracemalloc.get_traced_memory()
    finally:
        tracemalloc.stop()
    assert result is None and stopped
    assert peak < 4 * 1024**2  # Generous graph allowance; old pair list/set exceeded 20 MiB.


@pytest.mark.parametrize("found", [False, True])
@pytest.mark.parametrize(
    "priority", [OptimizationPriority.MINIMUM_ROUTERS, OptimizationPriority.MINIMUM_INFRASTRUCTURE]
)
def test_fallback_reuses_low_hop_result_and_restores_profiles(monkeypatch, found, priority):
    a = Site("A", 0, 0, kind=SiteKind.ENDPOINT_A)
    b = Site("B", 100, 0, kind=SiteKind.ENDPOINT_B)
    optimizer = RouteOptimizer(
        ArrayTerrain(np.zeros((2, 101)), resolution_m=1),
        RFSettings(),
        CandidateSettings(priority=priority),
    )
    original = optimizer._find_low_hop_route
    calls = 0

    def low_hop(*args):
        nonlocal calls
        calls += 1
        return original(*args) if found else None

    monkeypatch.setattr(optimizer, "_find_low_hop_route", low_hop)
    monkeypatch.setattr(
        "rf_router_planner.optimization.optimizer.solve_topologies", lambda *a, **kw: []
    )
    result = optimizer.optimize(a, b, candidates=[a, b])
    assert calls == 1
    assert result.found == found
    if found:
        assert all(link.profile is not None for link in result.links)
        assert not result.active_solution.resilient  # Default requests two independent paths.


def test_solution_progress_publishes_certified_then_complete_route(monkeypatch):
    a = Site("A", 0, 0, kind=SiteKind.ENDPOINT_A)
    b = Site("B", 100, 0, kind=SiteKind.ENDPOINT_B)
    optimizer = RouteOptimizer(
        ArrayTerrain(np.zeros((2, 101)), resolution_m=1), RFSettings(), CandidateSettings()
    )
    edge = ChainOnlyEvaluator._edge(a, b)
    optimizer.evaluator = ChainOnlyEvaluator({edge})  # type: ignore[assignment]
    monkeypatch.setattr(
        "rf_router_planner.optimization.optimizer.solve_topologies", lambda *a, **kw: []
    )
    updates = []

    def on_progress(snapshot, complete):
        updates.append((complete, snapshot.search_complete, [site.id for site in snapshot.route]))
        if not complete:
            snapshot.route[0].id = "mutated-detached-preview"

    result = optimizer.optimize(
        a,
        b,
        candidates=[a, b],
        solution_progress=on_progress,
    )

    assert updates == [(False, False, ["A", "B"]), (True, True, ["A", "B"])]
    assert result.search_complete
    assert [site.id for site in result.route] == ["A", "B"]


def test_all_alternative_profiles_restored_and_preparation_cancellable():
    a = Site("A", 0, 0, kind=SiteKind.ENDPOINT_A)
    b = Site("B", 100, 0, kind=SiteKind.ENDPOINT_B)
    sites = [a, Site("R", 50, 1), b]
    optimizer = RouteOptimizer(
        ArrayTerrain(np.zeros((3, 101)), resolution_m=1), RFSettings(), CandidateSettings()
    )
    result = optimizer.optimize(a, b, candidates=sites)
    assert len(result.alternatives) >= 2
    assert all(
        link.profile is not None for solution in result.alternatives for link in solution.links
    )
    stopped = False

    def progress(stage, done, total):
        nonlocal stopped
        if stage == "Preparing solution profiles" and done == 1:
            stopped = True

    result = optimizer.optimize(
        a, b, candidates=sites, progress=progress, cancelled=lambda: stopped
    )
    assert not result.found
    assert result.diagnostics == ["Optimization cancelled"]


def test_optimizer_finds_long_summit_chain_beyond_two_routers(monkeypatch) -> None:
    """Long-range hops between summits must be screened and assembled.

    Each summit is surrounded by a dense cluster, so nearest-neighbor screening
    only yields intra-cluster edges and the exact low-hop pass stops at two
    routers.  The valid route needs five long summit-to-summit hops.
    """
    rng = random.Random(20261118)
    endpoint_a = Site("A", 0, 100, kind=SiteKind.ENDPOINT_A)
    endpoint_b = Site("B", 6000, 100, kind=SiteKind.ENDPOINT_B)
    summits = [Site(f"S{n}", 1000.0 * n, 100, site_quality=1.0) for n in range(1, 6)]
    clutter = [
        Site(
            f"S{n}-near-{k}",
            1000.0 * n + rng.uniform(-90, 90),
            100 + rng.uniform(-90, 90),
            site_quality=rng.uniform(0.0, 0.5),
        )
        for n in range(1, 6)
        for k in range(20)
    ]
    sites = [endpoint_a, *summits, *clutter, endpoint_b]
    chain = [endpoint_a, *summits, endpoint_b]
    valid_edges = {
        frozenset(((left.x, left.y), (right.x, right.y)))
        for left, right in zip(chain, chain[1:], strict=False)
    }
    evaluator = ChainOnlyEvaluator(valid_edges)

    def fake_evaluate_pairs(_terrain, _rf, pairs, step, **_kwargs):
        return [evaluator.evaluate(source, target, step) for source, target in pairs]

    monkeypatch.setattr(
        "rf_router_planner.optimization.optimizer.evaluate_link_pairs", fake_evaluate_pairs
    )
    settings = CandidateSettings(
        maximum_neighbors_per_site=16,
        maximum_solution_routers=6,
        reliability_paths=1,
        refine_radius_m=0,
        coarse_sample_step_m=1,
        medium_sample_step_m=1,
        final_sample_step_m=1,
    )
    optimizer = RouteOptimizer(
        ArrayTerrain(np.zeros((201, 6001)), resolution_m=1), RFSettings(), settings
    )
    optimizer.evaluator = evaluator  # type: ignore[assignment]

    result = optimizer.optimize(endpoint_a, endpoint_b, candidates=sites)

    assert result.found
    assert [site.id for site in result.route] == [site.id for site in chain]


@pytest.mark.parametrize("angle_deg", [0.0, 45.0, 120.0])
def test_backbone_keeps_spread_summits_in_any_corridor_orientation(angle_deg) -> None:
    from rf_router_planner.models.site import SiteOrigin
    from rf_router_planner.optimization.optimizer import backbone_site_indices

    rng = random.Random(7)
    cos, sin = np.cos(np.radians(angle_deg)), np.sin(np.radians(angle_deg))

    def place(site_id, along, across, quality, origin=SiteOrigin.OPTIMIZED):
        return Site(
            site_id,
            along * cos - across * sin,
            along * sin + across * cos,
            site_quality=quality,
            origin=origin,
        )

    sites = [place("A", 0, 0, 0.0)]
    # Dense, slightly higher-quality clutter near A plus many existing sites.
    sites += [place(f"c{k}", rng.uniform(0, 4000), rng.uniform(-500, 500), 0.9) for k in range(300)]
    sites += [
        place(f"k{k}", rng.uniform(0, 2000), rng.uniform(-200, 200), 0.95, SiteOrigin.KNOWN)
        for k in range(120)
    ]
    summits = [place(f"S{n}", 30_000.0 * n, 0, 0.8) for n in range(1, 6)]
    sites += summits
    sites += [
        place(f"f{k}", rng.uniform(0, 180_000), rng.uniform(-5000, 5000), 0.1) for k in range(300)
    ]
    sites.append(place("B", 180_000, 0, 0.0))

    selected = backbone_site_indices(sites, 64, {0, len(sites) - 1})
    chosen = {sites[index].id for index in selected}

    assert len(selected) <= 64
    assert {summit.id for summit in summits} <= chosen
    assert sum(sites[index].origin == SiteOrigin.KNOWN for index in selected) <= 16


def test_validation_explores_router_headroom_but_returns_only_within_limit(monkeypatch) -> None:
    import rf_router_planner.optimization.optimizer as optimizer_module

    real_solve = optimizer_module.solve_topologies
    limits: list[int] = []

    def recording_solve(sites, links, settings, *args, **kwargs):
        limits.append(settings.maximum_solution_routers)
        return real_solve(sites, links, settings, *args, **kwargs)

    monkeypatch.setattr(optimizer_module, "solve_topologies", recording_solve)
    endpoint_a = Site("A", 0, 100, kind=SiteKind.ENDPOINT_A)
    endpoint_b = Site("B", 5000, 100, kind=SiteKind.ENDPOINT_B)
    chain = [endpoint_a, *(Site(f"S{n}", 1000.0 * n, 100) for n in range(1, 5)), endpoint_b]
    evaluator = ChainOnlyEvaluator(
        {
            frozenset(((left.x, left.y), (right.x, right.y)))
            for left, right in zip(chain, chain[1:], strict=False)
        }
    )
    monkeypatch.setattr(
        optimizer_module,
        "evaluate_link_pairs",
        lambda _t, _rf, pairs, step, **_k: [evaluator.evaluate(a, b, step) for a, b in pairs],
    )
    settings = CandidateSettings(
        maximum_solution_routers=4,
        reliability_paths=1,
        refine_radius_m=0,
        coarse_sample_step_m=1,
        medium_sample_step_m=1,
        final_sample_step_m=1,
    )
    optimizer = RouteOptimizer(
        ArrayTerrain(np.zeros((201, 5001)), resolution_m=1), RFSettings(), settings
    )
    optimizer.evaluator = evaluator  # type: ignore[assignment]

    result = optimizer.optimize(endpoint_a, endpoint_b, candidates=chain)

    assert result.found
    assert limits[0] == 4 + optimizer_module.SEARCH_ROUTER_HEADROOM
    assert limits[-1] == 4
    assert all(solution.router_count <= 4 for solution in result.alternatives)
