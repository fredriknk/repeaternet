from pathlib import Path

import pytest

from rf_router_planner.terrain.kartverket import KartverketProvider, RouteCorridor, WCSService


def provider(tmp_path: Path, **kwargs) -> KartverketProvider:
    service = WCSService(
        "EPSG:25833",
        "https://example.test/dtm",
        "https://example.test/dom",
        "dtm",
        "dom",
    )
    return KartverketProvider({"EPSG:25833": service}, tmp_path, maximum_area_km2=400, **kwargs)


def test_large_area_is_split_instead_of_rejected(tmp_path) -> None:
    planner = provider(tmp_path)
    plan = planner.plan_download(
        (0, 0, 100_000, 20_000),
        10,
        corridor=(0, 10_000, 100_000, 10_000, 10_000),
        auto_resolution=False,
    )
    assert len(plan.tiles) == 5
    assert plan.download_area_km2 == pytest.approx(2_000)
    assert all(tile.area_km2 <= 400 for tile in plan.tiles)
    assert all(tile.pixel_count <= 4_000_000 for tile in plan.tiles)


def test_diagonal_corridor_avoids_most_bounding_box_tiles(tmp_path) -> None:
    planner = provider(tmp_path)
    full = planner.plan_download((0, 0, 100_000, 100_000), 25, auto_resolution=False)
    corridor = planner.plan_download(
        (0, 0, 100_000, 100_000),
        25,
        corridor=(0, 0, 100_000, 100_000, 5_000),
        auto_resolution=False,
    )
    assert len(corridor.tiles) <= len(full.tiles) * 0.55
    assert corridor.download_area_km2 <= full.download_area_km2 * 0.55


def test_auto_resolution_respects_total_pixel_budget(tmp_path) -> None:
    planner = provider(tmp_path)
    plan = planner.plan_download(
        (0, 0, 500_000, 20_000),
        10,
        corridor=(0, 10_000, 500_000, 10_000, 10_000),
        auto_resolution=True,
        maximum_total_pixels=25_000_000,
    )
    assert plan.effective_resolution_m > 10
    assert plan.pixel_count <= 25_000_000
    assert plan.effective_resolution_m in {15, 20, 25, 30, 50}


def test_manual_one_metre_job_gets_actionable_tile_limit_error(tmp_path) -> None:
    planner = provider(tmp_path, maximum_tiles=10)
    with pytest.raises(ValueError, match="Enable automatic resolution"):
        planner.plan_download(
            (0, 0, 100_000, 20_000),
            1,
            corridor=(0, 10_000, 100_000, 10_000, 10_000),
            auto_resolution=False,
        )


def test_fetch_plan_reuses_content_addressed_cache(tmp_path, monkeypatch) -> None:
    planner = provider(tmp_path)
    plan = planner.plan_download((0, 0, 20_000, 20_000), 10, auto_resolution=False)
    calls: list[tuple[float, float, float, float]] = []

    def fake_fetch(product, crs, bounds, resolution, progress=None):  # type: ignore[no-untyped-def]
        calls.append(bounds)
        destination = planner.cache_path(product, crs, bounds, resolution)
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.touch()
        return destination

    monkeypatch.setattr(planner, "fetch", fake_fetch)
    first = planner.fetch_plan("dtm", "EPSG:25833", plan)
    assert len(first) == len(plan.tiles)
    assert len(calls) == len(plan.tiles)
    assert planner.cached_tile_count("dtm", "EPSG:25833", plan) == len(plan.tiles)


def test_route_corridor_follows_bends_in_selected_route(tmp_path) -> None:
    planner = provider(tmp_path)
    route = RouteCorridor(((0, 0), (80_000, 0), (80_000, 80_000)), 2_000)
    full = planner.plan_download(
        (-2_000, -2_000, 82_000, 82_000), 25, auto_resolution=False
    )
    plan = planner.plan_download(
        (-2_000, -2_000, 82_000, 82_000),
        25,
        corridor=route,
        auto_resolution=False,
    )
    assert len(plan.tiles) < len(full.tiles) * 0.6
    assert any(tile.bounds[0] <= 80_000 <= tile.bounds[2] for tile in plan.tiles)
