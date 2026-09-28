from __future__ import annotations

import pytest

from rf_router_planner.coverage.profiles import bounded_profile_step


def test_profile_sample_limit_accepts_exact_boundary_without_changing_step() -> None:
    step, samples, reason = bounded_profile_step(200.0, 100.0, 50.0, 3)

    assert step == 100.0
    assert samples == 3
    assert reason is None


def test_profile_sample_limit_reports_overflow_without_coarsening() -> None:
    step, samples, reason = bounded_profile_step(300.0, 100.0, 50.0, 3)

    assert step == 100.0
    assert samples == 4
    assert reason is not None
    assert "4 terrain-profile samples" in reason
    assert "configured maximum is 3" in reason


@pytest.mark.parametrize(
    ("distance", "requested_step", "terrain_resolution", "maximum"),
    [(-1, 10, 10, 8), (10, 0, 10, 8), (10, 10, float("inf"), 8), (10, 10, 10, 2)],
)
def test_profile_limit_rejects_invalid_parameters(
    distance, requested_step, terrain_resolution, maximum
) -> None:
    with pytest.raises(ValueError):
        bounded_profile_step(distance, requested_step, terrain_resolution, maximum)
