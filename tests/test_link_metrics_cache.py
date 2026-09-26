import time
from concurrent.futures import ThreadPoolExecutor
from threading import Event

import numpy as np

from rf_router_planner.models.settings import RFSettings
from rf_router_planner.models.site import Site, SiteKind
from rf_router_planner.optimization.cache import LinkMetricsCache
from rf_router_planner.rf.propagation import LinkEvaluator
from rf_router_planner.terrain.raster import ArrayTerrain


def endpoints() -> tuple[Site, Site]:
    return (
        Site("A", 10, 50, kind=SiteKind.ENDPOINT_A),
        Site("B", 90, 50, kind=SiteKind.ENDPOINT_B),
    )


def test_warm_cache_reuses_compact_metrics_and_returns_independent_values(monkeypatch):
    terrain = ArrayTerrain(np.zeros((11, 11)), resolution_m=10)
    settings = RFSettings()
    source, target = endpoints()
    cache = LinkMetricsCache(max_entries=8)
    original_sample = type(terrain).sample
    sampled_points = 0

    def count_samples(self, x, y, surface=False):
        nonlocal sampled_points
        if self is terrain:
            sampled_points += np.size(x)
        return original_sample(self, x, y, surface)

    monkeypatch.setattr(type(terrain), "sample", count_samples)
    first_evaluator = LinkEvaluator(terrain, settings, cache, "workspace-a")
    first = first_evaluator.evaluate(source, target, 10, include_profile=False)
    after_cold = sampled_points
    second_evaluator = LinkEvaluator(terrain, settings, cache, "workspace-a")
    second = second_evaluator.evaluate(source, target, 10, include_profile=False)

    assert after_cold > 0
    assert sampled_points == after_cold
    assert first.profile is None and second.profile is None
    assert first.forward.usable_margin_db == second.forward.usable_margin_db
    first.forward.usable_margin_db = -999
    assert second.forward.usable_margin_db != -999
    assert cache.stats("workspace-a") == {
        "hits": 1,
        "misses": 1,
        "evictions": 0,
        "entries": 1,
        "capacity": 8,
    }


def test_cache_key_covers_direction_settings_terrain_and_sample_spacing():
    source, target = endpoints()
    cache = LinkMetricsCache(max_entries=16)
    flat = ArrayTerrain(np.zeros((11, 11)), resolution_m=10)
    LinkEvaluator(flat, RFSettings(), cache, "workspace-a").evaluate(
        source, target, 10, include_profile=False
    )
    reverse = LinkEvaluator(flat, RFSettings(), cache, "workspace-a").evaluate(
        target, source, 10, include_profile=False
    )
    changed_rf = RFSettings(frequency_mhz=868.0)
    changed = LinkEvaluator(flat, changed_rf, cache, "workspace-a").evaluate(
        source, target, 10, include_profile=False
    )
    taller = Site("A", source.x, source.y, kind=source.kind, antenna_height_m=30)
    LinkEvaluator(flat, RFSettings(), cache, "workspace-a").evaluate(
        taller, target, 10, include_profile=False
    )
    LinkEvaluator(flat, RFSettings(), cache, "workspace-a").evaluate(
        source, target, 20, include_profile=False
    )
    ridge = np.zeros((11, 11))
    ridge[:, 5] = 40
    changed_terrain = ArrayTerrain(ridge, resolution_m=10)
    LinkEvaluator(changed_terrain, RFSettings(), cache, "workspace-a").evaluate(
        source, target, 10, include_profile=False
    )

    assert reverse.source_id == "B"
    assert changed.distance_m == 80
    assert cache.stats("workspace-a")["misses"] == 6
    assert cache.stats("workspace-a")["hits"] == 0


def test_invalid_rf_metrics_are_cached_but_profile_requests_are_regenerated():
    terrain = ArrayTerrain(np.zeros((11, 11)), resolution_m=10)
    source, target = endpoints()
    cache = LinkMetricsCache(max_entries=8)
    weak_settings = RFSettings(tx_power_dbm=-100)
    evaluator = LinkEvaluator(terrain, weak_settings, cache, "workspace-a")
    invalid = evaluator.evaluate(source, target, 10, include_profile=False)
    assert not invalid.valid
    assert not evaluator.evaluate(source, target, 10, include_profile=False).valid
    profiled = evaluator.evaluate(source, target, 10)
    assert profiled.profile is not None
    assert cache.stats("workspace-a")["hits"] == 1
    assert cache.stats("workspace-a")["misses"] == 1


