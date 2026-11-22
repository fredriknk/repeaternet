"""Write the external RF reference predictions from calibration JSON Lines."""

import argparse
from contextlib import closing, contextmanager
import ctypes as ct
from functools import lru_cache
import hashlib
import importlib.resources
import importlib.util
from io import StringIO
from itertools import groupby, islice, product
import json
from math import ceil, exp, hypot, isclose, isfinite, log10
import multiprocessing as mp
import os
from pathlib import Path
import sys
from tempfile import NamedTemporaryFile
from time import perf_counter
from unittest.mock import patch

import numpy as np


DEFAULT_REVIEW = Path(r"C:\Users\fnk\pythonprojects\repeaternet\rust\target\rf-review")
DEFAULT_ENGINES = ("itm_p2p", "p1812", "py1812_delta_bullington")
ITM_SETTINGS = {"itm_p2p": (50, 0), "itm_p2p_t90": (90, 1)}
ENGINES = (*ITM_SETTINGS, *DEFAULT_ENGINES[1:])
HEIGHTS = (2, 5, 10, 20)
EARTH_M = 6_371_000.0
DEFAULT_FREQ_MHZ = 869.5


def safe_path(path):
    path = Path(path).resolve()
    local = os.environ.get("LOCALAPPDATA")
    if local and path.is_relative_to((Path(local) / "RepeaterNet" / "data").resolve()):
        raise ValueError(f"Path is inside the live RepeaterNet data directory: {path}")
    return path


def digest_file(path):
    with path.open("rb") as file:
        return hashlib.file_digest(file, "sha256").hexdigest()


@contextmanager
def atomic_output(path):
    path.parent.mkdir(parents=True, exist_ok=True)
    with NamedTemporaryFile(dir=path.parent, prefix=path.name + ".", suffix=".partial",
                            delete_on_close=False, mode="w+b") as file:
        yield file
        file.close()  # Windows requires closing the temporary file before replacement.
        os.replace(file.name, path)


def map_sources(review):
    return {key: safe_path(review / "vendor" / "itu-recommendations" / "maps7" / f"{key}.TXT")
            for key in ("DN50", "N050")}


def prepare_maps(sources, cache):
    fingerprint = hashlib.sha256("".join(map(digest_file, sources.values())).encode()).hexdigest()
    path = safe_path(cache / f"p1812-{fingerprint}" / "P1812.npz")
    if not path.is_relative_to(cache):
        raise ValueError("The ITU map archive must be inside the calibration cache")
    if not path.exists():
        with atomic_output(path) as file:
            np.savez(file, **{key: np.loadtxt(source) for key, source in sources.items()})
    return path


def py1812_source(review):
    return safe_path(review / "vendor" / "py1812" / "src" / "Py1812" / "P1812.py")


@lru_cache(maxsize=1)
def load_py1812(source, maps):
    src = str(source.parent.parent)
    if src not in sys.path:
        sys.path.insert(0, src)
    # The pinned module reads its package resource at import. Redirect only that
    # lookup to the calibration cache, including when the vendor npz exists.
    original_files = importlib.resources.files

    def resource_files(package):
        return maps.parent if package == "Py1812" else original_files(package)

    spec = importlib.util.spec_from_file_location("Py1812.P1812", source)
    module = importlib.util.module_from_spec(spec)
    with patch("importlib.resources.files", resource_files), patch.object(sys, "dont_write_bytecode", True):
        spec.loader.exec_module(module)
    return module


class ItmIntermediate(ct.Structure):
    _fields_ = [(name, ct.c_double * 2) for name in ("angles", "horizons", "effective_height")] + [
        (name, ct.c_double) for name in ("ns", "delta_h", "aref", "fs", "distance_km")
    ] + [("mode", ct.c_int)]


@lru_cache(maxsize=1)
def load_itm(path):
    run = ct.CDLL(str(path)).ITM_P2P_CR_Ex
    double, integer, pointer = ct.c_double, ct.c_int, ct.POINTER
    run.argtypes = [double, double, pointer(double), integer, double, double, integer,
                    double, double, integer, double, double, pointer(double), pointer(ct.c_long),
                    pointer(ItmIntermediate)]
    run.restype = integer
    return run


