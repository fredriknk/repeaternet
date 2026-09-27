"""Single-process, self-hosted planner with isolated browser workspaces."""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import secrets
import shutil
import sqlite3
import tempfile
import threading
import time
from collections.abc import Iterator
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
from .coverage.compare import compare_coverage_streams
from .coverage.engine import calculate_coverage, make_grid
from .coverage.scenarios import analyze_node_failures
from .export.csv_export import export_route_csv
from .export.geojson import export_route_geojson
from .integrations.corescope import CoreScopeClient
from .models.coverage import ClientRadioProfile, CoverageMode, CoverageSettings
from .models.settings import (
    CandidateSettings,
    InfrastructurePolicy,
    RFSettings,
    TerrainSettings,
    ValidationMode,
)
from .models.site import Site, SiteKind, SiteOrigin
from .optimization.cache import LinkMetricsCache
from .optimization.optimizer import OptimizationResult, RouteOptimizer
from .project_store import ProjectStore
from .rf.propagation import LinkEvaluator
from .terrain.kartverket import KartverketProvider, RouteCorridor, load_services
from .terrain.raster import RasterTerrain

ASSETS = Path(__file__).parent / "web_assets"
MAX_COVERAGE_RESULT_BYTES = 100 * 1024 * 1024
MAX_COVERAGE_BACKBONE_REVALIDATION_LINKS = 4_096
COVERAGE_MODEL_VERSION = "coverage-v1"


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


def parse_coverage_settings(value: Any) -> CoverageSettings:
    if not isinstance(value, dict):
        raise ValueError("Coverage settings must be an object")
    defaults = CoverageSettings()
    client_values = value.get("client", {})
    if not isinstance(client_values, dict):
        raise ValueError("Client radio settings must be an object")
    client = ClientRadioProfile()
    for key in (
        "name",
        "height_agl_m",
        "tx_power_dbm",
        "gain_dbi",
        "feed_loss_db",
        "sensitivity_dbm",
        "miscellaneous_loss_db",
    ):
        if key in client_values:
            setattr(client, key, client_values[key])
    if not isinstance(client.name, str) or not 1 <= len(client.name) <= 80:
        raise ValueError("Client radio profile name must contain 1 to 80 characters")
    for key, low, high in (
        ("height_agl_m", 0.1, 1_000),
        ("tx_power_dbm", -20, 60),
        ("gain_dbi", -30, 60),
        ("feed_loss_db", 0, 100),
        ("sensitivity_dbm", -200, 0),
        ("miscellaneous_loss_db", 0, 200),
    ):
        number = getattr(client, key)
        if not isinstance(number, (int, float)) or isinstance(number, bool):
            raise ValueError(f"Client {key} must be numeric")
        if not math.isfinite(number) or not low <= number <= high:
            raise ValueError(f"Client {key} must be between {low} and {high}")
        setattr(client, key, float(number))

    mode = CoverageMode(value.get("mode", defaults.mode.value))
    area_mode = value.get("area_mode", defaults.area_mode)
    if area_mode not in {"mesh", "view"}:
        raise ValueError("Coverage area mode must be mesh or view")

    def positive_number(key: str, default: float, maximum: float) -> float:
        item = value.get(key, default)
        if not isinstance(item, (int, float)) or isinstance(item, bool):
            raise ValueError(f"Coverage {key} must be numeric")
        if not math.isfinite(item) or not 0 < item <= maximum:
            raise ValueError(f"Coverage {key} must be above zero and at most {maximum}")
        return float(item)

    def positive_int(key: str, default: int, maximum: int) -> int:
        item = value.get(key, default)
        if not isinstance(item, int) or isinstance(item, bool) or not 1 <= item <= maximum:
            raise ValueError(f"Coverage {key} must be between 1 and {maximum}")
        return item

    include_endpoints = value.get("include_endpoints", defaults.include_endpoints)
    if not isinstance(include_endpoints, bool):
        raise ValueError("include_endpoints must be a boolean")
    source_ids = value.get("source_ids", defaults.source_ids)
    if (
        not isinstance(source_ids, list)
        or len(source_ids) > defaults.maximum_sources
        or any(
            not isinstance(source_id, str)
            or not source_id
            or len(source_id) > 128
            or any(ord(character) < 32 for character in source_id)
            for source_id in source_ids
        )
        or len(source_ids) != len(set(source_ids))
    ):
        raise ValueError("Coverage source_ids must be a unique list of source IDs")
    return CoverageSettings(
        mode=mode,
        area_mode=area_mode,
        cell_size_m=positive_number("cell_size_m", defaults.cell_size_m, 100_000),
        area_buffer_m=positive_number("area_buffer_m", defaults.area_buffer_m, 500_000),
        profile_step_m=positive_number("profile_step_m", defaults.profile_step_m, 10_000),
        maximum_profile_samples=positive_int(
            "maximum_profile_samples", defaults.maximum_profile_samples, 16_384
        ),
        maximum_cells=positive_int("maximum_cells", defaults.maximum_cells, 16_384),
        maximum_sources=positive_int("maximum_sources", defaults.maximum_sources, 64),
        maximum_evaluations=positive_int(
            "maximum_evaluations", defaults.maximum_evaluations, 250_000
        ),
        include_endpoints=include_endpoints,
        source_ids=source_ids,
        client=client,
    )


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
    def __init__(
        self,
        directory: Path,
        project_store: ProjectStore,
        workspace_key: str,
        project: dict[str, Any],
    ):
        self.directory = directory
        directory.mkdir(parents=True, exist_ok=True)
        self.project_store = project_store
        self.workspace_key = workspace_key
        self.project_id = str(project["id"])
        self.project = project
        self.lock = threading.Lock()
        self.cancel = threading.Event()
        self.coverage_cancel = threading.Event()
        self.inspection_cancels: dict[str, threading.Event] = {}
        self.inspection_job: dict[str, Any] | None = None
        self.inspection_result: dict[str, Any] | None = None
        recovered = project["run_state"] == "interrupted"
        self.status: dict[str, Any] = {
            "state": "interrupted" if recovered else "idle",
            "stage": "Previous search interrupted by restart; rerun to recover." if recovered else "Ready to plan",
            "done": 0,
            "total": 1,
        }
        self.result: Any = None
        self.result_plan_fingerprint: str | None = None
        self.coverage_job: dict[str, Any] | None = None
        self.restore_coverage_job()
        self.inputs: dict[str, Any] = project["plan"]
        self.result_summary: dict[str, Any] | None = project["result_summary"]
        self.job_id: str | None = None
        self.input_revision = 1 if self.inputs else 0
        self.snapshot_version = 0
        self.keep_result_on_cancel = False
        self.storage_usage_bytes: int | None = None

    @property
    def coverage_job_path(self) -> Path:
        return self.directory / "coverage-job.json"

    def restore_coverage_job(self) -> None:
        try:
            job = json.loads(self.coverage_job_path.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            self.coverage_job = None
            return
        if job.get("state") in {"queued", "running"}:
            job["state"] = "interrupted"
            job["stage"] = "Coverage job interrupted by restart; calculate again to continue."
            _write_json_atomic(self.coverage_job_path, job)
        self.coverage_job = job

    def activate_project(self, project: dict[str, Any]) -> None:
        self.project = project
        self.project_id = str(project["id"])
        self.directory = self.project_store.project_path(self.workspace_key, project)
        self.directory.mkdir(parents=True, exist_ok=True)
        self.inputs = project["plan"]
        self.result_summary = project["result_summary"]
        self.result = None
        self.result_plan_fingerprint = None
        self.job_id = None
        self.coverage_cancel = threading.Event()
        for cancel_event in self.inspection_cancels.values():
            cancel_event.set()
        self.inspection_cancels.clear()
        self.inspection_job = None
        self.inspection_result = None
        self.restore_coverage_job()
        self.input_revision += 1
        self.snapshot_version = 0
        self.storage_usage_bytes = None
        self.status = {
            "state": "interrupted" if project["run_state"] == "interrupted" else "idle",
            "stage": (
                "Previous search interrupted by restart; rerun to recover."
                if project["run_state"] == "interrupted"
                else "Project opened"
            ),
            "done": 0,
            "total": 1,
        }


def _terrain_paths(workspace: Workspace, kind: str) -> list[Path]:
    flat = list((workspace.directory / kind).glob("*.tif"))
    generated = list(
        (workspace.directory / "terrain-generations").glob(f"generation-*/{kind}/*.tif")
    )
    return sorted([*flat, *generated])


def _write_json_atomic(path: Path, value: Any) -> None:
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(value, indent=2), encoding="utf-8")
    temporary.replace(path)


def _save_coverage_manifest(workspace: Workspace, job: dict[str, Any]) -> None:
    _write_json_atomic(workspace.coverage_job_path, job)
    archive = workspace.directory / "coverage-jobs"
    archive.mkdir(parents=True, exist_ok=True)
    _write_json_atomic(archive / f"{job['job_id']}.json", job)


