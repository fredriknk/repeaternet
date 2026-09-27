from __future__ import annotations

import json
import time
from threading import Event

import numpy as np
import pytest
from fastapi.testclient import TestClient
from pyproj import Transformer
from rasterio.io import MemoryFile
from rasterio.transform import from_origin

import rf_router_planner.web as web_module
from rf_router_planner.models.coverage import CoverageCell, CoverageState
from rf_router_planner.models.link import DirectionResult, LinkResult
from rf_router_planner.models.network import NetworkSolution
from rf_router_planner.models.site import Site, SiteKind
from rf_router_planner.optimization.optimizer import OptimizationResult, RouteOptimizer
from rf_router_planner.web import create_app


def terrain_tile() -> bytes:
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


def coordinates(x: float, y: float) -> list[float]:
    reverse = Transformer.from_crs(25833, 4326, always_xy=True)
    longitude, latitude = reverse.transform(x, y)
    return [latitude, longitude]


def fake_optimizer(self, endpoint_a, endpoint_b, **kwargs):
    del self, kwargs
    router = Site(
        "R-coverage",
        500500.0,
        6650500.0,
        kind=SiteKind.ROUTER,
        antenna_height_m=50.0,
    )
    solution = NetworkSolution(
        "One coverage router",
        [endpoint_a, router, endpoint_b],
        [],
        [endpoint_a.id, endpoint_b.id],
        [router.id],
        {(endpoint_a.id, endpoint_b.id): [[endpoint_a.id, router.id, endpoint_b.id]]},
    )
    return OptimizationResult(
        [endpoint_a, router, endpoint_b], [], [router], [], alternatives=[solution]
    )


def fake_two_router_optimizer(self, endpoint_a, endpoint_b, **kwargs):
    del self, kwargs
    first = Site("R-1", 500500.0, 6650500.0, kind=SiteKind.ROUTER, antenna_height_m=50.0)
    second = Site("R-2", 520500.0, 6650500.0, kind=SiteKind.ROUTER, antenna_height_m=50.0)
    forward = DirectionResult(
        "R-1", "R-2", 20_000.0, 100.0, 0.0, 0.0, 100.0, 2.15, 2.15,
        -70.0, -130.0, 60.0, 50.0, 0.0, 0.0, True,
    )
    reverse = DirectionResult(
        "R-2", "R-1", 20_000.0, 100.0, 0.0, 0.0, 100.0, 2.15, 2.15,
        -70.0, -130.0, 60.0, 50.0, 0.0, 0.0, True,
    )
    link = LinkResult(
        "R-1", "R-2", 20_000.0, forward, reverse, True, True, True,
        10.0, 0.8, 10_000.0, 100.0, 0.0,
    )
    solution = NetworkSolution(
        "Two-router route",
        [endpoint_a, first, second, endpoint_b],
        [link],
        [endpoint_a.id, endpoint_b.id],
        [first.id, second.id],
        {(endpoint_a.id, endpoint_b.id): [[endpoint_a.id, first.id, second.id, endpoint_b.id]]},
    )
    return OptimizationResult(
        [endpoint_a, first, second, endpoint_b], [link], [first, second], [], alternatives=[solution]
    )


def wait_for(client: TestClient, job_id: str, terminal: set[str]) -> dict:
    for _ in range(500):
        response = client.get(f"/api/coverage/jobs/{job_id}")
        if (
            response.status_code == 200
            and response.json()["state"] == "failed"
            and "failed" not in terminal
        ):
            pytest.fail(f"Coverage job {job_id} failed: {response.json()['stage']}")
        if response.status_code == 200 and response.json()["state"] in terminal:
            return response.json()
        time.sleep(0.01)
    pytest.fail(f"Coverage job {job_id} did not reach {terminal}")


