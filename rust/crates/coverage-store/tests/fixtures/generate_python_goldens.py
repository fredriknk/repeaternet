"""Regenerate python-goldens.json from the Python reference implementation.

Developer-only. The Rust tests read the checked-in JSON and never run Python.

    PYTHONPATH=src .venv/Scripts/python.exe \
        rust/crates/coverage-store/tests/fixtures/generate_python_goldens.py

Every value is produced by the functions in ``rf_router_planner.web`` so the
goldens follow the reference code rather than a re-implementation.
"""

from __future__ import annotations

import hashlib
import json
import math
import os
import platform
import random
import struct
import subprocess
import sys
import tempfile
from pathlib import Path
from types import SimpleNamespace

from rf_router_planner import web
from rf_router_planner.models.settings import CandidateSettings, RFSettings

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[4]


def canonical(value: object) -> str:
    # Mirrors web._plan_fingerprint; checked against it below for every case.
    return json.dumps(value, sort_keys=True, separators=(",", ":"))


def case(text: str) -> dict[str, str]:
    value = json.loads(text)
    encoded = canonical(value)
    digest = hashlib.sha256(encoded.encode("utf-8")).hexdigest()
    if isinstance(value, dict):
        assert web._plan_fingerprint(value) == digest
    return {"input": text, "canonical": encoded, "sha256": digest}


def float_texts() -> list[str]:
    fixed = [
        "0.0", "-0.0", "1.0", "-1.0", "0.1", "0.2", "0.30000000000000004", "2.5",
        "2.675", "100.0", "1e-07", "1.5e-05", "0.0001", "0.00001", "0.001",
        "1e15", "1e16", "9999999999999998.0", "1e+16", "1E5", "1.0e-5", "1e22",
        "1e23", "1.5e300", "5e-324", "2.2250738585072014e-308",
        "1.7976931348623157e308", "123456789012345680.0", "0.000123456789",
        "1.000000000000000111", "4.35", "868.0", "868.1", "-273.15",
        "12345.678900000001", "1234567890123456.7", "0.5e-4", "50E-6",
        "60.123456789012344", "10.000000000000002", "9007199254740993.0",
        "3.0000000000000004e-05", "6.02214076e23", "1.602176634e-19",
    ]
    rng = random.Random(20261119)
    generated = []
    for _ in range(1500):
        bits = rng.getrandbits(64)
        value = struct.unpack("<d", struct.pack("<Q", bits))[0]
        if math.isfinite(value):
            generated.append(repr(value))
    for _ in range(500):
        value = rng.uniform(-1e6, 1e6) * 10 ** rng.randint(-12, 12)
        generated.append(repr(value))
        generated.append(f"{value:.25e}")
        generated.append(f"{value:.17g}")
    for _ in range(200):
        # Short decimals near binary midpoints exercise round-to-nearest-even.
        generated.append(f"{rng.randint(1, 10**9)}.{rng.randint(0, 99999):05d}5")
    return fixed + generated


def integer_texts() -> list[str]:
    return [
        "0", "-0", "1", "-1", "9007199254740993", "18446744073709551615",
        "18446744073709551616", "-9223372036854775809",
        "12345678901234567890123456789", "1734567890123456789",
    ]


def string_cases() -> list[str]:
    strings = [
        "", "plain", "Blåbær \U0001f3d4", "\u0000\u0001\u001f",
        "\b\f\n\r\t", "\u007f\u0080ÿ", "quote\" back\\ slash/", "  ",
        "￿﻿", "\U0001f600\U0010ffff", "éé", "<>&'",
    ]
    return [json.dumps(item) for item in strings] + [
        json.dumps(item, ensure_ascii=False) for item in strings
    ]


def structure_cases() -> list[str]:
    return [
        "{}", "[]", "true", "false", "null",
        '{"b":1,"a":2,"B":3,"\\u00e9":4,"\\ud83d\\ude00":5,"\\uffff":6,"":7,"a0":8,"a ":9}',
        '{"z":{"y":[{"b":1.0,"a":-0.0}],"x":null},"a":[1,1.0,1e0,true,"1"]}',
        '{"dup":1,"dup":2}',
        '{"lora_sf":{"12":-20.0,"7":-7.5,"10":-15.0}}',
        '  { "spaced" : [ 1 , 2.50 , "x" ] }  ',
    ]