def itm_prediction(run, step_m, terrain, freq, tx, rx, reliability, mdvar):
    intervals = len(terrain) - 1
    trim = intervals // 10
    n0 = 301 * exp(float(np.mean(terrain[trim:len(terrain) - trim])) / 9460)
    profile = np.r_[intervals, step_m, terrain].astype(np.float64)
    loss, warnings, data = ct.c_double(), ct.c_long(), ItmIntermediate()
    code = run(tx, rx, profile.ctypes.data_as(ct.POINTER(ct.c_double)), 5, n0, freq, 1,
               15, 0.005, mdvar, 50, reliability, ct.byref(loss), ct.byref(warnings), ct.byref(data))
    if code not in (0, 1):
        raise ValueError(f"ITM_P2P_CR_Ex returned {code} (warning flags {warnings.value:#x})")
    curvature = 157e-9 * (1 - 0.04665 * exp(data.ns / 179.3))
    diag = {"mode": {1: "LOS", 2: "diffraction", 3: "scatter"}[data.mode],
            "return_code": code, "warning_flags": warnings.value, "warning_hex": hex(warnings.value),
            "horizons_m": list(data.horizons), "angles_rad": list(data.angles), "delta_h_m": data.delta_h,
            "native_fspl_db": data.fs, "reference_excess_db": data.aref, "ns": data.ns,
            "confidence_pct": 50, "reliability_pct": reliability, "mdvar": mdvar}
    return loss.value, 1 / (curvature * EARTH_M), diag


def sea_proxy(dtm):
    zones = np.where(dtm <= 100, 3, 4)
    low = np.flatnonzero(dtm < 2)
    for run in np.split(low, np.flatnonzero(np.diff(low) > 1) + 1):
        if len(run) >= 15 and np.median(dtm[run]) < 0.25:
            zones[run] = 1
    return zones


def coast_distances(d_km, zones):
    sea = np.flatnonzero(zones == 1)
    if not len(sea):
        return {}
    first, last = sea[0], sea[-1]
    return {"dct": (d_km[first - 1] + d_km[first]) / 2 if first else 0.0,
            "dcr": d_km[-1] - (d_km[last] + d_km[last + 1]) / 2 if last < len(zones) - 1 else 0.0}


def published_scatter(model, d, dtm, freq, tx, rx, ends, components):
    """P.1812-7 §4.4 substitution at p=50%, using Py1812's named angles."""
    ae = components["ae (km)"]
    distance = d[-1]
    ht, hr = dtm[0] + tx, dtm[-1] + rx
    at, ar = (np.tan(max(0, components[f"th_{end} (mrad)"]) / 1000 + distance / (2 * ae))
              for end in ("t", "r"))
    cv = float(np.clip((distance * ar - (ht - hr) / 1000) / (at + ar), 0, distance))
    src, dst = ends
    lon, lat = model.great_circle_path(dst["lon"], src["lon"], dst["lat"], src["lat"], EARTH_M / 1000, cv)[:2]
    dn, n0 = (model.interp2(model.DigitalMaps[key], lon, lat, 1.5, 1.5) for key in ("DN50", "N050"))
    hs = np.interp(cv, d, dtm) / 1000
    theta = max(components["th (mrad)"], 1e-6)
    scatter = max(92.4 + 20 * log10(freq / 1000) + 20 * log10(hypot(distance, (ht - hr) / 1000)),
                  0.18 * n0 * exp(-hs / 7.35) - 0.23 * dn + 22 * log10(freq) + 35 * log10(theta)
                  + 17 * log10(distance) + 0.07 * exp(0.055 * 4.3))
    total = -5 / np.log(10) * np.logaddexp(-0.2 * np.log(10) * scatter,
                                          -0.2 * np.log(10) * components["Lbam (dB)"])
    return {"scatter_db": float(scatter), "total_db": float(max(components["Lb0p"], total)),
            "common_volume_km": cv, "dn_cv": float(dn), "n0_cv": float(n0)}


def p1812_prediction(model, d, dtm, cover, zones, freq, tx, rx, ends):
    src, dst = ends
    debug = StringIO()
    loss, _ = model.bt_loss(freq / 1000, 50, d, dtm, cover, zones, tx, rx, 2,
                            src["lat"], dst["lat"], src["lon"], dst["lon"],
                            pL=50, sigmaL=0, Gtx=2.15, Grx=2.15, debug=1, fid_log=debug,
                            **coast_distances(d, zones))
    components = {}
    for line in debug.getvalue().splitlines():
        fields = line.rsplit(",", 2)
        if len(fields) == 3 and fields[1] and not line.startswith("#"):
            components[line.split(",", 1)[0].strip()] = float(fields[1])
    diag = {"scatter": "p1812_6_itu_advised", "dn": components["DN"], "n0": components["N0"],
            "sea_fraction": components["w"], "dct_km": components["dct (km)"],
            "dcr_km": components["dcr (km)"], "p_pct": 50, "pL_pct": 50,
            "components_db": {key: components[key] for key in (
                "Lbfs", "Lb0p", "Lbulla (dB)", "Lbulls (dB)", "Ldsph (dB)",
                "Ld50 (dB)", "Lbd50 (dB)", "Lba (dB)", "Lbam (dB)", "Lbs (dB)")},
            "published_p1812_7": published_scatter(model, d, dtm, freq, tx, rx, ends, components)}
    return float(loss), components["ae (km)"] / (EARTH_M / 1000), diag


