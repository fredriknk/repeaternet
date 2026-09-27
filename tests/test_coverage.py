from __future__ import annotations

import threading

import numpy as np
import pytest

from rf_router_planner.coverage import engine
from rf_router_planner.models.coverage import (
    ClientRadioProfile,
    CoverageSettings,
    CoverageState,
)
from rf_router_planner.models.link import DirectionResult, LinkResult
from rf_router_planner.models.settings import CandidateSettings, RFSettings, ValidationMode
from rf_router_planner.models.site import Site, SiteKind
from rf_router_planner.rf.propagation import LinkEvaluator
from rf_router_planner.terrain.raster import ArrayTerrain


@pytest.fixture
def terrain_cases() -> dict[str, ArrayTerrain]:
    flat = np.zeros((21, 21), dtype=float)
    ridge = flat.copy()
    ridge[:, 10] = 200.0
    valley = flat.copy()
    valley[:, 10] = -100.0
    nodata = flat.copy()
    nodata[:, 10] = np.nan
    return {
        name: ArrayTerrain(values, resolution_m=100.0)
        for name, values in {
            "flat": flat,
            "ridge": ridge,
            "valley": valley,
            "nodata": nodata,
        }.items()
    }


def test_client_profile_produces_an_asymmetric_radio_budget() -> None:
    profile = ClientRadioProfile(
        height_agl_m=1.2,
        tx_power_dbm=14.0,
        gain_dbi=0.0,
        feed_loss_db=1.0,
        sensitivity_dbm=-122.0,
        miscellaneous_loss_db=2.0,
    )

    budget = profile.as_radio_budget()

    assert budget.tx_power_dbm == 14.0
    assert budget.sensitivity_dbm == -122.0
    assert budget.miscellaneous_loss_db == 2.0
    assert budget.antenna.height_agl_m == 1.2
    assert budget.antenna.gain_dbi == 0.0
    assert budget.antenna.feed_loss_db == 1.0


@pytest.mark.parametrize(
    ("terrain_name", "los_expected"),
    [("flat", True), ("ridge", False), ("valley", True)],
)
def test_reference_flat_ridge_and_valley_profiles(
    terrain_cases: dict[str, ArrayTerrain], terrain_name: str, los_expected: bool
) -> None:
    rf = RFSettings(validation_mode=ValidationMode.STRICT_LOS)
    source = Site("router", 200.0, 1_000.0, kind=SiteKind.ROUTER, antenna_height_m=100.0)
    client = Site("client", 1_800.0, 1_000.0, kind=SiteKind.CLIENT, antenna_height_m=1.5)

    result = LinkEvaluator(terrain_cases[terrain_name], rf).evaluate(
        source, client, sample_step_m=100.0
    )

    assert result.los_clear is los_expected
    if terrain_name == "ridge":
        assert result.fresnel_clear is False
        assert result.valid is False
    else:
        assert result.valid is True


def _sample_direction(
    source_id: str, target_id: str, margin_db: float, tx_power_dbm: float
) -> DirectionResult:
    return DirectionResult(
        source_id,
        target_id,
        500.0,
        0.0,
        0.0,
        0.0,
        0.0,
        0.0,
        0.0,
        margin_db,
        -130.0,
        margin_db,
        margin_db,
        0.0,
        0.0,
        margin_db >= 0,
        tx_power_dbm,
    )


