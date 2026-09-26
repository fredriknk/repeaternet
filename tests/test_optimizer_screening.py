import numpy as np

from rf_router_planner.models.link import DirectionResult, LinkResult
from rf_router_planner.models.settings import CandidateSettings, OptimizationPriority, RFSettings
from rf_router_planner.models.site import Site, SiteKind
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
    valid = {edge(u, v) for u, v in [(a, x), (x, y), (y, z), (z, b), (a, left), (left, right), (right, b)]}
    assert (sites.index(left), sites.index(right)) not in screening_pair_indices(sites, 1)
    evaluator = ChainOnlyEvaluator(valid)
    mock_batch(monkeypatch, evaluator)
    optimizer = RouteOptimizer(ArrayTerrain(np.zeros((40, 110)), resolution_m=1), RFSettings(), CandidateSettings(maximum_neighbors_per_site=1, maximum_solution_routers=3))
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
    optimizer = RouteOptimizer(ArrayTerrain(np.zeros((40, 110)), resolution_m=1), RFSettings(), CandidateSettings(maximum_solution_routers=1, final_sample_step_m=1, priority=OptimizationPriority.MAXIMUM_RELIABILITY))
    optimizer.evaluator = evaluator
    result = optimizer.optimize(a, b, candidates=[a, *routers, b])
    assert result.found
    assert result.route[1].id == "R29"
    assert all((edge(next(s for s in result.candidates if s.id == link.source_id), next(s for s in result.candidates if s.id == link.target_id)), 1) in evaluator.calls for link in result.links)


def test_final_valid_link_is_not_rejected_by_coarse_classification():
    a, b = Site("A", 0, 0), Site("B", 100, 0)
    class CoarseRejects(ChainOnlyEvaluator):
        def evaluate(self, source, target, step):
            return self._result(source, target, step == 1, 10)
    optimizer = RouteOptimizer(ArrayTerrain(np.zeros((3, 101)), resolution_m=1), RFSettings(), CandidateSettings(final_sample_step_m=1))
    optimizer.evaluator = CoarseRejects({ChainOnlyEvaluator._edge(a, b)})
    assert optimizer._progressively_validate_link(a, b).valid


def test_cancellation_during_final_validation_returns_no_route(monkeypatch):
    a, b = Site("A", 0, 0, kind=SiteKind.ENDPOINT_A), Site("B", 100, 0, kind=SiteKind.ENDPOINT_B)
    evaluator = ChainOnlyEvaluator({ChainOnlyEvaluator._edge(a, b)})
    mock_batch(monkeypatch, evaluator)
    optimizer = RouteOptimizer(ArrayTerrain(np.zeros((3, 101)), resolution_m=1), RFSettings(), CandidateSettings(priority=OptimizationPriority.MAXIMUM_RELIABILITY))
    optimizer.evaluator = evaluator
    stopped = False
    def progress(stage, done, total):
        nonlocal stopped
        if stage == "Validating solution alternatives":
            stopped = True
    result = optimizer.optimize(a, b, candidates=[a, b], progress=progress, cancelled=lambda: stopped)
    assert not result.found
    assert "cancelled" in result.diagnostics[0]


def test_pre_cancelled_search_skips_candidate_generation(monkeypatch):
    def fail(*args, **kwargs):
        raise AssertionError("Candidate generation should not run")
    monkeypatch.setattr("rf_router_planner.optimization.optimizer.generate_candidates", fail)
    optimizer = RouteOptimizer(ArrayTerrain(np.zeros((3, 101)), resolution_m=1), RFSettings(), CandidateSettings())
    result = optimizer.optimize(Site("A", 0, 0), Site("B", 100, 0), cancelled=lambda: True)
    assert not result.found
    assert result.diagnostics == ["Optimization cancelled"]