def delta_prediction(model, d, terrain, freq, tx, rx, k=1.0):
    ae = EARTH_M * k / 1000
    f_ghz = freq / 1000
    smooth = model.smooth_earth_heights(d, terrain, np.zeros_like(terrain), tx, rx, ae, f_ghz)
    loss, actual, flat, spherical = model.dl_delta_bull(
        d, terrain, terrain[0] + tx, terrain[-1] + rx, *smooth[4:6], ae, f_ghz, 0, 0)
    return float(loss[1]), {"lba_db": float(actual), "lbs_db": float(flat),
                            "lsph_db": float(spherical[1]), "line_of_sight": bool(smooth[-1] == 1)}


def fspl_db(freq, horizontal_m, antenna_delta_m):
    return 32.45 + 20 * log10(freq) + 20 * log10(hypot(horizontal_m, antenna_delta_m) / 1000)


def directed_profile(profile, tx_node, rx_node):
    a, b = profile["a"], profile["b"]
    forward = (tx_node, rx_node) == (a["node"], b["node"])
    if not forward and (tx_node, rx_node) != (b["node"], a["node"]):
        raise ValueError("Link endpoints do not match the profile")
    ends = (a, b) if forward else (b, a)
    surfaces = {}
    for surface in ("dtm", "dom"):
        if profile.get(surface) is None:
            continue
        terrain = np.array(profile[surface], dtype=np.float64)
        if not forward:
            terrain = terrain[::-1]
        if terrain.ndim != 1 or len(terrain) < 2 or not np.isfinite(terrain).all():
            raise ValueError(f"Invalid {surface} samples")
        # The raw DOM is an interior obstruction. Antenna AGL uses DTM ground.
        terrain[[0, -1]] = [end["ground_m"] for end in ends]
        surfaces[surface] = terrain
    step = profile["step_m"]
    distance = (len(surfaces["dtm"]) - 1) * step
    if not isfinite(step) or step <= 0 or not isclose(distance, profile["distance_m"], abs_tol=1e-6):
        raise ValueError("Profile distance must match the uniform step and sample count")
    if "dom" in surfaces and surfaces["dom"].shape != surfaces["dtm"].shape:
        raise ValueError("DTM and DOM must share the same grid")
    return ends, surfaces


def predict_link(engine, profile, link, source, maps, heights, cache_key):
    try:
        return _predict_link(engine, profile, link, source, maps, heights, cache_key)
    except (ValueError, IndexError, KeyError) as error:
        raise ValueError(f"{engine}, link {link['link_id']}: {error}") from error


