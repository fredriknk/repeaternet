"""Bounded, offline-friendly coverage exports and printable report fragments."""

from __future__ import annotations

import html
import json
import math
from collections.abc import Callable, Iterable, Iterator
from pathlib import PurePosixPath
from typing import Any

from pyproj import Transformer
from shapely import intersects_xy
from shapely.geometry import Polygon, box
from shapely.ops import transform as transform_geometry

GRID_STATES = (
    "covered",
    "uncovered",
    "unknown_terrain",
    "unresolved",
    "not_evaluated",
    "outside_area",
)


def iter_complete_grid(
    cells: Iterable[dict[str, Any]],
    job: dict[str, Any],
    *,
    on_stored_cell: Callable[[], None] | None = None,
) -> Iterator[dict[str, Any]]:
    """Yield every requested grid index, synthesizing explicit unsampled cells."""
    requested = int(job.get("requested_cells", job.get("total", 0)))
    rows, columns = int(job["rows"]), int(job["columns"])
    if requested != rows * columns or requested < 1:
        raise ValueError("Saved coverage grid dimensions are inconsistent")
    left, bottom, right, top = (float(value) for value in job["grid_bounds_projected"])
    cell_size = float(job["effective_cell_size_m"])
    reverse = Transformer.from_crs(job["terrain_crs"], 4326, always_xy=True)
    settings = job.get("settings", {})
    raw_polygon = (
        settings.get("area_polygon_wgs84")
        if isinstance(settings, dict) and settings.get("area_mode") == "drawn"
        else None
    )
    area_geometry = box(left, bottom, right, top)
    if isinstance(raw_polygon, list) and len(raw_polygon) >= 4:
        forward = Transformer.from_crs(4326, job["terrain_crs"], always_xy=True)
        area_geometry = transform_geometry(
            forward.transform, Polygon(raw_polygon)
        )
    iterator = iter(cells)
    current = next(iterator, None)
    previous_index = -1
    for index in range(requested):
        if current is not None:
            current_index = current.get("index")
            if not isinstance(current_index, int) or current_index <= previous_index:
                raise ValueError("Coverage result cells are not strictly ordered")
            if current_index < index:
                raise ValueError("Coverage result contains an out-of-grid index")
        if current is not None and current.get("index") == index:
            previous_index = index
            if on_stored_cell:
                on_stored_cell()
            yield current
            current = next(iterator, None)
            continue
        row, column = divmod(index, columns)
        x = left + (column + 0.5) * cell_size
        y = top - (row + 0.5) * cell_size
        longitude, latitude = reverse.transform(x, y)
        outside_area = area_geometry is not None and not bool(
            intersects_xy(area_geometry, x, y)
        )
        yield {
            "index": index,
            "x": x,
            "y": y,
            "longitude": longitude,
            "latitude": latitude,
            "state": "outside_area" if outside_area else "not_evaluated",
            "source_count": 0,
            "unknown_sources": 0,
            "unresolved_sources": 0,
            "best_margin_db": None,
            "best_source_id": None,
            "sources": [],
        }
    if current is not None or next(iterator, None) is not None:
        raise ValueError("Coverage result contains more cells than its grid")


