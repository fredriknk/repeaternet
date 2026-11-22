"""Fit and compare path-loss engines against the measured-link dataset.

Usage::

    python -I tools/rf-calibration/calibrate.py [--work-dir DIR] [--links PATH]
        [--region norway|world|all] [--folds 5] [--block-km 20] [--seed 8695]

Reads ``<work>/dataset/links.jsonl``, ``<work>/predictions/*.jsonl`` and,
if present, ``<work>/path_features.jsonl`` and ``<work>/dataset/nodes.jsonl``.
Writes ``<work>/report/results.json`` and ``<work>/report/report.md``.
See ``docs/RF_CALIBRATION.md`` for the method.
"""

from __future__ import annotations

import argparse
import json
import math
import os
import sys
from collections import defaultdict
from pathlib import Path

import numpy as np

sys.path.insert(0, str(Path(__file__).resolve().parent))

import calib_model as cm  # noqa: E402

# Planner defaults (rf-core RfSettings::default): 22 dBm, 2.15 dBi and 1 dB
# feed loss at each end, 3 m antennas.
PLANNER_BUDGET_DB = 22.0 + 2 * (2.15 - 1.0)
PLANNER_HEIGHT_M = 3.0

EXCLUDE_LINK_FLAGS = {"over_300km", "mqtt_suspect", "via_mqtt", "relayed", "self_link", "radio_mismatch"}
EXCLUDE_ENDPOINT_FLAGS = {"default_position", "moved"}
SNR_SATURATION_DB = 9.0
DEFAULT_SIGMA_T = 2.5
DEFAULT_NOISE_DBM = -117.0

# Block constants: prior mean and sd (inf = flat). SNR blocks absorb the noise
# floor, so their constant is not a hardware budget and gets a flat prior.
BLOCKS = {
    "corescope_signal": (PLANNER_BUDGET_DB, 4.0),
    "meshtastic_signal": (PLANNER_BUDGET_DB, 5.0),
    "meshtastic_snr": (0.0, math.inf),
}

ROLE_HEIGHT_PRIOR = {
    "repeater": 6.0,
    "room": 6.0,
    "router": 6.0,
    "router_client": 6.0,
    "router_late": 6.0,
    "observer": 4.0,
    "companion": 2.0,
    "client": 2.0,
    "client_mute": 2.0,
    "client_base": 4.0,
    "tracker": 1.5,
    "sensor": 2.0,
}
UNKNOWN_HEIGHT_PRIOR = 4.0


def default_work_dir() -> Path:
    if os.environ.get("RF_CALIBRATION_DIR"):
        return Path(os.environ["RF_CALIBRATION_DIR"])
    base = os.environ.get("LOCALAPPDATA") or os.environ.get("XDG_DATA_HOME")
    if not base:
        base = str(Path.home() / ".local" / "share")
    return Path(base) / "RepeaterNet" / "calibration"


def refuse_user_data(path: Path) -> None:
    base = os.environ.get("LOCALAPPDATA") or os.environ.get("XDG_DATA_HOME")
    if not base:
        return
    real = (Path(base) / "RepeaterNet" / "data").resolve()
    p = path.resolve()
    if p == real or real in p.parents:
        raise SystemExit(f"refusing to use {path}: inside the real RepeaterNet data directory")


def read_jsonl(path: Path):
    with path.open(encoding="utf-8") as fh:
        for line in fh:
            line = line.strip()
            if line:
                yield json.loads(line)


# ---------------------------------------------------------------------------
# Observations
# ---------------------------------------------------------------------------


def link_block(rec: dict) -> str | None:
    ev = rec["evidence"]
    if ev.startswith("corescope"):
        return "corescope_signal"
    if rec.get("signal_dbm"):
        return "meshtastic_signal"
    if rec.get("snr_db"):
        return "meshtastic_snr"
    return None


def usable(rec: dict) -> tuple[bool, str]:
    flags = set(rec.get("flags", []))
    bad = flags & EXCLUDE_LINK_FLAGS
    if bad:
        return False, sorted(bad)[0]
    for end in ("tx", "rx"):
        bad = set(rec[end].get("flags", [])) & EXCLUDE_ENDPOINT_FLAGS
        if bad:
            return False, f"{end}_{sorted(bad)[0]}"
    if rec["distance_m"] < 30:
        return False, "too_short"
    if rec["evidence"] != "corescope_non_detection" and rec.get("n", 0) < 1:
        return False, "no_samples"
    return True, ""