def _predict_link(engine, profile, link, source, maps, heights, cache_key):
    start = perf_counter()
    ends, surfaces = directed_profile(profile, link["tx"]["node"], link["rx"]["node"])
    freq = link_frequency(link)
    step_m = profile["step_m"]
    dtm = surfaces["dtm"]
    d = np.arange(len(dtm)) * (step_m / 1000)
    zones = sea_proxy(dtm) if engine == "p1812" else None
    if engine in ITM_SETTINGS:
        model = load_itm(source)
    else:
        model = load_py1812(source, maps)
        minimum_samples = 5 if engine == "p1812" else 3
        if len(d) < minimum_samples:
            # Subdivide each segment equally, preserving every input sample.
            subdivision = ceil((minimum_samples - 1) / (len(d) - 1))
            finer = np.linspace(0, d[-1], (len(d) - 1) * subdivision + 1)
            surfaces = {name: np.interp(finer, d, samples) for name, samples in surfaces.items()}
            dtm, d = surfaces["dtm"], finer
            step_m /= subdivision
            if engine == "p1812":
                zones = sea_proxy(dtm)

    records = []
    for surface, terrain in sorted(surfaces.items()):
        if engine == "p1812":
            cover = terrain - dtm
            strips = (d <= 0.1) | (d[-1] - d <= 0.1)
            strips[[0, -1]] = False
            woodland = np.where(strips, np.maximum(cover, 15), cover)
        for tx, rx in product(heights, repeat=2):
            fspl = fspl_db(freq, profile["distance_m"], (dtm[-1] - dtm[0]) + (rx - tx))
            if engine in ITM_SETTINGS:
                total, k, diag = itm_prediction(model, step_m, terrain, freq, tx, rx, *ITM_SETTINGS[engine])
            elif engine == "p1812":
                total, k, diag = p1812_prediction(model, d, dtm, cover, zones, freq, tx, rx, ends)
                diag.update(height_grid_m=heights, height_grid_subset=tuple(heights) != HEIGHTS)
            else:
                k = 1.0
                loss, diag = delta_prediction(model, d, terrain, freq, tx, rx, k)
                total = fspl + loss
            common = {"schema": 1, "link_id": link["link_id"], "path_id": profile["path_id"],
                      "engine": engine, "surface": surface, "tx_height_m": tx, "rx_height_m": rx,
                      "k_factor": k, "step_m": step_m, "freq_mhz": freq, "fspl_db": fspl,
                      "v_max": None, "line_of_sight": diag.get("line_of_sight")}
            diag.update(cache_key=cache_key, input_step_m=profile["step_m"])
            records.append(dict(common, clutter="none", total_db=total, excess_db=total - fspl,
                                clutter_db=0.0, diag=diag))
            if engine == "p1812":
                wooded, _, wood_diag = p1812_prediction(model, d, dtm, woodland, zones, freq, tx, rx, ends)
                # P.1812 reruns the propagation calculation with a changed profile.
                # The scenario difference is available separately from its joint loss.
                wood_diag.update(height_grid_m=heights, height_grid_subset=tuple(heights) != HEIGHTS,
                                 cache_key=cache_key, input_step_m=profile["step_m"],
                                 woodland_increment_db=wooded - total)
                records.append(dict(common, clutter="p1812_woodland", total_db=wooded,
                                    excess_db=wooded - fspl, clutter_db=0.0, diag=wood_diag))
    return records, perf_counter() - start


def link_frequency(link):
    freq = (link.get("radio") or {}).get("freq_mhz")
    if freq is None:
        return DEFAULT_FREQ_MHZ
    if not isfinite(freq) or freq <= 0:
        raise ValueError(f"Invalid frequency for link {link['link_id']}: {freq}")
    return freq


def profile_index(file):
    by_link = {}
    while True:
        offset = file.tell()
        line = file.readline()
        if not line:
            return by_link
        if not line.strip():
            continue
        profile = json.loads(line)
        entry = (offset, hashlib.sha256(line).hexdigest(), 1 + (profile.get("dom") is not None))
        for link_id in profile["link_ids"]:
            if link_id in by_link:
                raise ValueError(f"Multiple profiles for link {link_id}")
            by_link[link_id] = entry


def json_lines(file):
    for line in file:
        if line.strip():
            yield json.loads(line)


def existing_records(path):
    if path.exists():
        with path.open("rb") as file:
            yield from json_lines(file)


def engine_inputs(file, by_link, links, output, engine, version, source, maps, heights):
    # Release the old file before replacing it on Windows.
    with closing(existing_records(output)) as records:
        groups = groupby(records, key=lambda row: row["link_id"])
        old_id, rows = next(groups, (None, ()))
        for link in links:
            link_id = link["link_id"]
            offset, profile_hash, surface_count = by_link[link_id]
            parameters = [version, profile_hash, link_frequency(link), link["tx"]["node"], link["rx"]["node"]]
            key = hashlib.sha256(json.dumps(parameters).encode()).hexdigest()
            while old_id is not None and old_id < link_id:
                old_id, rows = next(groups, (None, ()))
            if old_id == link_id:
                cached = list(rows)
                count = surface_count * len(heights) ** 2 * (2 if engine == "p1812" else 1)
                if len(cached) == count and all((row.get("diag") or {}).get("cache_key") == key for row in cached):
                    yield cached, None
                    continue
            file.seek(offset)
            yield None, (engine, json.loads(file.readline()), link, source, maps, heights, key)


