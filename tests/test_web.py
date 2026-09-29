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
from rf_router_planner.models.network import NetworkSolution
from rf_router_planner.models.site import Site, SiteKind, SiteOrigin
from rf_router_planner.optimization.optimizer import OptimizationResult, RouteOptimizer
from rf_router_planner.project_store import ProjectStore
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


def test_real_route_upload_optimize_profile_export(client, tmp_path):
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
    assert client.get("/api/state").json()["projects"]["previous_result"]["found"]
    with TestClient(create_app(tmp_path)) as restarted:
        restarted.cookies.set("planner_workspace", client.cookies.get("planner_workspace"))
        recovered = restarted.get("/api/state").json()
        assert recovered["plan"]["a"] == a
        assert recovered["projects"]["previous_result"]["stale"] is False
    assert result["active_alternative_id"]
    assert result["alternatives"]
    alternative = next(
        item for item in result["alternatives"] if item["id"] == result["active_alternative_id"]
    )
    assert alternative["selected"]
    assert alternative["primary_distance_m"] > 0
    assert alternative["total_link_distance_m"] >= alternative["primary_distance_m"]
    misses = client.get("/api/state").json()["rf_cache"]["misses"]
    selected = client.post(
        f"/api/alternatives/{result['active_alternative_id']}/select"
    ).json()
    assert selected["active_alternative_id"] == result["active_alternative_id"]
    assert client.get("/api/state").json()["rf_cache"]["misses"] == misses
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


def test_selecting_an_alternative_changes_active_route_without_search(client, monkeypatch):
    reverse = Transformer.from_crs(25833, 4326, always_xy=True)
    a = list(reversed(reverse.transform(500100, 6650500)))
    b = list(reversed(reverse.transform(500800, 6650500)))
    assert client.post("/api/terrain/dtm", files={"file": ("ground.tif", tile())}).status_code == 200

    def fake_optimize(self, endpoint_a, endpoint_b, **kwargs):
        del self, kwargs
        x = (endpoint_a.x + endpoint_b.x) / 2
        y = (endpoint_a.y + endpoint_b.y) / 2
        manual = Site(
            "M-alt",
            x,
            y,
            kind=SiteKind.ROUTER,
            origin=SiteOrigin.MANUAL,
        )
        direct = NetworkSolution(
            "0 routers",
            [endpoint_a, endpoint_b],
            [],
            [endpoint_a.id, endpoint_b.id],
            [],
            {("A", "B"): [["A", "B"]]},
        )
        via_manual = NetworkSolution(
            "1 router",
            [endpoint_a, manual, endpoint_b],
            [],
            [endpoint_a.id, endpoint_b.id],
            [manual.id],
            {("A", "B"): [["A", manual.id, "B"]]},
        )
        return OptimizationResult([], [], [], [], alternatives=[direct, via_manual])

    monkeypatch.setattr(RouteOptimizer, "optimize", fake_optimize)
    response = client.post(
        "/api/optimize",
        json={"a": a, "b": b, "rf": {}, "candidates": {"maximum_candidates": 20}},
    )
    assert response.status_code == 200, response.text
    for _ in range(200):
        job = client.get("/api/state").json()["job"]
        if job["state"] not in {"running", "queued"}:
            break
        time.sleep(0.02)
    assert job["state"] == "complete", job
    before = client.get("/api/result").json()
    assert before["router_count"] == 0
    selected_id = next(item["id"] for item in before["alternatives"] if item["router_count"] == 1)
    revision = before["input_revision"]
    misses = client.get("/api/state").json()["rf_cache"]["misses"]
    selected = client.post(f"/api/alternatives/{selected_id}/select").json()
    assert selected["active_alternative_id"] == selected_id
    assert selected["router_count"] == 1
    assert "M-alt" in {site["id"] for site in selected["route"]}
    assert selected["input_revision"] == revision
    assert client.get("/api/state").json()["rf_cache"]["misses"] == misses
    assert (
        client.get("/api/state").json()["projects"]["previous_result"]["active_alternative_id"]
        == selected_id
    )
    exported = client.get("/api/export/geojson").json()
    assert "M-alt" in {
        feature["properties"]["id"]
        for feature in exported["features"]
        if feature["geometry"]["type"] == "Point"
    }