def build_observations(links: list[dict], nodes_meta: dict) -> tuple[list[dict], dict]:
    """Turn link records into censored link-level observations."""
    drops: dict[str, int] = defaultdict(int)
    # Receiver noise from detected links (median of per-link noise medians).
    noise_by_rx: dict[str, list[float]] = defaultdict(list)
    for rec in links:
        if rec.get("noise_dbm"):
            noise_by_rx[rec["rx"]["node"]].append(rec["noise_dbm"]["median"])
    # Reference detection rate per receiver: what a strong link achieves.
    ref_p: dict[str, float] = {}
    strong: dict[str, list[float]] = defaultdict(list)
    for rec in links:
        det = rec.get("detection")
        snr = rec.get("snr_db")
        if det and det["opportunities"] >= 20 and snr and snr["median"] > 5:
            strong[rec["rx"]["node"]].append(det["heard"] / det["opportunities"])
    for rx, ps in strong.items():
        ref_p[rx] = float(np.quantile(ps, 0.9))

    best: dict[tuple, dict] = {}
    for rec in links:
        ok, why = usable(rec)
        if not ok:
            drops[why] += 1
            continue
        block = link_block(rec)
        if block is None:
            drops["no_signal"] += 1
            continue
        rx = rec["rx"]["node"]
        sf = (rec.get("radio") or {}).get("sf")
        if noise_by_rx.get(rx):
            noise = float(np.median(noise_by_rx[rx]))
        elif nodes_meta.get(rx, {}).get("noise_floor_dbm") is not None:
            noise = float(nodes_meta[rx]["noise_floor_dbm"])
        else:
            noise = DEFAULT_NOISE_DBM
        floor = noise + cm.detection_floor_snr(sf)
        obs = {
            "link_id": rec["link_id"],
            "path_id": None,
            "tx": rec["tx"]["node"],
            "rx": rx,
            "block": block,
            "evidence": rec["evidence"],
            "source": rec["source"],
            "region": rec["region"],
            "distance_m": rec["distance_m"],
            "tx_role": rec["tx"].get("role"),
            "rx_role": rx_role(rec),
            "tx_pos": (rec["tx"]["lat"], rec["tx"]["lon"]),
            "rx_pos": (rec["rx"]["lat"], rec["rx"]["lon"]),
            "coarse": "coarse_position" in rec["tx"].get("flags", []) + rec["rx"].get("flags", []),
            "n": rec.get("n", 0),
            "sigma_t": None,
            "censoring_corrected": False,
        }
        if rec["evidence"] == "corescope_non_detection":
            det = rec.get("detection") or {}
            m_eff = det.get("opportunities", 0) * ref_p.get(rx, 0.5)
            if m_eff < 5:
                drops["non_detection_few_opportunities"] += 1
                continue
            obs.update(y=float("nan"), cens=-1, bound=cm.non_detection_bound(floor, DEFAULT_SIGMA_T, m_eff))
        elif block == "meshtastic_snr":
            s = rec["snr_db"]
            if s["median"] >= SNR_SATURATION_DB:
                obs.update(y=float("nan"), cens=1, bound=SNR_SATURATION_DB)
            else:
                obs.update(y=float(s["median"]), cens=0, bound=float("nan"))
            obs["sigma_t"] = max(s.get("mad") or 0.0, 1.0)
        else:
            s = rec.get("signal_dbm")
            if not s:
                drops["no_signal"] += 1
                continue
            sigma_t = max(s.get("mad") or 0.0, (s["p90"] - s["median"]) / 1.2816, 1.0)
            y = float(s["median"])
            det = rec.get("detection")
            if det and det["opportunities"] >= 10:
                p = det["heard"] / det["opportunities"]
                p_adj = min(1.0, p / ref_p.get(rx, 1.0)) if ref_p.get(rx) else 1.0
                if p_adj < 0.95:
                    y = cm.censoring_corrected_mean(y, sigma_t, p_adj)
                    obs["censoring_corrected"] = True
            obs.update(y=y, cens=0, bound=float("nan"), sigma_t=sigma_t)
        key = (obs["tx"], obs["rx"], block)
        prev = best.get(key)
        if prev is None or (obs["n"], obs["cens"] == 0) > (prev["n"], prev["cens"] == 0):
            if prev is not None:
                drops["duplicate_pair"] += 1
            best[key] = obs
        else:
            drops["duplicate_pair"] += 1
    return sorted(best.values(), key=lambda o: o["link_id"]), dict(drops)


