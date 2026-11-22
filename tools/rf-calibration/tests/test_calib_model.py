"""Synthetic recovery tests for the censored mixed model (no real data)."""

import sys
import unittest
from pathlib import Path

import numpy as np

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import calib_model as cm  # noqa: E402


def simulate(seed=1, n_nodes=60, n_links=700, c_true=24.0, sigma=5.0, su=3.0, sw=2.0, floor=-125.0):
    rng = np.random.default_rng(seed)
    tx = rng.integers(0, n_nodes, n_links)
    rx = (tx + 1 + rng.integers(0, n_nodes - 1, n_links)) % n_nodes
    u = rng.normal(0, su, n_nodes)
    w = rng.normal(0, sw, n_nodes)
    pl = rng.uniform(100, 160, n_links)
    y_true = c_true + u[tx] + w[rx] - pl + rng.normal(0, sigma, n_links)
    cens = np.where(y_true < floor, -1, 0)
    y = np.where(cens == 0, y_true, np.nan)
    bound = np.full(n_links, floor)
    d = cm.LinkData(y=np.nan_to_num(y), cens=cens, bound=bound, block=np.zeros(n_links, int),
                    tx=tx, rx=rx, pl=pl, n_blocks=1, n_nodes=n_nodes)
    return d, u, w


class CensoredMixedModel(unittest.TestCase):
    def test_recovers_constant_and_variances_under_censoring(self):
        d, u, w = simulate()
        self.assertGreater((d.cens == -1).mean(), 0.15)
        pri = cm.Priors(c_mean=np.array([24.3]), c_sd=np.array([np.inf]))
        f = cm.fit(d, pri)
        self.assertTrue(f.converged)
        self.assertAlmostEqual(f.c[0], 24.0, delta=1.0)
        self.assertAlmostEqual(f.sigma, 5.0, delta=0.6)
        self.assertAlmostEqual(f.sigma_u, 3.0, delta=1.0)
        self.assertGreater(np.corrcoef(f.u, u)[0, 1], 0.7)

    def test_ignoring_censoring_biases_constant_up(self):
        d, _, _ = simulate()
        keep = d.cens == 0
        naive = cm.fit(d.subset(keep), cm.Priors(c_mean=np.array([0.0]), c_sd=np.array([np.inf])))
        full = cm.fit(d, cm.Priors(c_mean=np.array([0.0]), c_sd=np.array([np.inf])))
        self.assertGreater(naive.c[0], full.c[0] + 0.3)

    def test_interp_pl_is_exact_on_grid_and_monotone_between(self):
        grid = np.zeros((1, 4, 4))
        for a, ha in enumerate(cm.GRID_HEIGHTS):
            for b, hb in enumerate(cm.GRID_HEIGHTS):
                grid[0, a, b] = 150 - 10 * np.log10(ha) - 5 * np.log10(hb)
        self.assertAlmostEqual(cm.interp_pl(grid, np.array([5.0]), np.array([10.0]))[0], grid[0, 1, 2])
        mid = cm.interp_pl(grid, np.array([7.0]), np.array([2.0]))[0]
        self.assertTrue(grid[0, 2, 0] < mid < grid[0, 1, 0])
        self.assertAlmostEqual(cm.interp_pl(grid, np.array([50.0]), np.array([1.0]))[0], grid[0, 3, 0])

    def test_height_choice_recovers_tall_node(self):
        rng = np.random.default_rng(3)
        n_nodes, n = 20, 400
        tx = rng.integers(0, n_nodes, n)
        rx = (tx + 1 + rng.integers(0, n_nodes - 1, n)) % n_nodes
        base = rng.uniform(110, 150, n)
        # Strong height dependence that is not a constant offset: it scales with base loss.
        grid = np.empty((n, 4, 4))
        for a, ha in enumerate(cm.GRID_HEIGHTS):
            for b, hb in enumerate(cm.GRID_HEIGHTS):
                grid[:, a, b] = base - (base - 100) * 0.25 * (np.log(ha / 2) + np.log(hb / 2))
        h_true = np.full(n_nodes, 2.0)
        h_true[0] = 20.0
        pl = cm.interp_pl(grid, h_true[tx], h_true[rx])
        y = 24.0 - pl + rng.normal(0, 2.0, n)
        d = cm.LinkData(y=y, cens=np.zeros(n, int), bound=np.zeros(n), block=np.zeros(n, int),
                        tx=tx, rx=rx, pl=pl, n_blocks=1, n_nodes=n_nodes)
        pri = cm.Priors(c_mean=np.array([24.0]), c_sd=np.array([1.0]), sigma_u0=1.0, sigma_w0=1.0)
        f, h = cm.fit_with_heights(d, grid, np.full(n_nodes, 3.0), pri)
        self.assertGreaterEqual(h[0], 14.0)
        self.assertTrue(np.all(h[1:] <= 3.0))

    def test_censoring_helpers(self):
        self.assertAlmostEqual(cm.censoring_corrected_mean(-110, 2.0, 1.0), -110)
        self.assertLess(cm.censoring_corrected_mean(-110, 2.0, 0.2), -112)
        self.assertLess(cm.non_detection_bound(-120, 2.0, 20), -123)
        self.assertEqual(cm.detection_floor_snr(8), -10.0)
        self.assertAlmostEqual(cm.auc(np.array([2.0, 3.0]), np.array([0.0, 1.0])), 1.0)
        self.assertAlmostEqual(cm.auc(np.array([1.0]), np.array([1.0])), 0.5)


if __name__ == "__main__":
    unittest.main()
