"""Regenerate test oracles from the Python implementation at the repository root."""
import json
import tempfile
from datetime import UTC, datetime
from pathlib import Path
from runpy import run_path

from rf_router_planner.export import export_route_csv, export_route_geojson, export_route_html
from rf_router_planner.web import encode, result_payload

destination = Path(__file__).parent / "fixtures"
c = run_path("tests/test_coverage_exports.py")
job, cells, targets = c["export_job"](), c["cells"](), c["target_report"]()
metadata = c["coverage_export_metadata"](
    job, c["summarize_coverage_cells"](cells, job), job["terrain_fingerprint"],
    stale=True,
)
metadata.update(target_report_id=targets["report_id"], target_count=len(targets["targets"]))
formats = {}
for name, renderer in (("json", "json_export_chunks"), ("geojson", "geojson_chunks")):
    formats[name] = json.loads("".join(c[renderer](iter(cells), job, metadata, targets)))
(destination / "coverage.json").write_text(
    json.dumps(dict(job=job, cells=cells, targets=targets, **formats), indent=2) + "\n", encoding="utf-8"
)
r = run_path("tests/test_route_report.py")
result, rf, candidates = r["synthetic_route"]()
for link in result.links:
    link.profile = None
route = result_payload(result, "route-job", 1, 2, encode(rf))
route["alternatives"][1]["diagnostics"] = result.active_solution.diagnostics
route["rf_settings"]["effective_sensitivity_dbm"] = rf.effective_sensitivity_dbm
stamp = datetime(2026, 1, 1, tzinfo=UTC)
with tempfile.TemporaryDirectory() as directory:
    csv_path, geojson_path = Path(directory) / "route.csv", Path(directory) / "route.geojson"
    export_route_csv(result, csv_path)
    export_route_geojson(result, geojson_path)
    report = export_route_html(result, rf, candidates, generated_at=stamp)
    map_json = report.split('id="route-map-data" type="application/json">')[1].split("</script>")[0]
    oracle = dict(route=route, candidates=encode(candidates), stamp=stamp.isoformat(),
                  csv=csv_path.read_bytes().decode(), geojson=json.loads(geojson_path.read_text(encoding="utf-8")),
                  map=json.loads(map_json))
(destination / "route.json").write_text(json.dumps(oracle, indent=2) + "\n", encoding="utf-8")