def _load_coverage_manifest(workspace: Workspace, job_id: str) -> dict[str, Any] | None:
    if len(job_id) != 32 or any(character not in "0123456789abcdef" for character in job_id):
        return None
    if workspace.coverage_job and workspace.coverage_job.get("job_id") == job_id:
        return dict(workspace.coverage_job)
    try:
        value = json.loads(
            (workspace.directory / "coverage-jobs" / f"{job_id}.json").read_text(
                encoding="utf-8"
            )
        )
    except (OSError, ValueError):
        return None
    return value if isinstance(value, dict) and value.get("job_id") == job_id else None


def _iter_coverage_cells(
    directory: Path, job: dict[str, Any]
) -> Iterator[dict[str, Any]]:
    result_path = (directory / str(job.get("result_file", ""))).resolve()
    if directory.resolve() not in result_path.parents or not result_path.is_file():
        raise FileNotFoundError("Coverage results are unavailable")
    with result_path.open(encoding="utf-8") as source:
        for line in source:
            if line:
                try:
                    chunk = json.loads(line)
                except ValueError as exc:
                    raise OSError("Coverage result chunk is malformed") from exc
                if not isinstance(chunk, list):
                    raise OSError("Coverage result chunk is malformed")
                yield from chunk


def _terrain_fingerprint(workspace: Workspace) -> list[list[str | int]]:
    return [
        list(item)
        for item in sorted(
            (
                path.relative_to(workspace.directory).as_posix(),
                path.stat().st_size,
                path.stat().st_mtime_ns,
            )
            for kind in ("dtm", "dom")
            for path in _terrain_paths(workspace, kind)
        )
    ]


def _coverage_area_bounds_wgs84(
    bounds: tuple[float, float, float, float], source_crs: str
) -> list[float]:
    left, bottom, right, top = bounds
    reverse = Transformer.from_crs(source_crs, 4326, always_xy=True)
    corners = [
        reverse.transform(left, bottom),
        reverse.transform(left, top),
        reverse.transform(right, top),
        reverse.transform(right, bottom),
    ]
    latitudes = [latitude for _longitude, latitude in corners]
    longitudes = [longitude for longitude, _latitude in corners]
    return [min(latitudes), min(longitudes), max(latitudes), max(longitudes)]


def _plan_fingerprint(plan: dict[str, Any]) -> str:
    return hashlib.sha256(
        json.dumps(plan, sort_keys=True, separators=(",", ":")).encode("utf-8")
    ).hexdigest()


def _route_plan_fingerprint(plan: dict[str, Any]) -> str:
    return _plan_fingerprint(
        {
            key: value
            for key, value in plan.items()
            if key not in {"coverage", "resolved_search"}
        }
    )


def _refresh_recovery_summary(workspace: Workspace) -> None:
    summary = workspace.result_summary
    if summary is None:
        return
    summary["stale"] = (
        summary.get("plan_fingerprint")
        not in {
            _route_plan_fingerprint(workspace.inputs),
            _plan_fingerprint(workspace.inputs),  # Older saved summaries included derived search metadata.
        }
        or summary.get("terrain_fingerprint") != _terrain_fingerprint(workspace)
    )