def rx_role(rec: dict) -> str | None:
    role = rec["rx"].get("role")
    if role:
        return role
    if rec["evidence"].startswith("corescope"):
        return "observer"
    return None


# ---------------------------------------------------------------------------
# Predictions
# ---------------------------------------------------------------------------


FREE_SPACE = "free_space|-|none|-|-"


def config_key(p: dict) -> str:
    return f"{p['engine']}|{p['surface']}|{p['clutter']}|{round(p["step_m"]):g}|{p["k_factor"]:.3g}"


def load_predictions(pred_dir: Path, wanted: set[str]):
    """Return {config: {link_id: (total grid, excess grid, v grid)}} and path ids."""
    hidx = {h: i for i, h in enumerate(cm.GRID_HEIGHTS)}
    grids: dict[str, dict[str, np.ndarray]] = defaultdict(dict)
    path_of: dict[str, str] = {}
    for f in sorted(pred_dir.glob("*.jsonl")):
        for p in read_jsonl(f):
            lid = p["link_id"]
            if lid not in wanted:
                continue
            a = hidx.get(float(p["tx_height_m"]))
            b = hidx.get(float(p["rx_height_m"]))
            if a is None or b is None:
                continue
            key = config_key(p)
            g = grids[key].get(lid)
            if g is None:
                g = np.full((3, 4, 4), np.nan)
                grids[key][lid] = g
            fs = grids[FREE_SPACE].get(lid)
            if fs is None:
                fs = np.full((3, 4, 4), np.nan)
                fs[1] = 0.0
                grids[FREE_SPACE][lid] = fs
            fs[0, a, b] = p["fspl_db"]
            g[0, a, b] = p["total_db"]
            g[1, a, b] = p["excess_db"] + p.get("clutter_db", 0.0)
            g[2, a, b] = np.nan if p.get("v_max") is None else p["v_max"]
            path_of[lid] = p["path_id"]
    return grids, path_of


def edge_fill(grid: np.ndarray) -> np.ndarray:
    """Fill missing heights from the nearest available height on each axis."""
    g = grid.copy()
    for _ in range(2):
        for axis in (0, 1):
            for i in range(4):
                for j in range(4):
                    idx = (i, j) if axis == 1 else (j, i)
                    if np.isnan(g[idx]):
                        line = g[i, :] if axis == 1 else g[:, i]
                        ok = np.where(~np.isnan(line))[0]
                        if ok.size:
                            near = ok[np.argmin(np.abs(ok - j))]
                            g[idx] = line[near]
    return g


# ---------------------------------------------------------------------------
# Spatial folds
# ---------------------------------------------------------------------------


def node_positions(obs: list[dict]) -> dict[str, tuple[float, float]]:
    pos = {}
    for o in obs:
        pos.setdefault(o["tx"], o["tx_pos"])
        pos.setdefault(o["rx"], o["rx_pos"])
    return pos


def spatial_folds(pos: dict, block_km: float, k: int, seed: int) -> dict[str, int]:
    lat0 = np.median([p[0] for p in pos.values()]) if pos else 60.0
    kx = 111.32 * math.cos(math.radians(lat0))
    blocks = {}
    for node, (lat, lon) in pos.items():
        blocks[node] = (math.floor(lon * kx / block_km), math.floor(lat * 110.57 / block_km))
    uniq = sorted(set(blocks.values()))
    rng = np.random.default_rng(seed)
    perm = rng.permutation(len(uniq))
    fold_of_block = {b: int(perm[i] % k) for i, b in enumerate(uniq)}
    return {node: fold_of_block[b] for node, b in blocks.items()}


# ---------------------------------------------------------------------------
# Evaluation of one configuration
# ---------------------------------------------------------------------------


def prior_heights(obs: list[dict], node_ix: dict[str, int]) -> np.ndarray:
    h = np.full(len(node_ix), UNKNOWN_HEIGHT_PRIOR)
    for o in obs:
        for end in ("tx", "rx"):
            role = (o[f"{end}_role"] or "").lower()
            if role in ROLE_HEIGHT_PRIOR:
                h[node_ix[o[end]]] = ROLE_HEIGHT_PRIOR[role]
    return h