def prepare_workspace(client: TestClient, monkeypatch, optimizer=fake_optimizer) -> dict:
    monkeypatch.setattr(RouteOptimizer, "optimize", optimizer)
    assert client.get("/").status_code == 200
    assert client.post("/api/terrain/dtm", files={"file": ("ground.tif", terrain_tile())}).status_code == 200
    coverage_settings = {
        "mode": "two_way",
        "area_mode": "mesh",
        "cell_size_m": 500.0,
        "area_buffer_m": 500.0,
        "profile_step_m": 100.0,
        "maximum_profile_samples": 128,
        "maximum_cells": 4,
        "maximum_evaluations": 100,
        "client": {
            "name": "test handheld",
            "height_agl_m": 1.5,
            "tx_power_dbm": 20.0,
            "gain_dbi": 2.15,
            "feed_loss_db": 0.0,
            "sensitivity_dbm": -130.0,
            "miscellaneous_loss_db": 0.0,
        },
    }
    route = client.post(
        "/api/optimize",
        json={
            "a": coordinates(500100.0, 6650500.0),
            "b": coordinates(500900.0, 6650500.0),
            "rf": {},
            "candidates": {"maximum_candidates": 20, "refine_radius_m": 0},
            "coverage": coverage_settings,
        },
    )
    assert route.status_code == 200, route.text
    for _ in range(500):
        state = client.get("/api/state").json()
        if state["job"]["state"] not in {"queued", "running", "preparing"}:
            break
        time.sleep(0.01)
    assert state["job"]["state"] == "complete", state["job"]
    result = client.get("/api/result").json()
    assert result["router_count"] >= 1
    body = {
        "snapshot_version": state["job"]["snapshot_version"],
        "alternative_id": result["active_alternative_id"],
        "settings": coverage_settings,
    }
    return {"coverage": body, "result": result, "plan": state["plan"]}


def start_coverage(client: TestClient, prepared: dict) -> dict:
    response = client.post("/api/coverage/jobs", json=prepared["coverage"])
    assert response.status_code == 200, response.text
    return response.json()


def test_coverage_job_estimate_paging_isolation_reconnect_and_recovery(
    tmp_path, monkeypatch
) -> None:
    monkeypatch.delenv("RF_PLANNER_TOKEN", raising=False)
    monkeypatch.setenv("RF_PLANNER_MAX_ACTIVE_JOBS", "1")
    app = create_app(tmp_path)
    with TestClient(app) as client:
        prepared = prepare_workspace(client, monkeypatch)
        estimate = client.post("/api/coverage/estimate", json=prepared["coverage"])
        assert estimate.status_code == 200, estimate.text
        assert estimate.json()["source_count"] == 1
        assert estimate.json()["requested_cells"] == 4
        assert estimate.json()["planned_evaluations"] == 4

        changed = json.loads(json.dumps(prepared["plan"]))
        changed["coverage"]["client"]["tx_power_dbm"] = 10.0
        assert client.post("/api/projects/autosave", json={"plan": changed}).status_code == 200
        assert client.post("/api/coverage/jobs", json=prepared["coverage"]).status_code == 409
        assert (
            client.post("/api/projects/autosave", json={"plan": prepared["plan"]}).status_code
            == 200
        )

        job = start_coverage(client, prepared)
        completed = wait_for(client, job["job_id"], {"complete"})
        assert completed["done"] == completed["total"] == 4
        assert completed["evaluated_cells"] == 4

        pages = []
        cursor = 0
        while True:
            page = client.get(
                f"/api/coverage/jobs/{job['job_id']}/cells",
                params={"cursor": cursor, "limit": 1},
            )
            assert page.status_code == 200
            pages.extend(page.json()["cells"])
            cursor = page.json()["next_cursor"]
            if cursor is None:
                break
        assert [cell["index"] for cell in pages] == [0, 1, 2, 3]
        assert client.get(
            f"/api/coverage/jobs/{job['job_id']}/cells", params={"cursor": 0, "limit": 257}
        ).status_code == 422
        assert client.post(f"/api/coverage/jobs/{job['job_id']}/cancel").status_code == 409

        isolated = TestClient(app)
        isolated.get("/")
        assert isolated.get(f"/api/coverage/jobs/{job['job_id']}").status_code == 404
        assert isolated.get(f"/api/coverage/jobs/{job['job_id']}/cells").status_code == 404
        isolated.close()
        workspace_key = client.cookies.get("planner_workspace")

    # A completed calculation is available to the same browser after reconnecting
    # to this process, without submitting a second job.
    with TestClient(app) as reconnected:
        reconnected.cookies.set("planner_workspace", workspace_key)
        state = reconnected.get("/api/state").json()
        assert state["coverage_job"]["job_id"] == job["job_id"]
        assert state["coverage_job"]["state"] == "complete"
        assert reconnected.get(
            f"/api/coverage/jobs/{job['job_id']}/cells", params={"cursor": 0, "limit": 4}
        ).json()["stored_cells"] == 4

    manifests = list((tmp_path / workspace_key / "projects").glob("*/coverage-job.json"))
    assert len(manifests) == 1
    manifest = json.loads(manifests[0].read_text(encoding="utf-8"))
    manifest["state"] = "running"
    manifests[0].write_text(json.dumps(manifest), encoding="utf-8")
    with TestClient(create_app(tmp_path)) as restarted:
        restarted.cookies.set("planner_workspace", workspace_key)
        recovered = restarted.get("/api/state").json()["coverage_job"]
        assert recovered["state"] == "interrupted"
        retained = restarted.get(
            f"/api/coverage/jobs/{job['job_id']}/cells", params={"cursor": 0, "limit": 4}
        )
        assert retained.status_code == 200
        assert retained.json()["stored_cells"] == 4


