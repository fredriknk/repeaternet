import time
from datetime import UTC, datetime
from threading import Event

import numpy as np
import pytest
from fastapi.testclient import TestClient
from pyproj import Transformer
from rasterio.io import MemoryFile
from rasterio.transform import from_origin

from rf_router_planner.integrations.corescope import CoreScopeClient, CoreScopeRepeater
from rf_router_planner.optimization.optimizer import OptimizationResult, RouteOptimizer
from rf_router_planner.web import create_app


@pytest.fixture
def client(tmp_path, monkeypatch):
    monkeypatch.delenv("RF_PLANNER_TOKEN", raising=False)
    with TestClient(create_app(tmp_path)) as client:
        assert client.get("/").status_code == 200
        yield client


def tile():
    with MemoryFile() as memory:
        with memory.open(
            driver="GTiff",
            width=101,
            height=101,
            count=1,
            dtype="float32",
            crs="EPSG:25833",
            transform=from_origin(500000, 6651000, 10, 10),
        ) as dataset:
            dataset.write(np.zeros((1, 101, 101), dtype=np.float32))
        return memory.read()


def tile_with_nodata_gap():
    with MemoryFile() as memory:
        with memory.open(
            driver="GTiff",
            width=101,
            height=101,
            count=1,
            dtype="float32",
            crs="EPSG:25833",
            transform=from_origin(500000, 6651000, 10, 10),
            nodata=-9999,
        ) as dataset:
            values = np.zeros((101, 101), dtype=np.float32)
            values[49:54, 49:54] = -9999
            dataset.write(values, 1)
        return memory.read()


def test_real_route_upload_optimize_profile_export(client):
    assert (
        client.post("/api/terrain/dtm", files={"file": ("ground.tif", tile())}).status_code == 200
    )
    reverse = Transformer.from_crs(25833, 4326, always_xy=True)
    a = list(reversed(reverse.transform(500100, 6650500)))
    b = list(reversed(reverse.transform(500800, 6650500)))
    payload = {
        "a": a,
        "b": b,
        "rf": {},
        "candidates": {"maximum_candidates": 20, "refine_radius_m": 0},
    }
    response = client.post("/api/optimize", json=payload)
    assert response.status_code == 200, response.text
    for _ in range(200):
        job = client.get("/api/state").json()["job"]
        if job["state"] != "running":
            break
        time.sleep(0.02)
    assert job["state"] == "complete", job
    result = client.get("/api/result").json()
    assert result["found"]
    assert result["router_count"] == 0
    assert len(result["links"][0]["profile"]["distances_m"]) > 2
    assert client.get("/api/export/csv").text.startswith("hop,from,to")
    assert client.get("/api/export/geojson").json()["type"] == "FeatureCollection"
    assert client.get("/api/export/project").json()["a"] == a
    cold_cache = client.get("/api/state").json()["rf_cache"]
    assert cold_cache["misses"] > 0
    warm = client.post("/api/optimize", json=payload)
    assert warm.status_code == 200
    for _ in range(200):
        job = client.get("/api/state").json()["job"]
        if job["state"] == "complete":
            break
        time.sleep(0.02)
    assert job["state"] == "complete"
    warm_cache = client.get("/api/state").json()["rf_cache"]
    assert warm_cache["hits"] > cold_cache["hits"]
    objective_only = {
        **payload,
        "candidates": {**payload["candidates"], "priority": "minimum_infrastructure"},
    }
    objective_change = client.post("/api/optimize", json=objective_only)
    assert objective_change.status_code == 200
    for _ in range(200):
        job = client.get("/api/state").json()["job"]
        if job["state"] == "complete":
            break
        time.sleep(0.02)
    assert job["state"] == "complete"
    assert client.get("/api/state").json()["rf_cache"]["hits"] > warm_cache["hits"]
    assert client.delete("/api/cache").status_code == 200
    assert client.get("/api/state").json()["rf_cache"]["entries"] == 0
    revision = client.get("/api/state").json()["job"]["input_revision"]
    assert (
        client.post("/api/terrain/dtm", files={"file": ("replacement.tif", tile())}).status_code
        == 200
    )
    assert client.get("/api/state").json()["job"]["input_revision"] == revision + 1
    assert client.get("/api/result").status_code == 404


