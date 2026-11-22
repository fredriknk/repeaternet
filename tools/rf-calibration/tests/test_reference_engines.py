import json
from math import isclose
import os
from pathlib import Path
import subprocess
import sys
from tempfile import TemporaryDirectory
import unittest
from unittest.mock import patch

import numpy as np

sys.path.insert(0, str(Path(__file__).resolve().parent))
from convert_review import convert, write_jsonl
import reference_engines as ref


REVIEW = ref.safe_path(os.environ.get("RF_REVIEW_DIR") or ref.DEFAULT_REVIEW)
DLL = REVIEW / "ntia-itm.dll"
PY1812_SOURCE = ref.py1812_source(REVIEW)
HAS_ITM = DLL.is_file()
HAS_PY1812 = PY1812_SOURCE.is_file() and all(path.is_file() for path in ref.map_sources(REVIEW).values())
HAS_ORACLES = all((REVIEW / name).is_file() for name in (
    "results.json", "drammenmesh_links.json", "drammenmesh_sites.json"))


def ground_fixture(edge=False):
    dtm = [100.0] * 501
    if edge:
        dtm[150] += 50
    return dict(schema=1, path_id="a~b", link_ids=["go", "back"], a=dict(node="a", ground_m=100),
                b=dict(node="b", ground_m=110), distance_m=5000, step_m=10, dtm=dtm, dom=None)


def link_fixture(link_id, tx, rx, freq=None):
    return dict(link_id=link_id, tx=dict(node=tx), rx=dict(node=rx), radio=dict(freq_mhz=freq))


class Contracts(unittest.TestCase):
    def test_direction_and_free_space(self):
        profile = ground_fixture(edge=True)
        profile["dom"] = [h + 4 for h in profile["dtm"]]
        ends, surfaces = ref.directed_profile(profile, "b", "a")
        self.assertEqual([end["node"] for end in ends], ["b", "a"])
        self.assertEqual(int(surfaces["dtm"].argmax()), 350)
        self.assertEqual(surfaces["dom"][[0, -1]].tolist(), [110, 100])
        self.assertEqual(surfaces["dom"][350], 154)
        self.assertAlmostEqual(ref.fspl_db(100, 3000, 4000), 86.42940008672)

    def test_sea_proxy_at_endpoints(self):
        dtm = np.r_[np.zeros(15), [2.0, 120], np.zeros(15)]
        d = np.arange(len(dtm)) / 100
        zones = ref.sea_proxy(dtm)
        self.assertEqual(zones.tolist(), [1] * 15 + [3, 4] + [1] * 15)
        self.assertEqual(ref.coast_distances(d, zones), {"dct": 0, "dcr": 0})

    def test_live_paths_are_rejected(self):
        with TemporaryDirectory() as work, patch.dict(os.environ, LOCALAPPDATA=work):
            with self.assertRaisesRegex(ValueError, "live RepeaterNet data"):
                ref.safe_path(Path(work) / "RepeaterNet" / "data" / "nested")

    @unittest.skipUnless(HAS_ITM, "Review ITM DLL is unavailable")
    def test_itm_cli_grid_cache_and_frequency(self):
        with TemporaryDirectory() as folder:
            work = Path(folder)
            profile = ground_fixture(edge=True)
            write_jsonl(work / "profiles.jsonl", [profile])
            links = [link_fixture("go", "a", "b"), link_fixture("back", "b", "a", 434.5)]
            write_jsonl(work / "custom.jsonl", links)
            cmd = [sys.executable, "-I", str(Path(ref.__file__)), "--work-dir", folder,
                   "--review-dir", str(REVIEW), "--links", str(work / "custom.jsonl"), "--engines", "itm_p2p"]

            def execute():
                result = subprocess.run(cmd, capture_output=True, text=True)
                self.assertEqual(result.returncode, 0, result.stderr)
                return result.stdout

            self.assertIn("2 links computed, 0 reused", execute())
            path = work / "predictions" / "itm_p2p.jsonl"
            rows = [json.loads(line) for line in path.read_text().splitlines()]
            self.assertEqual(len(rows), 32)
            self.assertEqual([row["link_id"] for row in rows], ["back"] * 16 + ["go"] * 16)
            self.assertEqual([row["freq_mhz"] for row in rows], [434.5] * 16 + [869.5] * 16)
            self.assertEqual([row["tx_height_m"] for row in rows[:16]], sorted(ref.HEIGHTS * 4))
            self.assertTrue(all(row["surface"] == "dtm" and row["clutter_db"] == 0 for row in rows))
            self.assertTrue(all(1.33 < row["k_factor"] < 1.34 for row in rows))
            self.assertIn("0 links computed, 2 reused", execute())
            profile["dtm"][150] -= 10
            write_jsonl(work / "profiles.jsonl", [profile])
            self.assertIn("2 links computed, 0 reused", execute())
            self.assertEqual(sorted(path.name for path in (work / "predictions").iterdir()),
                             ["itm_p2p.jsonl", "itm_p2p_t90.jsonl"])