def summarize_coverage_cells(
    cells: Iterable[dict[str, Any]], job: dict[str, Any]
) -> dict[str, Any]:
    counts = dict.fromkeys(GRID_STATES, 0)
    evaluated_cells = covered_cells = unknown_source_evaluations = 0
    unresolved_source_evaluations = 0
    stored_cells = 0

    def count_stored_cell() -> None:
        nonlocal stored_cells
        stored_cells += 1

    for cell in iter_complete_grid(cells, job, on_stored_cell=count_stored_cell):
        state = cell.get("state")
        if state not in counts:
            raise ValueError(f"Unknown coverage cell state: {state}")
        counts[state] += 1
        covered_cells += state == "covered"
        evaluated_cells += state in {"covered", "uncovered"}
        unknown_source_evaluations += int(cell.get("unknown_sources") or 0)
        unresolved_source_evaluations += int(cell.get("unresolved_sources") or 0)
    return {
        "requested_cells": int(job.get("requested_cells", job.get("total", 0))),
        "stored_cells": stored_cells,
        "evaluated_cells": evaluated_cells,
        "covered_cells": covered_cells,
        "unknown_cells": counts["unknown_terrain"],
        "unresolved_cells": counts["unresolved"],
        "not_evaluated_cells": counts["not_evaluated"],
        "outside_area_cells": counts["outside_area"],
        "unknown_source_evaluations": unknown_source_evaluations,
        "unresolved_source_evaluations": unresolved_source_evaluations,
        "state_counts": counts,
    }


def coverage_export_metadata(
    job: dict[str, Any],
    summary: dict[str, Any],
    terrain_fingerprint: list[list[str | int]],
    *,
    stale: bool,
) -> dict[str, Any]:
    terrain_inputs = [
        {
            "kind": "surface" if "/dom/" in f"/{path}/" else "ground",
            "filename": PurePosixPath(str(path)).name,
            "size_bytes": size,
            "modified_ns": modified,
        }
        for path, size, modified in terrain_fingerprint
    ]
    return {
        "schema": "repeaternet-coverage-export-v1",
        "job_id": job["job_id"],
        "project_id": job.get("project_id"),
        "alternative_id": job.get("alternative_id"),
        "snapshot_version": job.get("snapshot_version"),
        "input_revision": job.get("input_revision"),
        "job_state": job.get("state"),
        "complete": (
            job.get("state") == "complete"
            and not stale
            and summary["not_evaluated_cells"] == 0
            and summary["unresolved_cells"] == 0
        ),
        "partial": (
            job.get("state") != "complete"
            or summary["not_evaluated_cells"] > 0
            or summary["unresolved_cells"] > 0
        ),
        "stale": stale,
        "created_at": job.get("created_at"),
        "finished_at": job.get("finished_at"),
        "route_fingerprint": job.get("route_fingerprint"),
        "terrain_fingerprint": terrain_fingerprint,
        "terrain_crs": job.get("terrain_crs"),
        "coordinate_reference_system": "OGC:CRS84",
        "terrain_inputs": terrain_inputs,
        "area_bounds_wgs84": job.get("area_bounds_wgs84"),
        "grid_bounds_projected": job.get("grid_bounds_projected"),
        "rows": job.get("rows"),
        "columns": job.get("columns"),
        "requested_cell_size_m": job.get("settings", {}).get("cell_size_m"),
        "effective_cell_size_m": job.get("effective_cell_size_m"),
        "profile_step_m": job.get("settings", {}).get("profile_step_m"),
        "maximum_profile_samples": job.get("settings", {}).get("maximum_profile_samples"),
        "surface_available": job.get("surface_available"),
        "mode": job.get("settings", {}).get("mode"),
        "settings": job.get("settings"),
        "radio_settings": job.get("radio_settings"),
        "source_ids": job.get("source_ids", []),
        "source_sites": job.get("source_sites", []),
        "router_ids": job.get("router_ids", []),
        "network_sites": job.get("network_sites", job.get("source_sites", [])),
        "network_links": job.get("report_links", job.get("network_links", [])),
        "counts": summary,
        "coverage_model_version": job.get("model_version"),
        "prediction_warning": "Terrain-based RF planning estimate; not measured field coverage or a delivery guarantee.",
    }


def _json(value: Any) -> str:
    return json.dumps(value, ensure_ascii=False, separators=(",", ":"), allow_nan=False)