def test_quick_effort_reports_resolved_search_scope(client):
    assert (
        client.post("/api/terrain/dtm", files={"file": ("ground.tif", tile())}).status_code == 200
    )
    reverse = Transformer.from_crs(25833, 4326, always_xy=True)
    a = list(reversed(reverse.transform(500100, 6650500)))
    b = list(reversed(reverse.transform(500800, 6650500)))
    response = client.post(
        "/api/optimize",
        json={
            "a": a,
            "b": b,
            "rf": {},
            "candidates": {"maximum_candidates": 1200, "maximum_neighbors_per_site": 20},
            "search_effort": "quick",
        },
    )
    assert response.status_code == 200, response.text
    job = response.json()
    assert job["search_effort"] == "quick"
    assert job["resolved_search"] == {
        "candidate_limit": 200,
        "requested_candidate_limit": 1200,
        "neighbor_limit": 8,
        "generated_candidate_limit": 200,
        "known_router_count": 0,
        "excluded_router_count": 0,
        "infrastructure_policy": "existing_and_proposed",
    }
    assert job["started_at"] > 0


@pytest.mark.parametrize("keep", [True, False])
def test_stop_or_cancel_handles_certified_snapshot(client, monkeypatch, keep):
    reverse = Transformer.from_crs(25833, 4326, always_xy=True)
    a = list(reversed(reverse.transform(500100, 6650500)))
    b = list(reversed(reverse.transform(500800, 6650500)))
    assert (
        client.post("/api/terrain/dtm", files={"file": ("ground.tif", tile())}).status_code == 200
    )

    def slow_optimize(self, endpoint_a, endpoint_b, *, solution_progress, cancelled, **_kwargs):
        link = self.evaluator.evaluate(endpoint_a, endpoint_b, 10)
        assert link.valid
        snapshot = OptimizationResult(
            [endpoint_a, endpoint_b],
            [link],
            [endpoint_a, endpoint_b],
            [link],
            search_complete=False,
        )
        solution_progress(snapshot, False)
        deadline = time.monotonic() + 5
        while not cancelled() and time.monotonic() < deadline:
            time.sleep(0.005)
        return snapshot

    monkeypatch.setattr(RouteOptimizer, "optimize", slow_optimize)
    submitted = client.post("/api/optimize", json={"a": a, "b": b, "rf": {}, "candidates": {}})
    assert submitted.status_code == 200, submitted.text
    job_id = submitted.json()["job_id"]
    for _ in range(200):
        job = client.get("/api/state").json()["job"]
        if job["snapshot_version"]:
            break
        time.sleep(0.01)
    assert job["snapshot_version"] == 1
    action = "/api/stop-and-keep" if keep else "/api/cancel"
    assert client.post(action, json={"job_id": job_id}).status_code == 200
    for _ in range(200):
        job = client.get("/api/state").json()["job"]
        if job["state"] in {"stopped", "cancelled"}:
            break
        time.sleep(0.01)
    assert job["state"] == ("stopped" if keep else "cancelled")
    result = client.get("/api/result")
    assert result.status_code == (200 if keep else 404)
    if keep:
        assert result.json()["found"]
        assert not result.json()["search_complete"]
        assert client.get("/api/export/csv").status_code == 200
    else:
        assert client.get("/api/export/csv").status_code == 404