def test_running_coverage_can_be_cancelled_and_locks_project_lifecycle(
    tmp_path, monkeypatch
) -> None:
    monkeypatch.delenv("RF_PLANNER_TOKEN", raising=False)
    app = create_app(tmp_path)
    original = web_module.calculate_coverage
    started, release = Event(), Event()

    def blocked_calculation(*args, **kwargs):
        started.set()
        assert release.wait(3)
        return original(*args, **kwargs)

    monkeypatch.setattr(web_module, "calculate_coverage", blocked_calculation)
    with TestClient(app) as client:
        prepared = prepare_workspace(client, monkeypatch)
        job = start_coverage(client, prepared)
        assert started.wait(2)
        assert job["state"] in {"running", "queued"}

        state = client.get("/api/state").json()
        project_id = state["projects"]["active_id"]
        assert client.post("/api/projects/autosave", json={"plan": prepared["plan"]}).status_code == 409
        assert client.post(f"/api/projects/{project_id}/activate").status_code == 409
        assert client.post(
            "/api/terrain/dtm", files={"file": ("replacement.tif", terrain_tile())}
        ).status_code == 409
        assert client.post(f"/api/coverage/jobs/{job['job_id']}/cancel").status_code == 200
        release.set()
        cancelled = wait_for(client, job["job_id"], {"cancelled"})
        assert cancelled["done"] == 0
        assert client.get(
            f"/api/coverage/jobs/{job['job_id']}/cells", params={"cursor": 0, "limit": 4}
        ).json()["stored_cells"] == 0


def test_queued_coverage_cancellation_releases_shared_scheduler_capacity(
    tmp_path, monkeypatch
) -> None:
    monkeypatch.delenv("RF_PLANNER_TOKEN", raising=False)
    monkeypatch.setenv("RF_PLANNER_MAX_ACTIVE_JOBS", "1")
    app = create_app(tmp_path)
    original = web_module.calculate_coverage
    started, release = Event(), Event()
    call_count = 0

    def block_first_workspace(*args, **kwargs):
        nonlocal call_count
        call_count += 1
        if call_count == 1:
            started.set()
            assert release.wait(3)
        return original(*args, **kwargs)

    monkeypatch.setattr(web_module, "calculate_coverage", block_first_workspace)
    with TestClient(app) as first, TestClient(app) as second:
        first_prepared = prepare_workspace(first, monkeypatch)
        second_prepared = prepare_workspace(second, monkeypatch)
        first_job = start_coverage(first, first_prepared)
        assert started.wait(2)
        second_job = start_coverage(second, second_prepared)
        assert second_job["state"] == "queued"
        assert second.post(
            f"/api/coverage/jobs/{second_job['job_id']}/cancel"
        ).status_code == 200
        release.set()
        assert wait_for(first, first_job["job_id"], {"complete"})["state"] == "complete"
        assert wait_for(second, second_job["job_id"], {"cancelled"})["state"] == "cancelled"

        # The slot freed by the first job is available to a subsequent job even
        # though the queued predecessor was cancelled.
        next_job = start_coverage(second, second_prepared)
        assert wait_for(second, next_job["job_id"], {"complete"})["state"] == "complete"


def test_coverage_output_quota_fails_visibly_without_partial_cells(tmp_path, monkeypatch) -> None:
    monkeypatch.delenv("RF_PLANNER_TOKEN", raising=False)
    monkeypatch.setattr(web_module, "MAX_COVERAGE_RESULT_BYTES", 1)
    with TestClient(create_app(tmp_path)) as client:
        prepared = prepare_workspace(client, monkeypatch)
        job = start_coverage(client, prepared)
        failed = wait_for(client, job["job_id"], {"failed"})
        assert "storage limit" in failed["stage"]
        page = client.get(
            f"/api/coverage/jobs/{job['job_id']}/cells", params={"cursor": 0, "limit": 4}
        )
        assert page.status_code == 200
        assert page.json()["stored_cells"] == 0


