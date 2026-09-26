"""Single-process, self-hosted planner with isolated browser workspaces."""

from __future__ import annotations

import argparse
import json
import math
import os
import secrets
import threading
from dataclasses import fields, is_dataclass
from enum import Enum
from pathlib import Path
from typing import Any

import numpy as np
from fastapi import FastAPI, HTTPException, Request, UploadFile
from fastapi.responses import FileResponse, JSONResponse
from fastapi.staticfiles import StaticFiles
from pyproj import CRS, Transformer

from .export.csv_export import export_route_csv
from .export.geojson import export_route_geojson
from .models.settings import CandidateSettings, RFSettings
from .models.site import Site, SiteKind
from .optimization.optimizer import RouteOptimizer
from .terrain.raster import RasterTerrain

ASSETS = Path(__file__).parent / "web_assets"


def encode(value: Any) -> Any:
    if is_dataclass(value):
        return {f.name: encode(getattr(value, f.name)) for f in fields(value)}
    if isinstance(value, Enum):
        return value.value
    if isinstance(value, np.ndarray):
        return encode(value.tolist())
    if isinstance(value, dict):
        return {str(k): encode(v) for k, v in value.items()}
    if isinstance(value, (list, tuple)):
        return [encode(v) for v in value]
    if isinstance(value, (float, np.floating)):
        return float(value) if math.isfinite(value) else None
    if isinstance(value, np.bool_):
        return bool(value)
    return value


def settings(cls: Any, values: dict[str, Any]) -> Any:
    if not isinstance(values, dict):
        raise ValueError("Settings must be a JSON object")
    result = cls()
    known = {f.name for f in fields(result)}
    for key, value in values.items():
        if key not in known:
            raise ValueError(f"Unknown setting: {key}")
        old = getattr(result, key)
        if key == "pattern_csv" and value:
            raise ValueError("Server filesystem antenna paths are not supported")
        if is_dataclass(old):
            value = settings(type(old), value)
        elif isinstance(old, Enum):
            value = type(old)(value)
        elif isinstance(old, bool):
            if not isinstance(value, bool):
                raise ValueError(f"{key} must be a boolean")
        elif isinstance(old, (int, float)) or key == "maximum_link_distance_m":
            if value is None and key == "maximum_link_distance_m":
                pass
            elif (
                not isinstance(value, (int, float))
                or isinstance(value, bool)
                or not math.isfinite(value)
            ):
                raise ValueError(f"{key} must be a finite number")
            elif isinstance(old, int) and int(value) != value:
                raise ValueError(f"{key} must be an integer")
            elif isinstance(old, int):
                value = int(value)
        setattr(result, key, value)
    return result


def validate(rf: RFSettings, candidates: CandidateSettings) -> None:
    for key in ("frequency_mhz", "k_factor"):
        if getattr(rf, key) <= 0:
            raise ValueError(f"{key} must be positive")
    if not 0 <= rf.required_fresnel_clearance <= 1:
        raise ValueError("Fresnel clearance must be between 0 and 1")
    for antenna in (rf.endpoint_a, rf.endpoint_b, rf.router):
        if not 0 < antenna.height_agl_m <= 1000:
            raise ValueError("Antenna height must be between 0 and 1000 m")
    for key in (
        "grid_spacing_m",
        "cell_size_m",
        "coarse_sample_step_m",
        "medium_sample_step_m",
        "final_sample_step_m",
        "refine_step_m",
        "router_height_step_m",
        "corridor_width_m",
    ):
        if getattr(candidates, key) <= 0:
            raise ValueError(f"{key} must be positive")
    if not 2 <= candidates.maximum_candidates <= 2000:
        raise ValueError("Maximum candidates must be between 2 and 2000")
    if not 1 <= candidates.maximum_neighbors_per_site <= 200:
        raise ValueError("Neighbors must be between 1 and 200")
    if not 0 <= candidates.refine_radius_m <= 2000:
        raise ValueError("Refinement radius must be between 0 and 2000 m")
    if candidates.minimum_router_height_m > candidates.maximum_router_height_m:
        raise ValueError("Minimum height exceeds maximum height")
    if rf.lora.bandwidth_hz <= 0:
        raise ValueError("LoRa bandwidth must be positive")
    rf.lora.snr_thresholds_db = {int(k): v for k, v in rf.lora.snr_thresholds_db.items()}
    _ = rf.effective_sensitivity_dbm
    candidates.parallel_workers = 1


class Workspace:
    def __init__(self, directory: Path):
        self.directory = directory
        directory.mkdir(parents=True, exist_ok=True)
        self.lock = threading.Lock()
        self.cancel = threading.Event()
        self.status: dict[str, Any] = {
            "state": "idle",
            "stage": "Ready to plan",
            "done": 0,
            "total": 1,
        }
        self.result: Any = None
        self.inputs: dict[str, Any] = {}