def test_planner_queue_is_bounded(tmp_path, monkeypatch):
    monkeypatch.delenv("RF_PLANNER_TOKEN", raising=False)
    monkeypatch.setenv("RF_PLANNER_MAX_ACTIVE_JOBS", "1")
    app = create_app(tmp_path)
    entered = Event()

    def slow_optimize(self, *_args, cancelled, **_kwargs):
        entered.set()
        while not cancelled():
            time.sleep(0.005)
        return OptimizationResult([], [], [], [])

    monkeypatch.setattr(RouteOptimizer, "optimize", slow_optimize)
    with TestClient(app) as first, TestClient(app) as second:
        first.get("/")
        second.get("/")
        for client in (first, second):
            assert (
                client.post("/api/terrain/dtm", files={"file": ("ground.tif", tile())}).status_code
                == 200
            )
        reverse = Transformer.from_crs(25833, 4326, always_xy=True)
        a = list(reversed(reverse.transform(500100, 6650500)))
        b = list(reversed(reverse.transform(500800, 6650500)))
        payload = {"a": a, "b": b, "rf": {}, "candidates": {}}
        active = first.post("/api/optimize", json=payload)
        assert active.status_code == 200
        assert entered.wait(2)
        queued = second.post("/api/optimize", json=payload)
        assert queued.status_code == 200
        assert queued.json()["state"] == "queued"
        assert first.get("/api/state").json()["job"]["state"] == "running"
        first.post("/api/cancel", json={"job_id": active.json()["job_id"]})
        second.post("/api/cancel", json={"job_id": queued.json()["job_id"]})
        for _ in range(200):
            states = [
                first.get("/api/state").json()["job"]["state"],
                second.get("/api/state").json()["job"]["state"],
            ]
            if states == ["cancelled", "cancelled"]:
                break
            time.sleep(0.01)
        assert states == ["cancelled", "cancelled"]


def test_meshcore_routers_can_be_selected_as_optional_candidates(client, monkeypatch):
    reverse = Transformer.from_crs(25833, 4326, always_xy=True)
    a = list(reversed(reverse.transform(500100, 6650500)))
    b = list(reversed(reverse.transform(500800, 6650500)))
    repeater_lat, repeater_lon = reversed(reverse.transform(500450, 6650500))
    repeater = CoreScopeRepeater(
        public_key="abcdef1234567890",
        name="Midpoint MeshCore",
        latitude=repeater_lat,
        longitude=repeater_lon,
        last_heard=datetime(2026, 9, 26, tzinfo=UTC),
        relay_active=True,
        relay_count_24h=8,
    )
    outside = CoreScopeRepeater(
        public_key="outside1234567890",
        name="Outside corridor",
        latitude=59.93,
        longitude=10.75,
    )
    monkeypatch.setattr(CoreScopeClient, "fetch_repeaters", lambda self: [repeater, outside])
    assert (
        client.post("/api/terrain/dtm", files={"file": ("ground.tif", tile())}).status_code == 200
    )
    imported = client.post(
        "/api/meshcore",
        json={"a": a, "b": b, "corridor_width_m": 500},
    )
    assert imported.status_code == 200, imported.text
    assert [item["id"] for item in imported.json()["routers"]] == [repeater.id]

    payload = {
        "a": a,
        "b": b,
        "rf": {},
        "candidates": {"maximum_candidates": 20, "refine_radius_m": 0},
        "known_routers": imported.json()["routers"],
    }
    assert client.post("/api/optimize", json=payload).status_code == 200
    for _ in range(200):
        job = client.get("/api/state").json()["job"]
        if job["state"] != "running":
            break
        time.sleep(0.02)
    assert job["state"] == "complete", job
    result = client.get("/api/result").json()
    known_site = next(site for site in result["candidates"] if site["id"] == repeater.id)
    assert known_site["origin"] == "known"
    assert known_site["kind"] == "router"
    assert known_site["locked"]
    assert not known_site["required"]
    assert result["router_count"] == 0
    assert client.get("/api/export/project").json()["known_routers"][0]["id"] == repeater.id