def _cell_properties(cell: dict[str, Any]) -> dict[str, Any]:
    return {
        "feature_kind": "coverage_cell",
        "index": cell["index"],
        "state": cell["state"],
        "source_count": cell.get("source_count", 0),
        "unknown_sources": cell.get("unknown_sources", 0),
        "unresolved_sources": cell.get("unresolved_sources", 0),
        "best_margin_db": cell.get("best_margin_db"),
        "best_source_id": cell.get("best_source_id"),
        "sources": cell.get("sources", []),
    }


def geojson_chunks(
    cells: Iterable[dict[str, Any]],
    job: dict[str, Any],
    metadata: dict[str, Any],
    target_report: dict[str, Any] | None = None,
) -> Iterator[str]:
    """Stream WGS84 cell polygons and saved target geometries as one collection."""
    left, bottom, right, top = (float(value) for value in job["grid_bounds_projected"])
    half = float(job["effective_cell_size_m"]) / 2
    reverse = Transformer.from_crs(job["terrain_crs"], 4326, always_xy=True)
    yield _json({"type": "FeatureCollection", "name": f"coverage-{job['job_id']}", "metadata": metadata, "features": []})[:-2]
    first = True
    for cell in iter_complete_grid(cells, job):
        if cell.get("state") == "outside_area":
            feature = {
                "type": "Feature",
                "id": f"cell-{cell['index']}",
                "geometry": None,
                "properties": _cell_properties(cell),
            }
            yield ("" if first else ",") + _json(feature)
            first = False
            continue
        x, y = float(cell["x"]), float(cell["y"])
        corners = [
            (max(left, x - half), max(bottom, y - half)),
            (min(right, x + half), max(bottom, y - half)),
            (min(right, x + half), min(top, y + half)),
            (max(left, x - half), min(top, y + half)),
            (max(left, x - half), max(bottom, y - half)),
        ]
        coordinates = [list(reverse.transform(cx, cy)) for cx, cy in corners]
        feature = {
            "type": "Feature",
            "id": f"cell-{cell['index']}",
            "geometry": {"type": "Polygon", "coordinates": [coordinates]},
            "properties": _cell_properties(cell),
        }
        yield ("" if first else ",") + _json(feature)
        first = False
    export_settings = job.get("settings", {})
    area_polygon = (
        export_settings.get("area_polygon_wgs84")
        if isinstance(export_settings, dict) and export_settings.get("area_mode") == "drawn"
        else None
    )
    if isinstance(area_polygon, list) and len(area_polygon) >= 4:
        feature = {
            "type": "Feature",
            "id": "coverage-analysis-area",
            "geometry": {"type": "Polygon", "coordinates": [area_polygon]},
            "properties": {
                "feature_kind": "analysis_area",
                "state": "requested_area",
            },
        }
        yield ("" if first else ",") + _json(feature)
        first = False
    if target_report:
        outcomes = {item["id"]: item for item in target_report.get("targets", [])}
        for target in target_report.get("target_features", []):
            item = outcomes.get(target.get("id"), {})
            feature = {
                "type": "Feature",
                "id": f"target-{target.get('id')}",
                "geometry": target.get("geometry"),
                "properties": {
                    **target.get("properties", {}),
                    "feature_kind": "coverage_target",
                    "target_id": target.get("id"),
                    "coverage_state": item.get("state", "not_assessed"),
                    "coverage_message": item.get("message"),
                },
            }
            yield ("" if first else ",") + _json(feature)
            first = False
    yield "]}"


def json_export_chunks(
    cells: Iterable[dict[str, Any]],
    job: dict[str, Any],
    metadata: dict[str, Any],
    target_report: dict[str, Any] | None = None,
    scenario_comparison: dict[str, Any] | None = None,
) -> Iterator[str]:
    """Stream compact settings, assumptions, summaries, and all grid cell data."""
    prefix = {"metadata": metadata, "target_report": target_report, "scenario_comparison": scenario_comparison}
    prefix_text = _json(prefix)
    yield prefix_text[:-1] + ',"cells":['
    for index, cell in enumerate(iter_complete_grid(cells, job)):
        yield ("" if index == 0 else ",") + _json(cell)
    yield "]}"