def create_app(data_dir: Path | None = None) -> FastAPI:
    root = (data_dir or Path(os.environ.get("RF_PLANNER_DATA", "web-data"))).resolve()
    root.mkdir(parents=True, exist_ok=True)
    project_store = ProjectStore(root / "projects.sqlite3", root)
    app = FastAPI(title="RF Router Planner")
    workspaces: dict[str, Workspace] = {}
    registry_lock = threading.Lock()
    rf_cache = LinkMetricsCache(
        max_entries=max(1, int(os.environ.get("RF_PLANNER_RF_CACHE_ENTRIES", "50000")))
    )
    coverage_rf_cache = LinkMetricsCache(
        max_entries=max(
            1, int(os.environ.get("RF_PLANNER_COVERAGE_CACHE_ENTRIES", "50000"))
        )
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
                project = project_store.active(key)
                directory = project_store.project_path(key, project)
                workspaces[key] = Workspace(directory, project_store, key, project)
                _refresh_recovery_summary(workspaces[key])
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

    def coverage_context(ws: Workspace, body: dict[str, Any]) -> dict[str, Any]:
        coverage_settings = parse_coverage_settings(body.get("settings", {}))
        snapshot_version = body.get("snapshot_version")
        if not isinstance(snapshot_version, int) or isinstance(snapshot_version, bool):
            raise ValueError("Coverage request needs a route snapshot version")
        with ws.lock:
            if ws.status["state"] in {"queued", "running", "preparing"}:
                raise HTTPException(409, "Wait until route search or terrain preparation finishes")
            result = ws.result
            if result is None or result.active_solution is None or not result.search_complete:
                raise HTTPException(409, "Finish a route search before calculating coverage")
            if snapshot_version != ws.snapshot_version:
                raise HTTPException(409, "The selected route changed; refresh before calculating coverage")
            if ws.result_plan_fingerprint != _route_plan_fingerprint(ws.inputs):
                raise HTTPException(409, "The current plan differs from the certified route")
            alternative_id = body.get("alternative_id")
            if alternative_id != solution_id(result.active_solution):
                raise HTTPException(409, "The selected route alternative changed")
            sources = [
                site
                for site in result.active_solution.sites
                if site.kind not in {SiteKind.ENDPOINT_A, SiteKind.ENDPOINT_B, SiteKind.CLIENT}
            ]
            if coverage_settings.include_endpoints:
                known_source_ids = {site.id for site in sources}
                sources.extend(
                    site
                    for site in result.candidates
                    if site.kind in {SiteKind.ENDPOINT_A, SiteKind.ENDPOINT_B, SiteKind.CLIENT}
                    and site.id not in known_source_ids
                )
            available_sources = {site.id: site for site in sources}
            if coverage_settings.source_ids:
                router_ids = {
                    site.id
                    for site in sources
                    if site.kind not in {SiteKind.ENDPOINT_A, SiteKind.ENDPOINT_B, SiteKind.CLIENT}
                }
                unknown_source_ids = set(coverage_settings.source_ids) - router_ids
                if unknown_source_ids:
                    raise HTTPException(
                        422,
                        "Selected coverage sources are not in this route alternative; refresh the source list",
                    )
                selected_ids = set(coverage_settings.source_ids)
                sources = [
                    site
                    for site_id, site in available_sources.items()
                    if site_id in selected_ids
                    or (
                        coverage_settings.include_endpoints
                        and site.kind in {SiteKind.ENDPOINT_A, SiteKind.ENDPOINT_B}
                    )
                ]
            if not sources:
                raise HTTPException(422, "This route has no radio sources; include endpoints to preview their coverage")
            if len(sources) > coverage_settings.maximum_sources:
                raise HTTPException(422, f"Select no more than {coverage_settings.maximum_sources} radio sources")
            dtm_paths = _terrain_paths(ws, "dtm")
            dom_paths = _terrain_paths(ws, "dom")
            if not dtm_paths:
                raise HTTPException(422, "Prepare or upload ground terrain first")
            plan = dict(ws.inputs)
            route_fingerprint = _route_plan_fingerprint(plan)
            terrain_fingerprint = _terrain_fingerprint(ws)
            project_id = ws.project_id
            input_revision = ws.input_revision
            selected_snapshot = ws.snapshot_version
            selected_alternative = solution_id(result.active_solution)

        requested_bounds = None
        if coverage_settings.area_mode == "view":
            values = body.get("map_bounds")
            if not isinstance(values, list) or len(values) != 4:
                raise ValueError("Map coverage needs south, west, north, and east bounds")
            south, west, north, east = values
            south, west = coordinate_pair({"southwest": [south, west]}, "southwest")
            north, east = coordinate_pair({"northeast": [north, east]}, "northeast")
            if south >= north or west >= east:
                raise ValueError("Map coverage bounds must have positive width and height")
            with RasterTerrain(dtm_paths) as terrain:
                forward = Transformer.from_crs(4326, terrain.crs, always_xy=True)
                first = forward.transform(west, south)
                last = forward.transform(east, north)
                requested_bounds = (
                    min(first[0], last[0]),
                    min(first[1], last[1]),
                    max(first[0], last[0]),
                    max(first[1], last[1]),
                )
        return {
            "settings": coverage_settings,
            "sources": sources,
            "solution": result.active_solution,
            "rf": settings(RFSettings, plan.get("rf", {})),
            "candidates": settings(CandidateSettings, plan.get("candidates", {})),
            "dtm_paths": dtm_paths,
            "dom_paths": dom_paths,
            "requested_bounds": requested_bounds,
            "route_fingerprint": route_fingerprint,
            "terrain_fingerprint": terrain_fingerprint,
            "project_id": project_id,
            "input_revision": input_revision,
            "snapshot_version": selected_snapshot,
            "alternative_id": selected_alternative,
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
        projects = project_store.list(ws.workspace_key)
        active_project = project_store.get(ws.workspace_key, ws.project_id) or ws.project
        if ws.storage_usage_bytes is None:
            ws.storage_usage_bytes = project_store.storage_usage(ws.workspace_key)
        return {
            "job": {
                **ws.status,
                "job_id": ws.job_id,
                "input_revision": ws.input_revision,
                "snapshot_version": ws.snapshot_version,
            },
            "coverage_job": dict(ws.coverage_job) if ws.coverage_job else None,
            "rf_cache": rf_cache.stats(ws.directory.name),
            "rf": encode(RFSettings()),
            "candidates": encode(CandidateSettings()),
            "terrain": {
                kind: [p.name for p in _terrain_paths(ws, kind)]
                for kind in ("dtm", "dom")
            },
            "plan": ws.inputs or None,
            "coverage_settings": (ws.inputs or {}).get("coverage", encode(CoverageSettings())),
            "projects": {
                "active_id": ws.project_id,
                "active_name": ws.project["name"],
                "has_previous_revision": active_project["previous_plan"] is not None,
                "items": [
                    {"id": item["id"], "name": item["name"], "updated_at": item["updated_at"]}
                    for item in projects
                ],
                "storage_usage_bytes": ws.storage_usage_bytes,
                "previous_result": ws.result_summary,
            },
        }

    def coverage_current(ws: Workspace, job: dict[str, Any]) -> bool:
        try:
            with ws.lock:
                result = ws.result
                current_settings = encode(
                    parse_coverage_settings((ws.inputs or {}).get("coverage", {}))
                )
                route_matches = bool(
                    ws.project_id == job["project_id"]
                    and result is not None
                    and result.active_solution is not None
                    and ws.snapshot_version == job["snapshot_version"]
                    and ws.result_plan_fingerprint == job["route_fingerprint"]
                    and _route_plan_fingerprint(ws.inputs) == job["route_fingerprint"]
                    and solution_id(result.active_solution) == job["alternative_id"]
                    and _plan_fingerprint(current_settings) == job["settings_fingerprint"]
                )
                return route_matches and _terrain_fingerprint(ws) == job["terrain_fingerprint"]
        except (ValueError, TypeError, KeyError):
            return False

    @app.post("/api/coverage/estimate")
    def estimate_mesh_coverage(body: dict[str, Any], request: Request) -> Any:
        ws = workspace(request)
        try:
            context = coverage_context(ws, body)
            coverage_settings: CoverageSettings = context["settings"]
            sources: list[Site] = context["sources"]
            bounded_settings = replace(
                coverage_settings,
                maximum_cells=min(
                    coverage_settings.maximum_cells,
                    coverage_settings.maximum_evaluations // len(sources),
                ),
            )
            with RasterTerrain(context["dtm_paths"], context["dom_paths"]) as terrain:
                grid, xs, ys, _latitudes, _longitudes = make_grid(
                    sources, terrain, bounded_settings, context["requested_bounds"]
                )
                terrain_cells = int(np.isfinite(terrain.sample(xs, ys)).sum())
                area_bounds = _coverage_area_bounds_wgs84(grid.bounds, terrain.crs)
            return {
                "source_count": len(sources),
                "source_ids": [site.id for site in sources],
                "requested_cells": grid.requested_cells,
                "effective_cell_size_m": grid.effective_cell_size_m,
                "terrain_available_cells": terrain_cells,
                "unknown_cells": grid.requested_cells - terrain_cells,
                "planned_evaluations": grid.requested_cells * len(sources),
                "mode": coverage_settings.mode.value,
                "surface_available": grid.surface_available,
                "area_bounds_wgs84": area_bounds,
            }
        except HTTPException:
            raise
        except (ValueError, TypeError, KeyError) as exc:
            raise HTTPException(422, str(exc)) from exc

    @app.post("/api/coverage/jobs")
    def start_mesh_coverage(body: dict[str, Any], request: Request) -> Any:
        ws = workspace(request)
        try:
            context = coverage_context(ws, body)
            coverage_settings: CoverageSettings = context["settings"]
            sources: list[Site] = context["sources"]
            raw_height_overrides = body.get("source_height_overrides", {})
            if not isinstance(raw_height_overrides, dict) or len(raw_height_overrides) > 64:
                raise ValueError("Source-height scenario must be a map of at most 64 router IDs")
            source_ids = set(context["solution"].router_ids) & {
                site.id for site in sources
            }
            height_overrides: dict[str, float] = {}
            for source_id, height in raw_height_overrides.items():
                if not isinstance(source_id, str) or source_id not in source_ids:
                    raise ValueError("Height scenarios may change only selected router sources")
                if (
                    not isinstance(height, (int, float))
                    or isinstance(height, bool)
                    or not math.isfinite(height)
                    or not 0.1 <= height <= 500
                ):
                    raise ValueError("Scenario antenna height must be between 0.1 and 500 m AGL")
                height_overrides[source_id] = float(height)
            if height_overrides:
                sources = [
                    replace(
                        source,
                        antenna_height_m=height_overrides.get(
                            source.id, source.antenna_height_m
                        ),
                        height_override=source.id in height_overrides,
                    )
                    for source in sources
                ]
            if coverage_settings.maximum_evaluations // len(sources) < 1:
                raise ValueError("Coverage limits allow no source evaluations")
            with ws.lock:
                if (
                    ws.coverage_job
                    and ws.coverage_job.get("state") in {"queued", "running"}
                ) or (
                    ws.inspection_job
                    and ws.inspection_job.get("state") in {"queued", "running"}
                ):
                    raise HTTPException(409, "A coverage calculation or inspection is already running")
                if (
                    ws.snapshot_version != context["snapshot_version"]
                    or ws.project_id != context["project_id"]
                    or ws.result_plan_fingerprint != context["route_fingerprint"]
                    or _terrain_fingerprint(ws) != context["terrain_fingerprint"]
                    or _plan_fingerprint(
                        encode(parse_coverage_settings((ws.inputs or {}).get("coverage", {})))
                    )
                    != _plan_fingerprint(encode(coverage_settings))
                ):
                    raise HTTPException(409, "Save coverage settings and refresh the route before calculating")
                with scheduler_lock:
                    if job_slots["outstanding"] >= max_outstanding_jobs:
                        raise HTTPException(429, "The planner queue is full; retry shortly")
                    queued = job_slots["outstanding"] >= max_workers
                    job_slots["outstanding"] += 1
                job_id = secrets.token_hex(16)
                result_directory = ws.directory / "coverage-results"
                result_directory.mkdir(parents=True, exist_ok=True)
                result_name = f"coverage-results/{job_id}.jsonl"
                (ws.directory / result_name).write_text("", encoding="utf-8")
                job = {
                    "job_id": job_id,
                    "project_id": ws.project_id,
                    "state": "queued" if queued else "running",
                    "stage": "Waiting for a planner worker" if queued else "Opening terrain",
                    "done": 0,
                    "total": 0,
                    "covered_cells": 0,
                    "unknown_cells": 0,
                    "evaluated_cells": 0,
                    "created_at": time.time(),
                    "result_file": result_name,
                    "route_fingerprint": context["route_fingerprint"],
                    "terrain_fingerprint": context["terrain_fingerprint"],
                    "settings_fingerprint": _plan_fingerprint(encode(coverage_settings)),
                    "settings": encode(coverage_settings),
                    "radio_fingerprint": _plan_fingerprint(
                        {
                            "rf": encode(context["rf"]),
                            "candidates": encode(context["candidates"]),
                        }
                    ),
                    "model_version": COVERAGE_MODEL_VERSION,
                    "source_ids": [site.id for site in sources],
                    "source_sites": encode(sources),
                    "source_height_overrides": height_overrides,
                    "router_ids": list(context["solution"].router_ids),
                    "network_links": [
                        {
                            "source_id": link.source_id,
                            "target_id": link.target_id,
                            "valid": bool(link.valid),
                        }
                        for link in context["solution"].links
                    ],
                    "alternative_id": context["alternative_id"],
                    "snapshot_version": context["snapshot_version"],
                    "input_revision": context["input_revision"],
                    "surface_available": bool(context["dom_paths"]),
                    "mode": coverage_settings.mode.value,
                    "stale": False,
                }
                ws.coverage_cancel.clear()
                ws.coverage_job = job
                _save_coverage_manifest(ws, job)
        except HTTPException:
            raise
        except (ValueError, TypeError, KeyError) as exc:
            raise HTTPException(422, str(exc)) from exc

        def run_coverage() -> None:
            result_path = ws.directory / result_name

            def update_job(**values: Any) -> None:
                with ws.lock:
                    if not ws.coverage_job or ws.coverage_job.get("job_id") != job_id:
                        return
                    ws.coverage_job.update(values)
                    _save_coverage_manifest(ws, ws.coverage_job)

            def stream_chunk(cells: list[Any]) -> None:
                if not coverage_current(ws, job):
                    ws.coverage_cancel.set()
                    update_job(state="stale", stale=True, stage="Route, terrain, or settings changed")
                    return
                line = json.dumps(encode(cells), separators=(",", ":")) + "\n"
                with ws.lock:
                    if (
                        not ws.coverage_job
                        or ws.coverage_job.get("job_id") != job_id
                        or ws.coverage_cancel.is_set()
                    ):
                        return
                    if (
                        result_path.stat().st_size + len(line.encode("utf-8"))
                        > MAX_COVERAGE_RESULT_BYTES
                    ):
                        ws.coverage_cancel.set()
                        ws.coverage_job.update(
                            state="failed", stage="Coverage result exceeded the project storage limit"
                        )
                        _save_coverage_manifest(ws, ws.coverage_job)
                        raise ValueError("Coverage result exceeded the project storage limit")
                    with result_path.open("a", encoding="utf-8") as output:
                        output.write(line)
                    states = [cell.state.value for cell in cells]
                    if cells:
                        ws.coverage_job["done"] = max(
                            ws.coverage_job["done"],
                            max(cell.index for cell in cells) + 1,
                        )
                    ws.coverage_job["covered_cells"] += states.count("covered")
                    ws.coverage_job["unknown_cells"] += states.count("unknown_terrain")
                    ws.coverage_job["evaluated_cells"] += states.count("covered") + states.count("uncovered")
                    ws.coverage_job["stage"] = "Calculating mesh coverage"
                    _save_coverage_manifest(ws, ws.coverage_job)

            try:
                update_job(state="running", stage="Opening terrain")
                with RasterTerrain(context["dtm_paths"], context["dom_paths"]) as terrain:
                    if height_overrides:
                        router_ids = set(context["solution"].router_ids)
                        original_sites = {site.id: site for site in context["solution"].sites}
                        adjusted_sites = {site.id: site for site in sources}
                        affected = [
                            link
                            for link in context["solution"].links
                            if link.valid
                            and link.source_id in router_ids
                            and link.target_id in router_ids
                            and bool(
                                {link.source_id, link.target_id} & set(height_overrides)
                            )
                        ]
                        if len(affected) > MAX_COVERAGE_BACKBONE_REVALIDATION_LINKS:
                            raise ValueError(
                                "Height scenario affects too many certified mesh links to revalidate safely"
                            )
                        evaluator = LinkEvaluator(
                            terrain,
                            context["rf"],
                            cache=coverage_rf_cache,
                            cache_namespace=f"coverage-height:{ws.directory.name}",
                        )
                        validity: dict[tuple[str, str], bool] = {}
                        for link in affected:
                            left = adjusted_sites.get(
                                link.source_id, original_sites[link.source_id]
                            )
                            right = adjusted_sites.get(
                                link.target_id, original_sites[link.target_id]
                            )
                            distance = left.distance_to(right)
                            step = max(
                                coverage_settings.profile_step_m,
                                distance
                                / max(1, coverage_settings.maximum_profile_samples - 1),
                            )
                            try:
                                validity[(link.source_id, link.target_id)] = evaluator.evaluate(
                                    left, right, step, include_profile=False
                                ).valid
                            except ValueError:
                                validity[(link.source_id, link.target_id)] = False
                        network_links = [
                            {
                                "source_id": link.source_id,
                                "target_id": link.target_id,
                                "valid": validity.get(
                                    (link.source_id, link.target_id), bool(link.valid)
                                ),
                            }
                            for link in context["solution"].links
                        ]
                        update_job(
                            network_links=network_links,
                            backbone_revalidated_edges=len(affected),
                            backbone_invalidated_edges=sum(
                                not valid for valid in validity.values()
                            ),
                            stage="Revalidated affected certified mesh links",
                        )
                    bounded_settings = replace(
                        coverage_settings,
                        maximum_cells=min(
                            coverage_settings.maximum_cells,
                            coverage_settings.maximum_evaluations // len(sources),
                        ),
                    )
                    estimate_grid = make_grid(
                        sources,
                        terrain,
                        bounded_settings,
                        context["requested_bounds"],
                    )[0]
                    update_job(
                        total=estimate_grid.requested_cells,
                        requested_cells=estimate_grid.requested_cells,
                        effective_cell_size_m=estimate_grid.effective_cell_size_m,
                        rows=estimate_grid.rows,
                        columns=estimate_grid.columns,
                        area_bounds_wgs84=_coverage_area_bounds_wgs84(
                            estimate_grid.bounds, terrain.crs
                        ),
                    )
                    grid = calculate_coverage(
                        terrain,
                        context["rf"],
                        context["candidates"],
                        sources,
                        coverage_settings,
                        requested_bounds=context["requested_bounds"],
                        progress=lambda done, total: update_job(done=done, total=total),
                        cancelled=ws.coverage_cancel.is_set,
                        on_chunk=stream_chunk,
                        evaluation_cache=coverage_rf_cache,
                        cache_namespace=f"coverage:{ws.directory.name}",
                    )
                with ws.lock:
                    current_job = ws.coverage_job
                    if current_job is not None and current_job.get("job_id") == job_id:
                        if current_job.get("state") not in {"stale", "failed"}:
                            if ws.coverage_cancel.is_set():
                                state = "cancelled"
                                stage = "Coverage calculation cancelled; completed cells are available"
                            else:
                                state = "complete"
                                stage = "Coverage calculation complete"
                            current_job.update(
                                state=state,
                                stage=stage,
                                done=grid.completed_cells,
                                total=grid.requested_cells,
                                terrain_available_cells=grid.terrain_available_cells,
                                unknown_cells=max(
                                    current_job["unknown_cells"], grid.unknown_cells
                                ),
                                effective_cell_size_m=grid.effective_cell_size_m,
                                rows=grid.rows,
                                columns=grid.columns,
                                requested_cells=grid.requested_cells,
                                finished_at=time.time(),
                            )
                        _save_coverage_manifest(ws, current_job)
            except Exception as exc:
                update_job(state="failed", stage=str(exc), finished_at=time.time())
            finally:
                with scheduler_lock:
                    job_slots["outstanding"] = max(0, job_slots["outstanding"] - 1)

        try:
            scheduler.submit(run_coverage)
        except RuntimeError as exc:
            with scheduler_lock:
                job_slots["outstanding"] = max(0, job_slots["outstanding"] - 1)
            with ws.lock:
                ws.coverage_job.update(state="failed", stage="Could not queue coverage job")
                _save_coverage_manifest(ws, ws.coverage_job)
            raise HTTPException(503, "Could not queue coverage job") from exc
        return dict(ws.coverage_job)

    @app.get("/api/coverage/jobs")
    def list_mesh_coverage_jobs(request: Request) -> Any:
        ws = workspace(request)
        with ws.lock:
            current_job = dict(ws.coverage_job) if ws.coverage_job else None
            directory = ws.directory
        manifests: dict[str, dict[str, Any]] = {}
        archive = directory / "coverage-jobs"
        if archive.is_dir():
            for path in archive.glob("*.json"):
                try:
                    item = json.loads(path.read_text(encoding="utf-8"))
                except (OSError, ValueError):
                    continue
                if isinstance(item, dict) and isinstance(item.get("job_id"), str):
                    manifests[item["job_id"]] = item
        if current_job:
            manifests[current_job["job_id"]] = current_job
        recent = sorted(
            manifests.values(), key=lambda value: value.get("created_at", 0), reverse=True
        )[:50]
        return {
            "jobs": [
                {
                    "job_id": item["job_id"],
                    "project_id": item.get("project_id"),
                    "state": item.get("state"),
                    "created_at": item.get("created_at"),
                    "alternative_id": item.get("alternative_id"),
                    "source_ids": item.get("source_ids", []),
                    "router_ids": item.get("router_ids", []),
                    "settings": item.get("settings", {}),
                    "requested_cells": item.get("requested_cells", item.get("total", 0)),
                    "done": item.get("done", 0),
                }
                for item in recent
            ]
        }

    @app.post("/api/coverage/compare")
    def compare_mesh_coverage(body: dict[str, Any], request: Request) -> Any:
        ws = workspace(request)
        baseline_id, scenario_id = body.get("baseline_job_id"), body.get("scenario_job_id")
        reference_id = body.get("reference_id")
        if (
            not isinstance(baseline_id, str)
            or not isinstance(scenario_id, str)
            or not isinstance(reference_id, str)
            or baseline_id == scenario_id
        ):
            raise HTTPException(422, "Choose two different completed runs and a shared reference router")
        with ws.lock:
            baseline = _load_coverage_manifest(ws, baseline_id)
            scenario = _load_coverage_manifest(ws, scenario_id)
            if baseline is None or scenario is None:
                raise HTTPException(404, "Coverage run not found in this project workspace")
            if baseline.get("state") != "complete" or scenario.get("state") != "complete":
                raise HTTPException(409, "Both coverage runs must be complete")
            if baseline.get("project_id") != ws.project_id or scenario.get("project_id") != ws.project_id:
                raise HTTPException(404, "Coverage run not found in this project workspace")
            if (
                baseline.get("terrain_fingerprint") != scenario.get("terrain_fingerprint")
                or baseline.get("terrain_fingerprint") != _terrain_fingerprint(ws)
            ):
                raise HTTPException(409, "Runs use different or changed terrain; recalculate on the same terrain")
            if baseline.get("radio_fingerprint") != scenario.get("radio_fingerprint"):
                raise HTTPException(409, "Runs use different radio or propagation settings")
            if baseline.get("model_version") != scenario.get("model_version"):
                raise HTTPException(409, "Runs were calculated with different coverage model versions")
            baseline_settings, scenario_settings = baseline.get("settings", {}), scenario.get("settings", {})
            compatible_settings = all(
                baseline_settings.get(key) == scenario_settings.get(key)
                for key in ("profile_step_m", "maximum_profile_samples", "client")
            )
            if not compatible_settings:
                raise HTTPException(409, "Runs use different client radios or profile resolution")
            compatible_grid = all(
                baseline.get(key) == scenario.get(key)
                for key in ("rows", "columns", "effective_cell_size_m", "area_bounds_wgs84")
            )
            if not compatible_grid:
                raise HTTPException(409, "Runs use different grid bounds or cell spacing")
            baseline_routers = set(baseline.get("router_ids", []))
            scenario_routers = set(scenario.get("router_ids", []))
            if (
                reference_id not in baseline_routers
                or reference_id not in scenario_routers
                or reference_id not in set(baseline.get("source_ids", []))
                or reference_id not in set(scenario.get("source_ids", []))
            ):
                raise HTTPException(422, "Reference router must be a selected repeater in both runs")
            requested_cells = baseline.get("requested_cells", baseline.get("total"))
            scenario_cells_count = scenario.get("requested_cells", scenario.get("total"))
            if requested_cells != scenario_cells_count or not isinstance(requested_cells, int):
                raise HTTPException(409, "Coverage runs are incomplete and cannot be compared")
            try:
                baseline_network = analyze_node_failures(
                    [],
                    list(baseline_routers),
                    baseline.get("source_ids", []),
                    baseline.get("network_links", []),
                    [],
                    reference_id,
                )
                scenario_network = analyze_node_failures(
                    [],
                    list(scenario_routers),
                    scenario.get("source_ids", []),
                    scenario.get("network_links", []),
                    [],
                    reference_id,
                )
                report = compare_coverage_streams(
                    _iter_coverage_cells(ws.directory, baseline),
                    _iter_coverage_cells(ws.directory, scenario),
                    set(baseline_network["connected_source_ids"]),
                    set(scenario_network["connected_source_ids"]),
                    expected_count=requested_cells,
                )
            except OSError as exc:
                raise HTTPException(404, "Coverage result pages are unavailable") from exc
            except ValueError as exc:
                raise HTTPException(409, str(exc)) from exc
        return {
            "baseline_job_id": baseline_id,
            "scenario_job_id": scenario_id,
            "reference_id": reference_id,
            "baseline_reference_available": baseline_network["reference_available"],
            "scenario_reference_available": scenario_network["reference_available"],
            "baseline_connected_source_ids": baseline_network["connected_source_ids"],
            "scenario_connected_source_ids": scenario_network["connected_source_ids"],
            "effective_cell_size_m": scenario["effective_cell_size_m"],
            "area_bounds_wgs84": scenario["area_bounds_wgs84"],
            "approximate_sampled_area_km2": (
                report["counts"]["evaluated_cells"]
                * float(baseline["effective_cell_size_m"]) ** 2
                / 1_000_000
            ),
            **report,
        }

    @app.get("/api/coverage/jobs/{job_id}")
    def get_mesh_coverage_job(job_id: str, request: Request) -> Any:
        ws = workspace(request)
        with ws.lock:
            if not ws.coverage_job or ws.coverage_job.get("job_id") != job_id:
                raise HTTPException(404, "Coverage job not found in this workspace")
            job = dict(ws.coverage_job)
        if not coverage_current(ws, job) and job["state"] in {
            "queued",
            "running",
            "complete",
            "cancelled",
            "interrupted",
        }:
            job["stale"] = True
        return job

    @app.get("/api/coverage/jobs/{job_id}/cells")
    def get_mesh_coverage_cells(
        job_id: str, request: Request, cursor: int = 0, limit: int = 128
    ) -> Any:
        ws = workspace(request)
        if cursor < 0 or not 1 <= limit <= 256:
            raise HTTPException(422, "Cursor must be nonnegative and page size between 1 and 256")
        with ws.lock:
            job = ws.coverage_job
            if not job or job.get("job_id") != job_id:
                raise HTTPException(404, "Coverage job not found in this workspace")
            result_path = (ws.directory / job["result_file"]).resolve()
            if ws.directory.resolve() not in result_path.parents:
                raise HTTPException(404, "Coverage results are unavailable")
            rows: list[Any] = []
            total = 0
            try:
                with result_path.open(encoding="utf-8") as source:
                    for line in source:
                        chunk = json.loads(line)
                        chunk_start, chunk_end = total, total + len(chunk)
                        if chunk_end > cursor and chunk_start < cursor + limit:
                            low = max(0, cursor - chunk_start)
                            high = min(len(chunk), cursor + limit - chunk_start)
                            rows.extend(chunk[low:high])
                        total = chunk_end
            except OSError as exc:
                raise HTTPException(404, "Coverage results are unavailable") from exc
            return {
                "cells": rows,
                "cursor": cursor,
                "next_cursor": cursor + len(rows) if cursor + len(rows) < total else None,
                "stored_cells": total,
                "requested_cells": job.get("requested_cells", job.get("total", 0)),
                "state": job["state"],
                "stale": bool(job.get("stale")),
            }

    @app.post("/api/coverage/jobs/{job_id}/cancel")
    def cancel_mesh_coverage(job_id: str, request: Request) -> Any:
        ws = workspace(request)
        with ws.lock:
            if not ws.coverage_job or ws.coverage_job.get("job_id") != job_id:
                raise HTTPException(404, "Coverage job not found in this workspace")
            if ws.coverage_job.get("state") not in {"queued", "running"}:
                raise HTTPException(409, "There is no active coverage job")
            ws.coverage_cancel.set()
            ws.coverage_job.update(stage="Cancelling coverage")
            _save_coverage_manifest(ws, ws.coverage_job)
        return {"ok": True}

    @app.post("/api/coverage/jobs/{job_id}/scenario")
    def analyze_coverage_failure_scenario(
        job_id: str, body: dict[str, Any], request: Request
    ) -> Any:
        ws = workspace(request)
        failed_ids = body.get("failed_ids", [])
        reference_id = body.get("reference_id")
        if (
            not isinstance(failed_ids, list)
            or len(failed_ids) > 64
            or any(not isinstance(item, str) for item in failed_ids)
            or not isinstance(reference_id, str)
        ):
            raise HTTPException(422, "Choose failed router IDs and one reference router")
        with ws.lock:
            job = ws.coverage_job
            if not job or job.get("job_id") != job_id:
                raise HTTPException(404, "Coverage job not found in this workspace")
            if job.get("state") != "complete":
                raise HTTPException(409, "Complete the coverage calculation before analyzing failures")
            job = dict(job)
        if job.get("stale") or not coverage_current(ws, job):
            raise HTTPException(409, "Coverage results are stale; recalculate before analyzing failures")
        with ws.lock:
            if not ws.coverage_job or ws.coverage_job.get("job_id") != job_id:
                raise HTTPException(409, "Coverage job changed; recalculate before analyzing failures")
            current_settings = encode(
                parse_coverage_settings((ws.inputs or {}).get("coverage", {}))
            )
            current = (
                ws.project_id == job["project_id"]
                and ws.snapshot_version == job["snapshot_version"]
                and ws.result is not None
                and ws.result.active_solution is not None
                and solution_id(ws.result.active_solution) == job["alternative_id"]
                and ws.result_plan_fingerprint == job["route_fingerprint"]
                and _route_plan_fingerprint(ws.inputs) == job["route_fingerprint"]
                and _plan_fingerprint(current_settings) == job["settings_fingerprint"]
                and _terrain_fingerprint(ws) == job["terrain_fingerprint"]
            )
            if not current:
                raise HTTPException(409, "Coverage results are stale; recalculate before analyzing failures")
            result_path = (ws.directory / job["result_file"]).resolve()
            if ws.directory.resolve() not in result_path.parents or not result_path.is_file():
                raise HTTPException(404, "Coverage results are unavailable")
            try:
                scenario = analyze_node_failures(
                    _iter_coverage_cells(ws.directory, job),
                    job.get("router_ids", []),
                    job.get("source_ids", []),
                    job.get("network_links", []),
                    failed_ids,
                    reference_id,
                )
            except ValueError as exc:
                raise HTTPException(422, str(exc)) from exc
            except OSError as exc:
                raise HTTPException(404, "Coverage results are unavailable") from exc
            if len(scenario["cells"]) != job.get("requested_cells", job.get("total", -1)):
                raise HTTPException(409, "Coverage results are incomplete; calculate again first")
        return {"job_id": job_id, **scenario}

    def _compute_mesh_coverage_inspection(
        ws: Workspace,
        body: dict[str, Any],
        cancelled: threading.Event,
    ) -> dict[str, Any]:
        try:
            context = coverage_context(ws, body)
            latitude, longitude = coordinate_pair(
                {"point": [body.get("latitude"), body.get("longitude")]}, "point"
            )
            settings_for_client = context["settings"]
            client = settings_for_client.client
            with RasterTerrain(context["dtm_paths"], context["dom_paths"]) as terrain:
                forward = Transformer.from_crs(4326, terrain.crs, always_xy=True)
                x, y = forward.transform(longitude, latitude)
                ground_values = terrain.sample(np.array([x]), np.array([y]))
                ground = float(ground_values[0])
                if not math.isfinite(ground):
                    return {
                        "state": "unknown_terrain",
                        "latitude": latitude,
                        "longitude": longitude,
                        "client": encode(client),
                        "surface_available": terrain.has_surface,
                        "message": "No ground-elevation data at this location; RF coverage is unknown.",
                        "sources": [],
                    }
                if cancelled.is_set():
                    raise InterruptedError("Location inspection superseded")
                surface_elevation = None
                if terrain.has_surface:
                    surface = float(terrain.sample(np.array([x]), np.array([y]), surface=True)[0])
                    surface_elevation = surface if math.isfinite(surface) else None
                target = Site(
                    "coverage-inspection-client",
                    float(x),
                    float(y),
                    latitude,
                    longitude,
                    SiteKind.CLIENT,
                    ground_elevation_m=ground,
                    surface_elevation_m=surface_elevation,
                    antenna_height_m=client.height_agl_m,
                )
                evaluator = LinkEvaluator(
                    terrain,
                    context["rf"],
                    cache=coverage_rf_cache,
                    cache_namespace=f"coverage:{ws.directory.name}",
                )
                client_radio = client.as_radio_budget()
                profile_floor = context["candidates"].final_sample_step_m
                source_results: list[dict[str, Any]] = []
                profile_steps: dict[str, float] = {}
                structural_validation = (
                    context["rf"].validation_mode != ValidationMode.STRICT_LOS
                )
                for source in context["sources"]:
                    if cancelled.is_set():
                        raise InterruptedError("Location inspection superseded")
                    distance = source.distance_to(target)
                    profile_step = max(
                        profile_floor,
                        distance / max(1, settings_for_client.maximum_profile_samples - 1),
                    )
                    profile_steps[source.id] = profile_step
                    try:
                        link = evaluator.evaluate(
                            source,
                            target,
                            profile_step,
                            include_profile=False,
                            target_radio=client_radio,
                        )
                    except ValueError:
                        source_results.append(
                            {
                                "source_id": source.id,
                                "distance_m": distance,
                                "downlink_margin_db": None,
                                "uplink_margin_db": None,
                                "two_way_margin_db": None,
                                "valid_downlink": False,
                                "valid_uplink": False,
                                "valid_two_way": False,
                                "rejection": "unknown_terrain",
                            }
                        )
                        continue
                    structural_ok = structural_validation or (
                        link.los_clear and link.fresnel_clear
                    )
                    downlink = link.forward.usable_margin_db
                    uplink = link.reverse.usable_margin_db
                    downlink_valid = structural_ok and link.forward.valid
                    uplink_valid = structural_ok and link.reverse.valid
                    two_way_valid = downlink_valid and uplink_valid
                    source_results.append(
                        {
                            "source_id": source.id,
                            "distance_m": distance,
                            "downlink_margin_db": downlink,
                            "uplink_margin_db": uplink,
                            "two_way_margin_db": min(downlink, uplink),
                            "valid_downlink": downlink_valid,
                            "valid_uplink": uplink_valid,
                            "valid_two_way": two_way_valid,
                            "rejection": (
                                None
                                if two_way_valid
                                else "LOS/Fresnel validation"
                                if not structural_ok
                                else "radio margin"
                            ),
                        }
                    )

                source_results.sort(
                    key=lambda item: (
                        not item["valid_two_way"],
                        -item["two_way_margin_db"]
                        if item["two_way_margin_db"] is not None
                        else math.inf,
                        item["source_id"],
                    )
                )
                best_source = None
                profile_link = None
                if source_results and source_results[0]["two_way_margin_db"] is not None:
                    best_id = source_results[0]["source_id"]
                    best_source = next(
                        source for source in context["sources"] if source.id == best_id
                    )
                    if cancelled.is_set():
                        raise InterruptedError("Location inspection superseded")
                    try:
                        profile_link = evaluator.evaluate(
                            best_source,
                            target,
                            profile_steps[best_id],
                            include_profile=True,
                            target_radio=client_radio,
                        )
                    except ValueError:
                        # The scalar link evaluation is still useful if only the
                        # denser display profile encounters a terrain-data gap.
                        profile_link = None

                solution = context["solution"]
                adjacency: dict[str, set[str]] = {site.id: set() for site in solution.sites}
                for link in solution.links:
                    if link.valid:
                        adjacency.setdefault(link.source_id, set()).add(link.target_id)
                        adjacency.setdefault(link.target_id, set()).add(link.source_id)
                component_by_site: dict[str, tuple[str, int]] = {}
                remaining = set(adjacency)
                while remaining:
                    first = min(remaining)
                    pending = [first]
                    component: set[str] = set()
                    while pending:
                        node = pending.pop()
                        if node in component:
                            continue
                        component.add(node)
                        pending.extend(adjacency.get(node, set()) - component)
                    remaining.difference_update(component)
                    component_label = min(component)
                    for node in component:
                        component_by_site[node] = (component_label, len(component))
                best_component = (
                    component_by_site.get(best_source.id) if best_source is not None else None
                )
                inspection = {
                    "state": (
                        "unknown_links"
                        if source_results
                        and all(item["rejection"] == "unknown_terrain" for item in source_results)
                        else "evaluated"
                    ),
                    "latitude": latitude,
                    "longitude": longitude,
                    "ground_elevation_m": ground,
                    "surface_available": terrain.has_surface,
                    "surface_sample_available": surface_elevation is not None,
                    "surface_elevation_m": surface_elevation,
                    "client": encode(client),
                    "selected_source_id": best_source.id if best_source else None,
                    "selected_source": encode(best_source) if best_source else None,
                    "selected_source_component": best_component[0] if best_component else None,
                    "selected_source_component_size": best_component[1] if best_component else None,
                    "sources": encode(source_results),
                    "profile_link": encode(profile_link),
                    "message": (
                        "Ground is known here, but terrain data is missing along every candidate path; RF coverage is unknown."
                        if source_results
                        and all(item["rejection"] == "unknown_terrain" for item in source_results)
                        else None
                    ),
                }
                guard = {
                    "project_id": context["project_id"],
                    "route_fingerprint": context["route_fingerprint"],
                    "terrain_fingerprint": context["terrain_fingerprint"],
                    "settings_fingerprint": _plan_fingerprint(encode(settings_for_client)),
                    "snapshot_version": context["snapshot_version"],
                    "alternative_id": context["alternative_id"],
                }
                if not coverage_current(ws, guard):
                    raise HTTPException(409, "Route, terrain, project, or coverage settings changed during inspection")
                return inspection
        except HTTPException:
            raise
        except (ValueError, TypeError, KeyError) as exc:
            raise HTTPException(422, str(exc)) from exc

    @app.post("/api/coverage/inspect")
    def start_coverage_inspection(body: dict[str, Any], request: Request) -> Any:
        ws = workspace(request)
        try:
            context = coverage_context(ws, body)
            latitude, longitude = coordinate_pair(
                {"point": [body.get("latitude"), body.get("longitude")]}, "point"
            )
            with ws.lock:
                if ws.coverage_job and ws.coverage_job.get("state") in {"queued", "running"}:
                    raise HTTPException(409, "Wait for coverage calculation to finish before inspecting a point")
                if (
                    ws.project_id != context["project_id"]
                    or ws.snapshot_version != context["snapshot_version"]
                    or ws.result_plan_fingerprint != context["route_fingerprint"]
                    or _terrain_fingerprint(ws) != context["terrain_fingerprint"]
                    or _plan_fingerprint(
                        encode(parse_coverage_settings((ws.inputs or {}).get("coverage", {})))
                    )
                    != _plan_fingerprint(encode(context["settings"]))
                ):
                    raise HTTPException(409, "The selected route, terrain, or saved coverage settings changed; refresh before inspecting")
                with scheduler_lock:
                    if job_slots["outstanding"] >= max_outstanding_jobs:
                        raise HTTPException(429, "The planner queue is full; retry shortly")
                    queued = job_slots["outstanding"] >= max_workers
                    job_slots["outstanding"] += 1
                previous = ws.inspection_job
                if previous and previous.get("state") in {"queued", "running"}:
                    previous_event = ws.inspection_cancels.get(previous["job_id"])
                    if previous_event:
                        previous_event.set()
                    previous.update(state="superseded", stage="Replaced by a newer map click")
                job_id = secrets.token_hex(16)
                cancel_event = threading.Event()
                ws.inspection_cancels[job_id] = cancel_event
                job = {
                    "job_id": job_id,
                    "state": "queued" if queued else "running",
                    "stage": "Waiting for a planner worker" if queued else "Opening terrain",
                    "created_at": time.time(),
                    "project_id": context["project_id"],
                    "route_fingerprint": context["route_fingerprint"],
                    "terrain_fingerprint": context["terrain_fingerprint"],
                    "settings_fingerprint": _plan_fingerprint(encode(context["settings"])),
                    "snapshot_version": context["snapshot_version"],
                    "alternative_id": context["alternative_id"],
                    "latitude": latitude,
                    "longitude": longitude,
                    "source_count": len(context["sources"]),
                }
                ws.inspection_job = job
                ws.inspection_result = None
        except HTTPException:
            raise
        except (ValueError, TypeError, KeyError) as exc:
            raise HTTPException(422, str(exc)) from exc

        def run_inspection() -> None:
            try:
                with ws.lock:
                    if ws.inspection_job and ws.inspection_job.get("job_id") == job_id:
                        ws.inspection_job.update(
                            state="running", stage="Evaluating client links and terrain profile"
                        )
                if cancel_event.is_set():
                    raise InterruptedError("Location inspection superseded")
                inspection_body = {
                    **body,
                    "latitude": latitude,
                    "longitude": longitude,
                }
                result = _compute_mesh_coverage_inspection(ws, inspection_body, cancel_event)
                if not coverage_current(ws, job):
                    raise HTTPException(409, "Route, terrain, project, or coverage settings changed during inspection")
                with ws.lock:
                    if ws.inspection_job and ws.inspection_job.get("job_id") == job_id:
                        if cancel_event.is_set():
                            ws.inspection_job.update(
                                state="cancelled", stage="Location inspection cancelled"
                            )
                        else:
                            ws.inspection_result = result
                            ws.inspection_job.update(
                                state="complete",
                                stage="Location inspection complete",
                                finished_at=time.time(),
                            )
            except InterruptedError:
                with ws.lock:
                    if ws.inspection_job and ws.inspection_job.get("job_id") == job_id:
                        ws.inspection_job.update(
                            state="cancelled", stage="Location inspection superseded"
                        )
            except Exception as exc:
                detail = exc.detail if isinstance(exc, HTTPException) else str(exc)
                with ws.lock:
                    if ws.inspection_job and ws.inspection_job.get("job_id") == job_id:
                        ws.inspection_job.update(
                            state="failed", stage=str(detail), finished_at=time.time()
                        )
            finally:
                with ws.lock:
                    ws.inspection_cancels.pop(job_id, None)
                with scheduler_lock:
                    job_slots["outstanding"] = max(0, job_slots["outstanding"] - 1)

        try:
            scheduler.submit(run_inspection)
        except RuntimeError as exc:
            with scheduler_lock:
                job_slots["outstanding"] = max(0, job_slots["outstanding"] - 1)
            with ws.lock:
                ws.inspection_cancels.pop(job_id, None)
                if ws.inspection_job and ws.inspection_job.get("job_id") == job_id:
                    ws.inspection_job.update(state="failed", stage="Could not queue inspection")
            raise HTTPException(503, "Could not queue location inspection") from exc
        return dict(job)

    @app.get("/api/coverage/inspect/{job_id}")
    def get_coverage_inspection(job_id: str, request: Request) -> Any:
        ws = workspace(request)
        with ws.lock:
            job = ws.inspection_job
            if not job or job.get("job_id") != job_id:
                raise HTTPException(404, "Location inspection was superseded or is unavailable")
            response = dict(job)
            result = ws.inspection_result if job.get("state") == "complete" else None
        if job.get("state") == "complete" and not coverage_current(ws, job):
            response["stale"] = True
        if result is not None:
            response["result"] = encode(result)
        return response

    @app.post("/api/coverage/inspect/{job_id}/cancel")
    def cancel_coverage_inspection(job_id: str, request: Request) -> Any:
        ws = workspace(request)
        with ws.lock:
            job = ws.inspection_job
            if not job or job.get("job_id") != job_id:
                raise HTTPException(404, "Location inspection was superseded or is unavailable")
            if job.get("state") not in {"queued", "running"}:
                raise HTTPException(409, "There is no active location inspection")
            cancel_event = ws.inspection_cancels.get(job_id)
            if cancel_event:
                cancel_event.set()
            job.update(stage="Cancelling location inspection")
        return {"ok": True}

    def project_name(value: Any) -> str:
        if not isinstance(value, str):
            raise HTTPException(422, "Project name must be text")
        name = value.strip()
        if not name or len(name) > 80 or any(ord(character) < 32 for character in name):
            raise HTTPException(422, "Project name must be 1–80 printable characters")
        return name

    def ensure_projects_idle(ws: Workspace) -> None:
        if ws.status["state"] in {"queued", "running", "preparing"} or (
            ws.coverage_job and ws.coverage_job.get("state") in {"queued", "running"}
        ) or (
            ws.inspection_job and ws.inspection_job.get("state") in {"queued", "running"}
        ):
            raise HTTPException(409, "Wait for the current job before switching projects")

    @app.post("/api/projects/autosave")
    def autosave_project(body: dict[str, Any], request: Request) -> Any:
        ws = workspace(request)
        plan = body.get("plan")
        if not isinstance(plan, dict):
            raise HTTPException(422, "Plan must be a JSON object")
        if body.get("project_id") is not None and body.get("project_id") != ws.project_id:
            raise HTTPException(409, "Autosave belongs to a project that is no longer active")
        with ws.lock:
            if ws.status["state"] in {"queued", "running", "preparing"} or (
                ws.coverage_job and ws.coverage_job.get("state") in {"queued", "running"}
            ):
                raise HTTPException(409, "Plan inputs are locked while a job is active")
            if ws.inspection_job and ws.inspection_job.get("state") in {"queued", "running"}:
                cancel_event = ws.inspection_cancels.get(ws.inspection_job["job_id"])
                if cancel_event:
                    cancel_event.set()
            route_inputs_changed = _route_plan_fingerprint(ws.inputs or {}) != _route_plan_fingerprint(plan)
            if not project_store.autosave(ws.workspace_key, ws.project_id, plan):
                raise HTTPException(404, "Active project is no longer available")
            ws.inputs = plan
            if route_inputs_changed:
                ws.result_summary = None
                project_store.save_result_summary(ws.workspace_key, ws.project_id, None)
            _write_json_atomic(ws.directory / "plan.json", plan)
        saved_project = project_store.get(ws.workspace_key, ws.project_id)
        return {
            "ok": True,
            "project_id": ws.project_id,
            "updated_at": time.time(),
            "has_previous_revision": bool(saved_project and saved_project["previous_plan"] is not None),
        }

    @app.post("/api/projects/recover-previous")
    def recover_previous_project(request: Request) -> Any:
        ws = workspace(request)
        ensure_projects_idle(ws)
        project = project_store.get(ws.workspace_key, ws.project_id)
        if project is None or project["previous_plan"] is None:
            raise HTTPException(404, "No previous autosaved revision is available")
        plan = project["previous_plan"]
        with ws.lock:
            if not project_store.autosave(ws.workspace_key, ws.project_id, plan):
                raise HTTPException(404, "Active project is no longer available")
            ws.inputs = plan
            ws.result_summary = None
            project_store.save_result_summary(ws.workspace_key, ws.project_id, None)
            _write_json_atomic(ws.directory / "plan.json", plan)
        return {"id": ws.project_id, "name": ws.project["name"], "plan": plan}

    @app.post("/api/projects")
    def create_project(body: dict[str, Any], request: Request) -> Any:
        ws = workspace(request)
        ensure_projects_idle(ws)
        name = project_name(body.get("name"))
        plan = body.get("plan", {})
        if not isinstance(plan, dict):
            raise HTTPException(422, "Plan must be a JSON object")
        try:
            project = project_store.create(ws.workspace_key, name, plan)
        except sqlite3.IntegrityError as exc:
            raise HTTPException(409, "A project with that name already exists") from exc
        ws.activate_project(project)
        ws.storage_usage_bytes = None
        _refresh_recovery_summary(ws)
        _write_json_atomic(ws.directory / "plan.json", plan)
        return {"id": ws.project_id, "name": ws.project["name"], "plan": ws.inputs}

    @app.post("/api/projects/{project_id}/activate")
    def activate_project(project_id: str, request: Request) -> Any:
        ws = workspace(request)
        ensure_projects_idle(ws)
        project = project_store.activate(ws.workspace_key, project_id)
        if project is None:
            raise HTTPException(404, "Project not found")
        ws.activate_project(project)
        ws.storage_usage_bytes = None
        _refresh_recovery_summary(ws)
        return {"id": ws.project_id, "name": ws.project["name"], "plan": ws.inputs}

    @app.post("/api/projects/{project_id}/duplicate")
    def duplicate_project(project_id: str, body: dict[str, Any], request: Request) -> Any:
        ws = workspace(request)
        ensure_projects_idle(ws)
        name = project_name(body.get("name"))
        try:
            project = project_store.duplicate(ws.workspace_key, project_id, name)
        except sqlite3.IntegrityError as exc:
            raise HTTPException(409, "A project with that name already exists") from exc
        if project is None:
            raise HTTPException(404, "Project not found")
        ws.activate_project(project)
        ws.storage_usage_bytes = None
        _refresh_recovery_summary(ws)
        _write_json_atomic(ws.directory / "plan.json", ws.inputs)
        return {"id": ws.project_id, "name": ws.project["name"], "plan": ws.inputs}

    @app.patch("/api/projects/{project_id}")
    def rename_project(project_id: str, body: dict[str, Any], request: Request) -> Any:
        ws = workspace(request)
        name = project_name(body.get("name"))
        try:
            renamed = project_store.rename(ws.workspace_key, project_id, name)
        except sqlite3.IntegrityError as exc:
            raise HTTPException(409, "A project with that name already exists") from exc
        if not renamed:
            raise HTTPException(404, "Project not found")
        if ws.project_id == project_id:
            ws.project["name"] = name
        return {"id": project_id, "name": name}

    @app.post("/api/projects/{project_id}/archive")
    def archive_project(project_id: str, request: Request) -> Any:
        ws = workspace(request)
        try:
            archived = project_store.archive(ws.workspace_key, project_id)
        except ValueError as exc:
            raise HTTPException(409, str(exc)) from exc
        if not archived:
            raise HTTPException(404, "Project not found")
        return {"ok": True}

    @app.delete("/api/projects/{project_id}")
    def delete_project(project_id: str, request: Request) -> Any:
        ws = workspace(request)
        target = project_store.get(ws.workspace_key, project_id)
        if target is None:
            raise HTTPException(404, "Project not found")
        workspace_root = (root / ws.workspace_key).resolve()
        if project_store.project_path(ws.workspace_key, target) == workspace_root:
            raise HTTPException(409, "The migrated workspace project cannot be permanently deleted")
        try:
            project = project_store.delete(ws.workspace_key, project_id)
        except ValueError as exc:
            raise HTTPException(409, str(exc)) from exc
        if project is None:
            raise HTTPException(404, "Project not found")
        directory = project_store.project_path(ws.workspace_key, project)
        shutil.rmtree(directory)
        ws.storage_usage_bytes = None
        return {"ok": True}

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
            if ws.status["state"] in {"queued", "running", "preparing"} or (
                ws.coverage_job and ws.coverage_job.get("state") in {"queued", "running"}
            ) or (
                ws.inspection_job and ws.inspection_job.get("state") in {"queued", "running"}
            ):
                raise HTTPException(409, "A planner or terrain job is already active")
            with scheduler_lock:
                if job_slots["outstanding"] >= max_outstanding_jobs:
                    raise HTTPException(429, "The planner queue is full; retry shortly")
                queued = job_slots["outstanding"] >= max_workers
                job_slots["outstanding"] += 1
            ws.cancel.clear()
            ws.result = None
            ws.result_plan_fingerprint = None
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
            project_store.set_run_state(
                ws.workspace_key, ws.project_id, "queued" if queued else "preparing"
            )

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
                    project_store.autosave(ws.workspace_key, ws.project_id, body)
                    ws.result_summary = None
                    project_store.save_result_summary(ws.workspace_key, ws.project_id, None)
                    _write_json_atomic(ws.directory / "plan.json", body)
                    ws.input_revision += 1
                    ws.storage_usage_bytes = None
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
                with ws.lock:
                    terrain_state = ws.status["state"]
                project_store.set_run_state(ws.workspace_key, ws.project_id, terrain_state)
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
            if ws.status["state"] in {"queued", "running", "preparing"} or (
                ws.coverage_job and ws.coverage_job.get("state") in {"queued", "running"}
            ) or (
                ws.inspection_job and ws.inspection_job.get("state") in {"queued", "running"}
            ):
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
                ws.result_plan_fingerprint = None
                ws.result_summary = None
                ws.snapshot_version = 0
                ws.storage_usage_bytes = None
                project_store.save_result_summary(ws.workspace_key, ws.project_id, None)
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
            if ws.status["state"] in {"queued", "running", "preparing"} or (
                ws.coverage_job and ws.coverage_job.get("state") in {"queued", "running"}
            ) or (
                ws.inspection_job and ws.inspection_job.get("state") in {"queued", "running"}
            ):
                raise HTTPException(409, "Wait for the current job")
            for path in _terrain_paths(ws, kind):
                path.unlink()
            ws.input_revision += 1
            ws.result = None
            ws.result_plan_fingerprint = None
            ws.result_summary = None
            ws.snapshot_version = 0
            ws.storage_usage_bytes = None
            project_store.save_result_summary(ws.workspace_key, ws.project_id, None)
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
                if not isinstance(item.get("height_override", False), bool):
                    raise ValueError("MeshCore height_override must be a boolean")
                if item.get("height_override"):
                    height = item.get("antenna_height_m")
                    if (
                        not isinstance(height, (int, float))
                        or isinstance(height, bool)
                        or not math.isfinite(height)
                        or not 0.1 <= height <= 500
                    ):
                        raise ValueError("MeshCore antenna-height override must be between 0.1 and 500 m")
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
            if ws.status["state"] in {"queued", "running", "preparing"} or (
                ws.coverage_job and ws.coverage_job.get("state") in {"queued", "running"}
            ) or (
                ws.inspection_job and ws.inspection_job.get("state") in {"queued", "running"}
            ):
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
            ws.result_plan_fingerprint = _route_plan_fingerprint(body)
            ws.result_summary = None
            ws.inputs = body
            ws.job_id = secrets.token_hex(12)
            ws.input_revision += 1
            ws.snapshot_version = 0
            ws.keep_result_on_cancel = False
            job_id = ws.job_id
            input_revision = ws.input_revision
            project_store.autosave(ws.workspace_key, ws.project_id, body)
            project_store.save_result_summary(ws.workspace_key, ws.project_id, None)
            _write_json_atomic(ws.directory / "plan.json", body)
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
            project_store.set_run_state(
                ws.workspace_key, ws.project_id, "queued" if queued else "running"
            )

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
                                antenna_height_m=(
                                    float(item["antenna_height_m"])
                                    if item.get("height_override")
                                    else rf.router.height_agl_m
                                ),
                                origin=SiteOrigin.KNOWN,
                                locked=True,
                                required=item.get("policy", "optional") == "required",
                                height_override=bool(item.get("height_override", False)),
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
                with ws.lock:
                    final_state = ws.status["state"]
                    final_result = ws.result
                    final_revision = ws.input_revision
                    final_snapshot = ws.snapshot_version
                if final_state in {"complete", "stopped"} and final_result is not None:
                    payload = result_payload(
                        final_result, job_id, final_revision, final_snapshot
                    )
                    summary = {
                        "found": payload["found"],
                        "router_count": payload["router_count"],
                        "existing_router_count": payload["existing_router_count"],
                        "proposed_router_count": payload["proposed_router_count"],
                        "elapsed_seconds": payload["elapsed_seconds"],
                        "search_complete": payload["search_complete"],
                        "active_alternative_id": payload["active_alternative_id"],
                        "alternatives": [
                            {key: value for key, value in item.items() if key != "route"}
                            for item in payload["alternatives"]
                        ],
                        "plan_fingerprint": _route_plan_fingerprint(ws.inputs),
                        "terrain_fingerprint": _terrain_fingerprint(ws),
                        "saved_at": time.time(),
                    }
                    ws.result_summary = summary
                    project_store.save_result_summary(ws.workspace_key, ws.project_id, summary)
                project_store.set_run_state(ws.workspace_key, ws.project_id, final_state)
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
            if ws.status["state"] in {"queued", "running", "preparing"} or (
                ws.coverage_job and ws.coverage_job.get("state") in {"queued", "running"}
            ) or (
                ws.inspection_job and ws.inspection_job.get("state") in {"queued", "running"}
            ):
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
            payload = result_payload(
                ws.result, ws.job_id, ws.input_revision, ws.snapshot_version
            )
            summary = ws.result_summary or {
                "found": payload["found"],
                "router_count": payload["router_count"],
                "existing_router_count": payload["existing_router_count"],
                "proposed_router_count": payload["proposed_router_count"],
                "elapsed_seconds": payload["elapsed_seconds"],
                "search_complete": payload["search_complete"],
                "plan_fingerprint": _route_plan_fingerprint(ws.inputs),
                "terrain_fingerprint": _terrain_fingerprint(ws),
                "saved_at": time.time(),
            }
            summary["active_alternative_id"] = payload["active_alternative_id"]
            summary["alternatives"] = [
                {key: value for key, value in item.items() if key != "route"}
                for item in payload["alternatives"]
            ]
            ws.result_summary = summary
            project_store.save_result_summary(ws.workspace_key, ws.project_id, summary)
            return payload

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
            if not ws.inputs:
                raise HTTPException(404, "Run a plan first")
            _write_json_atomic(path, ws.inputs)
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
