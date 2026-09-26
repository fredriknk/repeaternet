import math

import numpy as np
import pytest

from rf_router_planner.rf.antennas import ElevationPattern, elevation_angle_deg
from rf_router_planner.rf.curvature import earth_bulge_m
from rf_router_planner.rf.diffraction import knife_edge_loss_db, knife_edge_v
from rf_router_planner.rf.fresnel import first_fresnel_radius_m
from rf_router_planner.rf.link import free_space_path_loss_db, received_power_dbm


def test_fspl_known_value() -> None:
    assert free_space_path_loss_db(100_000, 100) == pytest.approx(112.44, abs=0.01)


def test_fresnel_midpoint() -> None:
    assert first_fresnel_radius_m(5_000, 5_000, 1_000) == pytest.approx(27.38, rel=0.002)


def test_earth_bulge_midpoint() -> None:
    assert earth_bulge_m(50_000, 100_000, 1.0) == pytest.approx(196.20, rel=0.001)
    assert earth_bulge_m(0, 100_000, 4 / 3) == 0


def test_link_budget() -> None:
    power = received_power_dbm(22, 2, 1, 120, 3, 1, 2)
    assert power == -97


def test_elevation_angle_and_pattern_interpolation() -> None:
    assert elevation_angle_deg(100, 0, 100) == pytest.approx(45)
    pattern = ElevationPattern(np.array([-10.0, 0.0, 10.0]), np.array([-4.0, 6.0, 2.0]))
    assert pattern.gain_at(5) == pytest.approx(4)


def test_knife_edge_reference_points() -> None:
    assert knife_edge_loss_db(-1) == 0
    assert knife_edge_loss_db(0) == pytest.approx(6.03, abs=0.05)
    assert math.isfinite(knife_edge_v(5, 1_000, 1_000, 869.5))


def test_optimistic_screening_uses_peak_pattern_gain(tmp_path):
    from rf_router_planner.models.settings import RFSettings
    from rf_router_planner.models.site import Site, SiteKind
    from rf_router_planner.rf.propagation import LinkEvaluator
    from rf_router_planner.terrain.raster import ArrayTerrain

    pattern = tmp_path / "pattern.csv"
    pattern.write_text("elevation_deg,gain_dbi\n-90,0\n0,20\n90,0\n")
    settings = RFSettings(receiver_sensitivity_dbm=-65, fade_margin_db=0)
    settings.endpoint_a.pattern_csv = settings.endpoint_b.pattern_csv = str(pattern)
    terrain = ArrayTerrain(np.zeros((3, 101)), resolution_m=10)
    evaluator = LinkEvaluator(terrain, settings)
    a = Site("A", 0, 10, kind=SiteKind.ENDPOINT_A, antenna_height_m=20)
    b = Site("B", 1000, 10, kind=SiteKind.ENDPOINT_B, antenna_height_m=20)
    final = evaluator.evaluate(a, b)
    assert final.valid
    assert evaluator.optimistic_margin_db(a, b) >= final.worst_margin_db