def test_existing_only_uses_required_router_without_generating_sites(client, monkeypatch):
    import rf_router_planner.optimization.optimizer as optimizer_module

    def unexpected_generation(*args, **kwargs):
        raise AssertionError("existing-only policy must not generate candidate sites")

    monkeypatch.setattr(optimizer_module, "generate_candidates", unexpected_generation)
    reverse = Transformer.from_crs(25833, 4326, always_xy=True)
    a = list(reversed(reverse.transform(500100, 6650500)))
    b = list(reversed(reverse.transform(500800, 6650500)))
    repeater_lat, repeater_lon = reversed(reverse.transform(500450, 6650500))
    assert client.post("/api/terrain/dtm", files={"file": ("ground.tif", tile())}).status_code == 200
    response = client.post(
        "/api/optimize",
        json={
            "a": a,
            "b": b,
            "rf": {},
            "candidates": {
                "infrastructure_policy": "existing_only",
                "maximum_candidates": 20,
                "maximum_solution_routers": 1,
                "refine_radius_m": 0,
            },
            "known_routers": [
                {
                    "id": "K-required",
                    "name": "Required relay",
                    "latitude": repeater_lat,
                    "longitude": repeater_lon,
                    "policy": "required",
                }
            ],
        },
    )
    assert response.status_code == 200, response.text
    for _ in range(200):
        job = client.get("/api/state").json()["job"]
        if job["state"] not in {"running", "queued"}:
            break
        time.sleep(0.02)
    assert job["state"] == "complete", job
    result = client.get("/api/result").json()
    assert {site["id"] for site in result["candidates"]} == {"A", "B", "K-required"}
    assert "K-required" in {site["id"] for site in result["route"]}
    assert result["router_count"] == 1
    assert result["existing_router_count"] == 1
    assert result["proposed_router_count"] == 0


def test_excluded_existing_router_does_not_enter_existing_only_graph(client):
    reverse = Transformer.from_crs(25833, 4326, always_xy=True)
    a = list(reversed(reverse.transform(500100, 6650500)))
    b = list(reversed(reverse.transform(500800, 6650500)))
    repeater_lat, repeater_lon = reversed(reverse.transform(500450, 6650500))
    assert client.post("/api/terrain/dtm", files={"file": ("ground.tif", tile())}).status_code == 200
    response = client.post(
        "/api/optimize",
        json={
            "a": a,
            "b": b,
            "rf": {},
            "candidates": {"infrastructure_policy": "existing_only", "maximum_candidates": 20},
            "known_routers": [
                {
                    "id": "K-excluded",
                    "latitude": repeater_lat,
                    "longitude": repeater_lon,
                    "policy": "excluded",
                }
            ],
        },
    )
    assert response.status_code == 200, response.text
    for _ in range(200):
        job = client.get("/api/state").json()["job"]
        if job["state"] not in {"running", "queued"}:
            break
        time.sleep(0.02)
    assert job["state"] == "complete", job
    result = client.get("/api/result").json()
    assert "K-excluded" not in {site["id"] for site in result["candidates"]}
    assert result["router_count"] == 0


def test_terrain_estimate_reports_resolution_cache_and_transfer_budget(client):
    reverse = Transformer.from_crs(25833, 4326, always_xy=True)
    body = {
        "a": list(reversed(reverse.transform(500100, 6650500))),
        "b": list(reversed(reverse.transform(500800, 6650500))),
        "rf": {},
        "candidates": {"corridor_width_m": 500},
        "terrain": {"requested_resolution_m": 10, "auto_resolution": True, "include_dom": True},
    }
    response = client.post("/api/terrain/estimate", json=body)
    assert response.status_code == 200, response.text
    estimate = response.json()
    assert estimate["crs"] == "EPSG:25833"
    assert estimate["tile_count"] >= 1
    assert estimate["cache_hits"] == 0
    assert estimate["cache_total"] == estimate["tile_count"] * 2
    assert estimate["estimated_raw_mib"] > 0


def test_coverage_reports_tile_outlines_nodata_gaps_and_uncovered_sites(client):
    reverse = Transformer.from_crs(25833, 4326, always_xy=True)
    a = list(reversed(reverse.transform(500100, 6650500)))
    b = list(reversed(reverse.transform(500800, 6650500)))
    assert (
        client.post("/api/terrain/dtm", files={"file": ("ground.tif", tile_with_nodata_gap())}).status_code
        == 200
    )
    report = client.post("/api/terrain/coverage", json={"a": a, "b": b}).json()
    assert report["covered"]
    assert len(report["dtm_outlines"]) == 1
    assert report["dom_outlines"] == []
    assert report["line_gaps"]

    outside = list(reversed(reverse.transform(502000, 6650500)))
    missing = client.post("/api/terrain/coverage", json={"a": a, "b": outside}).json()
    assert not missing["covered"]
    assert missing["uncovered_sites"][0]["name"] == "Endpoint B"
    assert missing["uncovered_sites"][0]["reason"] == "outside_coverage"


