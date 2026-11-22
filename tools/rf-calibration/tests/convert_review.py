"""Convert the private review inputs into calibration-format smoke fixtures."""

import argparse
import json
import os
from pathlib import Path
import sys

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from reference_engines import DEFAULT_REVIEW, atomic_output, safe_path


def write_jsonl(path, rows):
    with atomic_output(safe_path(path)) as out:
        for row in rows:
            out.write((json.dumps(row, allow_nan=False) + "\n").encode())


def convert(review, work):
    review, work = safe_path(review), safe_path(work)
    for path in (work / "profiles.jsonl", work / "dataset" / "links.jsonl"):
        if not safe_path(path).is_relative_to(work) or safe_path(path).is_relative_to(review):
            raise ValueError("Smoke fixture outputs must be in a separate work directory")
    links = json.loads(safe_path(review / "drammenmesh_links.json").read_text(encoding="utf-8"))
    sites = json.loads(safe_path(review / "drammenmesh_sites.json").read_text(encoding="utf-8"))["sites"]
    sites = {site["id"]: (f"{i:016x}", site) for i, site in enumerate(sites)}
    profiles, directed_links = [], []
    for i, link in enumerate(links):
        link_id = f"review:{i}"
        p = link["profile"]
        dtm, dom = p["dtm_elevation_m"], p["surface_elevation_m"]
        distance = p["distances_m"][-1]
        ends = []
        for key, sample in (("source_id", 0), ("target_id", -1)):
            node, site = sites[link[key]]
            ends.append(dict(node=node, lat=site["latitude"], lon=site["longitude"],
                             x=site["x"], y=site["y"], ground_m=dtm[sample], surface_m=dom[sample]))
        a, b = sorted(ends, key=lambda end: end["node"])
        path_id = f"{a['node']}~{b['node']}"
        if a != ends[0]:
            dtm, dom = dtm[::-1], dom[::-1]
        profiles.append(dict(schema=1, path_id=path_id, link_ids=[link_id], terrain="kartverket",
                             crs_epsg=25833, resolution_m=10, step_m=distance / (len(dtm) - 1),
                             distance_m=distance, a=a, b=b, dtm=dtm, dom=dom, missing_fraction=0))
        tx, rx = ({key: end[key] for key in ("node", "lat", "lon")} |
                  dict(pos_source="review", pos_precision_m=None, altitude_m=None, role=None, flags=[])
                  for end in ends)
        radio = dict(freq_mhz=869.5, bw_khz=None, sf=None, cr=None, tx_power_dbm=None,
                     modem_preset=None, basis=["assumed"])
        directed_links.append(dict(schema=1, link_id=link_id, source="review", evidence="corescope_last_hop",
                                   region="norway", tx=tx, rx=rx, distance_m=distance, radio=radio, n=1,
                                   first_ms=None, last_ms=None, rssi_dbm=None, snr_db=None, signal_dbm=None,
                                   noise_dbm=None, detection=None, flags=[]))
    write_jsonl(work / "profiles.jsonl", profiles)
    write_jsonl(work / "dataset" / "links.jsonl", directed_links)
    return profiles, directed_links


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--work-dir", type=Path, required=True)
    parser.add_argument("--review-dir", type=Path, default=os.environ.get("RF_REVIEW_DIR") or DEFAULT_REVIEW)
    args = parser.parse_args()
    profiles, _ = convert(args.review_dir, args.work_dir)
    print(f"Wrote {len(profiles)} smoke profiles to {args.work_dir}")
