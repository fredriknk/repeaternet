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
    return KartverketProvider({"EPSG:25833": service}, tmp_path, maximum_tile_area_km2=400, **kwargs)


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


def test_route_corridor_includes_selected_sites_away_from_the_route(tmp_path) -> None:
    planner = provider(tmp_path)
    plan = planner.plan_download(
        (0, 0, 100_000, 20_000),
        100,
        corridor=RouteCorridor(((0, 10_000), (100_000, 10_000)), 2_000, ((50_000, 19_000),)),
        auto_resolution=False,
    )
    assert any(tile.bounds[0] <= 50_000 <= tile.bounds[2] for tile in plan.tiles)
    assert any(tile.bounds[1] <= 19_000 <= tile.bounds[3] for tile in plan.tiles)


def test_invalid_wcs_payload_retries_are_bounded_and_never_cached(tmp_path, monkeypatch) -> None:
    import requests

    planner = provider(tmp_path)
    plan = planner.plan_download((0, 0, 10_000, 10_000), 10, auto_resolution=False)
    calls = 0

    class Response:
        headers = {"content-length": "12"}

        def __enter__(self):
            return self

        def __exit__(self, *args):
            return None

        def raise_for_status(self):
            return None

        def iter_content(self, chunk_size):
            yield b"not a GeoTIFF"

    def fake_get(*args, **kwargs):
        nonlocal calls
        calls += 1
        return Response()

    monkeypatch.setattr(requests, "get", fake_get)
    tile = plan.tiles[0]
    destination = planner.cache_path("dtm", "EPSG:25833", tile.bounds, plan.effective_resolution_m)
    with pytest.raises(OSError):
        planner.fetch("dtm", "EPSG:25833", tile.bounds, plan.effective_resolution_m)
    assert calls == 3
    assert not destination.exists()
    assert not list(tmp_path.glob("*.part"))


def test_valid_workspace_cache_hit_does_not_require_network(tmp_path, monkeypatch) -> None:
    import numpy as np
    import requests
    from rasterio.io import MemoryFile
    from rasterio.transform import from_origin

    planner = provider(tmp_path, maximum_pixels_per_tile=10_000)
    plan = planner.plan_download((0, 0, 1_000, 1_000), 10, auto_resolution=False)
    tile = plan.tiles[0]
    path = planner.cache_path("dtm", "EPSG:25833", tile.bounds, plan.effective_resolution_m)
    path.parent.mkdir(parents=True, exist_ok=True)
    with MemoryFile() as memory:
        with memory.open(
            driver="GTiff",
            width=100,
            height=100,
            count=1,
            dtype="float32",
            crs="EPSG:25833",
            transform=from_origin(tile.bounds[0], tile.bounds[3], 10, 10),
        ) as dataset:
            dataset.write(np.zeros((100, 100), dtype=np.float32), 1)
        path.write_bytes(memory.read())

    def unexpected_network(*args, **kwargs):
        raise AssertionError("a valid cache hit must work offline")

    monkeypatch.setattr(requests, "get", unexpected_network)
    assert planner.fetch("dtm", "EPSG:25833", tile.bounds, plan.effective_resolution_m) == path


def test_fetch_plan_honors_cancellation_before_starting_a_tile(tmp_path) -> None:
    planner = provider(tmp_path)
    plan = planner.plan_download((0, 0, 10_000, 10_000), 10, auto_resolution=False)
    with pytest.raises(RuntimeError, match="cancelled"):
        planner.fetch_plan("dtm", "EPSG:25833", plan, cancelled=lambda: True)