def route_case() -> dict[str, object]:
    plan_text = json.dumps(
        {
            "format": "repeaternet-web",
            "version": 1,
            "a": [59.91, 10.75],
            "b": [60.39, 5.32],
            "rf": {"frequency_mhz": 868.0, "tx_power_dbm": 14, "margin": -0.0},
            "candidates": {"spacing_m": 250.0},
            "terrain": {"name": "Blåbær \U0001f3d4"},
            "coverage": {"cell_size_m": 100.0},
            "coverage_targets": [{"id": "t"}],
            "resolved_search": {"x": 1e-07},
            "unknown_outer": {"keep": [1.0, 2]},
        },
        ensure_ascii=False,
    )
    plan = json.loads(plan_text)
    return {
        "plan_text": plan_text,
        "route_fingerprint": web._route_plan_fingerprint(plan),
        "plan_fingerprint": web._plan_fingerprint(plan),
    }


def settings_cases() -> dict[str, object]:
    settings = web.encode(web.parse_coverage_settings({}))
    radio = {"rf": web.encode(RFSettings()), "candidates": web.encode(CandidateSettings())}
    return {
        "settings": settings,
        "settings_text": json.dumps(settings),
        "settings_fingerprint": web._plan_fingerprint(settings),
        "radio_settings": radio,
        "radio_text": json.dumps(radio),
        "radio_fingerprint": web._plan_fingerprint(radio),
    }


def reuse_case() -> dict[str, object]:
    job = json.loads((HERE / "python-job.json").read_text(encoding="utf-8"))["job"]
    text = json.dumps(job, indent=2)
    # web._save_coverage_manifest uses json.dumps(indent=2) with allow_nan, and
    # report_links margins are not passed through encode().
    nonfinite = dict(job)
    nonfinite["report_links"] = [
        {"source_id": "R-coverage", "target_id": "x", "worst_margin_db": -math.inf,
         "distance_m": math.nan, "note": "NaN -Infinity \"Infinity\""}
    ]
    return {
        "nonfinite_manifest_text": json.dumps(nonfinite, indent=2),
        "job_text": text,
        "reuse_fingerprint": web._coverage_reuse_fingerprint(json.loads(text)),
        "settings_fingerprint": web._plan_fingerprint(job["settings"]),
        "radio_fingerprint": web._plan_fingerprint(job["radio_settings"]),
    }


def terrain_case() -> dict[str, object]:
    files = [
        ("dtm/a.tif", 9214, 1734567890123456700),
        ("dom/bå.tif", 17, 1700000000000000000),
        ("dtm/z.tif", 0, -1234567800),
        ("terrain-generations/generation-0001/dtm/c.tif", 3, 1734567890000000100),
        ("terrain-generations/generation-0002/dom/d.tif", 5, 100),
        ("coverage-terrain-generations/generation-0003/dtm/e.tif", 7, 1600000000999999900),
        ("coverage-terrain-generations/generation-0003/dom/f.tif", 11, 1734567890123456800),
    ]
    with tempfile.TemporaryDirectory() as raw:
        directory = Path(raw)
        for relative, size, mtime_ns in files:
            path = directory / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(b"x" * size)
            os.utime(path, ns=(mtime_ns, mtime_ns))
        workspace = SimpleNamespace(
            directory=directory,
            coverage_terrain_directory=directory / "coverage-terrain-generations",
        )
        route = web._terrain_fingerprint(workspace)
        coverage = web._terrain_fingerprint(workspace, coverage=True)
    return {
        "files": [list(item) for item in files],
        "route_terrain_fingerprint": route,
        "coverage_terrain_fingerprint": coverage,
        "coverage_terrain_text": json.dumps(coverage),
    }


def main() -> None:
    texts = float_texts() + integer_texts() + string_cases() + structure_cases()
    commit = subprocess.run(
        ["git", "rev-parse", "HEAD"], cwd=ROOT, capture_output=True, text=True, check=True
    ).stdout.strip()
    goldens = {
        "generator": {
            "python": sys.version.split()[0],
            "platform": platform.platform(),
            "baseline_commit": commit,
            "model_version": web.COVERAGE_MODEL_VERSION,
            "reuse_fields": list(web._COVERAGE_REUSE_FIELDS),
        },
        "canonical": [case(text) for text in texts],
        # Python reads these as non-finite floats and writes Infinity/NaN, which
        # is not JSON. The Rust legacy encoder must refuse them.
        "unsupported": ["1e400", "-1e400", "[1e999]", '{"x":-2e308}'],
        "route": route_case(),
        "settings": settings_cases(),
        "reuse": reuse_case(),
        "terrain": terrain_case(),
    }
    output = HERE / "python-goldens.json"
    output.write_text(json.dumps(goldens, indent=1) + "\n", encoding="utf-8")
    print(f"wrote {len(goldens['canonical'])} canonical cases to {output}")


if __name__ == "__main__":
    main()
