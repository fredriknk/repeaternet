import time

import numpy as np
import pytest
from fastapi.testclient import TestClient
from pyproj import Transformer
from rasterio.io import MemoryFile
from rasterio.transform import from_origin

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