@unittest.skipUnless(HAS_PY1812, "Review Py1812 or ITU maps are unavailable")
class Py1812Cases(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cache = cls.enterClassContext(TemporaryDirectory())
        cls.maps = ref.prepare_maps(ref.map_sources(REVIEW), Path(cache))
        cls.model = ref.load_py1812(PY1812_SOURCE, cls.maps)

    def test_flat_delta_bullington(self):
        terrain = np.zeros(3001)
        loss, diag = ref.delta_prediction(self.model, np.linspace(0, 30, 3001), terrain, 869.5, 3, 3)
        self.assertAlmostEqual(diag["lbs_db"], 19.780, places=3)
        self.assertAlmostEqual(diag["lsph_db"], 52.456, places=3)
        self.assertAlmostEqual(loss, diag["lsph_db"], places=9)

    def test_knife_edge_and_height_subset(self):
        profile = ground_fixture(edge=True)
        profile["a"].update(lat=30, lon=75)
        profile["b"].update(lat=30, lon=75.05)
        link = link_fixture("go", "a", "b")
        p1812, _ = ref.predict_link("p1812", profile, link, PY1812_SOURCE, self.maps, (2, 5, 10), "fixture")
        self.assertEqual(len(p1812), 18)
        self.assertTrue(all(row["diag"]["height_grid_subset"] for row in p1812))
        self.assertTrue(all(isclose(row["total_db"], row["fspl_db"] + row["excess_db"] + row["clutter_db"])
                            for row in p1812))
        self.assertGreater(p1812[0]["diag"]["components_db"]["Ld50 (dB)"], 10)
        self.assertEqual(p1812[1]["clutter"], "p1812_woodland")
        self.assertAlmostEqual(p1812[1]["diag"]["woodland_increment_db"],
                               p1812[1]["total_db"] - p1812[0]["total_db"])

    def test_published_scatter_recombination(self):
        profile = ground_fixture(edge=True)
        profile["a"].update(lat=30, lon=75)
        profile["b"].update(lat=30, lon=75.05)
        ends, surfaces = ref.directed_profile(profile, "a", "b")
        dtm = surfaces["dtm"]
        d = np.linspace(0, 5, len(dtm))
        cover = np.zeros_like(dtm)
        zones = ref.sea_proxy(dtm)
        model = self.model
        _, _, diag = ref.p1812_prediction(model, d, dtm, cover, zones, 869.5, 2, 5, ends)
        published = diag["published_p1812_7"]
        with patch.object(model, "tl_tropo", return_value=published["scatter_db"]):
            expected, _ = model.bt_loss(0.8695, 50, d, dtm, cover, zones, 2, 5, 2, 30, 30, 75, 75.05)
        self.assertAlmostEqual(published["total_db"], expected, places=6)


@unittest.skipUnless(HAS_ORACLES, "Private review fixtures or results.json are unavailable")
class ReviewOracles(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        work = Path(cls.enterClassContext(TemporaryDirectory()))
        cls.profiles, cls.links = convert(REVIEW, work)
        cls.expected = json.loads((REVIEW / "results.json").read_text(encoding="utf-8"))
        cls.cache = work / "cache"

    @unittest.skipUnless(HAS_ITM, "Review ITM DLL is unavailable")
    def test_itm_review_totals(self):
        for profile, link, expected in zip(self.profiles, self.links, self.expected, strict=True):
            for engine, variant in (("itm_p2p", "50"), ("itm_p2p_t90", "90_fixed")):
                rows, _ = ref.predict_link(engine, profile, link, DLL, None, (3,), "review")
                for row in rows:
                    oracle = expected["itm"][row["surface"].upper()]["10"][variant]
                    with self.subTest(link=expected["i"], engine=engine, surface=row["surface"]):
                        self.assertAlmostEqual(row["total_db"], oracle["total"], places=8)
                        self.assertEqual(row["diag"]["warning_hex"], oracle["warning"])

    @unittest.skipUnless(HAS_PY1812, "Review Py1812 or ITU maps are unavailable")
    def test_py1812_review_totals(self):
        maps = ref.prepare_maps(ref.map_sources(REVIEW), self.cache)
        names = {("dtm", "none"): "DTM open", ("dom", "none"): "DOM",
                 ("dtm", "p1812_woodland"): "DTM woodland", ("dom", "p1812_woodland"): "DOM woodland"}
        for profile, link, expected in zip(self.profiles, self.links, self.expected, strict=True):
            rows, _ = ref.predict_link("p1812", profile, link, PY1812_SOURCE, maps, (3,), "review")
            for row in rows:
                label = names[row["surface"], row["clutter"]]
                with self.subTest(link=expected["i"], label=label):
                    oracle = expected["p1812"][label]
                    self.assertAlmostEqual(row["total_db"], oracle["advised"], places=8)
            delta, _ = ref.predict_link("py1812_delta_bullington", profile, link, PY1812_SOURCE, maps, (3,), "review")
            for row in delta:
                oracle = expected["diffraction"][row["surface"].upper()]["10/1.000"]["delta"]["py1812"][0]
                with self.subTest(link=expected["i"], surface=row["surface"]):
                    self.assertAlmostEqual(row["excess_db"], oracle, places=8)
