"""Synthetic SondeHub calibration checks."""

import json
import math
from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import sondehub_check as check


def sample(receiver, flight, elevation, offset):
    frequency, power, distance = 403e6, 60.0, 100_000.0
    return {
        "receiver": receiver, "flight": flight, "elevation_k1_deg": elevation,
        "terrain_k1_deg": 0.0, "tx_agl_m": 2000.0, "tx_height_basis": "reported_datum",
        "quality_flags": 0, "frequency_hz": frequency, "tx_power_mw": power,
        "slant_m": distance,
        "rssi": {"mean": float(offset - check.fspl_db(distance, frequency)
                               + 10 * math.log10(power))},
        "snr": None,
    }


class SondeHubCheck(unittest.TestCase):
    def test_fit_weights_pairs_equally_and_recovers_within_pair_pattern(self):
        rows = []
        for receiver, baseline in (("a", 10.0), ("b", 20.0)):
            for flight, bias, copies in (("one", -2.0, 1), ("two", 2.0, 20)):
                rows.extend([sample(receiver, flight, 10.0, baseline + bias)] * copies)
                rows.extend([sample(receiver, flight, 40.0, baseline + bias - 6)] * copies)
        fit, offsets = check.fit_signal(rows, "rssi")
        self.assertAlmostEqual(offsets["a"], 7.0)
        self.assertAlmostEqual(offsets["b"], 17.0)
        self.assertEqual(fit["receiver_flights"], 4)
        self.assertEqual(fit["by_receiver"]["a"]["flights"], 2)
        self.assertEqual(fit["by_flight"]["two"]["receivers"], 2)
        self.assertAlmostEqual(fit["receiver_offset_sigma_db"], 5.0)
        self.assertAlmostEqual(fit["receiver_flight_residual_sigma_db"], 2.0)
        self.assertAlmostEqual(fit["elevation"][3]["relative_mean_db"], -6.0)
        self.assertIsNone(fit["elevation"][0]["relative_mean_db"])
        self.assertEqual(check.fit_signal(rows, "snr")[0]["bins"], 0)

    def test_clear_requires_agl_and_local_horizon_and_rssi_floor_is_excluded(self):
        row = sample("a", "one", 10.0, 0.0)
        self.assertTrue(check.clear(row))
        self.assertFalse(check.clear({**row, "tx_agl_m": None}))
        self.assertFalse(check.clear({**row, "terrain_k1_deg": 10.0}))
        self.assertIsNone(check.signal_offset({**row, "quality_flags": 128}, "rssi"))

    def test_last_heard_is_one_per_pair_and_requires_a_later_witness(self):
        rows = []
        for rx, flight in (("a", "one"), ("a", "two"), ("b", "one"), ("b", "two")):
            row = {
                "receiver": rx, "flight": flight, "tx_msl_m": 2000,
                "antenna_msl_m": 20, "quality_flags": 0,
                "elevation_k1_deg": -0.2, "terrain_k1_deg": -0.6,
                "flight_witness_after_ms": 300_000,
                "distance_m": check.smooth_horizon_m(1.25, 20, 2000),
                "observed_at_ms": 500_000, "link_id": 2,
            }
            rows.extend([{**row, "observed_at_ms": 10_000, "tx_msl_m": 2400, "link_id": 1}, row])
        rows[-1]["flight_witness_after_ms"] = None
        result = check.horizon_fit(rows)
        self.assertEqual(result["candidate_receiver_flights"], 4)
        self.assertEqual(result["receiver_flights"], 3)
        self.assertAlmostEqual(result["apparent_k"], 1.25)
        self.assertFalse(result["constrains_physical_k"])
        for limit in result["cluster_interval_95"]:
            self.assertAlmostEqual(limit, 1.25)

    def test_shadow_compares_relative_loss_at_both_k_values(self):
        rows = [{**sample("a", "f", 1.0, 1.0), "link_id": 123}]
        profiles = {123: {"bullington_k1_db": 12.0, "bullington_k133_db": 10.0}}
        result = check.shadow_check(rows, profiles, {"rssi": {"a": 11.0}}, "rssi")
        self.assertEqual(result["receiver_flights"], 1)
        self.assertAlmostEqual(result["median_measured_extra_db"], 10.0)
        self.assertAlmostEqual(result["k1"]["median_absolute_error_db"], 2.0)
        self.assertAlmostEqual(result["k4_3"]["median_absolute_error_db"], 0.0)

    def test_report_and_atomic_replacement(self):
        result = check.analyze([], [], [], [])
        report = check.report(result)
        self.assertIn("# SondeHub air-to-ground check", report)
        with tempfile.TemporaryDirectory() as root:
            path = Path(root) / "results.json"
            check.atomic_text(path, "{}")
            check.atomic_text(path, json.dumps(result, allow_nan=False))
            self.assertEqual(json.loads(path.read_text(encoding="utf-8")), result)
            self.assertEqual(list(Path(root).iterdir()), [path])


if __name__ == "__main__":
    unittest.main()