def test_optimize_rejects_uncovered_sites_before_queuing(client):
    reverse = Transformer.from_crs(25833, 4326, always_xy=True)
    a = list(reversed(reverse.transform(500100, 6650500)))
    outside = list(reversed(reverse.transform(502000, 6650500)))
    assert client.post("/api/terrain/dtm", files={"file": ("ground.tif", tile())}).status_code == 200
    response = client.post("/api/optimize", json={"a": a, "b": outside, "rf": {}, "candidates": {}})
    assert response.status_code == 422
    assert "Endpoint B" in response.json()["detail"]
    assert client.get("/api/state").json()["job"]["state"] == "idle"


def test_terrain_preparation_publishes_only_complete_generation(client, tmp_path, monkeypatch):
    from rf_router_planner.terrain.kartverket import KartverketProvider

    dom_started, finish_dom = Event(), Event()

    def fake_fetch_plan(self, product, crs, plan, progress=None, cancelled=None):
        source = tmp_path / f"{product}-source.tif"
        source.write_bytes(tile())
        if progress is not None:
            for index, _ in enumerate(plan.tiles, 1):
                progress(index, len(plan.tiles), False)
        if product == "dom":
            dom_started.set()
            assert finish_dom.wait(3)
        return [source for _ in plan.tiles]

    monkeypatch.setattr(KartverketProvider, "fetch_plan", fake_fetch_plan)
    reverse = Transformer.from_crs(25833, 4326, always_xy=True)
    body = {
        "a": list(reversed(reverse.transform(500100, 6650500))),
        "b": list(reversed(reverse.transform(500800, 6650500))),
        "rf": {},
        "candidates": {"corridor_width_m": 500},
        "terrain": {"requested_resolution_m": 10, "auto_resolution": True, "include_dom": True},
    }
    response = client.post("/api/terrain/prepare", json=body)
    assert response.status_code == 200, response.text
    assert dom_started.wait(3)
    state = client.get("/api/state").json()
    assert state["job"]["state"] == "preparing"
    assert state["terrain"]["dtm"] == []
    assert state["terrain"]["dom"] == []
    finish_dom.set()
    for _ in range(200):
        state = client.get("/api/state").json()
        if state["job"]["state"] not in {"queued", "preparing"}:
            break
        time.sleep(0.02)
    assert state["job"]["state"] == "terrain_ready", state["job"]
    assert state["terrain"]["dtm"]
    assert state["terrain"]["dom"]
    coverage = client.post("/api/terrain/coverage", json=body).json()
    assert coverage["covered"]
    assert coverage["surface_available"]


def test_cancelled_terrain_preparation_keeps_previous_uploads(client, tmp_path, monkeypatch):
    from rf_router_planner.terrain.kartverket import KartverketProvider

    dom_started = Event()

    def fake_fetch_plan(self, product, crs, plan, progress=None, cancelled=None):
        if product == "dom":
            dom_started.set()
            while cancelled is not None and not cancelled():
                time.sleep(0.01)
            raise RuntimeError("Terrain download cancelled")
        source = tmp_path / "downloaded-dtm.tif"
        source.write_bytes(tile())
        return [source for _ in plan.tiles]

    monkeypatch.setattr(KartverketProvider, "fetch_plan", fake_fetch_plan)
    reverse = Transformer.from_crs(25833, 4326, always_xy=True)
    body = {
        "a": list(reversed(reverse.transform(500100, 6650500))),
        "b": list(reversed(reverse.transform(500800, 6650500))),
        "rf": {},
        "candidates": {"corridor_width_m": 500},
        "terrain": {"include_dom": True},
    }
    uploaded = client.post("/api/terrain/dtm", files={"file": ("manual.tif", tile())})
    assert uploaded.status_code == 200
    previous = client.get("/api/state").json()["terrain"]["dtm"]
    started = client.post("/api/terrain/prepare", json=body)
    assert started.status_code == 200, started.text
    assert dom_started.wait(3)
    assert client.post("/api/cancel", json={"job_id": started.json()["job_id"]}).status_code == 200
    for _ in range(200):
        state = client.get("/api/state").json()
        if state["job"]["state"] not in {"queued", "preparing"}:
            break
        time.sleep(0.02)
    assert state["job"]["state"] == "cancelled"
    assert state["terrain"]["dtm"] == previous
    assert state["terrain"]["dom"] == []