def test_failure_scenarios_use_completed_workspace_snapshot_without_rf_work(
    tmp_path, monkeypatch
) -> None:
    monkeypatch.delenv("RF_PLANNER_TOKEN", raising=False)
    with TestClient(create_app(tmp_path)) as client:
        prepared = prepare_workspace(client, monkeypatch)
        job = start_coverage(client, prepared)
        assert wait_for(client, job["job_id"], {"complete"})["state"] == "complete"
        scenario = client.post(
            f"/api/coverage/jobs/{job['job_id']}/scenario",
            json={"failed_ids": ["R-coverage"], "reference_id": "R-coverage"},
        )
        assert scenario.status_code == 200, scenario.text
        payload = scenario.json()
        assert payload["reference_available"] is False
        assert payload["failed_ids"] == ["R-coverage"]
        assert len(payload["cells"]) == 4
        assert client.post(
            f"/api/coverage/jobs/{job['job_id']}/scenario",
            json={"failed_ids": ["R-coverage", "R-coverage"], "reference_id": "R-coverage"},
        ).status_code == 422
        assert client.post(
            f"/api/coverage/jobs/{job['job_id']}/scenario",
            json={"failed_ids": [], "reference_id": "not-a-router"},
        ).status_code == 422

        other_workspace = TestClient(client.app)
        other_workspace.get("/")
        assert other_workspace.post(
            f"/api/coverage/jobs/{job['job_id']}/scenario",
            json={"failed_ids": [], "reference_id": "R-coverage"},
        ).status_code == 404
        other_workspace.close()


def test_saved_targets_are_project_scoped_and_assess_point_road_and_area_targets(
    tmp_path, monkeypatch
) -> None:
    monkeypatch.delenv("RF_PLANNER_TOKEN", raising=False)
    with TestClient(create_app(tmp_path)) as client:
        prepared = prepare_workspace(client, monkeypatch)
        job = start_coverage(client, prepared)
        assert wait_for(client, job["job_id"], {"complete"})["state"] == "complete"

        point_latitude, point_longitude = coordinates(500600.0, 6650500.0)
        xform = Transformer.from_crs(25833, 4326, always_xy=True)
        area_ring = [
            list(xform.transform(x, y))
            for x, y in (
                (500475.0, 6650475.0),
                (500525.0, 6650475.0),
                (500525.0, 6650525.0),
                (500475.0, 6650525.0),
                (500475.0, 6650475.0),
            )
        ]
        source = {
            "type": "FeatureCollection",
            "features": [
                {
                    "type": "Feature",
                    "geometry": {"type": "Point", "coordinates": [point_longitude, point_latitude]},
                    "properties": {
                        "name": "Point",
                        "minimum_margin_db": -100,
                        "reference_id": "R-coverage",
                    },
                },
                {
                    "type": "Feature",
                    "geometry": {
                        "type": "LineString",
                        "coordinates": [
                            list(xform.transform(500600.0, 6650500.0)),
                            list(xform.transform(500700.0, 6650500.0)),
                        ],
                    },
                    "properties": {"name": "Road", "minimum_margin_db": -100},
                },
                {
                    "type": "Feature",
                    "geometry": {"type": "Polygon", "coordinates": [area_ring]},
                    "properties": {"name": "Small area"},
                },
            ],
        }
        imported = client.post(
            "/api/coverage/targets/import",
            files={"file": ("targets.geojson", json.dumps(source), "application/geo+json")},
        )
        assert imported.status_code == 200, imported.text
        saved = client.put("/api/coverage/targets", json=imported.json())
        assert saved.status_code == 200, saved.text
        assert client.get("/api/state").json()["plan"]["coverage_targets"] == saved.json()["targets"]
        assert not client.get(f"/api/coverage/jobs/{job['job_id']}").json()["stale"]

        inspection = client.post(
            "/api/coverage/inspect",
            json={
                **prepared["coverage"],
                "latitude": point_latitude,
                "longitude": point_longitude,
            },
        )
        assert inspection.status_code == 200, inspection.text
        inspection_id = inspection.json()["job_id"]
        for _ in range(500):
            inspected = client.get(f"/api/coverage/inspect/{inspection_id}").json()
            if inspected["state"] in {"complete", "failed"}:
                break
            time.sleep(0.01)
        assert inspected["state"] == "complete", inspected

        report = client.post(
            f"/api/coverage/jobs/{job['job_id']}/targets",
            json={"road_spacing_m": 25},
        )
        assert report.status_code == 200, report.text
        targets = {item["name"]: item for item in report.json()["targets"]}
        assert targets["Point"]["state"] == "pass"
        inspect_margins = [
            row["two_way_margin_db"]
            for row in inspected["result"]["sources"]
            if row["source_id"] == "R-coverage" and row["valid_two_way"]
        ]
        assert targets["Point"]["best_two_way_margin_db"] == pytest.approx(max(inspect_margins))
        assert targets["Road"]["state"] == "pass"
        assert targets["Road"]["covered_length_m"] == pytest.approx(
            targets["Road"]["total_length_m"]
        )
        assert targets["Small area"]["state"] == "needs_finer_sampling"
        assert targets["Small area"]["unknown_area_m2"] == pytest.approx(
            targets["Small area"]["requested_area_m2"]
        )
        assert report.json()["assumptions"]["reference_connected_source_ids"]["R-coverage"] == [
            "R-coverage"
        ]

        isolated = TestClient(client.app)
        isolated.get("/")
        assert isolated.get("/api/state").json()["plan"] is None
        assert isolated.post(
            f"/api/coverage/jobs/{job['job_id']}/targets", json={}
        ).status_code == 404
        isolated.close()