def _color(cell: dict[str, Any], mode: str) -> str:
    if cell.get("state") == "outside_area":
        return "#f7f8f7"
    if cell.get("state") == "unknown_terrain":
        return "#a7afb0"
    if cell.get("state") == "unresolved":
        return "#9476b7"
    if cell.get("state") == "not_evaluated":
        return "#e2e5e3"
    sources = cell.get("sources", [])
    if mode == "overlap":
        count = sum(bool(item.get("valid_two_way")) for item in sources)
        return "#ce796d" if count == 0 else "#f0ca62" if count == 1 else "#61a88c" if count == 2 else "#287b64"
    direction = "downlink" if mode == "downlink" else "uplink" if mode == "uplink" else "two_way"
    valid = [
        item
        for item in sources
        if item.get(f"valid_{direction}") and item.get(f"{direction}_margin_db") is not None
    ]
    if not valid:
        return "#ce796d"
    margin = max(float(item[f"{direction}_margin_db"]) for item in valid)
    return "#287b64" if margin >= 10 else "#61a88c" if margin >= 5 else "#f0ca62" if margin >= 0 else "#ce796d"


def _map_point(longitude: float, latitude: float, bounds: tuple[float, float, float, float]) -> tuple[float, float]:
    south, west, north, east = bounds
    width = max(east - west, 1e-9)
    height = max(north - south, 1e-9)
    return 40 + (longitude - west) / width * 920, 40 + (north - latitude) / height * 520


def _html(value: Any) -> str:
    return html.escape(str(value), quote=True)