def test_named_projects_autosave_switch_and_duplicate_terrain(client, tmp_path):
    initial = client.get("/api/state").json()
    source_id = initial["projects"]["active_id"]
    assert client.post("/api/terrain/dtm", files={"file": ("ground.tif", tile())}).status_code == 200
    saved_plan = {
        "format": "repeaternet-web",
        "version": 1,
        "a": [59.1, 10.2],
        "b": [59.2, 10.3],
        "rf": {},
        "candidates": {},
        "manual_routers": [{"id": "M-one", "policy": "optional"}],
    }
    assert client.post("/api/projects/autosave", json={"plan": saved_plan}).status_code == 200
    created = client.post("/api/projects", json={"name": "Second plan", "plan": saved_plan})
    assert created.status_code == 200, created.text
    second_id = created.json()["id"]
    assert second_id != source_id
    assert client.post(
        "/api/projects/autosave",
        json={"project_id": source_id, "plan": {"wrong_project": True}},
    ).status_code == 409
    switched = client.post(f"/api/projects/{source_id}/activate")
    assert switched.status_code == 200
    assert client.get("/api/state").json()["plan"] == saved_plan
    duplicate = client.post(
        f"/api/projects/{source_id}/duplicate", json={"name": "Terrain copy"}
    )
    assert duplicate.status_code == 200, duplicate.text
    duplicate_id = duplicate.json()["id"]
    state = client.get("/api/state").json()
    assert state["terrain"]["dtm"]
    assert {item["name"] for item in state["projects"]["items"]} >= {
        "Untitled plan",
        "Second plan",
        "Terrain copy",
    }

    other = TestClient(create_app(tmp_path))
    try:
        isolated = other.get("/")
        assert isolated.status_code == 200
        other_state = other.get("/api/state").json()
        assert {item["name"] for item in other_state["projects"]["items"]} == {"Untitled plan"}
    finally:
        other.close()

    replacement = client.post("/api/projects", json={"name": "Replacement", "plan": {}})
    assert replacement.status_code == 200
    assert client.delete(f"/api/projects/{duplicate_id}").status_code == 200
    assert client.post(f"/api/projects/{source_id}/archive").status_code == 200
    archived = client.get("/api/state").json()["projects"]["archived_items"]
    assert any(item["id"] == source_id for item in archived)
    restored = client.post(
        f"/api/projects/{source_id}/restore", json={"name": "Untitled plan"}
    )
    assert restored.status_code == 200, restored.text
    assert restored.json()["id"] == source_id
    restored_state = client.get("/api/state").json()
    assert restored_state["projects"]["active_id"] == source_id
    assert restored_state["terrain"]["dtm"]
    assert all(item["id"] != source_id for item in restored_state["projects"]["archived_items"])


def test_autosaved_plan_and_interrupted_state_recover_after_app_restart(tmp_path, monkeypatch):
    monkeypatch.delenv("RF_PLANNER_TOKEN", raising=False)
    first_app = create_app(tmp_path)
    with TestClient(first_app) as first:
        first.get("/")
        state = first.get("/api/state").json()
        workspace_key = first.cookies.get("planner_workspace")
        project_id = state["projects"]["active_id"]
        old_plan = {"a": [58.0, 9.0], "b": [59.0, 10.0], "rf": {}, "candidates": {}}
        current_plan = {"a": [60.0, 11.0], "b": [61.0, 12.0], "rf": {}, "candidates": {}}
        assert first.post("/api/projects/autosave", json={"plan": old_plan}).status_code == 200
        assert first.post("/api/projects/autosave", json={"plan": current_plan}).status_code == 200
        store = ProjectStore(tmp_path / "projects.sqlite3", tmp_path)
        store.set_run_state(workspace_key, project_id, "running")

    with TestClient(create_app(tmp_path)) as restarted:
        restarted.cookies.set("planner_workspace", workspace_key)
        state = restarted.get("/api/state").json()
        assert state["plan"] == current_plan
        assert state["job"]["state"] == "interrupted"
        assert state["projects"]["has_previous_revision"]
        recovered = restarted.post("/api/projects/recover-previous")
        assert recovered.status_code == 200
        assert recovered.json()["plan"] == old_plan
        assert restarted.get("/api/state").json()["plan"] == old_plan


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
        "manual_router_count": 0,
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
    active_project = client.get("/api/state").json()["projects"]["active_id"]
    locked_plan = {"a": a, "b": b, "rf": {}, "candidates": {"new_input": True}}
    assert client.post(
        "/api/projects/autosave",
        json={"project_id": active_project, "plan": locked_plan},
    ).status_code == 409
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


def test_manual_proposed_router_is_a_fixed_height_search_site(client):
    reverse = Transformer.from_crs(25833, 4326, always_xy=True)
    a = list(reversed(reverse.transform(500100, 6650500)))
    b = list(reversed(reverse.transform(500800, 6650500)))
    latitude, longitude = reversed(reverse.transform(500450, 6650500))
    assert client.post("/api/terrain/dtm", files={"file": ("ground.tif", tile())}).status_code == 200
    response = client.post(
        "/api/optimize",
        json={
            "a": a,
            "b": b,
            "rf": {"router": {"height_agl_m": 5}},
            "candidates": {
                "maximum_candidates": 20,
                "maximum_solution_routers": 1,
                "refine_radius_m": 0,
            },
            "manual_routers": [
                {
                    "id": "M-test",
                    "latitude": latitude,
                    "longitude": longitude,
                    "antenna_height_m": 37,
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
    manual_site = next(site for site in result["route"] if site["id"] == "M-test")
    assert manual_site["origin"] == "manual"
    assert manual_site["antenna_height_m"] == 37
    assert manual_site["height_override"]
    assert client.get("/api/export/project").json()["manual_routers"][0]["policy"] == "required"


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