def test_target_import_and_saved_collection_enforce_validation(tmp_path, monkeypatch) -> None:
    monkeypatch.delenv("RF_PLANNER_TOKEN", raising=False)
    with TestClient(create_app(tmp_path)) as client:
        prepare_workspace(client, monkeypatch)
        invalid = {
            "type": "FeatureCollection",
            "features": [
                {
                    "type": "Feature",
                    "geometry": {"type": "Point", "coordinates": [181, 60]},
                    "properties": {"name": "Invalid"},
                }
            ],
        }
        assert client.post(
            "/api/coverage/targets/import",
            files={"file": ("bad.geojson", json.dumps(invalid), "application/geo+json")},
        ).status_code == 422
        assert client.put("/api/coverage/targets", json=invalid).status_code == 422
        oversized = client.post(
            "/api/coverage/targets/import",
            files={
                "file": (
                    "oversized.geojson",
                    b" " * (5 * 1024 * 1024 + 1),
                    "application/geo+json",
                )
            },
        )
        assert oversized.status_code == 422
        assert "5 MiB" in oversized.json()["detail"]


def test_saved_target_reports_compare_compatible_coverage_scenarios(tmp_path, monkeypatch) -> None:
    monkeypatch.delenv("RF_PLANNER_TOKEN", raising=False)
    with TestClient(create_app(tmp_path)) as client:
        prepared = prepare_workspace(client, monkeypatch)
        point_latitude, point_longitude = coordinates(500600.0, 6650500.0)
        target_collection = {
            "type": "FeatureCollection",
            "features": [
                {
                    "type": "Feature",
                    "id": "T-scenario",
                    "geometry": {
                        "type": "Point",
                        "coordinates": [point_longitude, point_latitude],
                    },
                    "properties": {"name": "Scenario point", "minimum_margin_db": -100},
                }
            ],
        }
        assert client.put("/api/coverage/targets", json=target_collection).status_code == 200

        baseline = start_coverage(client, prepared)
        assert wait_for(client, baseline["job_id"], {"complete"})["state"] == "complete"
        baseline_report = client.post(
            f"/api/coverage/jobs/{baseline['job_id']}/targets", json={"road_spacing_m": 100}
        )
        assert baseline_report.status_code == 200, baseline_report.text

        scenario_request = {
            **prepared["coverage"],
            "source_height_overrides": {"R-coverage": 10.0},
        }
        scenario = client.post("/api/coverage/jobs", json=scenario_request)
        assert scenario.status_code == 200, scenario.text
        assert wait_for(client, scenario.json()["job_id"], {"complete"})["state"] == "complete"
        scenario_report = client.post(
            f"/api/coverage/jobs/{scenario.json()['job_id']}/targets",
            json={"road_spacing_m": 100},
        )
        assert scenario_report.status_code == 200, scenario_report.text

        compared = client.post(
            "/api/coverage/target-reports/compare",
            json={
                "baseline_report_id": baseline_report.json()["report_id"],
                "scenario_report_id": scenario_report.json()["report_id"],
            },
        )
        assert compared.status_code == 200, compared.text
        assert compared.json()["counts"] == {
            "improved": 0,
            "regressed": 0,
            "unchanged": 1,
            "uncertain": 0,
        }
        listed = client.get("/api/coverage/target-reports").json()["reports"]
        assert {item["report_id"] for item in listed} >= {
            baseline_report.json()["report_id"],
            scenario_report.json()["report_id"],
        }


