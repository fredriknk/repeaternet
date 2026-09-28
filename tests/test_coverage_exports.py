from __future__ import annotations

import json

from pyproj import Transformer

from rf_router_planner.export.coverage_report import (
    coverage_export_metadata,
    geojson_chunks,
    iter_complete_grid,
    json_export_chunks,
    printable_report_chunks,
    summarize_coverage_cells,
)


def export_job() -> dict:
    reverse = Transformer.from_crs(25833, 4326, always_xy=True)
    southwest = reverse.transform(500000, 6650000)
    northeast = reverse.transform(500100, 6650100)
    return {
        "job_id": "a" * 32,
        "project_id": "project-1",
        "alternative_id": "alternative-1",
        "snapshot_version": 3,
        "input_revision": 2,
        "state": "cancelled",
        "created_at": 10,
        "finished_at": 12,
        "route_fingerprint": "route-fingerprint",
        "terrain_fingerprint": [["dtm/tile.tif", 100, 123]],
        "terrain_crs": "EPSG:25833",
        "grid_bounds_projected": [500000, 6650000, 500100, 6650100],
        "area_bounds_wgs84": [southwest[1], southwest[0], northeast[1], northeast[0]],
        "rows": 2,
        "columns": 2,
        "requested_cells": 4,
        "total": 4,
        "effective_cell_size_m": 50,
        "settings": {
            "mode": "two_way",
            "cell_size_m": 50,
            "profile_step_m": 100,
            "maximum_profile_samples": 128,
            "client": {"height_agl_m": 1.5, "tx_power_dbm": 20},
        },
        "radio_settings": {"rf": {"frequency_mhz": 915}, "candidates": {}},
        "source_ids": ["R-1"],
        "source_sites": [],
        "router_ids": ["R-1"],
        "network_sites": [
            {"id": "R-1", "latitude": 60.0, "longitude": 10.0, "origin": "known"},
            {"id": "R-2", "latitude": 60.001, "longitude": 10.001, "origin": "known"},
        ],
        "report_links": [
            {
                "source_id": "R-1",
                "target_id": "R-2",
                "distance_m": 100,
                "worst_margin_db": 4.5,
                "valid": True,
                "los_clear": True,
                "fresnel_clear": True,
            }
        ],
        "network_links": [],
        "model_version": "coverage-v1",
        "surface_available": False,
    }


def cells() -> list[dict]:
    return [
        {
            "index": 0,
            "x": 500025,
            "y": 6650075,
            "longitude": 10,
            "latitude": 60,
            "state": "covered",
            "source_count": 1,
            "unknown_sources": 0,
            "best_margin_db": 12,
            "best_source_id": "R-1",
            "sources": [
                {
                    "source_id": "R-1",
                    "two_way_margin_db": 12,
                    "valid_two_way": True,
                }
            ],
        },
        {
            "index": 2,
            "x": 500025,
            "y": 6650025,
            "longitude": 10,
            "latitude": 60,
            "state": "unknown_terrain",
            "source_count": 0,
            "unknown_sources": 1,
            "best_margin_db": None,
            "best_source_id": None,
            "sources": [],
        },
    ]


def metadata(job: dict) -> dict:
    return coverage_export_metadata(
        job,
        summarize_coverage_cells(cells(), job),
        job["terrain_fingerprint"],
        stale=False,
    )


def target_report() -> dict:
    return {
        "report_id": "b" * 32,
        "targets": [
            {"id": "T-1", "name": "<script>alert(1)</script>", "state": "pass", "message": "ok"}
        ],
        "target_features": [
            {
                "type": "Feature",
                "id": "T-1",
                "geometry": {"type": "Point", "coordinates": [10.0, 60.0]},
                "properties": {"name": "<script>alert(1)</script>", "minimum_margin_db": 5},
            }
        ],
    }


def test_partial_grid_exports_unknown_and_uncomputed_states_explicitly() -> None:
    job = export_job()
    rows = list(iter_complete_grid(cells(), job))
    assert [item["state"] for item in rows] == [
        "covered",
        "not_evaluated",
        "unknown_terrain",
        "not_evaluated",
    ]
    counts = summarize_coverage_cells(cells(), job)
    assert counts["requested_cells"] == 4
    assert counts["stored_cells"] == 2
    assert counts["not_evaluated_cells"] == 2
    assert counts["unknown_cells"] == 1


