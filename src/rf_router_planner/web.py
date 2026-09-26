"""Single-process, self-hosted planner with isolated browser workspaces."""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import secrets
import shutil
import tempfile
import threading
import time
from concurrent.futures import ThreadPoolExecutor
from dataclasses import fields, is_dataclass, replace
from enum import Enum
from pathlib import Path
from typing import Any

import numpy as np
from fastapi import FastAPI, HTTPException, Request, UploadFile
from fastapi.responses import FileResponse, JSONResponse
from fastapi.staticfiles import StaticFiles
from pyproj import CRS, Transformer

from .coordinates import norway_utm_epsg
from .export.csv_export import export_route_csv
from .export.geojson import export_route_geojson
from .integrations.corescope import CoreScopeClient
from .models.settings import CandidateSettings, InfrastructurePolicy, RFSettings, TerrainSettings
from .models.site import Site, SiteKind, SiteOrigin
from .optimization.cache import LinkMetricsCache
from .optimization.optimizer import OptimizationResult, RouteOptimizer
from .terrain.kartverket import KartverketProvider, RouteCorridor, load_services
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


def solution_id(solution: Any) -> str:
    """Identify an alternative by its topology rather than its list position."""
    signature = json.dumps(
        {
            "routers": sorted(solution.router_ids),
            "paths": [
                [list(path) for path in paths]
                for _, paths in sorted(solution.client_paths.items())
            ],
        },
        sort_keys=True,
        default=list,
        separators=(",", ":"),
    )
    return "alt-" + hashlib.sha256(signature.encode("utf-8")).hexdigest()[:16]


def result_payload(
    result: OptimizationResult,
    job_id: str | None,
    input_revision: int,
    snapshot_version: int,
) -> dict[str, Any]:
    solution = result.active_solution
    sites = solution.sites if solution else result.route
    existing_router_count = (
        solution.existing_router_count
        if solution
        else sum(site.origin == SiteOrigin.KNOWN for site in result.route[1:-1])
    )
    alternatives = []
    for item in result.alternatives:
        site_by_id = {site.id: site for site in item.sites}
        primary_ids = item.primary_path_ids
        primary_links = [
            next(
                (
                    link
                    for link in item.links
                    if {link.source_id, link.target_id} == {source, target}
                ),
                None,
            )
            for source, target in zip(primary_ids, primary_ids[1:], strict=False)
        ]
        alternatives.append(
            {
                "id": solution_id(item),
                "name": item.name,
                "router_count": item.router_count,
                "existing_router_count": item.existing_router_count,
                "proposed_router_count": item.proposed_router_count,
                "minimum_margin_db": (
                    min(link.worst_margin_db for link in item.links) if item.links else None
                ),
                "total_link_distance_m": sum(link.distance_m for link in item.links),
                "primary_distance_m": sum(
                    link.distance_m for link in primary_links if link is not None
                ),
                "requested_path_count": item.requested_path_count,
                "achieved_path_count": item.achieved_path_count,
                "resilient": item.resilient,
                "search_complete": result.search_complete,
                "selected": bool(solution and solution_id(item) == solution_id(solution)),
                "route": [encode(site_by_id[site_id]) for site_id in primary_ids],
            }
        )
    return {
        "found": result.found,
        "router_count": result.router_count,
        "existing_router_count": existing_router_count,
        "proposed_router_count": max(0, result.router_count - existing_router_count),
        "route": encode(sites),
        "links": [
            {**encode(link), "worst_margin_db": link.worst_margin_db} for link in result.links
        ],
        "diagnostics": result.diagnostics,
        "elapsed_seconds": result.elapsed_seconds,
        "search_complete": result.search_complete,
        "requested_path_count": solution.requested_path_count if solution else 1,
        "achieved_path_count": solution.achieved_path_count if solution else 1,
        "job_id": job_id,
        "input_revision": input_revision,
        "snapshot_version": snapshot_version,
        "candidates": encode(result.candidates),
        "active_alternative_id": solution_id(solution) if solution else None,
        "alternatives": alternatives,
    }


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


def coordinate_pair(body: dict[str, Any], key: str) -> tuple[float, float]:
    values = body.get(key)
    if not isinstance(values, list) or len(values) != 2:
        raise ValueError(f"{key} must contain latitude and longitude")
    latitude, longitude = values
    if (
        not isinstance(latitude, (int, float))
        or isinstance(latitude, bool)
        or not math.isfinite(latitude)
        or not -90 <= latitude <= 90
        or not isinstance(longitude, (int, float))
        or isinstance(longitude, bool)
        or not math.isfinite(longitude)
        or not -180 <= longitude <= 180
    ):
        raise ValueError(f"Invalid {key} coordinates")
    return float(latitude), float(longitude)