def test_coverage_geojson_json_and_offline_html_exports_are_workspace_scoped(
    tmp_path, monkeypatch
) -> None:
    monkeypatch.delenv("RF_PLANNER_TOKEN", raising=False)
    with TestClient(create_app(tmp_path)) as client:
        prepared = prepare_workspace(client, monkeypatch, fake_two_router_optimizer)
        targets = {
            "type": "FeatureCollection",
            "features": [
                {
                    "type": "Feature",
                    "id": "T-export",
                    "geometry": {
                        "type": "Point",
                        "coordinates": list(reversed(coordinates(500600.0, 6650500.0))),
                    },
                    "properties": {
                        "name": "Export target",
                        "minimum_margin_db": -100,
                        "reference_id": "R-1",
                    },
                }
            ],
        }
        assert client.put("/api/coverage/targets", json=targets).status_code == 200
        baseline = start_coverage(client, prepared)
        assert wait_for(client, baseline["job_id"], {"complete"})["state"] == "complete"
        baseline_report = client.post(
            f"/api/coverage/jobs/{baseline['job_id']}/targets", json={}
        )
        assert baseline_report.status_code == 200, baseline_report.text

        scenario_started = client.post(
            "/api/coverage/jobs",
            json={**prepared["coverage"], "source_height_overrides": {"R-1": 10.0}},
        )
        assert scenario_started.status_code == 200, scenario_started.text
        scenario = wait_for(client, scenario_started.json()["job_id"], {"complete"})
        scenario_report = client.post(f"/api/coverage/jobs/{scenario['job_id']}/targets", json={})
        assert scenario_report.status_code == 200, scenario_report.text

        geojson_response = client.get(
            f"/api/coverage/jobs/{baseline['job_id']}/export.geojson",
            params={"target_report_id": baseline_report.json()["report_id"]},
        )
        assert geojson_response.status_code == 200, geojson_response.text
        collection = geojson_response.json()
        assert collection["metadata"]["coordinate_reference_system"] == "OGC:CRS84"
        assert collection["metadata"]["counts"]["requested_cells"] == 4
        assert len(collection["features"]) == 5
        assert collection["features"][-1]["properties"]["coverage_state"] == "pass"

        json_response = client.get(
            f"/api/coverage/jobs/{baseline['job_id']}/export.json",
            params={
                "target_report_id": baseline_report.json()["report_id"],
                "scenario_job_id": scenario["job_id"],
                "reference_id": "R-1",
            },
        )
        assert json_response.status_code == 200, json_response.text
        exported = json_response.json()
        assert len(exported["cells"]) == 4
        assert exported["metadata"]["radio_settings"]["rf"]
        assert exported["scenario_comparison"]["counts"]

        report = client.get(
            f"/api/coverage/jobs/{baseline['job_id']}/report.html",
            params={
                "target_report_id": baseline_report.json()["report_id"],
                "scenario_job_id": scenario["job_id"],
                "reference_id": "R-1",
            },
        )
        assert report.status_code == 200, report.text
        assert "<svg" in report.text
        assert "Export target" in report.text
        assert "R-1 → R-2" in report.text
        assert "Scenario comparison" in report.text
        assert "https://" not in report.text

        isolated = TestClient(client.app)
        isolated.get("/")
        assert isolated.get(
            f"/api/coverage/jobs/{baseline['job_id']}/export.json"
        ).status_code == 404
        isolated.close()


