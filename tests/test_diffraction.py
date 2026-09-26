import numpy as np
import pytest

from rf_router_planner.rf.diffraction import bullington_loss_db, knife_edge_loss_db, knife_edge_v


def test_single_obstacle_matches_independent_geometry():
    d = np.array([0, 400, 1000])
    result = bullington_loss_db(d, np.array([0, 30, 0]), np.array([10, 10, 10]), 900)
    v = knife_edge_v(20, 400, 600, 900)
    single = knife_edge_loss_db(v)
    assert result.loss_db == pytest.approx(single + (1 - np.exp(-single / 6)) * 10.02)
    assert result.obstacles[0]["distance_m"] == pytest.approx(400)


def test_smooth_los_profile_does_not_accumulate_artificial_edges():
    losses = []
    for samples in [15, 71, 701]:
        d = np.linspace(0, 700, samples)
        result = bullington_loss_db(d, np.zeros(samples), np.full(samples, 3), 869.5)
        losses.append(result.loss_db)
        assert len(result.obstacles) == 1
    assert max(losses) - min(losses) < 1e-10
    assert 0 < losses[0] < 5


def test_clear_profile_and_reversal():
    d = np.array([0, 100, 350, 700, 1000])
    ground = np.array([0, 20, 50, 10, 0])
    los = np.linspace(10, 15, len(d))
    forward = bullington_loss_db(d, ground, los, 869.5)
    backward = bullington_loss_db(d[-1] - d[::-1], ground[::-1], los[::-1], 869.5)
    assert forward.loss_db == pytest.approx(backward.loss_db)
    assert bullington_loss_db(d, np.zeros(5), np.full(5, 100), 869.5).loss_db == 0


@pytest.mark.parametrize("distances,terrain,frequency", [
    ([0, 0, 1], [0, 0, 0], 900),
    ([0, 1, 2], [0, float('nan'), 0], 900),
    ([0, 1, 2], [0, 0], 900),
    ([0, 1, 2], [0, 0, 0], 0),
])
def test_invalid_profiles_rejected(distances, terrain, frequency):
    with pytest.raises(ValueError):
        bullington_loss_db(np.array(distances), np.array(terrain), np.ones(3), frequency)