def printable_report_chunks(
    cells: Iterable[dict[str, Any]],
    job: dict[str, Any],
    metadata: dict[str, Any],
    target_report: dict[str, Any] | None = None,
    scenario_comparison: dict[str, Any] | None = None,
) -> Iterator[str]:
    """Generate a self-contained HTML report with an inline, offline SVG map."""
    south, west, north, east = (
        float(value) for value in job["area_bounds_wgs84"]
    )
    bounds = (south, west, north, east)
    reverse = Transformer.from_crs(job["terrain_crs"], 4326, always_xy=True)
    report_state = "STALE" if metadata["stale"] else "PARTIAL" if metadata["partial"] else "COMPLETE"
    counts = metadata["counts"]
    export_settings = job.get("settings", {})
    yield """<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>Predicted mesh coverage</title><style>
    :root{color-scheme:light;--ink:#19372d;--muted:#5d7168;--line:#d5dfda;--paper:#fff;--bg:#eef2ef}*{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--ink);font:15px/1.5 Segoe UI,Arial,sans-serif}main{max-width:1120px;margin:28px auto;background:var(--paper);padding:36px 44px;box-shadow:0 8px 32px #142d2018}header{display:flex;justify-content:space-between;gap:24px;border-bottom:2px solid var(--ink);padding-bottom:18px}h1{font-size:30px;margin:0}h2{margin:30px 0 12px;font-size:20px}h3{font-size:16px}.sub,.muted{color:var(--muted)}.badge{align-self:flex-start;border:1px solid #537465;border-radius:999px;padding:5px 12px;font-weight:700;letter-spacing:.07em}.badge.PARTIAL,.badge.STALE{color:#8d441f;border-color:#bf8058}.metrics{display:grid;grid-template-columns:repeat(4,1fr);gap:10px;margin:20px 0}.metric{padding:12px;border:1px solid var(--line);border-radius:8px}.metric strong{display:block;font-size:21px}.metric span{color:var(--muted);font-size:12px}svg{display:block;width:100%;height:auto;border:1px solid var(--line);background:#f6f8f7}.legend{display:flex;flex-wrap:wrap;gap:14px;margin:12px 0}.swatch{display:inline-block;width:13px;height:13px;vertical-align:-2px;margin-right:5px;border:1px solid #50645b}table{width:100%;border-collapse:collapse;font-size:13px}th,td{text-align:left;padding:7px 9px;border-bottom:1px solid var(--line);vertical-align:top}th{background:#f1f5f2}pre{white-space:pre-wrap;overflow-wrap:anywhere;background:#f3f6f4;padding:14px;border-radius:6px;font-size:12px}.warning{background:#fff5e9;border-left:4px solid #bb7645;padding:12px}.print{position:fixed;right:20px;bottom:20px;border:0;border-radius:8px;background:#19372d;color:#fff;padding:12px 18px;font-weight:700;cursor:pointer}@media print{body{background:#fff}main{margin:0;max-width:none;box-shadow:none;padding:12mm}.print{display:none}h2{break-after:avoid}table,svg{break-inside:avoid}}@media(max-width:700px){main{margin:0;padding:22px}.metrics{grid-template-columns:repeat(2,1fr)}header{display:block}.badge{display:inline-block;margin-top:10px}}
    </style></head><body><button class="print" onclick="window.print()">Print report</button><main>"""
    yield f"<header><div><h1>Predicted mesh coverage</h1><div class='sub'>Project { _html(metadata.get('project_id')) } · alternative { _html(metadata.get('alternative_id')) } · job { _html(job['job_id']) }</div></div><span class='badge {report_state}'>{report_state}</span></header>"
    yield f"<p class='warning'>{_html(metadata['prediction_warning'])} {('This export includes partial or unresolved cells; inspect the state counts and profile-limit settings.' if metadata['partial'] else '')} {('Inputs changed since this run; treat it as a historical snapshot.' if metadata['stale'] else '')}</p>"
    yield "<section class='metrics'>"
    for label, value in (
        ("Covered cells", counts["covered_cells"]),
        ("Evaluated cells", counts["evaluated_cells"]),
        ("Unknown terrain", counts["unknown_cells"]),
        ("Unresolved profile limits", counts["unresolved_cells"]),
        ("Not evaluated", counts["not_evaluated_cells"]),
        ("Outside requested area", counts["outside_area_cells"]),
    ):
        yield f"<div class='metric'><strong>{_html(value)}</strong><span>{_html(label)}</span></div>"
    yield "</section><h2>Coverage and selected network</h2><svg viewBox='0 0 1000 600' role='img' aria-label='Offline map of saved coverage grid, network links and target locations'>"
    for cell in iter_complete_grid(cells, job):
        if cell.get("state") == "outside_area":
            continue
        x, y = float(cell["x"]), float(cell["y"])
        half = float(job["effective_cell_size_m"]) / 2
        corners = [
            (max(float(job["grid_bounds_projected"][0]), x - half), max(float(job["grid_bounds_projected"][1]), y - half)),
            (min(float(job["grid_bounds_projected"][2]), x + half), max(float(job["grid_bounds_projected"][1]), y - half)),
            (min(float(job["grid_bounds_projected"][2]), x + half), min(float(job["grid_bounds_projected"][3]), y + half)),
            (max(float(job["grid_bounds_projected"][0]), x - half), min(float(job["grid_bounds_projected"][3]), y + half)),
        ]
        points = " ".join(
            f"{_map_point(*reverse.transform(cx, cy), bounds)[0]:.1f},{_map_point(*reverse.transform(cx, cy), bounds)[1]:.1f}"
            for cx, cy in corners
        )
        yield f"<polygon points='{points}' fill='{_color(cell, str(job.get('mode', 'two_way')))}' stroke='#ffffff' stroke-width='.7'><title>Cell {cell['index']}: {_html(cell['state'])}</title></polygon>"
    area_polygon = (
        export_settings.get("area_polygon_wgs84")
        if isinstance(export_settings, dict) and export_settings.get("area_mode") == "drawn"
        else None
    )
    if isinstance(area_polygon, list) and len(area_polygon) >= 4:
        points = " ".join(
            f"{_map_point(float(point[0]), float(point[1]), bounds)[0]:.1f},{_map_point(float(point[0]), float(point[1]), bounds)[1]:.1f}"
            for point in area_polygon
        )
        yield f"<polygon points='{points}' fill='none' stroke='#19372d' stroke-width='2' stroke-dasharray='8 5'><title>Requested analysis area</title></polygon>"
    sites = job.get("network_sites", job.get("source_sites", []))
    sites_by_id = {item.get("id"): item for item in sites}
    for link in job.get("report_links", []):
        left_site, right_site = sites_by_id.get(link.get("source_id")), sites_by_id.get(link.get("target_id"))
        if not left_site or not right_site or left_site.get("latitude") is None or right_site.get("latitude") is None:
            continue
        x1, y1 = _map_point(float(left_site["longitude"]), float(left_site["latitude"]), bounds)
        x2, y2 = _map_point(float(right_site["longitude"]), float(right_site["latitude"]), bounds)
        color = (
            "#9476b7"
            if link.get("unresolved") or link.get("valid") is None
            else "#1b4f73"
            if link.get("valid")
            else "#b44735"
        )
        label = "unresolved profile limit" if color == "#9476b7" else "valid" if link.get("valid") else "invalid"
        yield f"<line x1='{x1:.1f}' y1='{y1:.1f}' x2='{x2:.1f}' y2='{y2:.1f}' stroke='{color}' stroke-width='3' stroke-dasharray='{'' if link.get('valid') is True else '7 5'}'><title>{_html(link.get('source_id'))} to {_html(link.get('target_id'))}: {label}</title></line>"
    for site in sites:
        if site.get("latitude") is None or site.get("longitude") is None:
            continue
        px, py = _map_point(float(site["longitude"]), float(site["latitude"]), bounds)
        yield f"<circle cx='{px:.1f}' cy='{py:.1f}' r='6' fill='#fff' stroke='#19372d' stroke-width='3'><title>{_html(site.get('id'))}</title></circle>"
    if target_report:
        outcomes = {item["id"]: item for item in target_report.get("targets", [])}
        for target in target_report.get("target_features", []):
            geometry, target_id = target.get("geometry", {}), target.get("id")
            state = outcomes.get(target_id, {}).get("state", "unknown")
            color = "#287b64" if state == "pass" else "#ce796d" if state == "fail" else "#aa702f"
            geometry_type, coordinates = geometry.get("type"), geometry.get("coordinates", [])
            if geometry_type == "Point":
                px, py = _map_point(float(coordinates[0]), float(coordinates[1]), bounds)
                yield f"<circle cx='{px:.1f}' cy='{py:.1f}' r='9' fill='{color}' stroke='#fff' stroke-width='2'><title>{_html(target.get('properties', {}).get('name'))}: {_html(state)}</title></circle>"
            elif geometry_type in {"LineString", "Polygon"}:
                line = coordinates if geometry_type == "LineString" else coordinates[0]
                points = " ".join(f"{_map_point(float(point[0]), float(point[1]), bounds)[0]:.1f},{_map_point(float(point[0]), float(point[1]), bounds)[1]:.1f}" for point in line)
                tag = "polyline" if geometry_type == "LineString" else "polygon"
                yield f"<{tag} points='{points}' fill='{color if geometry_type == 'Polygon' else 'none'}' fill-opacity='.18' stroke='{color}' stroke-width='3'><title>{_html(target.get('properties', {}).get('name'))}: {_html(state)}</title></{tag}>"
    yield "</svg><div class='legend'><span><i class='swatch' style='background:#287b64'></i> ≥ 10 dB</span><span><i class='swatch' style='background:#61a88c'></i> 5–10 dB</span><span><i class='swatch' style='background:#f0ca62'></i> 0–5 dB</span><span><i class='swatch' style='background:#ce796d'></i> Not usable</span><span><i class='swatch' style='background:#a7afb0'></i> Unknown terrain</span><span><i class='swatch' style='background:#9476b7'></i> Unresolved profile limit</span><span><i class='swatch' style='background:#e2e5e3'></i> Not evaluated</span><span><i class='swatch' style='background:#f7f8f7'></i> Outside requested area</span></div>"
    yield "<h2>Coverage states</h2><table><thead><tr><th>State</th><th>Cells</th></tr></thead><tbody>"
    for state, count in counts["state_counts"].items():
        yield f"<tr><td>{_html(state)}</td><td>{_html(count)}</td></tr>"
    yield "</tbody></table><h2>Coverage targets</h2>"
    if target_report:
        yield "<table><thead><tr><th>Target</th><th>Outcome</th><th>Margin / area / length detail</th></tr></thead><tbody>"
        for item in target_report.get("targets", []):
            detail = item.get("best_two_way_margin_db", item.get("covered_area_m2", item.get("covered_length_m")))
            yield f"<tr><td>{_html(item.get('name'))}</td><td>{_html(item.get('state'))}</td><td>{_html(detail if detail is not None else item.get('message'))}</td></tr>"
        yield "</tbody></table>"
    else:
        yield "<p class='muted'>No saved target-assessment report is attached to this coverage run.</p>"
    yield "<h2>Scenario comparison</h2>"
    if scenario_comparison:
        yield f"<p>{_html(scenario_comparison.get('baseline_job_id'))} → {_html(scenario_comparison.get('scenario_job_id'))}</p><pre>{_html(json.dumps(scenario_comparison.get('counts', {}), indent=2))}</pre>"
    else:
        yield "<p class='muted'>No compatible scenario comparison was selected for this report.</p>"
    yield "<h2>Selected network and weakest links</h2><table><thead><tr><th>Link</th><th>Distance</th><th>Worst margin</th><th>Validity</th></tr></thead><tbody>"
    links = sorted(job.get("report_links", []), key=lambda item: item.get("worst_margin_db", math.inf))[:10]
    for link in links:
        unresolved = bool(link.get("unresolved")) or link.get("valid") is None
        margin = link.get("worst_margin_db")
        validity = "unresolved: profile sample limit" if unresolved else "valid" if link.get("valid") else "invalid"
        if link.get("rejection_detail"):
            validity += f" · {link['rejection_detail']}"
        margin_text = f"{_html(margin)} dB" if margin is not None else "—"
        yield f"<tr><td>{_html(link.get('source_id'))} → {_html(link.get('target_id'))}</td><td>{_html(round(link.get('distance_m', 0)))} m</td><td>{margin_text}</td><td>{_html(validity)}</td></tr>"
    if not links:
        yield "<tr><td colspan='4'>Certified link metrics were not retained in this historical coverage snapshot.</td></tr>"
    yield "</tbody></table><h2>Terrain provenance and model assumptions</h2>"
    yield f"<p>CRS: {_html(metadata.get('terrain_crs'))} · grid: {_html(metadata.get('rows'))} × {_html(metadata.get('columns'))} at {_html(metadata.get('effective_cell_size_m'))} m · profile step: {_html(metadata.get('profile_step_m'))} m · surface data: {_html(metadata.get('surface_available'))}</p>"
    yield f"<pre>{_html(json.dumps({'terrain_inputs': metadata.get('terrain_inputs'), 'terrain_fingerprint': metadata.get('terrain_fingerprint'), 'settings': metadata.get('settings'), 'radio_settings': metadata.get('radio_settings'), 'source_ids': metadata.get('source_ids')}, indent=2, ensure_ascii=False))}</pre><footer class='muted'>Generated from saved, project-scoped results. No basemap or remote assets are embedded.</footer></main></body></html>"
