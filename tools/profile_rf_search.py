"""Bounded, single-process memory diagnosis for synthetic RF searches.

Run each case in a fresh process. Optional profile omission is an experiment,
not a production fix: it removes chart payloads from progressively checked
links without changing their computed RF metrics.
"""

from __future__ import annotations

import argparse
import json
import sys
import time
import weakref
from dataclasses import fields
from unittest.mock import patch

import numpy as np
from benchmark_rf_search import peak_working_set_mib, run_case

from rf_router_planner.optimization.optimizer import RouteOptimizer
from rf_router_planner.rf.propagation import LinkEvaluator


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--candidates", type=int, default=800)
    parser.add_argument("--distance-km", type=float, default=100)
    parser.add_argument("--topology", choices=("direct", "mesh", "long"), default="direct")
    parser.add_argument("--seconds", type=float, default=45)
    parser.add_argument("--profile-limit-mib", type=float, default=384)
    parser.add_argument("--omit-search-profiles", action="store_true")
    parser.add_argument(
        "--reuse-low-hop",
        action="store_true",
        help="Experimentally reuse the low-hop result on fallback",
    )
    parser.add_argument(
        "--optional-anchors",
        action="store_true",
        help="Make seeded chain sites optional to exercise ordinary route search",
    )
    args = parser.parse_args()
    if args.candidates < 4 or min(args.distance_km, args.seconds, args.profile_limit_mib) <= 0:
        parser.error("use at least four candidates and positive distance/time/memory limits")

    started = time.perf_counter()
    phase = "initialization"
    last_report = started
    live_bytes = 0
    peak_bytes = 0
    evaluations = 0
    solution_metrics: dict = {}
    low_hop_calls: list[dict] = []
    low_hop_cache: dict = {}
    stopped: str | None = None
    live: dict[int, weakref.ReferenceType] = {}
    original_evaluate = LinkEvaluator.evaluate
    original_optimize = RouteOptimizer.optimize
    original_validate = RouteOptimizer._progressively_validate_link
    original_low_hop = RouteOptimizer._find_low_hop_route

    def evaluate(self, *pos, **kw):
        nonlocal live_bytes, peak_bytes, evaluations
        link = original_evaluate(self, *pos, **kw)
        evaluations += 1
        profile = link.profile
        if profile is not None:
            size = sum(
                array.nbytes
                for field in fields(profile)
                if isinstance(array := getattr(profile, field.name), np.ndarray)
            )
            key = id(profile.distances_m)

            def released(_ref):
                nonlocal live_bytes
                live_bytes -= size
                live.pop(key, None)

            live[key] = weakref.ref(profile.distances_m, released)
            live_bytes += size
            peak_bytes = max(peak_bytes, live_bytes)
        return link

    def report(stage, done, total):
        row = {
            "event": "phase",
            "seconds": round(time.perf_counter() - started, 3),
            "phase": stage,
            "done": done,
            "total": total,
            "evaluations": evaluations,
            "live_profiles": len(live),
            "live_profile_arrays_mib": round(live_bytes / 2**20, 2),
            "peak_rss_mib": peak_working_set_mib(),
        }
        frame = sys._getframe(1)
        try:
            while frame is not None:
                if frame.f_code.co_name == "_find_low_hop_route":
                    for name in ("from_a", "to_b", "frontier_pairs", "seen_pairs", "cross_links"):
                        container = frame.f_locals.get(name)
                        if container is not None:
                            row[name] = len(container)
                            if name in {"frontier_pairs", "seen_pairs"}:
                                row[name + "_mib"] = round(
                                    (sys.getsizeof(container) + sum(map(sys.getsizeof, container)))
                                    / 2**20,
                                    2,
                                )
                    break
                frame = frame.f_back
        finally:
            del frame
        print(json.dumps(row), flush=True)

    def cancelled():
        nonlocal stopped
        if stopped is None:
            if time.perf_counter() - started >= args.seconds:
                stopped = "time limit"
            elif live_bytes >= args.profile_limit_mib * 2**20:
                stopped = "retained-profile limit"
        return stopped is not None

    def optimize(self, *pos, **kw):
        original_progress = kw.get("progress")
        if args.optional_anchors:
            for site in kw.get("candidates", []):
                site.required = False
            kw["required_routers"] = None

        def progress(stage, done, total):
            nonlocal phase, last_report
            now = time.perf_counter()
            if stage != phase or now - last_report >= 5 or done == total:
                report(stage, done, total)
                last_report = now
            phase = stage
            if original_progress:
                original_progress(stage, done, total)

        kw.update(progress=progress, cancelled=cancelled)
        result = original_optimize(self, *pos, **kw)
        solution_metrics.update(
            route_ids=[site.id for site in result.route],
            route_margins_db=[link.worst_margin_db for link in result.links],
            alternatives=[solution.router_ids for solution in result.alternatives],
        )
        return result

    def validate(self, *pos, **kw):
        link = original_validate(self, *pos, **kw)
        if args.omit_search_profiles and link is not None:
            link.profile = None
        return link

    def low_hop(self, sites, notify, is_cancelled):
        # One optimize call per process; geometry/settings stay fixed in this
        # diagnostic scenario. This is not a general-purpose cache key.
        key = (id(self), tuple(site.id for site in sites))
        began = time.perf_counter()
        cached = args.reuse_low_hop and key in low_hop_cache
        value = (
            low_hop_cache[key] if cached else original_low_hop(self, sites, notify, is_cancelled)
        )
        if args.reuse_low_hop and not is_cancelled():
            low_hop_cache[key] = value
        low_hop_calls.append(
            {
                "reused": cached,
                "seconds": round(time.perf_counter() - began, 3),
                "route_ids": [site.id for site in value[0]] if value else [],
            }
        )
        return value

    print(json.dumps({"event": "configuration", **vars(args)}), flush=True)
    with (
        patch.object(LinkEvaluator, "evaluate", evaluate),
        patch.object(RouteOptimizer, "optimize", optimize),
        patch.object(RouteOptimizer, "_progressively_validate_link", validate),
        patch.object(RouteOptimizer, "_find_low_hop_route", low_hop),
    ):
        result = run_case(args.candidates, 20260926, args.topology, args.distance_km)
    print(
        json.dumps(
            {
                "event": "result",
                "stopped": stopped,
                "last_phase": phase,
                "peak_profile_arrays_mib": round(peak_bytes / 2**20, 2),
                "evaluations": evaluations,
                "low_hop_calls": low_hop_calls,
                **solution_metrics,
                **result,
            }
        ),
        flush=True,
    )


if __name__ == "__main__":
    main()