def make_data(obs, node_ix, block_ix, pl):
    n = len(obs)
    return cm.LinkData(
        y=np.array([0.0 if math.isnan(o["y"]) else o["y"] for o in obs]),
        cens=np.array([o["cens"] for o in obs], int),
        bound=np.array([0.0 if math.isnan(o["bound"]) else o["bound"] for o in obs]),
        block=np.array([block_ix[o["block"]] for o in obs], int),
        tx=np.array([node_ix[o["tx"]] for o in obs], int),
        rx=np.array([node_ix[o["rx"]] for o in obs], int),
        pl=pl if pl is not None else np.zeros(n),
        n_blocks=len(block_ix),
        n_nodes=len(node_ix),
    )


def priors_for(block_ix: dict, budget_sd_scale: float = 1.0) -> cm.Priors:
    names = sorted(block_ix, key=block_ix.get)
    return cm.Priors(
        c_mean=np.array([BLOCKS[b][0] for b in names]),
        c_sd=np.array([BLOCKS[b][1] * budget_sd_scale for b in names]),
    )


def evaluate_config(obs, grids_total, grids_excess, folds, args, features):
    node_ids = sorted({o["tx"] for o in obs} | {o["rx"] for o in obs})
    node_ix = {n: i for i, n in enumerate(node_ids)}
    block_names = sorted({o["block"] for o in obs})
    block_ix = {b: i for i, b in enumerate(block_names)}
    d = make_data(obs, node_ix, block_ix, None)
    h_prior = prior_heights(obs, node_ix)
    pri = priors_for(block_ix)

    # In-sample: node offsets and fitted heights (engine skill given the nodes).
    f_in, h_fit = cm.fit_with_heights(d, grids_total, h_prior, pri)
    d_in = d.subset(np.ones(d.y.size, bool), pl=cm.interp_pl(grids_total, h_fit[d.tx], h_fit[d.rx]))
    res_in = np.where(d.cens == 0, d.y - f_in.predict(d_in), np.nan)

    # Planner as configured: default budget, 3 m antennas, no fitting at all.
    pl_cfg = cm.interp_pl(grids_total, np.full(d.y.size, PLANNER_HEIGHT_M), np.full(d.y.size, PLANNER_HEIGHT_M))
    c_cfg = np.array([BLOCKS[b][0] for b in block_names])
    mu_cfg = c_cfg[d.block] - pl_cfg

    # Spatial-block CV: fit on training nodes, predict unseen nodes with prior
    # heights and no node offsets (what a planner faces at a new site).
    pl_prior = cm.interp_pl(grids_total, h_prior[d.tx], h_prior[d.rx])
    mu_cv = np.full(d.y.size, np.nan)
    mu_cv_scaled = np.full(d.y.size, np.nan)
    c_folds, scale_folds = [], []
    fold_tx = np.array([folds[node_ids[i]] for i in d.tx])
    fold_rx = np.array([folds[node_ids[i]] for i in d.rx])
    exc_prior = cm.interp_pl(grids_excess, h_prior[d.tx], h_prior[d.rx])
    # Fold pairs: test links whose endpoint folds are {k, j}; train on links that
    # touch neither fold, so no test node is seen in training.
    for k in range(args.folds):
        for j in range(k, args.folds):
            test = ((fold_tx == k) & (fold_rx == j)) | ((fold_tx == j) & (fold_rx == k))
            train = (fold_tx != k) & (fold_rx != k) & (fold_tx != j) & (fold_rx != j)
            if test.sum() == 0 or (train & (d.cens == 0)).sum() < 20:
                continue
            dtr = d.subset(train)
            f_tr, _ = cm.fit_with_heights(dtr, grids_total[train], h_prior, pri, rounds=2)
            c_folds.append(f_tr.c.tolist())
            mu_cv[test] = f_tr.c[d.block[test]] - pl_prior[test]
            # Correction: scale the excess loss (PL = FSPL + a * excess), fitted on train.
            dtr_x = dtr.subset(np.ones(dtr.y.size, bool))
            dtr_x.pl = pl_prior[train]
            dtr_x.x = exc_prior[train][:, None]
            f_sc = cm.fit(dtr_x, pri)
            scale_folds.append(1.0 - float(f_sc.beta[0]))
            mu_cv_scaled[test] = f_sc.c[d.block[test]] - pl_prior[test] + f_sc.beta[0] * exc_prior[test]

    observed = d.cens == 0
    out = {
        "n_links": int(d.y.size),
        "n_observed": int(observed.sum()),
        "n_non_detection": int((d.cens == -1).sum()),
        "n_saturated": int((d.cens == 1).sum()),
        "n_nodes": len(node_ids),
        "in_sample": {
            "c": dict(zip(block_names, f_in.c.tolist())),
            "sigma": f_in.sigma,
            "sigma_tx": f_in.sigma_u,
            "sigma_rx": f_in.sigma_w,
            "loglik": f_in.loglik,
            "residual": cm.summarize(res_in[observed]),
            "height_counts": {f"{h:g}": int(np.sum(h_fit == h)) for h in cm.CANDIDATE_HEIGHTS},
        },
        "as_configured": {},
        "cv": {},
        "cv_scaled": {},
        "cv_fold_c": c_folds,
        "cv_excess_scale": scale_folds,
    }
    for b, bi in block_ix.items():
        sel = observed & (d.block == bi)
        if b != "meshtastic_snr":
            out["as_configured"][b] = cm.summarize(d.y[sel] - mu_cfg[sel])
        tested = sel & np.isfinite(mu_cv)
        out["cv"][b] = cm.summarize(d.y[tested] - mu_cv[tested])
        out["cv_scaled"][b] = cm.summarize(d.y[tested] - mu_cv_scaled[tested])
        # Detection discrimination on held-out links: predicted margin over floor.
        nd = (d.cens == -1) & (d.block == bi) & np.isfinite(mu_cv)
        if nd.any() and tested.any():
            out["cv"][b]["auc_heard_vs_unheard"] = cm.auc(mu_cv[tested], mu_cv[nd] + (0.0))
            out["cv"][b]["n_unheard_tested"] = int(nd.sum())
    out["_arrays"] = {
        "res_in": res_in,
        "err_cv": np.where(observed, d.y - mu_cv, np.nan),
        "err_cfg": np.where(observed, d.y - mu_cfg, np.nan),
        "h_fit": h_fit,
        "node_ids": node_ids,
        "d": d,
    }
    return out