def test_two_way_coverage_never_combines_different_best_sources(monkeypatch) -> None:
    class ContradictoryDirectionEvaluator:
        def __init__(self, _terrain, _settings) -> None:
            pass

        def evaluate(self, source, target, *_args, **_kwargs) -> LinkResult:
            margins = {
                "downlink-favourite": (5.0, -2.0),
                "uplink-favourite": (-1.0, 6.0),
            }[source.id]
            return LinkResult(
                source.id,
                target.id,
                500.0,
                _sample_direction(source.id, target.id, margins[0], 22.0),
                _sample_direction(target.id, source.id, margins[1], 20.0),
                False,
                True,
                True,
                10.0,
                1.0,
                250.0,
                10.0,
                0.0,
            )

    monkeypatch.setattr(engine, "LinkEvaluator", ContradictoryDirectionEvaluator)
    terrain = ArrayTerrain(np.zeros((11, 11)), resolution_m=100.0)
    sources = [
        Site("downlink-favourite", 200.0, 500.0, kind=SiteKind.ROUTER),
        Site("uplink-favourite", 800.0, 500.0, kind=SiteKind.ROUTER),
    ]
    settings = CoverageSettings(cell_size_m=500.0, maximum_cells=1, area_buffer_m=0.0)

    result = engine.calculate_coverage(
        terrain,
        RFSettings(),
        CandidateSettings(),
        sources,
        settings,
        requested_bounds=(400.0, 400.0, 600.0, 600.0),
    )

    assert result.cells[0].state is CoverageState.UNCOVERED
    assert result.cells[0].source_count == 0
    assert result.cells[0].sources[0].downlink_margin_db == 5.0
    assert result.cells[0].sources[1].uplink_margin_db == 6.0
    assert all(not source.valid_two_way for source in result.cells[0].sources)


def test_coverage_marks_path_nodata_as_unknown_not_uncovered(
    terrain_cases: dict[str, ArrayTerrain],
) -> None:
    terrain = terrain_cases["nodata"]
    source = Site("router", 400.0, 1_000.0, kind=SiteKind.ROUTER, antenna_height_m=100.0)
    settings = CoverageSettings(
        cell_size_m=500.0,
        maximum_cells=1,
        area_buffer_m=0.0,
        profile_step_m=100.0,
    )

    result = engine.calculate_coverage(
        terrain,
        RFSettings(),
        CandidateSettings(),
        [source],
        settings,
        requested_bounds=(1_400.0, 900.0, 1_600.0, 1_100.0),
    )

    assert result.cells[0].state is CoverageState.UNKNOWN_TERRAIN
    assert result.cells[0].unknown_sources == 1
    assert result.evaluated_cells == 0
    assert result.unknown_cells == 1


def test_coverage_counts_cells_without_ground_data_as_unknown(
    terrain_cases: dict[str, ArrayTerrain],
) -> None:
    source = Site("router", 400.0, 1_000.0, kind=SiteKind.ROUTER, antenna_height_m=100.0)
    settings = CoverageSettings(cell_size_m=500.0, maximum_cells=1, area_buffer_m=0.0)

    result = engine.calculate_coverage(
        terrain_cases["nodata"],
        RFSettings(),
        CandidateSettings(),
        [source],
        settings,
        requested_bounds=(900.0, 900.0, 1_100.0, 1_100.0),
    )

    assert result.cells[0].state is CoverageState.UNKNOWN_TERRAIN
    assert result.evaluated_cells == 0
    assert result.unknown_cells == 1


def test_cancellation_does_not_publish_a_partially_evaluated_cell(monkeypatch) -> None:
    started = threading.Event()

    class CancellingEvaluator:
        def __init__(self, _terrain, _settings) -> None:
            pass

        def evaluate(self, source, target, *_args, **_kwargs) -> LinkResult:
            started.set()
            direction = _sample_direction(source.id, target.id, 3.0, 22.0)
            reverse = _sample_direction(target.id, source.id, 3.0, 22.0)
            return LinkResult(
                source.id,
                target.id,
                500.0,
                direction,
                reverse,
                True,
                True,
                True,
                10.0,
                1.0,
                250.0,
                10.0,
                0.0,
            )

    monkeypatch.setattr(engine, "LinkEvaluator", CancellingEvaluator)
    terrain = ArrayTerrain(np.zeros((11, 11)), resolution_m=100.0)
    sources = [
        Site("router-a", 200.0, 500.0, kind=SiteKind.ROUTER),
        Site("router-b", 800.0, 500.0, kind=SiteKind.ROUTER),
    ]
    settings = CoverageSettings(cell_size_m=500.0, maximum_cells=1, area_buffer_m=0.0)

    result = engine.calculate_coverage(
        terrain,
        RFSettings(),
        CandidateSettings(),
        sources,
        settings,
        requested_bounds=(400.0, 400.0, 600.0, 600.0),
        cancelled=lambda: started.is_set(),
    )

    assert result.completed_cells == 0
    assert result.cells == []