def test_pattern_content_change_invalidates_the_metrics(tmp_path):
    source, target = endpoints()
    pattern = tmp_path / "antenna.csv"
    cache = LinkMetricsCache(max_entries=8)
    pattern.write_text("elevation_deg,gain_dbi\n-90,2\n0,2\n90,2\n", encoding="utf-8")
    first_settings = RFSettings()
    first_settings.endpoint_a.pattern_csv = str(pattern)
    first = LinkEvaluator(ArrayTerrain(np.zeros((11, 11)), resolution_m=10), first_settings, cache)
    initial = first.evaluate(source, target, 10, include_profile=False)

    pattern.write_text("elevation_deg,gain_dbi\n-90,0\n0,0\n90,0\n", encoding="utf-8")
    second_settings = RFSettings()
    second_settings.endpoint_a.pattern_csv = str(pattern)
    second = LinkEvaluator(
        ArrayTerrain(np.zeros((11, 11)), resolution_m=10), second_settings, cache
    )
    changed = second.evaluate(source, target, 10, include_profile=False)

    assert changed.forward.usable_margin_db < initial.forward.usable_margin_db
    assert cache.stats("default")["misses"] == 2


def test_failed_computations_are_not_cached():
    cache = LinkMetricsCache(max_entries=1)
    source, target = endpoints()
    link = LinkEvaluator(ArrayTerrain(np.zeros((11, 11)), resolution_m=10), RFSettings()).evaluate(
        source, target, 10
    )
    calls = 0

    def fail_once():
        nonlocal calls
        calls += 1
        if calls == 1:
            raise ValueError("terrain sample failed")
        return link

    try:
        cache.get_or_compute("workspace-a", ("link",), fail_once)
    except ValueError:
        pass
    assert cache.get_or_compute("workspace-a", ("link",), fail_once).valid
    assert calls == 2
    assert cache.stats("workspace-a")["misses"] == 2


def test_cache_is_workspace_scoped_bounded_and_clearable():
    source, target = endpoints()
    terrain = ArrayTerrain(np.zeros((11, 11)), resolution_m=10)
    cache = LinkMetricsCache(max_entries=1)
    LinkEvaluator(terrain, RFSettings(), cache, "workspace-a").evaluate(
        source, target, 10, include_profile=False
    )
    LinkEvaluator(terrain, RFSettings(), cache, "workspace-b").evaluate(
        source, target, 10, include_profile=False
    )
    assert cache.stats("workspace-a")["hits"] == 0
    assert cache.stats("workspace-b")["hits"] == 0
    assert cache.stats("workspace-a")["evictions"] == 1
    assert cache.stats("workspace-b")["entries"] == 1
    assert cache.clear("workspace-b") == 1
    assert cache.stats("workspace-b")["entries"] == 0


def test_concurrent_identical_cache_misses_are_coalesced():
    source, target = endpoints()
    link = LinkEvaluator(ArrayTerrain(np.zeros((11, 11)), resolution_m=10), RFSettings()).evaluate(
        source, target, 10
    )
    cache = LinkMetricsCache(max_entries=2)
    entered, release = Event(), Event()
    calls = 0

    def compute():
        nonlocal calls
        calls += 1
        entered.set()
        assert release.wait(2)
        return link

    with ThreadPoolExecutor(max_workers=2) as executor:
        first = executor.submit(cache.get_or_compute, "workspace-a", ("same",), compute)
        assert entered.wait(2)
        second = executor.submit(cache.get_or_compute, "workspace-a", ("same",), compute)
        time.sleep(0.02)
        release.set()
        assert first.result(timeout=2).valid
        assert second.result(timeout=2).valid

    assert calls == 1
    assert cache.stats("workspace-a")["misses"] == 1
    assert cache.stats("workspace-a")["hits"] == 1
