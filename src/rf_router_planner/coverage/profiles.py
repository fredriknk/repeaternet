"""Explicit coverage-profile sampling limits and diagnostic reasons."""

from __future__ import annotations

import math


def bounded_profile_step(
    distance_m: float,
    requested_step_m: float,
    terrain_resolution_m: float,
    maximum_samples: int,
) -> tuple[float, int, str | None]:
    """Resolve the actual terrain sampling step without silently exceeding the cap."""
    distance = float(distance_m)
    requested_step = float(requested_step_m)
    resolution = float(terrain_resolution_m)
    if not math.isfinite(distance) or distance < 0:
        raise ValueError("Link distance must be finite and nonnegative")
    if not math.isfinite(requested_step) or requested_step <= 0:
        raise ValueError("Requested profile step must be finite and positive")
    if not math.isfinite(resolution) or resolution <= 0:
        raise ValueError("Terrain resolution must be finite and positive")
    if maximum_samples < 3:
        raise ValueError("Maximum profile samples must be at least 3")
    step = max(requested_step, resolution)
    required = max(3, math.ceil(distance / step) + 1)
    if required <= maximum_samples:
        return step, required, None
    return step, required, (
        f"This link requires {required:,} terrain-profile samples at {step:g} m spacing; "
        f"the configured maximum is {maximum_samples:,}. Increase the profile step "
        "explicitly or raise the profile sample limit."
    )
