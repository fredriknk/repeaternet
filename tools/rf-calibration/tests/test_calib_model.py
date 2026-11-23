"""Synthetic recovery tests for the censored mixed model (no real data)."""

import contextlib
import io
import json
import sys
import tempfile
import unittest
from pathlib import Path

import numpy as np

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import calib_model as cm  # noqa: E402
import calibrate  # noqa: E402


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
    def test_recovers_budget_and_spread_from_both_censoring_directions(self):
        rng = np.random.default_rng(8695)
        n = 3500
        pl = rng.uniform(130.0, 160.0, n)
        signal = 24.0 - pl + rng.normal(0.0, 4.0, n)
        cens = np.where(signal < -127.0, -1, 1)
        cens[rng.random(n) < 0.25] = 0
        zeros = np.zeros(n, dtype=int)
        d = cm.LinkData(
            y=np.where(cens == 0, signal, np.nan), cens=cens, bound=np.full(n, -127.0),
            block=zeros, tx=zeros, rx=zeros, pl=pl, n_nodes=1,
        )
        train = np.arange(n) % 4 != 0
        pri = cm.Priors(c_mean=np.array([0.0]), c_sd=np.array([np.inf]), node_effects=False)
        fitted = cm.fit(d.subset(train), pri)
        self.assertTrue(fitted.converged)
        self.assertAlmostEqual(fitted.c[0], 24.0, delta=0.35)
        self.assertAlmostEqual(fitted.sigma, 4.0, delta=0.35)
        held = d.subset(~train & (cens == 1))
        mean = fitted.predict(held)
        score = cm.loglik_terms(held, mean, fitted.sigma)
        self.assertTrue(np.isfinite(score).all())
        self.assertTrue(np.all(score > cm.loglik_terms(held, mean - 6.0, fitted.sigma)))

    def test_new_evidence_builds_lower_bounds_and_snr_blocks(self):
        def record(evidence, **fields):
            return dict(
                evidence=evidence, source="corescope:example.test", link_id=evidence,
                tx={"node": "x", "lat": 60, "lon": 10},
                rx={"node": "y", "lat": 60.1, "lon": 10},
                region="norway", distance_m=11000, n=2, radio={"sf": 10}, **fields,
            )

        links = [
            record(calibrate.RELAY),
            record("corescope_trace_hop", snr_db={"median": -2}),
            record("corescope_neighbor_report", snr_db={"median": -2}, receiver_noise_floor_dbm=-113),
        ]
        obs, drops = calibrate.build_observations(links, {})
        self.assertEqual(drops, {})
        by_type = {o["evidence"]: o for o in obs}
        self.assertEqual(by_type[calibrate.RELAY]["cens"], 1)
        self.assertEqual(by_type[calibrate.RELAY]["bound"], -132)
        self.assertEqual(by_type["corescope_trace_hop"]["y"], -2)
        self.assertEqual(by_type["corescope_neighbor_report"]["y"], 2)
        self.assertEqual(len({o["block"] for o in obs}), 3)
        links[0]["flags"] = ["implausible_distance"]
        _, drops = calibrate.build_observations(links, {})
        self.assertEqual(drops, {"implausible_distance": 1})

    def test_reports_score_new_evidence_and_select_relay_only(self):
        with tempfile.TemporaryDirectory() as folder:
            work = Path(folder)
            (work / "dataset").mkdir()
            (work / "predictions").mkdir()
            links, predictions = [], []
            evidence_types = ("corescope_last_hop", "corescope_non_detection", *calibrate.NEW_EVIDENCE)
            for i in range(35):
                for evidence in evidence_types:
                    link_id = f"{i}-{evidence}"
                    unheard = evidence == "corescope_non_detection"
                    loss = 116.0 + i % 9 + (30 if unheard else 0)
                    links.append(dict(
                        source="corescope:fixture.test", link_id=link_id, evidence=evidence, region="norway",
                        tx={"node": f"a{link_id}", "lat": 58 + i / 8, "lon": 10.0},
                        rx={"node": f"b{link_id}", "lat": 58 + i / 8, "lon": 10.03},
                        radio={"sf": 8}, n=0 if unheard else 5, distance_m=2000,
                        signal_dbm={"median": 24 - loss, "p90": 30 - loss} if evidence == "corescope_last_hop" else None,
                        snr_db={"median": 140 - loss} if evidence in calibrate.CORE_SNR_BLOCKS else None,
                        detection={"heard": 0, "opportunities": 50} if unheard else None,
                    ))
                    predictions.append(dict(
                        link_id=link_id, path_id=link_id, engine="fixture", surface="dtm", clutter="none",
                        step_m=10, k_factor=1, tx_height_m=2, rx_height_m=2,
                        total_db=loss, fspl_db=loss - 3, excess_db=3, v_max=0.0,
                    ))
            for name, records in [("dataset/links.jsonl", links), ("predictions/fixture.jsonl", predictions)]:
                (work / name).write_text("".join(json.dumps(x) + "\n" for x in records), encoding="utf-8")
            args = ["--work-dir", str(work), "--configs", "fixture"]
            with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                self.assertEqual(calibrate.main(args), 0)
                self.assertEqual(calibrate.main(args + ["--evidence", calibrate.RELAY, "--report-name", "relay"]), 0)
            default = json.loads((work / "report/results.json").read_text(encoding="utf-8"))
            self.assertIn("norway", default["regions"])
            self.assertEqual(set(default["evidence"]), set(calibrate.NEW_EVIDENCE))
            for evidence, result in default["evidence"].items():
                cfg = result["regions"]["norway"]["configs"]["fixture|dtm|none|10|1"]
                cv = next(iter(cfg["cv"].values()))
                self.assertEqual(cv["n_scored"], 35)
                if evidence == calibrate.RELAY:
                    self.assertEqual(cv["auc_heard_vs_unheard"], 1.0)
            selected = json.loads((work / "relay/results.json").read_text(encoding="utf-8"))
            self.assertEqual(selected["regions"], {})
            self.assertEqual(set(selected["evidence"]), {calibrate.RELAY})

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