def test_existing_only_avoids_generated_candidate_rf_evaluations(tmp_path, monkeypatch):
    monkeypatch.setenv("RF_PLANNER_TOKEN", "")
    app = create_app(tmp_path)
    reverse = Transformer.from_crs(25833, 4326, always_xy=True)
    a = list(reversed(reverse.transform(500100, 6650500)))
    b = list(reversed(reverse.transform(500800, 6650500)))
    repeater_lat, repeater_lon = reversed(reverse.transform(500450, 6650500))

    def run_search(client, policy):
        assert client.post("/api/terrain/dtm", files={"file": ("ground.tif", tile())}).status_code == 200
        response = client.post(
            "/api/optimize",
            json={
                "a": a,
                "b": b,
                "rf": {},
                "candidates": {
                    "infrastructure_policy": policy,
                    "maximum_candidates": 5,
                    "maximum_solution_routers": 1,
                    "grid_spacing_m": 500,
                    "cell_size_m": 500,
                    "refine_radius_m": 0,
                },
                "known_routers": [
                    {
                        "id": "K-existing",
                        "latitude": repeater_lat,
                        "longitude": repeater_lon,
                        "policy": "optional",
                    }
                ],
            },
        )
        assert response.status_code == 200, response.text
        for _ in range(200):
            job = client.get("/api/state").json()["job"]
            if job["state"] not in {"running", "queued"}:
                break
            time.sleep(0.02)
        assert job["state"] == "complete", job
        return client.get("/api/state").json()["rf_cache"]["misses"]

    with TestClient(app) as existing_client, TestClient(app) as mixed_client:
        assert existing_client.get("/").status_code == 200
        assert mixed_client.get("/").status_code == 200
        existing_count = run_search(existing_client, "existing_only")
        mixed_count = run_search(mixed_client, "existing_and_proposed")
    assert mixed_count > existing_count


def test_invalid_upload_and_settings(client):
    assert client.post("/api/terrain/dtm", files={"file": ("bad.tif", b"bad")}).status_code == 400
    assert client.get("/api/state").json()["terrain"]["dtm"] == []
    body = {"a": [60, 10], "b": [60.1, 10.1], "rf": {"frequency_mhz": -1}}
    assert client.post("/api/optimize", json=body).status_code == 422
    body["rf"] = {"router": {"pattern_csv": "C:/private.csv"}}
    assert client.post("/api/optimize", json=body).status_code == 422
    assert (
        client.post("/api/cancel", headers={"origin": "https://other.example"}).status_code == 403
    )


def test_workspace_isolation_and_token(tmp_path, monkeypatch):
    monkeypatch.setenv("RF_PLANNER_TOKEN", "example-secret")
    app = create_app(tmp_path)
    with TestClient(app) as first, TestClient(app) as second:
        assert first.get("/api/state").status_code == 401
        assert first.post("/login", data={"token": "wrong"}).status_code == 401
        first.post("/login", data={"token": "example-secret"})
        second.post("/login", data={"token": "example-secret"})
        first.post("/api/terrain/dtm", files={"file": ("ground.tif", tile())})
        assert len(first.get("/api/state").json()["terrain"]["dtm"]) == 1
        assert second.get("/api/state").json()["terrain"]["dtm"] == []
