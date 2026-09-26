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