def test_stale_and_partial_coverage_exports_require_explicit_opt_in(tmp_path, monkeypatch) -> None:
    monkeypatch.delenv("RF_PLANNER_TOKEN", raising=False)
    with TestClient(create_app(tmp_path)) as client:
        prepared = prepare_workspace(client, monkeypatch)
        job = start_coverage(client, prepared)
        assert wait_for(client, job["job_id"], {"complete"})["state"] == "complete"
        assert client.post(
            "/api/terrain/dtm", files={"file": ("extra.tif", terrain_tile())}
        ).status_code == 200
        export_url = f"/api/coverage/jobs/{job['job_id']}/export.geojson"
        assert client.get(export_url).status_code == 409
        stale = client.get(export_url, params={"include_stale": "true"})
        assert stale.status_code == 200, stale.text
        assert stale.json()["metadata"]["stale"] is True

    def partial_coverage(terrain, rf, candidates, sources, settings, **kwargs):
        grid, xs, ys, latitudes, longitudes = web_module.make_grid(
            sources, terrain, settings
        )
        kwargs["on_chunk"](
            [
                CoverageCell(
                    0,
                    float(xs[0]),
                    float(ys[0]),
                    float(latitudes[0]),
                    float(longitudes[0]),
                    state=CoverageState.UNKNOWN_TERRAIN,
                )
            ]
        )
        raise ValueError("Injected partial coverage failure")

    monkeypatch.setattr(web_module, "calculate_coverage", partial_coverage)
    with TestClient(create_app(tmp_path / "partial")) as client:
        prepared = prepare_workspace(client, monkeypatch)
        job = start_coverage(client, prepared)
        assert wait_for(client, job["job_id"], {"failed"})["state"] == "failed"
        export_url = f"/api/coverage/jobs/{job['job_id']}/export.geojson"
        assert client.get(export_url).status_code == 409
        partial = client.get(export_url, params={"include_partial": "true"})
        assert partial.status_code == 200, partial.text
        assert partial.json()["metadata"]["partial"] is True
        assert partial.json()["metadata"]["counts"]["not_evaluated_cells"] == 3


def test_coverage_export_size_limit_cleans_up_temporary_file(tmp_path, monkeypatch) -> None:
    monkeypatch.delenv("RF_PLANNER_TOKEN", raising=False)
    with TestClient(create_app(tmp_path)) as client:
        prepared = prepare_workspace(client, monkeypatch)
        job = start_coverage(client, prepared)
        assert wait_for(client, job["job_id"], {"complete"})["state"] == "complete"
        monkeypatch.setattr(web_module, "MAX_COVERAGE_EXPORT_BYTES", 64)
        response = client.get(f"/api/coverage/jobs/{job['job_id']}/export.geojson")
        assert response.status_code == 413
        assert "100 MiB" in response.json()["detail"]
        export_directory = next(tmp_path.rglob("coverage-exports"))
        assert not list(export_directory.glob("*"))


def test_compatible_coverage_runs_are_archived_and_compared_on_common_grid(
    tmp_path, monkeypatch
) -> None:
    monkeypatch.delenv("RF_PLANNER_TOKEN", raising=False)
    with TestClient(create_app(tmp_path)) as client:
        prepared = prepare_workspace(client, monkeypatch)
        baseline = start_coverage(client, prepared)
        assert wait_for(client, baseline["job_id"], {"complete"})["state"] == "complete"
        scenario = start_coverage(client, prepared)
        assert wait_for(client, scenario["job_id"], {"complete"})["state"] == "complete"

        available = client.get("/api/coverage/jobs").json()["jobs"]
        assert {item["job_id"] for item in available} >= {
            baseline["job_id"],
            scenario["job_id"],
        }
        comparison = client.post(
            "/api/coverage/compare",
            json={
                "baseline_job_id": baseline["job_id"],
                "scenario_job_id": scenario["job_id"],
                "reference_id": "R-coverage",
            },
        )
        assert comparison.status_code == 200, comparison.text
        result = comparison.json()
        assert result["counts"]["gained_local"] == 0
        assert result["counts"]["lost_local"] == 0
        assert result["counts"]["retained_local"] == result["counts"]["scenario_covered_cells"]
        assert result["counts"]["evaluated_cells"] == 4
        assert len(result["cells"]) == 4