# ---------------------------------------------------------------------------
# Binning
# ---------------------------------------------------------------------------

BINS = {
    "distance_km": [0, 2, 5, 10, 20, 50, 1e9],
    "v_max": [-1e9, -0.78, 0, 1, 3, 6, 1e9],
    "delta_h_m": [0, 20, 50, 100, 200, 1e9],
    "terminal_clutter_m": [-1e9, 1, 3, 8, 15, 1e9],
}


def bin_table(values: np.ndarray, err: np.ndarray, edges) -> list[dict]:
    rows = []
    for lo, hi in zip(edges[:-1], edges[1:]):
        sel = (values >= lo) & (values < hi) & np.isfinite(err)
        s = cm.summarize(err[sel])
        s["lo"], s["hi"] = lo, hi
        rows.append(s)
    return rows


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--work-dir", type=Path, default=None)
    ap.add_argument("--links", type=Path, default=None)
    ap.add_argument("--region", choices=["norway", "world", "all"], default="all")
    ap.add_argument("--folds", type=int, default=5)
    ap.add_argument("--block-km", type=float, default=20.0)
    ap.add_argument("--seed", type=int, default=8695)
    ap.add_argument("--configs", default="", help="comma-separated substrings to select configs")
    ap.add_argument("--reference", default="rfcore_bullington|dom|none", help="config prefix used for v bins")
    args = ap.parse_args(argv)
    if hasattr(sys.stdout, "reconfigure"):
        sys.stdout.reconfigure(encoding="utf-8")
        sys.stderr.reconfigure(encoding="utf-8")

    work = args.work_dir or default_work_dir()
    refuse_user_data(work)
    links_path = args.links or work / "dataset" / "links.jsonl"
    links = list(read_jsonl(links_path))
    nodes_meta = {}
    if (work / "dataset" / "nodes.jsonl").exists():
        nodes_meta = {n["node"]: n for n in read_jsonl(work / "dataset" / "nodes.jsonl")}
    features = {}
    if (work / "path_features.jsonl").exists():
        features = {f["path_id"]: f for f in read_jsonl(work / "path_features.jsonl")}

    obs_all, drops = build_observations(links, nodes_meta)
    wanted = {o["link_id"] for o in obs_all}
    grids, path_of = load_predictions(work / "predictions", wanted)
    if not grids:
        raise SystemExit(f"no predictions for these links in {work / 'predictions'}")
    sel = [s for s in args.configs.split(",") if s]
    configs = sorted(k for k in grids if not sel or any(s in k for s in sel))

    regions = ["norway", "world"] if args.region == "all" else [args.region]
    results = {"links_file": str(links_path), "drops": drops, "regions": {}}
    for region in regions:
        robs_all = [o for o in obs_all if o["region"] == region]
        if not robs_all:
            continue
        # Use links that every selected config predicts, so engines are compared
        # on the same set.
        ids = {o["link_id"] for o in robs_all}
        rconfigs = []
        for c in configs:
            cov = len(ids & set(grids[c])) / len(ids)
            if cov < 0.8:
                print(f"[{region}] dropping {c}: covers {cov:.0%} of links", file=sys.stderr)
            else:
                rconfigs.append(c)
        common = set.intersection(*[set(grids[c]) for c in rconfigs]) if rconfigs else set()
        robs = [o for o in robs_all if o["link_id"] in common]
        for o in robs:
            o["path_id"] = path_of.get(o["link_id"])
        pos = node_positions(robs)
        folds = spatial_folds(pos, args.block_km, args.folds, args.seed)
        reg = {
            "n_candidates": len(robs_all),
            "n_common": len(robs),
            "time_sigma_db": time_variability(robs),
            "configs": {},
        }
        ref_cfg = next((c for c in rconfigs if c.startswith(args.reference)), rconfigs[0])
        for cfg in rconfigs:
            tot = np.stack([edge_fill(grids[cfg][o["link_id"]][0]) for o in robs])
            exc = np.stack([edge_fill(grids[cfg][o["link_id"]][1]) for o in robs])
            if np.isnan(tot).any():
                keep = ~np.isnan(tot).reshape(len(robs), -1).any(axis=1)
                print(f"[{region}] {cfg}: {np.sum(~keep)} links with missing predictions", file=sys.stderr)
            r = evaluate_config(robs, tot, exc, folds, args, features)
            reg["configs"][cfg] = r
            print(
                f"[{region}] {cfg:55s} n={r['n_observed']:4d} insample sd={r['in_sample']['sigma']:.2f} "
                f"cv={fmt_block(r['cv'])} cfg={fmt_block(r['as_configured'])}",
                file=sys.stderr,
            )
        # Bins for the reference config, in-sample and CV.
        ref = reg["configs"][ref_cfg]["_arrays"]
        vgrid = np.stack([edge_fill(grids[ref_cfg][o["link_id"]][2]) for o in robs])
        h = ref["h_fit"]
        d = ref["d"]
        vals = {
            "distance_km": np.array([o["distance_m"] / 1000 for o in robs]),
            "v_max": cm.interp_pl(vgrid, h[d.tx], h[d.rx]),
            "delta_h_m": np.array([features.get(o["path_id"], {}).get("delta_h_m", np.nan) for o in robs]),
            "terminal_clutter_m": np.array([terminal_clutter(features.get(o["path_id"])) for o in robs]),
        }
        reg["bins"] = {}
        for cfg in rconfigs:
            arr = reg["configs"][cfg]["_arrays"]
            reg["bins"][cfg] = {
                name: {
                    "in_sample": bin_table(v, arr["res_in"], BINS[name]),
                    "cv": bin_table(v, arr["err_cv"], BINS[name]),
                    "as_configured": bin_table(v, arr["err_cfg"], BINS[name]),
                }
                for name, v in vals.items()
            }
        reg["reference_config"] = ref_cfg
        results["regions"][region] = reg

    out_dir = work / "report"
    out_dir.mkdir(parents=True, exist_ok=True)
    clean = strip_arrays(results)
    tmp = out_dir / "results.json.partial"
    tmp.write_text(json.dumps(clean, indent=1, default=float), encoding="utf-8")
    tmp.replace(out_dir / "results.json")
    md = render_markdown(clean)
    tmp = out_dir / "report.md.partial"
    tmp.write_text(md, encoding="utf-8")
    tmp.replace(out_dir / "report.md")
    print(md)
    return 0