def write_engine(file, by_link, links, output, engine, source, maps, heights, jobs):
    version = [engine, digest_file(Path(__file__)), digest_file(source), sys.version, np.__version__,
               maps.parent.name if maps else None, heights]
    start = perf_counter()
    computed = reused = cases = 0
    compute_s = 0.0
    workers = min(jobs, len(links)) if engine == "p1812" else 1
    inputs = engine_inputs(file, by_link, links, output, engine, version, source, maps, heights)
    pool = None
    try:
        with atomic_output(output) as out:
            while batch := list(islice(inputs, max(1, workers) * 2)):
                misses = [task for _, task in batch if task is not None]
                if pool is None and workers > 1 and len(misses) > 1:
                    pool = mp.Pool(workers)
                results = iter(pool.starmap(predict_link, misses) if pool and misses else
                               (predict_link(*task) for task in misses))
                for cached, _ in batch:
                    if cached is None:
                        records, elapsed = next(results)
                        computed += 1
                        cases += len(records)
                        compute_s += elapsed
                    else:
                        reused += 1
                        records = cached
                    for record in records:
                        out.write((json.dumps(record, allow_nan=False, separators=(",", ":")) + "\n").encode())
    finally:
        inputs.close()
        if pool:
            pool.terminate()
            pool.join()
    wall_s = perf_counter() - start
    summary = f"{engine}: {wall_s:.3f} s wall, {computed} links computed, {reused} reused"
    if computed:
        pairs = computed * len(heights) ** 2
        summary += (f", {cases} predictions, {compute_s * 1000 / pairs:.3f} ms/link-height pair"
                    f", {compute_s * 1000 / cases:.3f} ms/prediction (summed worker time)")
    print(summary, flush=True)


def choices(value, allowed, parse=str):
    items = tuple(dict.fromkeys(parse(item.strip()) for item in value.split(",")))
    unsupported = set(items) - set(allowed)
    if unsupported:
        raise ValueError(f"Unsupported choices: {', '.join(map(str, sorted(unsupported)))}")
    return items


def run(args):
    local = os.environ.get("LOCALAPPDATA")
    work_arg = args.work_dir or os.environ.get("RF_CALIBRATION_DIR")
    if work_arg is None:
        if not local:
            raise ValueError("Set --work-dir or RF_CALIBRATION_DIR when LOCALAPPDATA is unset")
        work_arg = Path(local) / "RepeaterNet" / "calibration"
    work = safe_path(work_arg)
    review = safe_path(args.review_dir or os.environ.get("RF_REVIEW_DIR") or DEFAULT_REVIEW)
    profile_path = safe_path(work / "profiles.jsonl")
    link_path = safe_path(args.links or work / "dataset" / "links.jsonl")
    engines = list(choices(args.engines, ENGINES))
    if "itm_p2p" in engines and "itm_p2p_t90" not in engines:
        engines.insert(engines.index("itm_p2p") + 1, "itm_p2p_t90")
    p1812_heights = tuple(sorted(choices(args.p1812_heights, HEIGHTS, int)))
    if args.jobs < 1:
        raise ValueError("--jobs must be at least 1")

    outputs = {engine: safe_path(work / "predictions" / f"{engine}.jsonl") for engine in engines}
    cache = safe_path(work / "cache")
    if any(not path.is_relative_to(work) or path.is_relative_to(review) for path in (*outputs.values(), cache)):
        raise ValueError("Outputs and cache must stay inside the work directory, outside the read-only review")
    if link_path in outputs.values() or profile_path in outputs.values() or len(set(outputs.values())) != len(outputs):
        raise ValueError("Prediction files must be distinct from each other and from the inputs")

    with link_path.open("rb") as file:
        links = sorted(json_lines(file), key=lambda row: row["link_id"])
    if len({link["link_id"] for link in links}) != len(links):
        raise ValueError("Duplicate link_id in the links file")

    with profile_path.open("rb") as file:
        by_link = profile_index(file)
        matched = [link for link in links if link["link_id"] in by_link]
        if len(matched) != len(links):
            print(f"Skipping {len(links) - len(matched)} links without profiles", file=sys.stderr)
        if links and not matched:
            raise ValueError("No links matched the profiles file")
        maps = None
        for engine in engines:
            itm = engine in ITM_SETTINGS
            source = safe_path(review / "ntia-itm.dll") if itm else py1812_source(review)
            if not source.is_file():
                raise ValueError(f"Reference dependency missing: {source}. See tools/rf-calibration/README.md")
            if not itm and maps is None:
                maps = prepare_maps(map_sources(review), cache)
            write_engine(file, by_link, matched, outputs[engine], engine, source, None if itm else maps,
                         p1812_heights if engine == "p1812" else HEIGHTS, args.jobs)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--work-dir", type=Path)
    parser.add_argument("--links", type=Path)
    parser.add_argument("--review-dir", type=Path)
    parser.add_argument("--engines", default=",".join(DEFAULT_ENGINES))
    parser.add_argument("--jobs", type=int, default=max(1, (os.cpu_count() or 1) - 1))
    parser.add_argument("--p1812-heights", default=",".join(map(str, HEIGHTS)))
    args = parser.parse_args()
    try:
        run(args)
    except (OSError, ValueError) as error:
        parser.exit(1, f"{parser.prog}: {error}\n")


if __name__ == "__main__":
    main()
