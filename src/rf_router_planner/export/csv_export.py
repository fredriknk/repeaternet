from __future__ import annotations

import csv
from pathlib import Path

from rf_router_planner.optimization.optimizer import OptimizationResult


def export_route_csv(result: OptimizationResult, path: str | Path) -> None:
    fields = [
        "hop",
        "from",
        "to",
        "distance_km",
        "fspl_db",
        "diffraction_loss_db",
        "total_path_loss_db",
        "rx_power_dbm",
        "sensitivity_dbm",
        "raw_margin_db",
        "fade_adjusted_margin_db",
        "reverse_margin_db",
        "worst_margin_db",
        "minimum_fresnel_clearance_percent",
        "los_clear",
        "valid",
    ]
    with Path(path).open("w", newline="", encoding="utf-8") as handle:
        writer = csv.DictWriter(handle, fieldnames=fields)
        writer.writeheader()
        for number, link in enumerate(result.links, 1):
            writer.writerow(
                {
                    "hop": number,
                    "from": link.source_id,
                    "to": link.target_id,
                    "distance_km": f"{link.distance_m / 1000:.3f}",
                    "fspl_db": f"{link.forward.fspl_db:.2f}",
                    "diffraction_loss_db": f"{link.diffraction_loss_db:.2f}",
                    "total_path_loss_db": f"{link.forward.total_path_loss_db:.2f}",
                    "rx_power_dbm": f"{link.forward.received_power_dbm:.2f}",
                    "sensitivity_dbm": f"{link.forward.sensitivity_dbm:.2f}",
                    "raw_margin_db": f"{link.forward.raw_margin_db:.2f}",
                    "fade_adjusted_margin_db": f"{link.forward.usable_margin_db:.2f}",
                    "reverse_margin_db": f"{link.reverse.usable_margin_db:.2f}",
                    "worst_margin_db": f"{link.worst_margin_db:.2f}",
                    "minimum_fresnel_clearance_percent": f"{100 * link.minimum_fresnel_clearance_ratio:.1f}",
                    "los_clear": link.los_clear,
                    "valid": link.valid,
                }
            )
