import base64
import json
import re
import struct
from datetime import UTC, datetime
from html.parser import HTMLParser

import numpy as np

from rf_router_planner.export import export_route_html
from rf_router_planner.export.coverage_report import (
    coverage_export_metadata,
    summarize_coverage_cells,
)
from rf_router_planner.export.route_report import MAX_PROFILE_POINTS
from rf_router_planner.export.route_report_coverage import (
    MAX_COVERAGE_IMAGE_SIDE,
    build_report_coverage,
)
from rf_router_planner.models.network import NetworkSolution
from rf_router_planner.models.settings import CandidateSettings, RFSettings
from rf_router_planner.models.site import Site, SiteKind, SiteOrigin
from rf_router_planner.optimization.optimizer import OptimizationResult
from rf_router_planner.rf.propagation import LinkEvaluator
from rf_router_planner.terrain.raster import ArrayTerrain
from rf_router_planner.terrain.roads import RoadOverlay

MALICIOUS = 'R-2</script><img src=x onerror="alert(1)">'


def synthetic_route(*, surface=True, repeater_id=MALICIOUS):
    terrain_x = np.arange(3601)
    elevations = 140 + 3 * np.sin(terrain_x / 180) + 2 * np.sin(terrain_x / 75)
    ground = np.tile(elevations, (3, 1))
    ground[:, 1653] = 180.0
    terrain = ArrayTerrain(ground, resolution_m=1, dom=ground + 2 if surface else None)
    sites = [
        Site("A", 0, 1, 60.0, 10.0, SiteKind.ENDPOINT_A, 140, antenna_height_m=6),
        Site(
            "R-1",
            1200,
            1,
            60.002,
            10.018,
            SiteKind.ROUTER,
            elevations[1200],
            antenna_height_m=6,
            origin=SiteOrigin.KNOWN,
        ),
        Site(
            repeater_id,
            2400,
            1,
            60.001,
            10.039,
            SiteKind.ROUTER,
            elevations[2400],
            antenna_height_m=6,
        ),
        Site("B", 3600, 1, 60.005, 10.060, SiteKind.ENDPOINT_B, elevations[-1], antenna_height_m=6),
    ]
    rf = RFSettings(tx_power_dbm=14)
    candidates = CandidateSettings(maximum_link_distance_m=3000)
    evaluator = LinkEvaluator(terrain, rf)
    a, left, right, b = sites
    first = evaluator.evaluate(a, left)
    weak = evaluator.evaluate(right, left)
    weak.forward.usable_margin_db = 1.0
    weak.reverse.usable_margin_db = 2.0
    last = evaluator.evaluate(right, b)
    assert weak.dominant_obstacles and not weak.los_clear
    route_ids = [site.id for site in sites]
    active = NetworkSolution(
        "Two repeaters",
        sites,
        [last, weak, first],
        [a.id, b.id],
        [left.id, right.id],
        {(a.id, b.id): [route_ids]},
        requested_path_count=2,
        achieved_path_count=1,
        diagnostics=["One independent path passed validation"],
    )
    other = NetworkSolution(
        "Fallback <north>",
        [a, left, b],
        [first, evaluator.evaluate(left, b)],
        [a.id, b.id],
        [left.id],
        {(a.id, b.id): [[a.id, left.id, b.id]]},
    )
    result = OptimizationResult(
        sites,
        active.links,
        [],
        [],
        ["Corridor search complete"],
        elapsed_seconds=1.3,
        alternatives=[other, active],
        active_solution_index=1,
        road_overlay=RoadOverlay(
            [((60.0004, 10.013), (60.0005, 10.043)), ((60.01, 10.0), (60.02, 10.0))],
            [[1], [0]],
            [{}, {left.id: 100.0, right.id: 162.0}],
        ),
    )
    return result, rf, candidates


def render_synthetic(
    *, surface=True, with_profiles=True, with_roads=True, coverage=None, repeater_id=MALICIOUS
):
    result, rf, candidates = synthetic_route(surface=surface, repeater_id=repeater_id)
    if not with_profiles:
        for link in result.links:
            link.profile = None
    if not with_roads:
        result.road_overlay = None
    return export_route_html(
        result,
        rf,
        candidates,
        generated_at=datetime(2026, 11, 19, 15, 0, tzinfo=UTC),
        terrain_resolution_m=1,
        coverage=coverage,
    )


class ReportParser(HTMLParser):
    void_tags = {"meta", "link", "input", "br", "hr", "img"}

    def __init__(self):
        super().__init__()
        self.open_tags = []
        self.profile_points = []
        self.series_points = []
        self.map_data = None

    def handle_starttag(self, tag, attrs):
        if tag not in self.void_tags:
            self.open_tags.append(tag)
        attrs = dict(attrs)
        if tag == "svg":
            self.profile_points.append(int(attrs["data-profile-points"]))
        if tag == "polyline":
            self.series_points.append(len(attrs["points"].split()))
        if attrs.get("id") == "route-map-data":
            self.map_data = ""

    def handle_endtag(self, tag):
        assert self.open_tags.pop() == tag

    def handle_startendtag(self, tag, attrs):
        self.handle_starttag(tag, attrs)
        self.handle_endtag(tag)

    def handle_data(self, data):
        if self.map_data == "" and self.open_tags[-1:] == ["script"]:
            self.map_data = json.loads(data)