def test_height_scenario_is_bounded_and_saved_without_editing_baseline(tmp_path, monkeypatch) -> None:
    monkeypatch.delenv("RF_PLANNER_TOKEN", raising=False)
    with TestClient(create_app(tmp_path)) as client:
        prepared = prepare_workspace(client, monkeypatch)
        invalid = {**prepared["coverage"], "source_height_overrides": {"R-coverage": 0}}
        assert client.post("/api/coverage/jobs", json=invalid).status_code == 422
        scenario_request = {
            **prepared["coverage"],
            "source_height_overrides": {"R-coverage": 10.0},
        }
        response = client.post("/api/coverage/jobs", json=scenario_request)
        assert response.status_code == 200, response.text
        completed = wait_for(client, response.json()["job_id"], {"complete"})
        assert completed["source_height_overrides"] == {"R-coverage": 10.0}
        assert completed["source_sites"][0]["antenna_height_m"] == 10.0
        assert prepared["result"]["route"][1]["antenna_height_m"] == 50.0


def test_height_scenario_revalidates_certified_backbone_edges(tmp_path, monkeypatch) -> None:
    monkeypatch.delenv("RF_PLANNER_TOKEN", raising=False)
    with TestClient(create_app(tmp_path)) as client:
        prepared = prepare_workspace(client, monkeypatch, fake_two_router_optimizer)
        baseline = start_coverage(client, prepared)
        assert wait_for(client, baseline["job_id"], {"complete"})["state"] == "complete"
        request = {
            **prepared["coverage"],
            "source_height_overrides": {"R-1": 10.0},
        }
        started = client.post("/api/coverage/jobs", json=request)
        assert started.status_code == 200, started.text
        completed = wait_for(client, started.json()["job_id"], {"complete"})
        assert completed["backbone_revalidated_edges"] == 1
        assert completed["backbone_invalidated_edges"] == 1
        assert completed["network_links"] == [
            {"source_id": "R-1", "target_id": "R-2", "valid": False}
        ]
        comparison = client.post(
            "/api/coverage/compare",
            json={
                "baseline_job_id": baseline["job_id"],
                "scenario_job_id": completed["job_id"],
                "reference_id": "R-1",
            },
        )
        assert comparison.status_code == 200, comparison.text
        assert comparison.json()["baseline_connected_source_ids"] == ["R-1", "R-2"]
        assert comparison.json()["scenario_connected_source_ids"] == ["R-1"]


def test_comparison_rejects_different_client_profile_resolution(tmp_path, monkeypatch) -> None:
    monkeypatch.delenv("RF_PLANNER_TOKEN", raising=False)
    with TestClient(create_app(tmp_path)) as client:
        prepared = prepare_workspace(client, monkeypatch)
        baseline = start_coverage(client, prepared)
        assert wait_for(client, baseline["job_id"], {"complete"})["state"] == "complete"
        settings = json.loads(json.dumps(prepared["coverage"]["settings"]))
        settings["profile_step_m"] = 50.0
        plan = json.loads(json.dumps(prepared["plan"]))
        plan["coverage"] = settings
        assert client.post("/api/projects/autosave", json={"plan": plan}).status_code == 200
        request = {**prepared["coverage"], "settings": settings}
        scenario = client.post("/api/coverage/jobs", json=request)
        assert scenario.status_code == 200, scenario.text
        assert wait_for(client, scenario.json()["job_id"], {"complete"})["state"] == "complete"
        compared = client.post(
            "/api/coverage/compare",
            json={
                "baseline_job_id": baseline["job_id"],
                "scenario_job_id": scenario.json()["job_id"],
                "reference_id": "R-coverage",
            },
        )
        assert compared.status_code == 409
        assert "profile resolution" in compared.json()["detail"]


def test_comparison_rejects_terrain_changed_after_snapshot(tmp_path, monkeypatch) -> None:
    monkeypatch.delenv("RF_PLANNER_TOKEN", raising=False)
    with TestClient(create_app(tmp_path)) as client:
        prepared = prepare_workspace(client, monkeypatch)
        baseline = start_coverage(client, prepared)
        assert wait_for(client, baseline["job_id"], {"complete"})["state"] == "complete"
        scenario = start_coverage(client, prepared)
        assert wait_for(client, scenario["job_id"], {"complete"})["state"] == "complete"
        assert client.post(
            "/api/terrain/dtm", files={"file": ("extra-ground.tif", terrain_tile())}
        ).status_code == 200
        compared = client.post(
            "/api/coverage/compare",
            json={
                "baseline_job_id": baseline["job_id"],
                "scenario_job_id": scenario["job_id"],
                "reference_id": "R-coverage",
            },
        )
        assert compared.status_code == 409
        assert "terrain" in compared.json()["detail"]