def terminal_clutter(f: dict | None) -> float:
    if not f:
        return float("nan")
    vals = [f.get(k) for k in ("a_clutter_m", "b_clutter_m", "clutter_a_m", "clutter_b_m") if f.get(k) is not None]
    return max(vals) if vals else float("nan")


def time_variability(obs: list[dict]) -> dict:
    s = np.array([o["sigma_t"] for o in obs if o["sigma_t"] is not None and o["n"] >= 10])
    if s.size == 0:
        return {"n": 0}
    return {"n": int(s.size), "median": float(np.median(s)), "p90": float(np.quantile(s, 0.9))}


def fmt_block(blocks: dict) -> str:
    parts = []
    for b, s in blocks.items():
        if s.get("n"):
            parts.append(f"{b.split('_')[0][:2]}:{s['bias']:+.1f}/{s['rmse']:.1f}")
    return " ".join(parts)


def strip_arrays(obj):
    if isinstance(obj, dict):
        return {k: strip_arrays(v) for k, v in obj.items() if k != "_arrays"}
    if isinstance(obj, list):
        return [strip_arrays(v) for v in obj]
    if isinstance(obj, float) and not math.isfinite(obj):
        return None
    return obj


def render_markdown(res: dict) -> str:
    lines = ["# RF calibration report", ""]
    lines.append(f"Dropped observations: {json.dumps(res['drops'], sort_keys=True)}")
    lines.append("")
    for region, reg in res["regions"].items():
        lines.append(f"## {region.title()}")
        lines.append("")
        lines.append(
            f"{reg['n_common']} links predicted by every engine (of {reg['n_candidates']} usable). "
            f"Within-link time variability (σ_t, links with n ≥ 10): {json.dumps(reg['time_sigma_db'])}."
        )
        lines.append("")
        lines.append(
            "Errors are measured minus predicted signal, in dB. Positive means the link is better than "
            "predicted, so the engine is pessimistic."
        )
        lines.append("")
        lines.append(
            "| Config | Obs | Unheard | In-sample σ | σ tx / rx | As configured bias / RMSE | "
            "CV bias / RMSE / sd | CV P10 | AUC heard | CV excess scale |"
        )
        lines.append("|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|")
        rows = []
        for cfg, r in reg["configs"].items():
            for b in r["cv"]:
                cv = r["cv"][b]
                ac = r["as_configured"].get(b, {})
                if not cv.get("n"):
                    continue
                scale = r["cv_excess_scale"]
                rows.append(
                    (
                        cv["sd"] if cv.get("sd") is not None else 99,
                        f"| `{cfg}` {b} | {r['n_observed']} | {r['n_non_detection']} | {r['in_sample']['sigma']:.2f} | "
                        f"{r['in_sample']['sigma_tx']:.1f} / {r['in_sample']['sigma_rx']:.1f} | "
                        + (f"{ac['bias']:+.1f} / {ac['rmse']:.1f}" if ac.get("n") else "—")
                        + f" | {cv['bias']:+.1f} / {cv['rmse']:.1f} / {cv['sd']:.1f} (n={cv['n']}) | {cv['p10']:+.1f} | "
                        + (f"{cv['auc_heard_vs_unheard']:.2f}" if cv.get("auc_heard_vs_unheard") is not None else "—")
                        + " | "
                        + (f"{np.mean(scale):.2f}" if scale else "—")
                        + " |",
                    )
                )
        lines += [r for _, r in sorted(rows)]
        lines.append("")
        ref = reg["reference_config"]
        for name in ("distance_km", "v_max"):
            lines.append(f"### Residuals by {name} (`{ref}`, in-sample with node offsets / CV)")
            lines.append("")
            lines.append("| Bin | n | In-sample bias | In-sample sd | CV n | CV bias | CV sd |")
            lines.append("|---|---:|---:|---:|---:|---:|---:|")
            b = reg["bins"][ref][name]
            for ins, cv in zip(b["in_sample"], b["cv"]):
                lines.append(
                    f"| {fmt_edge(ins['lo'])}–{fmt_edge(ins['hi'])} | {ins['n']} | {fmtv(ins.get('bias'))} | "
                    f"{fmtv(ins.get('sd'))} | {cv['n']} | {fmtv(cv.get('bias'))} | {fmtv(cv.get('sd'))} |"
                )
            lines.append("")
    return "\n".join(lines)


def fmt_edge(x):
    if x <= -1e8:
        return "−∞"
    if x >= 1e8:
        return "∞"
    return f"{x:g}"


def fmtv(x):
    return "—" if x is None or (isinstance(x, float) and not math.isfinite(x)) else f"{x:+.1f}"


if __name__ == "__main__":
    raise SystemExit(main())
