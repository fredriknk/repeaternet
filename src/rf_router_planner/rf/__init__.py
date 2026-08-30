from .curvature import earth_bulge_m
from .fresnel import first_fresnel_radius_m
from .link import free_space_path_loss_db

__all__ = ["LinkEvaluator", "earth_bulge_m", "first_fresnel_radius_m", "free_space_path_loss_db"]


def __getattr__(name: str):  # type: ignore[no-untyped-def]
    if name == "LinkEvaluator":
        from .propagation import LinkEvaluator

        return LinkEvaluator
    raise AttributeError(name)