def create_app(data_dir: Path | None = None) -> FastAPI:
    root = (data_dir or Path(os.environ.get("RF_PLANNER_DATA", "web-data"))).resolve()
    root.mkdir(parents=True, exist_ok=True)
    app = FastAPI(title="RF Router Planner")
    workspaces: dict[str, Workspace] = {}
    registry_lock = threading.Lock()

    @app.middleware("http")
    async def security(request: Request, call_next: Any) -> Any:
        origin = request.headers.get("origin")
        if (
            request.method not in {"GET", "HEAD", "OPTIONS"}
            and origin
            and origin != str(request.base_url).rstrip("/")
        ):
            return JSONResponse({"detail": "Cross-origin request rejected"}, status_code=403)
        token = os.environ.get("RF_PLANNER_TOKEN")
        if (
            token
            and request.cookies.get("planner_access") != token
            and request.url.path != "/login"
        ):
            if request.url.path.startswith("/api/"):
                return JSONResponse({"detail": "Sign in required"}, status_code=401)
            return FileResponse(ASSETS / "login.html")
        response = await call_next(request)
        response.headers["X-Content-Type-Options"] = "nosniff"
        response.headers["X-Frame-Options"] = "DENY"
        return response

    @app.post("/login")
    async def login(request: Request) -> Any:
        form = await request.form()
        if not secrets.compare_digest(
            str(form.get("token", "")), os.environ.get("RF_PLANNER_TOKEN", "")
        ):
            raise HTTPException(401, "Incorrect access token")
        from fastapi.responses import RedirectResponse

        response = RedirectResponse("/", status_code=303)
        response.set_cookie(
            "planner_access",
            str(form["token"]),
            httponly=True,
            samesite="strict",
            secure=request.url.scheme == "https",
        )
        return response

    def workspace(request: Request) -> Workspace:
        key = request.cookies.get("planner_workspace", "")
        if len(key) != 48 or any(c not in "0123456789abcdef" for c in key):
            raise HTTPException(401, "Open the planner to initialize a workspace")
        with registry_lock:
            if key not in workspaces:
                workspaces[key] = Workspace(root / key)
            return workspaces[key]

    @app.get("/")
    def index(request: Request) -> Any:
        response = FileResponse(ASSETS / "index.html")
        key = request.cookies.get("planner_workspace", "")
        if len(key) != 48 or any(c not in "0123456789abcdef" for c in key):
            response.set_cookie(
                "planner_workspace",
                secrets.token_hex(24),
                httponly=True,
                samesite="strict",
                max_age=31536000,
                secure=request.url.scheme == "https",
            )
        return response

    @app.get("/api/state")
    def state(request: Request) -> Any:
        ws = workspace(request)
        saved = ws.directory / "plan.json"
        return {
            "job": ws.status,
            "rf": encode(RFSettings()),
            "candidates": encode(CandidateSettings()),
            "terrain": {
                kind: [p.name for p in (ws.directory / kind).glob("*.tif")]
                for kind in ("dtm", "dom")
            },
            "plan": json.loads(saved.read_text()) if saved.exists() else None,
        }

    @app.post("/api/terrain/{kind}")
    def upload(kind: str, request: Request, file: UploadFile) -> Any:
        if kind not in {"dtm", "dom"}:
            raise HTTPException(404)
        ws = workspace(request)
        with ws.lock:
            if ws.status["state"] == "running":
                raise HTTPException(409, "Wait for the current job")
            directory = ws.directory / kind
            directory.mkdir(exist_ok=True)
            path = directory / (secrets.token_hex(8) + ".tif")
            try:
                size = 0
                with path.open("wb") as out:
                    while chunk := file.file.read(1024 * 1024):
                        size += len(chunk)
                        if size > 512 * 1024 * 1024:
                            raise ValueError("Tile exceeds 512 MiB")
                        out.write(chunk)
                with RasterTerrain([path]) as terrain:
                    crs = CRS(terrain.crs)
                    if not crs.is_projected or not all(
                        a.unit_name == "metre" for a in crs.axis_info
                    ):
                        raise ValueError("Use a projected GeoTIFF with metre coordinates")
                    reverse = Transformer.from_crs(crs, 4326, always_xy=True)
                    left, bottom, right, top = terrain.bounds
                    bounds = [
                        list(reversed(reverse.transform(left, bottom))),
                        list(reversed(reverse.transform(right, top))),
                    ]
                return {"name": path.name, "bounds": bounds}
            except Exception as exc:
                path.unlink(missing_ok=True)
                raise HTTPException(400, str(exc)) from exc

    @app.delete("/api/terrain/{kind}")
    def clear(kind: str, request: Request) -> Any:
        if kind not in {"dtm", "dom"}:
            raise HTTPException(404)
        ws = workspace(request)
        with ws.lock:
            if ws.status["state"] == "running":
                raise HTTPException(409, "Wait for the current job")
            for path in (ws.directory / kind).glob("*.tif"):
                path.unlink()
        return {"ok": True}

    @app.post("/api/optimize")
    def optimize(body: dict[str, Any], request: Request) -> Any:
        ws = workspace(request)
        try:
            rf = settings(RFSettings, body.get("rf", {}))
            candidates = settings(CandidateSettings, body.get("candidates", {}))
            validate(rf, candidates)
            for name in ("a", "b"):
                lat, lon = body[name]
                if (
                    not math.isfinite(lat)
                    or not math.isfinite(lon)
                    or not -90 <= lat <= 90
                    or not -180 <= lon <= 180
                ):
                    raise ValueError("Invalid endpoint coordinates")
            if body["a"] == body["b"]:
                raise ValueError("Place endpoints at different locations")
        except (ValueError, TypeError, KeyError) as exc:
            raise HTTPException(422, str(exc)) from exc
        with ws.lock:
            if ws.status["state"] == "running":
                raise HTTPException(409, "A job is already running")
            dtm = list((ws.directory / "dtm").glob("*.tif"))
            dom = list((ws.directory / "dom").glob("*.tif"))
            if not dtm:
                raise HTTPException(422, "Upload DTM terrain first")
            ws.cancel.clear()
            ws.result = None
            ws.inputs = body
            (ws.directory / "plan.json").write_text(json.dumps(body), encoding="utf-8")
            ws.status = {"state": "running", "stage": "Opening terrain", "done": 0, "total": 1}

        def run() -> None:
            try:
                with RasterTerrain(dtm, dom) as terrain:
                    forward = Transformer.from_crs(4326, terrain.crs, always_xy=True)
                    reverse = Transformer.from_crs(terrain.crs, 4326, always_xy=True)
                    endpoints = []
                    for name, kind, antenna in (
                        ("a", SiteKind.ENDPOINT_A, rf.endpoint_a),
                        ("b", SiteKind.ENDPOINT_B, rf.endpoint_b),
                    ):
                        lat, lon = body[name]
                        x, y = forward.transform(lon, lat)
                        ground = float(terrain.sample(np.array([x]), np.array([y]))[0])
                        if not math.isfinite(ground):
                            raise ValueError(f"Endpoint {name.upper()} is outside terrain coverage")
                        endpoints.append(
                            Site(
                                name.upper(),
                                x,
                                y,
                                lat,
                                lon,
                                kind,
                                ground_elevation_m=ground,
                                antenna_height_m=antenna.height_agl_m,
                            )
                        )

                    def progress(stage: str, done: int, total: int) -> None:
                        ws.status = {
                            "state": "running",
                            "stage": stage,
                            "done": done,
                            "total": total,
                        }

                    result = RouteOptimizer(terrain, rf, candidates).optimize(
                        endpoints[0], endpoints[1], progress=progress, cancelled=ws.cancel.is_set
                    )
                    solution_sites = (
                        result.active_solution.sites if result.active_solution else result.route
                    )
                    for site in [*solution_sites, *result.candidates]:
                        site.longitude, site.latitude = reverse.transform(site.x, site.y)
                    if ws.cancel.is_set():
                        ws.status = {"state": "cancelled", "stage": "Optimization cancelled"}
                    else:
                        ws.result = result
                        ws.status = {
                            "state": "complete",
                            "stage": "Route found" if result.found else "No route found",
                        }
            except Exception as exc:
                ws.status = {"state": "error", "stage": str(exc)}

        threading.Thread(target=run, daemon=True).start()
        return ws.status

    @app.post("/api/cancel")
    def cancel(request: Request) -> Any:
        workspace(request).cancel.set()
        return {"ok": True}

    @app.get("/api/result")
    def result(request: Request) -> Any:
        ws = workspace(request)
        if ws.result is None:
            raise HTTPException(404, "No completed result")
        result = ws.result
        sites = result.active_solution.sites if result.active_solution else result.route
        return {
            "found": result.found,
            "router_count": result.router_count,
            "route": encode(sites),
            "links": [
                {**encode(link), "worst_margin_db": link.worst_margin_db} for link in result.links
            ],
            "diagnostics": result.diagnostics,
            "elapsed_seconds": result.elapsed_seconds,
            "candidates": encode(result.candidates),
        }

    @app.get("/api/export/{kind}")
    def export(kind: str, request: Request) -> Any:
        ws = workspace(request)
        if kind == "project":
            path = ws.directory / "plan.json"
            if not path.exists():
                raise HTTPException(404, "Run a plan first")
            return FileResponse(path, filename="route.webplan.json")
        if kind not in {"csv", "geojson"} or ws.result is None:
            raise HTTPException(404, "No result to export")
        path = ws.directory / f"route.{kind}"
        (export_route_csv if kind == "csv" else export_route_geojson)(ws.result, path)
        return FileResponse(path, filename=path.name)

    app.mount("/assets", StaticFiles(directory=ASSETS), name="assets")
    app.mount(
        "/leaflet",
        StaticFiles(directory=Path(__file__).parent / "data" / "leaflet"),
        name="leaflet",
    )
    return app


def main() -> None:
    import uvicorn

    parser = argparse.ArgumentParser(description="Self-hosted RF Router Planner")
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=8000)
    args = parser.parse_args()
    uvicorn.run(create_app(), host=args.host, port=args.port)


if __name__ == "__main__":
    main()
