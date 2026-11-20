"""Regenerate coverage-python-v3.json with the Python migration baseline.

Run from the repository root with the development Python environment.
"""
import json
from pathlib import Path

import numpy as np

from rf_router_planner.coverage.engine import calculate_coverage
from rf_router_planner.models.coverage import ClientRadioProfile, CoverageSettings
from rf_router_planner.models.settings import CandidateSettings, RFSettings, ValidationMode
from rf_router_planner.models.site import Site, SiteKind
from rf_router_planner.web import encode
from rf_router_planner.terrain.raster import ArrayTerrain


def fixture(terrain_kind, settings, strict=False, source_x=400.0, target_x=1500.0):
    values = np.zeros((21, 21))
    values[:, 10] = {"flat": 0.0, "ridge": 200.0, "valley": -100.0, "nodata": np.nan}[
        terrain_kind
    ]
    terrain = ArrayTerrain(values, resolution_m=100.0, crs="EPSG:25833")
    source = Site("router", source_x, 1000.0, kind=SiteKind.ROUTER, antenna_height_m=100.0)
    rf = RFSettings()
    if strict:
        rf.validation_mode = ValidationMode.STRICT_LOS
    return encode(
        calculate_coverage(
            terrain,
            rf,
            CandidateSettings(),
            [source],
            settings,
            requested_bounds=(target_x - 250, 750, target_x + 250, 1250),
        )
    )


default = CoverageSettings(cell_size_m=500.0, maximum_cells=1, profile_step_m=100.0)
cases = {
    "defaults": encode(CoverageSettings()),
    **{kind: fixture(kind, default, True) for kind in ("flat", "ridge", "valley")},
    "path_nodata": fixture("nodata", default),
    "ground_nodata": fixture("nodata", default, target_x=1100.0),
    "capped": fixture(
        "flat", CoverageSettings(cell_size_m=500.0, maximum_cells=1,
                                 profile_step_m=100.0, maximum_profile_samples=8)
    ),
    "asymmetric": fixture(
        "flat", CoverageSettings(cell_size_m=500.0, maximum_cells=1, profile_step_m=100.0,
                                 client=ClientRadioProfile(height_agl_m=1.2, tx_power_dbm=14.0,
                                                           gain_dbi=0.0, feed_loss_db=1.0,
                                                           sensitivity_dbm=-75.0,
                                                           miscellaneous_loss_db=2.0))
    ),
}
Path(__file__).with_name("coverage-python-v3.json").write_text(
    json.dumps(cases, indent=2, ensure_ascii=False) + "\n", encoding="utf-8"
)