def distance_to_segment_m(
    point: tuple[float, float],
    start: tuple[float, float],
    end: tuple[float, float],
) -> float:
    latitude_scale = 111_320.0
    longitude_scale = latitude_scale * max(0.1, math.cos(math.radians((start[0] + end[0]) / 2)))
    px, py = point[1] * longitude_scale, point[0] * latitude_scale
    ax, ay = start[1] * longitude_scale, start[0] * latitude_scale
    bx, by = end[1] * longitude_scale, end[0] * latitude_scale
    dx, dy = bx - ax, by - ay
    length_squared = dx * dx + dy * dy
    if length_squared == 0:
        return math.hypot(px - ax, py - ay)
    fraction = max(0.0, min(1.0, ((px - ax) * dx + (py - ay) * dy) / length_squared))
    return math.hypot(px - (ax + fraction * dx), py - (ay + fraction * dy))


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
        self.job_id: str | None = None
        self.input_revision = 0
        self.snapshot_version = 0
        self.keep_result_on_cancel = False


def _terrain_paths(workspace: Workspace, kind: str) -> list[Path]:
    flat = list((workspace.directory / kind).glob("*.tif"))
    generated = list(
        (workspace.directory / "terrain-generations").glob(f"generation-*/{kind}/*.tif")
    )
    return sorted([*flat, *generated])