def test_route_report_has_ordered_hops_and_bounded_profiles():
    report = render_synthetic()
    parser = ReportParser()
    parser.feed(report)
    parser.close()
    assert report.startswith("<!doctype html>") and not parser.open_tags
    assert len(parser.profile_points) == 3
    assert all(990 <= points <= MAX_PROFILE_POINTS for points in parser.profile_points)
    assert max(parser.series_points) <= MAX_PROFILE_POINTS
    assert "L.map('route-map'" in report and "tile.openstreetmap.org" in report
    assert "Ground + earth bulge" in report and "Surface + earth bulge" in report
    assert "Diffraction obstacle" in report and "Minimum Fresnel clearance" in report
    assert [site["id"] for site in parser.map_data["sites"]] == ["A", "R-1", MALICIOUS, "B"]
    assert [link["dashed"] for link in parser.map_data["links"]] == [False, True, False]
    assert parser.map_data["roads"] == [[[60.0004, 10.013], [60.0005, 10.043]]]
    assert parser.map_data["coverage"] is None
    assert "Nearest road · 162 m" in parser.map_data["sites"][2]["details"]
    assert 'href="#hop-1">1. A → R-1' in report
    assert '<tr class="weak-hop"><th scope="row"><a href="#hop-2">2. R-1 → R-2' in report
    assert 'href="#hop-3">3. R-2&lt;/script&gt;' in report
    assert "<td>2.00 / 1.00</td>" in report
    assert "Fallback &lt;north&gt;" in report
    assert "&lt;img src=x onerror=&quot;alert(1)&quot;&gt;" in report
    assert MALICIOUS not in report
    assert report.count("Directional link budget") == 3
    assert "No completed coverage run is available." in report
    assert '<section id="coverage">' not in report


def test_route_report_works_without_terrain_extras():
    no_surface = render_synthetic(surface=False, with_roads=False)
    assert "Surface + earth bulge" not in no_surface
    assert "Ground + earth bulge" in no_surface
    assert "Nearby roads" not in no_surface
    no_profiles = render_synthetic(surface=False, with_profiles=False, with_roads=False)
    assert no_profiles.count("Terrain profile unavailable for this hop.") == 3
    assert '<svg class="profile-chart"' not in no_profiles
    assert "Directional link budget" in no_profiles


def synthetic_coverage(side=16):
    job = {
        "job_id": "synthetic-coverage",
        "state": "complete",
        "rows": side,
        "columns": side,
        "requested_cells": side**2,
        "terrain_crs": "EPSG:25832",
        "grid_bounds_projected": [555100, 6650100, 560100, 6655100],
        "effective_cell_size_m": 5000 / side,
        "settings": {
            "mode": "two_way",
            "client": {
                "name": "Handheld <test>",
                "height_agl_m": 1.5,
                "tx_power_dbm": 20,
                "gain_dbi": 2.15,
                "sensitivity_dbm": -130,
            },
        },
    }
    rng = np.random.default_rng(8123)
    classes = rng.integers(0, 5, side**2)

    def cells():
        for index, category in enumerate(classes):
            if category < 3:
                yield {
                    "index": index,
                    "state": "covered",
                    "sources": [
                        {
                            "valid_two_way": True,
                            "two_way_margin_db": float(category * 6),
                        }
                    ],
                }
            else:
                yield {"index": index, "state": ["uncovered", "unknown_terrain"][category - 3]}

    counts = summarize_coverage_cells(cells(), job)
    metadata = coverage_export_metadata(job, counts, [], stale=False)
    return build_report_coverage(
        cells(),
        job,
        metadata,
        {
            "targets": [{"name": "Cabin <east>", "state": "pass", "best_two_way_margin_db": 3}],
        },
    )


def test_large_coverage_grid_is_bounded_and_retains_full_counts():
    side = 540
    coverage = synthetic_coverage(side)
    report = render_synthetic(coverage=coverage)
    assert len(report.encode()) < 450_000
    assert "L.imageOverlay(data.coverage.image" in report
    assert "L.control.layers" in report and '<section id="coverage">' in report
    assert "Handheld &lt;test&gt;" in report and "Cabin &lt;east&gt;" in report
    assert coverage.metadata["counts"]["requested_cells"] == side**2
    covered = coverage.metadata["counts"]["covered_cells"]
    assert f"<td>{covered:,}</td><td>{covered / side**2:.1%}</td>" in report
    png = base64.b64decode(coverage.image.split(",", 1)[1])
    assert png.startswith(b"\x89PNG\r\n\x1a\n")
    assert max(struct.unpack("!II", png[16:24])) == MAX_COVERAGE_IMAGE_SIDE
    assert len(png) < 140_000
    assert re.search(r'"coverage":\{"image":"data:image/png;base64,', report)