def test_unresolved_profile_cells_are_counted_and_exported_as_partial() -> None:
    job = export_job()
    rows = cells()
    rows[1]["state"] = "unresolved"
    rows[1]["unknown_sources"] = 0
    rows[1]["unresolved_sources"] = 1
    rows[1]["sources"] = [
        {
            "source_id": "R-1",
            "valid_two_way": False,
            "rejection": "profile_sample_limit",
            "rejection_detail": "raise the cap",
        }
    ]

    summary = summarize_coverage_cells(rows, job)
    export_metadata = coverage_export_metadata(
        {**job, "state": "complete"}, summary, job["terrain_fingerprint"], stale=False
    )
    feature_collection = json.loads(
        "".join(geojson_chunks(rows, job, export_metadata))
    )
    document = "".join(printable_report_chunks(rows, job, export_metadata))

    assert summary["unresolved_cells"] == 1
    assert summary["unresolved_source_evaluations"] == 1
    assert export_metadata["partial"] is True
    assert export_metadata["complete"] is False
    assert feature_collection["features"][2]["properties"]["state"] == "unresolved"
    assert feature_collection["features"][2]["properties"]["unresolved_sources"] == 1
    assert "Unresolved profile limit" in document
    assert "unresolved" in document


def test_drawn_area_exports_mark_outside_cells_and_include_the_area_outline() -> None:
    job = export_job()
    reverse = Transformer.from_crs(25833, 4326, always_xy=True)
    xy_ring = [
        (500000, 6650000),
        (500050, 6650000),
        (500050, 6650100),
        (500000, 6650100),
        (500000, 6650000),
    ]
    job["settings"]["area_mode"] = "drawn"
    job["settings"]["area_polygon_wgs84"] = [
        list(reverse.transform(x, y)) for x, y in xy_ring
    ]

    rows = list(iter_complete_grid([], job))

    assert [row["state"] for row in rows] == [
        "not_evaluated",
        "outside_area",
        "not_evaluated",
        "outside_area",
    ]
    counts = summarize_coverage_cells([], job)
    assert counts["stored_cells"] == 0
    assert counts["outside_area_cells"] == 2
    exported = json.loads(
        "".join(geojson_chunks([], job, coverage_export_metadata(
            job, counts, job["terrain_fingerprint"], stale=False
        )))
    )
    assert exported["features"][1]["geometry"] is None
    assert exported["features"][1]["properties"]["state"] == "outside_area"
    assert exported["features"][-1]["properties"]["feature_kind"] == "analysis_area"


def test_geojson_and_json_exports_keep_radio_settings_targets_and_explicit_states() -> None:
    job, report = export_job(), target_report()
    metadata_value = metadata(job)
    geojson_text = "".join(geojson_chunks(cells(), job, metadata_value, report))
    try:
        geojson = json.loads(geojson_text)
    except json.JSONDecodeError as exc:
        raise AssertionError(
            f"position {exc.pos} of {len(geojson_text)}: {geojson_text[:100]!r} ... "
            f"{geojson_text[exc.pos - 60 : exc.pos + 60]!r}"
        ) from exc
    assert geojson["metadata"]["coordinate_reference_system"] == "OGC:CRS84"
    assert len(geojson["features"]) == 5
    assert [feature["properties"]["state"] for feature in geojson["features"][:4]] == [
        "covered",
        "not_evaluated",
        "unknown_terrain",
        "not_evaluated",
    ]
    target = geojson["features"][-1]
    assert target["id"] == "target-T-1"
    assert target["properties"]["target_id"] == "T-1"
    assert target["properties"]["coverage_state"] == "pass"
    assert target["geometry"]["type"] == "Point"

    exported = json.loads("".join(json_export_chunks(cells(), job, metadata_value, report)))
    assert exported["metadata"]["radio_settings"] == job["radio_settings"]
    assert len(exported["cells"]) == 4
    assert exported["cells"][1]["state"] == "not_evaluated"
    assert exported["target_report"]["report_id"] == report["report_id"]


def test_printable_report_is_offline_and_escapes_target_names() -> None:
    job, report = export_job(), target_report()
    scenario = {"baseline_job_id": "a" * 32, "scenario_job_id": "c" * 32, "counts": {"gained_local": 1}}
    document = "".join(
        printable_report_chunks(cells(), job, metadata(job), report, scenario)
    )
    assert "<svg" in document
    assert "R-1 → R-2" in document
    assert "4.5 dB" in document
    assert "gained_local" in document
    assert "&lt;script&gt;alert(1)&lt;/script&gt;" in document
    assert "<script>alert(1)</script>" not in document
    assert "https://" not in document
    assert "not_evaluated" in document