def create_app(data_dir: Path | None = None) -> FastAPI:
    root = (data_dir or Path(os.environ.get("RF_PLANNER_DATA", "web-data"))).resolve()
    root.mkdir(parents=True, exist_ok=True)
    app = FastAPI(title="RF Router Planner")
    workspaces: dict[str, Workspace] = {}
    registry_lock = threading.Lock()
    rf_cache = LinkMetricsCache(
        max_entries=max(1, int(os.environ.get("RF_PLANNER_RF_CACHE_ENTRIES", "50000")))
    )
    max_workers = max(1, int(os.environ.get("RF_PLANNER_MAX_ACTIVE_JOBS", "1")))
    scheduler = ThreadPoolExecutor(max_workers=max_workers, thread_name_prefix="rf-plan")
    scheduler_lock = threading.Lock()
    job_slots = {"outstanding": 0}
    max_outstanding_jobs = max_workers * 2

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

    def build_download_plan(
        ws: Workspace, body: dict[str, Any]
    ) -> tuple[KartverketProvider, str, Any, bool]:
        candidates = settings(CandidateSettings, body.get("candidates", {}))
        options = body.get("terrain", {})
        if not isinstance(options, dict) or set(options) - {
            "requested_resolution_m",
            "auto_resolution",
            "include_dom",
        }:
            raise ValueError("Terrain options may contain resolution, auto_resolution, and include_dom")
        requested_resolution = options.get("requested_resolution_m", 10.0)
        if (
            not isinstance(requested_resolution, (int, float))
            or isinstance(requested_resolution, bool)
            or not math.isfinite(requested_resolution)
            or not 1 <= requested_resolution <= 1000
        ):
            raise ValueError("Terrain resolution must be between 1 and 1,000 m")
        auto_resolution = options.get("auto_resolution", True)
        include_dom = options.get("include_dom", False)
        if not isinstance(auto_resolution, bool) or not isinstance(include_dom, bool):
            raise ValueError("Terrain auto-resolution and surface-data options must be boolean")

        endpoints = [coordinate_pair(body, "a"), coordinate_pair(body, "b")]
        if endpoints[0] == endpoints[1]:
            raise ValueError("Place endpoints at different locations")
        router_points: list[tuple[float, float]] = []
        known_data = body.get("known_routers", [])
        if not isinstance(known_data, list):
            raise ValueError("known_routers must be a list")
        if candidates.infrastructure_policy != InfrastructurePolicy.PROPOSED_ONLY:
            for item in known_data:
                if not isinstance(item, dict):
                    raise ValueError("Each MeshCore router must be an object")
                if item.get("policy", "optional") != "excluded":
                    router = coordinate_pair(
                        {"router": [item.get("latitude"), item.get("longitude")]}, "router"
                    )
                    router_points.append(router)
        manual_data = body.get("manual_routers", [])
        if not isinstance(manual_data, list) or any(
            not isinstance(item, dict) for item in manual_data
        ):
            raise ValueError("manual_routers must be a list of router objects")
        if candidates.infrastructure_policy != InfrastructurePolicy.EXISTING_ONLY:
            for item in manual_data:
                if item.get("policy", "optional") != "excluded":
                    router_points.append(
                        coordinate_pair(
                            {"router": [item.get("latitude"), item.get("longitude")]}, "router"
                        )
                    )
        all_points = [*endpoints, *router_points]
        longitude = sum(point[1] for point in all_points) / len(all_points)
        crs = f"EPSG:{norway_utm_epsg(longitude)}"
        forward = Transformer.from_crs(4326, crs, always_xy=True)
        projected = [forward.transform(lon, lat) for lat, lon in endpoints]
        extra_points = tuple(forward.transform(lon, lat) for lat, lon in router_points)
        padding = float(candidates.corridor_width_m)
        all_x = [point[0] for point in [*projected, *extra_points]]
        all_y = [point[1] for point in [*projected, *extra_points]]
        bounds = (
            min(all_x) - padding,
            min(all_y) - padding,
            max(all_x) + padding,
            max(all_y) + padding,
        )
        defaults = TerrainSettings()
        provider = KartverketProvider(
            load_services(Path(__file__).parent / "data" / "kartverket_wcs.json"),
            ws.directory / "kartverket-cache",
            defaults.maximum_download_area_km2,
            defaults.maximum_pixels_per_tile,
            defaults.maximum_download_tiles,
        )
        route_corridor = RouteCorridor(tuple(projected), padding, extra_points)
        plan = provider.plan_download(
            bounds,
            float(requested_resolution),
            corridor=route_corridor,
            auto_resolution=auto_resolution,
            maximum_total_pixels=defaults.maximum_total_pixels,
        )
        return provider, crs, plan, include_dom

    def terrain_coverage_report(ws: Workspace, body: dict[str, Any]) -> dict[str, Any]:
        dtm_paths = _terrain_paths(ws, "dtm")
        dom_paths = _terrain_paths(ws, "dom")
        if not dtm_paths:
            raise ValueError("Prepare or upload ground terrain first")
        with RasterTerrain(dtm_paths, dom_paths) as terrain:
            forward = Transformer.from_crs(4326, terrain.crs, always_xy=True)
            reverse = Transformer.from_crs(terrain.crs, 4326, always_xy=True)
            named_sites = [("Endpoint A", *coordinate_pair(body, "a")), ("Endpoint B", *coordinate_pair(body, "b"))]
            candidates = settings(CandidateSettings, body.get("candidates", {}))
            if candidates.infrastructure_policy != InfrastructurePolicy.PROPOSED_ONLY:
                known_routers = body.get("known_routers", [])
                if not isinstance(known_routers, list) or any(
                    not isinstance(item, dict) for item in known_routers
                ):
                    raise ValueError("Known routers must be a list of router objects")
                for item in known_routers:
                    if item.get("policy", "optional") != "excluded":
                        lat, lon = coordinate_pair(
                            {"router": [item.get("latitude"), item.get("longitude")]}, "router"
                        )
                        named_sites.append((str(item.get("name") or item.get("id")), lat, lon))
            manual_routers = body.get("manual_routers", [])
            if not isinstance(manual_routers, list) or any(
                not isinstance(item, dict) for item in manual_routers
            ):
                raise ValueError("manual_routers must be a list of router objects")
            if candidates.infrastructure_policy != InfrastructurePolicy.EXISTING_ONLY:
                for item in manual_routers:
                    if item.get("policy", "optional") != "excluded":
                        lat, lon = coordinate_pair(
                            {"router": [item.get("latitude"), item.get("longitude")]}, "router"
                        )
                        named_sites.append((str(item.get("name") or item.get("id")), lat, lon))
            uncovered: list[dict[str, Any]] = []
            for name, lat, lon in named_sites:
                x, y = forward.transform(lon, lat)
                if not math.isfinite(float(terrain.sample(np.array([x]), np.array([y]))[0])):
                    left, bottom, right, top = terrain.bounds
                    inside = left <= x <= right and bottom <= y <= top
                    uncovered.append(
                        {
                            "name": name,
                            "latitude": lat,
                            "longitude": lon,
                            "reason": "no_data" if inside else "outside_coverage",
                        }
                    )

            start = forward.transform(named_sites[0][2], named_sites[0][1])
            end = forward.transform(named_sites[1][2], named_sites[1][1])
            distance = math.hypot(end[0] - start[0], end[1] - start[1])
            count = min(2001, max(2, math.ceil(distance / max(100.0, 4 * terrain.resolution_m)) + 1))
            line_gaps: list[list[list[float]]] = []
            active_gap: list[list[float]] = []
            for index in range(count):
                fraction = index / (count - 1)
                x = start[0] + (end[0] - start[0]) * fraction
                y = start[1] + (end[1] - start[1]) * fraction
                valid = math.isfinite(float(terrain.sample(np.array([x]), np.array([y]))[0]))
                if valid:
                    if active_gap:
                        line_gaps.append(active_gap)
                        active_gap = []
                else:
                    lon, lat = reverse.transform(x, y)
                    active_gap.append([lat, lon])
            if active_gap:
                line_gaps.append(active_gap)

        import rasterio

        def outlines(paths: list[Path]) -> list[list[list[float]]]:
            polygons = []
            for path in paths:
                with rasterio.open(path) as dataset:
                    if dataset.crs is None:
                        continue
                    transform = Transformer.from_crs(dataset.crs, 4326, always_xy=True)
                    bounds = dataset.bounds
                    corners = [
                        transform.transform(bounds.left, bounds.bottom),
                        transform.transform(bounds.left, bounds.top),
                        transform.transform(bounds.right, bounds.top),
                        transform.transform(bounds.right, bounds.bottom),
                    ]
                    polygons.append([[lat, lon] for lon, lat in corners])
            return polygons

        return {
            "covered": not uncovered,
            "uncovered_sites": uncovered,
            "line_gaps": line_gaps,
            "dtm_outlines": outlines(dtm_paths),
            "dom_outlines": outlines(dom_paths),
            "surface_available": bool(dom_paths),
        }

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
            "job": {
                **ws.status,
                "job_id": ws.job_id,
                "input_revision": ws.input_revision,
                "snapshot_version": ws.snapshot_version,
            },
            "rf_cache": rf_cache.stats(ws.directory.name),
            "rf": encode(RFSettings()),
            "candidates": encode(CandidateSettings()),
            "terrain": {
                kind: [p.name for p in _terrain_paths(ws, kind)]
                for kind in ("dtm", "dom")
            },
            "plan": json.loads(saved.read_text()) if saved.exists() else None,
        }

    @app.post("/api/terrain/estimate")
    def estimate_terrain(body: dict[str, Any], request: Request) -> Any:
        ws = workspace(request)
        try:
            provider, crs, plan, include_dom = build_download_plan(ws, body)
            products = 2 if include_dom else 1
            dtm_cached = provider.cached_tile_count("dtm", crs, plan)
            dom_cached = provider.cached_tile_count("dom", crs, plan) if include_dom else 0
            estimated_raw_mib = plan.estimated_raw_mib_per_product * products
            estimated_disk_mib = estimated_raw_mib * 2
            return {
                "crs": crs,
                "summary": plan.summary(products),
                "tile_count": len(plan.tiles),
                "pixel_count": plan.pixel_count,
                "download_area_km2": plan.download_area_km2,
                "requested_resolution_m": plan.requested_resolution_m,
                "effective_resolution_m": plan.effective_resolution_m,
                "estimated_raw_mib": estimated_raw_mib,
                "estimated_disk_mib": estimated_disk_mib,
                "available_disk_mib": shutil.disk_usage(ws.directory).free / (1024 * 1024),
                "cache_hits": dtm_cached + dom_cached,
                "cache_total": len(plan.tiles) * products,
                "include_dom": include_dom,
            }
        except (ValueError, TypeError, KeyError) as exc:
            raise HTTPException(422, str(exc)) from exc

    @app.post("/api/terrain/coverage")
    def terrain_coverage(body: dict[str, Any], request: Request) -> Any:
        ws = workspace(request)
        try:
            return terrain_coverage_report(ws, body)
        except (ValueError, TypeError, KeyError) as exc:
            raise HTTPException(422, str(exc)) from exc

    @app.post("/api/terrain/prepare")
    def prepare_terrain(body: dict[str, Any], request: Request) -> Any:
        ws = workspace(request)
        try:
            provider, crs, plan, include_dom = build_download_plan(ws, body)
        except (ValueError, TypeError, KeyError) as exc:
            raise HTTPException(422, str(exc)) from exc
        with ws.lock:
            if ws.status["state"] in {"queued", "running", "preparing"}:
                raise HTTPException(409, "A planner or terrain job is already active")
            with scheduler_lock:
                if job_slots["outstanding"] >= max_outstanding_jobs:
                    raise HTTPException(429, "The planner queue is full; retry shortly")
                queued = job_slots["outstanding"] >= max_workers
                job_slots["outstanding"] += 1
            ws.cancel.clear()
            ws.result = None
            ws.snapshot_version = 0
            ws.job_id = secrets.token_hex(12)
            ws.input_revision += 1
            job_id = ws.job_id
            total = len(plan.tiles) * (2 if include_dom else 1)
            ws.status = {
                "state": "queued" if queued else "preparing",
                "stage": "Waiting for a terrain worker" if queued else "Preparing Kartverket terrain",
                "job_id": job_id,
                "input_revision": ws.input_revision,
                "snapshot_version": 0,
                "done": 0,
                "total": total,
                "started_at": time.time(),
                "terrain_estimate": {
                    "crs": crs,
                    "tile_count": len(plan.tiles),
                    "include_dom": include_dom,
                    "effective_resolution_m": plan.effective_resolution_m,
                },
            }

        def run() -> None:
            generation_root = ws.directory / "terrain-generations"
            staging: Path | None = None
            try:
                with ws.lock:
                    if ws.job_id != job_id:
                        return
                    if ws.cancel.is_set():
                        ws.status = {**ws.status, "state": "cancelled", "stage": "Cancelled before starting"}
                        return
                    ws.status = {**ws.status, "state": "preparing", "stage": "Downloading ground terrain"}

                def report(product: str, tile_number: int, tile_total: int, cached: bool) -> None:
                    base = 0 if product == "dtm" else len(plan.tiles)
                    with ws.lock:
                        if ws.job_id == job_id:
                            ws.status = {
                                **ws.status,
                                "state": "preparing",
                                "stage": f"Kartverket {product.upper()} tile {tile_number}/{tile_total}"
                                + (" (cache hit)" if cached else ""),
                                "done": base + tile_number - 1,
                                "total": total,
                            }

                dtm_sources = provider.fetch_plan(
                    "dtm",
                    crs,
                    plan,
                    lambda index, count, cached: report("dtm", index, count, cached),
                    ws.cancel.is_set,
                )
                dom_sources: list[Path] = []
                if include_dom:
                    with ws.lock:
                        ws.status = {**ws.status, "stage": "Downloading surface terrain"}
                    dom_sources = provider.fetch_plan(
                        "dom",
                        crs,
                        plan,
                        lambda index, count, cached: report("dom", index, count, cached),
                        ws.cancel.is_set,
                    )
                if ws.cancel.is_set():
                    raise RuntimeError("Terrain download cancelled")

                generation_root.mkdir(parents=True, exist_ok=True)
                staging = Path(tempfile.mkdtemp(prefix=".staging-", dir=generation_root))
                (staging / "dtm").mkdir()
                if dom_sources:
                    (staging / "dom").mkdir()
                for index, source in enumerate(dtm_sources, 1):
                    shutil.copy2(source, staging / "dtm" / f"tile-{index:04d}.tif")
                for index, source in enumerate(dom_sources, 1):
                    shutil.copy2(source, staging / "dom" / f"tile-{index:04d}.tif")
                with RasterTerrain(
                    list((staging / "dtm").glob("*.tif")),
                    list((staging / "dom").glob("*.tif")) if dom_sources else [],
                ) as terrain:
                    projected = CRS(terrain.crs)
                    if not projected.is_projected or not all(
                        axis.unit_name == "metre" for axis in projected.axis_info
                    ):
                        raise ValueError("Kartverket terrain must use projected metre coordinates")
                if ws.cancel.is_set():
                    raise RuntimeError("Terrain download cancelled")

                with ws.lock:
                    if ws.job_id != job_id or ws.cancel.is_set():
                        raise RuntimeError("Terrain download cancelled")
                    destination = generation_root / f"generation-{job_id}"
                    staging.replace(destination)
                    staging = None
                    ws.inputs = body
                    (ws.directory / "plan.json").write_text(json.dumps(body), encoding="utf-8")
                    ws.input_revision += 1
                    ws.status = {
                        **ws.status,
                        "state": "terrain_ready",
                        "stage": "Terrain prepared and ready",
                        "done": total,
                        "total": total,
                        "input_revision": ws.input_revision,
                    }
            except RuntimeError as exc:
                with ws.lock:
                    if ws.job_id == job_id:
                        cancelled = ws.cancel.is_set() or "cancelled" in str(exc).lower()
                        ws.status = {
                            **ws.status,
                            "state": "cancelled" if cancelled else "failed",
                            "stage": str(exc),
                        }
            except Exception as exc:
                with ws.lock:
                    if ws.job_id == job_id:
                        ws.status = {**ws.status, "state": "failed", "stage": str(exc)}
            finally:
                if staging is not None:
                    shutil.rmtree(staging, ignore_errors=True)
                with scheduler_lock:
                    job_slots["outstanding"] = max(0, job_slots["outstanding"] - 1)

        scheduler.submit(run)
        return ws.status

    @app.post("/api/terrain/{kind}")
    def upload(kind: str, request: Request, file: UploadFile) -> Any:
        if kind not in {"dtm", "dom"}:
            raise HTTPException(404)
        ws = workspace(request)
        with ws.lock:
            if ws.status["state"] in {"queued", "running", "preparing"}:
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
                ws.input_revision += 1
                ws.result = None
                ws.snapshot_version = 0
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
            if ws.status["state"] in {"queued", "running", "preparing"}:
                raise HTTPException(409, "Wait for the current job")
            for path in _terrain_paths(ws, kind):
                path.unlink()
            ws.input_revision += 1
            ws.result = None
            ws.snapshot_version = 0
        return {"ok": True}

    @app.post("/api/meshcore")
    def meshcore(body: dict[str, Any], request: Request) -> Any:
        workspace(request)
        try:
            start, end = coordinate_pair(body, "a"), coordinate_pair(body, "b")
            corridor_width = body.get("corridor_width_m", 10_000)
            if (
                not isinstance(corridor_width, (int, float))
                or isinstance(corridor_width, bool)
                or not math.isfinite(corridor_width)
                or not 1 <= corridor_width <= 500_000
            ):
                raise ValueError("Corridor width must be between 1 and 500,000 m")
            repeaters = CoreScopeClient().fetch_repeaters()
            nearby: list[dict[str, Any]] = []
            for repeater in repeaters:
                distance = distance_to_segment_m(
                    (repeater.latitude, repeater.longitude), start, end
                )
                if distance > corridor_width:
                    continue
                nearby.append(
                    {
                        "id": repeater.id,
                        "name": repeater.name,
                        "latitude": repeater.latitude,
                        "longitude": repeater.longitude,
                        "corridor_distance_m": round(distance),
                        "freshness": repeater.freshness,
                        "last_heard": repeater.last_heard.isoformat()
                        if repeater.last_heard
                        else None,
                        "relay_active": repeater.relay_active,
                        "relay_count_24h": repeater.relay_count_24h,
                        "provenance": "CoreScope",
                    }
                )
            nearby.sort(key=lambda item: (item["corridor_distance_m"], item["name"].casefold()))
            return {"routers": nearby, "count": len(nearby)}
        except (ValueError, TypeError) as exc:
            raise HTTPException(422, str(exc)) from exc
        except Exception as exc:
            raise HTTPException(
                502, f"Could not load MeshCore routers from CoreScope: {exc}"
            ) from exc

    @app.post("/api/optimize")
    def optimize(body: dict[str, Any], request: Request) -> Any:
        ws = workspace(request)
        try:
            rf = settings(RFSettings, body.get("rf", {}))
            candidates = settings(CandidateSettings, body.get("candidates", {}))
            validate(rf, candidates)
            search_effort = body.get("search_effort", "balanced")
            if search_effort not in {"quick", "balanced", "thorough"}:
                raise ValueError("search_effort must be quick, balanced, or thorough")
            candidate_cap, neighbor_cap = {
                "quick": (200, 8),
                "balanced": (800, 12),
                "thorough": (candidates.maximum_candidates, candidates.maximum_neighbors_per_site),
            }[search_effort]
            requested_candidate_count = candidates.maximum_candidates
            candidates.maximum_candidates = min(candidates.maximum_candidates, candidate_cap)
            candidates.maximum_neighbors_per_site = min(
                candidates.maximum_neighbors_per_site, neighbor_cap
            )
            body = {
                **body,
                "search_effort": search_effort,
                "resolved_search": {
                    "candidate_limit": candidates.maximum_candidates,
                    "requested_candidate_limit": requested_candidate_count,
                    "neighbor_limit": candidates.maximum_neighbors_per_site,
                },
            }
            known_data = body.get("known_routers", [])
            if not isinstance(known_data, list):
                raise ValueError("known_routers must be a list")
            known_ids: set[str] = set()
            included_known_count = 0
            required_known_count = 0
            for item in known_data:
                if not isinstance(item, dict):
                    raise ValueError("Each MeshCore router must be an object")
                site_id = item.get("id")
                if (
                    not isinstance(site_id, str)
                    or not site_id.startswith("K-")
                    or len(site_id) > 64
                    or any(not (character.isalnum() or character in "-_") for character in site_id)
                ):
                    raise ValueError("Each MeshCore router needs a valid id")
                if site_id in known_ids:
                    raise ValueError("MeshCore router ids must be unique")
                known_ids.add(site_id)
                coordinate_pair({"router": [item.get("latitude"), item.get("longitude")]}, "router")
                router_policy = item.get("policy", "optional")
                if router_policy not in {"optional", "required", "excluded"}:
                    raise ValueError("MeshCore router policy must be optional, required, or excluded")
                if (
                    candidates.infrastructure_policy != InfrastructurePolicy.PROPOSED_ONLY
                    and router_policy != "excluded"
                ):
                    included_known_count += 1
                    required_known_count += router_policy == "required"
            manual_data = body.get("manual_routers", [])
            if not isinstance(manual_data, list):
                raise ValueError("manual_routers must be a list")
            included_manual_count = 0
            required_manual_count = 0
            for item in manual_data:
                if not isinstance(item, dict):
                    raise ValueError("Each proposed router must be an object")
                site_id = item.get("id")
                if (
                    not isinstance(site_id, str)
                    or not site_id.startswith("M-")
                    or len(site_id) > 64
                    or any(not (character.isalnum() or character in "-_") for character in site_id)
                ):
                    raise ValueError("Each proposed router needs a valid id")
                if site_id in known_ids:
                    raise ValueError("Router ids must be unique")
                known_ids.add(site_id)
                coordinate_pair({"router": [item.get("latitude"), item.get("longitude")]}, "router")
                router_policy = item.get("policy", "optional")
                if router_policy not in {"optional", "required", "excluded"}:
                    raise ValueError("Proposed router policy must be optional, required, or excluded")
                height = item.get("antenna_height_m", rf.router.height_agl_m)
                if (
                    not isinstance(height, (int, float))
                    or isinstance(height, bool)
                    or not math.isfinite(height)
                    or not 0 < height <= 1000
                ):
                    raise ValueError("Proposed-router antenna height must be between 0 and 1,000 m")
                if (
                    candidates.infrastructure_policy != InfrastructurePolicy.EXISTING_ONLY
                    and router_policy != "excluded"
                ):
                    included_manual_count += 1
                    required_manual_count += router_policy == "required"
            included_router_count = included_known_count + included_manual_count
            if included_router_count > candidates.maximum_candidates:
                raise ValueError("Selected routers exceed the Max candidates setting")
            if required_known_count + required_manual_count > candidates.maximum_solution_routers:
                raise ValueError("Required routers exceed the Max total routers setting")
            body["resolved_search"].update(
                {
                    "generated_candidate_limit": (
                        0
                        if candidates.infrastructure_policy == InfrastructurePolicy.EXISTING_ONLY
                        else candidates.maximum_candidates - included_router_count
                    ),
                    "known_router_count": included_known_count,
                    "excluded_router_count": len(known_data) - included_known_count,
                    "manual_router_count": included_manual_count,
                    "infrastructure_policy": candidates.infrastructure_policy.value,
                }
            )
            for name in ("a", "b"):
                coordinate_pair(body, name)
            if body["a"] == body["b"]:
                raise ValueError("Place endpoints at different locations")
        except (ValueError, TypeError, KeyError) as exc:
            raise HTTPException(422, str(exc)) from exc
        with ws.lock:
            if ws.status["state"] in {"queued", "running", "preparing"}:
                raise HTTPException(409, "A job is already queued or running")
            dtm = _terrain_paths(ws, "dtm")
            dom = _terrain_paths(ws, "dom")
            if not dtm:
                raise HTTPException(422, "Prepare or upload ground terrain first")
            try:
                coverage = terrain_coverage_report(ws, body)
            except (ValueError, TypeError, KeyError) as exc:
                raise HTTPException(422, str(exc)) from exc
            if coverage["uncovered_sites"]:
                names = ", ".join(item["name"] for item in coverage["uncovered_sites"])
                raise HTTPException(422, f"Ground-terrain coverage is missing at: {names}")
            with scheduler_lock:
                if job_slots["outstanding"] >= max_outstanding_jobs:
                    raise HTTPException(429, "The planner queue is full; retry shortly")
                queued = job_slots["outstanding"] >= max_workers
                job_slots["outstanding"] += 1
            ws.cancel.clear()
            ws.result = None
            ws.inputs = body
            ws.job_id = secrets.token_hex(12)
            ws.input_revision += 1
            ws.snapshot_version = 0
            ws.keep_result_on_cancel = False
            job_id = ws.job_id
            input_revision = ws.input_revision
            (ws.directory / "plan.json").write_text(json.dumps(body), encoding="utf-8")
            ws.status = {
                "state": "queued" if queued else "running",
                "stage": "Waiting for a planner worker" if queued else "Opening terrain",
                "job_id": job_id,
                "input_revision": input_revision,
                "snapshot_version": 0,
                "search_effort": search_effort,
                "resolved_search": body["resolved_search"],
                "started_at": time.time(),
            }

        def run() -> None:
            try:
                with ws.lock:
                    if ws.job_id != job_id:
                        return
                    if ws.cancel.is_set():
                        ws.status = {
                            **ws.status,
                            "state": "cancelled",
                            "stage": "Cancelled before starting",
                        }
                        return
                    ws.status = {
                        **ws.status,
                        "state": "running",
                        "stage": "Opening terrain",
                        "done": 0,
                        "total": 1,
                    }
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
                    active_known_data = (
                        []
                        if candidates.infrastructure_policy == InfrastructurePolicy.PROPOSED_ONLY
                        else [
                            item
                            for item in known_data
                            if item.get("policy", "optional") != "excluded"
                        ]
                    )
                    known_sites = []
                    for item in active_known_data:
                        latitude = float(item["latitude"])
                        longitude = float(item["longitude"])
                        x, y = forward.transform(longitude, latitude)
                        ground = float(terrain.sample(np.array([x]), np.array([y]))[0])
                        if not math.isfinite(ground):
                            raise ValueError(
                                f"MeshCore router {item['id']} is outside terrain coverage"
                            )
                        surface = None
                        if terrain.has_surface:
                            value = float(
                                terrain.sample(np.array([x]), np.array([y]), surface=True)[0]
                            )
                            surface = value if math.isfinite(value) else None
                        known_sites.append(
                            Site(
                                item["id"],
                                x,
                                y,
                                latitude,
                                longitude,
                                SiteKind.ROUTER,
                                ground_elevation_m=ground,
                                surface_elevation_m=surface,
                                antenna_height_m=rf.router.height_agl_m,
                                origin=SiteOrigin.KNOWN,
                                locked=True,
                                required=item.get("policy", "optional") == "required",
                            )
                        )
                    manual_sites = []
                    manual_data = body.get("manual_routers", [])
                    if candidates.infrastructure_policy != InfrastructurePolicy.EXISTING_ONLY:
                        for item in manual_data:
                            if item.get("policy", "optional") == "excluded":
                                continue
                            latitude = float(item["latitude"])
                            longitude = float(item["longitude"])
                            x, y = forward.transform(longitude, latitude)
                            ground = float(terrain.sample(np.array([x]), np.array([y]))[0])
                            if not math.isfinite(ground):
                                raise ValueError(
                                    f"Proposed router {item['id']} is outside terrain coverage"
                                )
                            surface = None
                            if terrain.has_surface:
                                value = float(
                                    terrain.sample(np.array([x]), np.array([y]), surface=True)[0]
                                )
                                surface = value if math.isfinite(value) else None
                            manual_sites.append(
                                Site(
                                    item["id"],
                                    x,
                                    y,
                                    latitude,
                                    longitude,
                                    SiteKind.ROUTER,
                                    ground_elevation_m=ground,
                                    surface_elevation_m=surface,
                                    antenna_height_m=float(
                                        item.get("antenna_height_m", rf.router.height_agl_m)
                                    ),
                                    origin=SiteOrigin.MANUAL,
                                    locked=True,
                                    required=item.get("policy", "optional") == "required",
                                    height_override=True,
                                )
                            )
                    required_sites = [
                        site for site in [*known_sites, *manual_sites] if site.required
                    ]
                    optional_sites = [
                        site for site in [*known_sites, *manual_sites] if not site.required
                    ]

                    def progress(stage: str, done: int, total: int) -> None:
                        with ws.lock:
                            if ws.job_id == job_id and ws.input_revision == input_revision:
                                ws.status = {
                                    **ws.status,
                                    "state": "running",
                                    "stage": stage,
                                    "done": done,
                                    "total": total,
                                }

                    def publish_snapshot(
                        snapshot: OptimizationResult, search_complete: bool
                    ) -> None:
                        published = snapshot
                        snapshot_sites = (
                            published.active_solution.sites
                            if published.active_solution
                            else published.route
                        )
                        for site in [*snapshot_sites, *published.candidates]:
                            site.longitude, site.latitude = reverse.transform(site.x, site.y)
                        with ws.lock:
                            if ws.job_id != job_id or ws.input_revision != input_revision:
                                return
                            ws.snapshot_version += 1
                            published.search_complete = search_complete
                            ws.result = published
                            ws.status = {
                                **ws.status,
                                "state": "running",
                                "stage": (
                                    "Route found; broader search continues"
                                    if not search_complete
                                    else "Search complete"
                                ),
                                "snapshot_version": ws.snapshot_version,
                                "search_complete": search_complete,
                            }

                    search_candidates = replace(
                        candidates,
                        maximum_candidates=(
                            0
                            if candidates.infrastructure_policy == InfrastructurePolicy.EXISTING_ONLY
                            else candidates.maximum_candidates - len(known_sites) - len(manual_sites)
                        ),
                    )
                    optimizer = RouteOptimizer(
                        terrain,
                        rf,
                        search_candidates,
                        evaluation_cache=rf_cache,
                        cache_namespace=ws.directory.name,
                    )
                    result = optimizer.optimize(
                        endpoints[0],
                        endpoints[1],
                        candidates=(
                            [endpoints[0], *required_sites, endpoints[1]]
                            if candidates.infrastructure_policy
                            == InfrastructurePolicy.EXISTING_ONLY
                            else None
                        ),
                        required_routers=required_sites,
                        optional_routers=optional_sites,
                        progress=progress,
                        cancelled=ws.cancel.is_set,
                        solution_progress=publish_snapshot,
                    )
                    solution_sites = (
                        result.active_solution.sites if result.active_solution else result.route
                    )
                    for site in [*solution_sites, *result.candidates]:
                        site.longitude, site.latitude = reverse.transform(site.x, site.y)
                    if ws.cancel.is_set():
                        with ws.lock:
                            stopped = ws.keep_result_on_cancel and ws.result is not None
                            if not stopped:
                                ws.result = None
                                ws.snapshot_version = 0
                            ws.status = {
                                **ws.status,
                                "state": "stopped" if stopped else "cancelled",
                                "stage": (
                                    "Stopped with the latest certified route"
                                    if stopped
                                    else "Optimization cancelled"
                                ),
                            }
                    else:
                        result.search_complete = True
                        with ws.lock:
                            if ws.job_id == job_id and ws.input_revision == input_revision:
                                ws.result = result
                                ws.snapshot_version += 1
                                ws.status = {
                                    **ws.status,
                                    "state": "complete",
                                    "stage": "Route found" if result.found else "No route found",
                                    "snapshot_version": ws.snapshot_version,
                                    "search_complete": True,
                                }
            except Exception as exc:
                with ws.lock:
                    if ws.job_id == job_id:
                        ws.status = {**ws.status, "state": "failed", "stage": str(exc)}
            finally:
                with scheduler_lock:
                    job_slots["outstanding"] = max(0, job_slots["outstanding"] - 1)

        scheduler.submit(run)
        return ws.status

    @app.post("/api/cancel")
    def cancel(request: Request, body: dict[str, Any] | None = None) -> Any:
        ws = workspace(request)
        with ws.lock:
            if body and body.get("job_id") not in {None, ws.job_id}:
                raise HTTPException(409, "This cancellation request refers to an older job")
            if ws.status["state"] not in {"queued", "running", "preparing"}:
                raise HTTPException(409, "There is no active job to cancel")
            ws.keep_result_on_cancel = False
            ws.cancel.set()
        return {"ok": True}

    @app.post("/api/stop-and-keep")
    def stop_and_keep(request: Request, body: dict[str, Any] | None = None) -> Any:
        ws = workspace(request)
        with ws.lock:
            if body and body.get("job_id") not in {None, ws.job_id}:
                raise HTTPException(409, "This request refers to an older job")
            if ws.status["state"] != "running" or ws.result is None:
                raise HTTPException(409, "A certified route is not available to keep")
            ws.keep_result_on_cancel = True
            ws.cancel.set()
        return {"ok": True}

    @app.get("/api/result")
    def result(request: Request) -> Any:
        ws = workspace(request)
        with ws.lock:
            if ws.result is None:
                raise HTTPException(404, "No certified route is available yet")
            return result_payload(
                ws.result, ws.job_id, ws.input_revision, ws.snapshot_version
            )

    @app.post("/api/alternatives/{alternative_id}/select")
    def select_alternative(alternative_id: str, request: Request) -> Any:
        ws = workspace(request)
        with ws.lock:
            if ws.status["state"] in {"queued", "running", "preparing"}:
                raise HTTPException(409, "Wait until the search completes before switching routes")
            if ws.result is None:
                raise HTTPException(404, "No route alternatives are available")
            selected_index = next(
                (
                    index
                    for index, alternative in enumerate(ws.result.alternatives)
                    if solution_id(alternative) == alternative_id
                ),
                None,
            )
            if selected_index is None:
                raise HTTPException(404, "That route alternative is no longer available")
            ws.result.select_solution(selected_index)
            ws.snapshot_version += 1
            ws.status = {**ws.status, "snapshot_version": ws.snapshot_version}
            return result_payload(
                ws.result, ws.job_id, ws.input_revision, ws.snapshot_version
            )

    @app.delete("/api/cache")
    def clear_cache(request: Request) -> Any:
        ws = workspace(request)
        with ws.lock:
            if ws.status["state"] in {"queued", "running", "preparing"}:
                raise HTTPException(409, "Wait for the current job")
            deleted = rf_cache.clear(ws.directory.name)
        return {"ok": True, "deleted_entries": deleted}

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
