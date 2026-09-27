from __future__ import annotations

import hashlib
import json
from collections.abc import Hashable
from dataclasses import asdict
from pathlib import Path
from typing import TYPE_CHECKING

import numpy as np

from rf_router_planner.models.coverage import RadioBudget
from rf_router_planner.models.link import DirectionResult, LinkResult
from rf_router_planner.models.settings import (
    AntennaSettings,
    DiffractionModel,
    RFSettings,
    ValidationMode,
)
from rf_router_planner.models.site import Site, SiteKind
from rf_router_planner.terrain.raster import TerrainSource

if TYPE_CHECKING:
    from rf_router_planner.optimization.cache import LinkMetricsCache
from rf_router_planner.terrain.sampling import TerrainProfile, sample_profile

from .antennas import ConstantGain, ElevationPattern, elevation_angle_deg
from .diffraction import bullington_loss_db, deygout_loss_db
from .link import free_space_path_loss_db, received_power_dbm

_CACHE_SCHEMA = "rf-metrics-v2"


def _terrain_fingerprint(terrain: TerrainSource) -> tuple[Hashable, ...]:
    common = (terrain.crs, terrain.resolution_m, terrain.bounds, terrain.has_surface)
    rasters = [*getattr(terrain, "_dtm", ()), *getattr(terrain, "_dom", ())]
    if rasters:
        metadata = []
        for dataset in rasters:
            path = Path(dataset.name).resolve()
            stat = path.stat()
            metadata.append(
                (
                    str(path),
                    stat.st_size,
                    stat.st_mtime_ns,
                    stat.st_ctime_ns,
                    str(dataset.crs),
                    tuple(dataset.transform),
                    dataset.width,
                    dataset.height,
                    tuple(dataset.dtypes),
                    dataset.nodata,
                )
            )
        return (*common, tuple(metadata))

    arrays = [getattr(terrain, "dtm", None), getattr(terrain, "dom", None)]
    if any(array is not None for array in arrays):
        digests: list[tuple[Hashable, ...] | None] = []
        for array in arrays:
            if array is None:
                digests.append(None)
                continue
            contiguous = np.ascontiguousarray(array)
            digests.append(
                (contiguous.shape, contiguous.dtype.str, hashlib.sha256(contiguous).hexdigest())
            )
        return (*common, tuple(digests))
    return (*common, id(terrain))


def _settings_fingerprint(settings: RFSettings) -> str:
    values = asdict(settings)
    pattern_hashes = []
    for antenna in (settings.endpoint_a, settings.endpoint_b, settings.router):
        path = antenna.pattern_csv
        if path:
            try:
                digest = hashlib.sha256(Path(path).read_bytes()).hexdigest()
            except OSError:
                digest = "missing"
            pattern_hashes.append((path, digest))
    payload = json.dumps(values, sort_keys=True, default=str)
    return hashlib.sha256((payload + repr(pattern_hashes)).encode()).hexdigest()


def _site_fingerprint(site: Site) -> tuple[Hashable, ...]:
    return (
        site.id,
        site.x,
        site.y,
        site.kind.value,
        site.ground_elevation_m,
        site.surface_elevation_m,
        site.antenna_height_m,
        site.height_reference.value,
        site.terrain_slope,
        site.site_quality,
        site.locked,
        site.origin.value,
        site.required,
        site.enabled,
    )


def _radio_budget_fingerprint(budget: RadioBudget | None) -> tuple[Hashable, ...] | None:
    if budget is None:
        return None
    antenna = budget.antenna
    pattern_hash = None
    if antenna.pattern_csv:
        try:
            pattern_hash = hashlib.sha256(Path(antenna.pattern_csv).read_bytes()).hexdigest()
        except OSError:
            pattern_hash = "missing"
    return (
        antenna.gain_dbi,
        antenna.feed_loss_db,
        antenna.height_agl_m,
        antenna.pattern_csv,
        pattern_hash,
        budget.tx_power_dbm,
        budget.sensitivity_dbm,
        budget.miscellaneous_loss_db,
    )


class LinkEvaluator:
    def __init__(
        self,
        terrain: TerrainSource,
        settings: RFSettings,
        cache: LinkMetricsCache | None = None,
        cache_namespace: str = "default",
    ) -> None:
        self.terrain = terrain
        self.settings = settings
        self.cache = cache
        self.cache_namespace = cache_namespace
        self._terrain_fingerprint = _terrain_fingerprint(terrain) if cache else ()
        self._settings_fingerprint = _settings_fingerprint(settings) if cache else ""
        self._patterns: dict[str, ConstantGain | ElevationPattern] = {}

    @property
    def cache_stats(self) -> dict[str, int] | None:
        return self.cache.stats(self.cache_namespace) if self.cache is not None else None

    def antenna_settings(self, site: Site) -> AntennaSettings:
        if site.kind in {SiteKind.ENDPOINT_A, SiteKind.CLIENT}:
            return self.settings.endpoint_a
        if site.kind == SiteKind.ENDPOINT_B:
            return self.settings.endpoint_b
        return self.settings.router

    def pattern(self, antenna: AntennaSettings) -> ConstantGain | ElevationPattern:
        key = antenna.pattern_csv or f"constant:{antenna.gain_dbi}"
        if key not in self._patterns:
            self._patterns[key] = (
                ElevationPattern.from_csv(antenna.pattern_csv)
                if antenna.pattern_csv
                else ConstantGain(antenna.gain_dbi)
            )
        return self._patterns[key]

    def optimistic_margin_db(self, source: Site, target: Site) -> float:
        distance = source.distance_to(target)
        tx = self.antenna_settings(source)
        rx = self.antenna_settings(target)
        power = received_power_dbm(
            self.settings.tx_power_dbm,
            float(np.max(self.pattern(tx).gain_dbi)),
            tx.feed_loss_db,
            free_space_path_loss_db(distance, self.settings.frequency_mhz),
            float(np.max(self.pattern(rx).gain_dbi)),
            rx.feed_loss_db,
            self.settings.miscellaneous_loss_db,
        )
        return power - self.settings.effective_sensitivity_dbm - self.settings.fade_margin_db

    def evaluate(
        self,
        source: Site,
        target: Site,
        sample_step_m: float | None = None,
        *,
        include_profile: bool = True,
        source_radio: RadioBudget | None = None,
        target_radio: RadioBudget | None = None,
    ) -> LinkResult:
        if self.cache is not None and not include_profile:
            return self.cache.get_or_compute(
                self.cache_namespace,
                self.cache_key(
                    source,
                    target,
                    sample_step_m,
                    source_radio=source_radio,
                    target_radio=target_radio,
                ),
                lambda: self._evaluate_uncached(
                    source, target, sample_step_m, False, source_radio, target_radio
                ),
            )
        return self._evaluate_uncached(
            source, target, sample_step_m, include_profile, source_radio, target_radio
        )

    def cache_key(
        self,
        source: Site,
        target: Site,
        sample_step_m: float | None,
        *,
        source_radio: RadioBudget | None = None,
        target_radio: RadioBudget | None = None,
    ) -> tuple[Hashable, ...]:
        return (
            _CACHE_SCHEMA,
            self._terrain_fingerprint,
            self._settings_fingerprint,
            _site_fingerprint(source),
            _site_fingerprint(target),
            sample_step_m,
            _radio_budget_fingerprint(source_radio),
            _radio_budget_fingerprint(target_radio),
        )

    def get_cached_metrics(
        self, source: Site, target: Site, sample_step_m: float
    ) -> LinkResult | None:
        if self.cache is None:
            return None
        return self.cache.get(self.cache_namespace, self.cache_key(source, target, sample_step_m))

    def remember_metrics(
        self, source: Site, target: Site, sample_step_m: float, link: LinkResult
    ) -> None:
        if self.cache is not None:
            self.cache.put(
                self.cache_namespace,
                self.cache_key(source, target, sample_step_m),
                link,
            )

    def _evaluate_uncached(
        self,
        source: Site,
        target: Site,
        sample_step_m: float | None,
        include_profile: bool,
        source_radio: RadioBudget | None = None,
        target_radio: RadioBudget | None = None,
    ) -> LinkResult:
        profile = sample_profile(
            self.terrain,
            source,
            target,
            self.settings.frequency_mhz,
            self.settings.k_factor,
            sample_step_m,
        )
        interior = slice(1, -1)
        min_clearance_index = 1 + int(np.argmin(profile.clearance_m[interior]))
        min_ratio_index = 1 + int(np.argmin(profile.fresnel_clearance_ratio[interior]))
        los_clear = bool(np.all(profile.clearance_m[interior] >= 0.0))
        min_ratio = float(profile.fresnel_clearance_ratio[min_ratio_index])
        fresnel_clear = min_ratio >= self.settings.required_fresnel_clearance
        model = DiffractionModel(self.settings.diffraction_model)
        diffraction_function = (
            bullington_loss_db if model == DiffractionModel.BULLINGTON else deygout_loss_db
        )
        diffraction = diffraction_function(
            profile.distances_m,
            profile.effective_obstruction_m,
            profile.los_elevation_m,
            self.settings.frequency_mhz,
        )
        clutter_loss = 0.0
        if self.settings.clutter_loss_enabled and profile.surface_available:
            excess = np.maximum(0.0, profile.surface_elevation_m - profile.dtm_elevation_m)
            clutter_loss = float(np.max(excess)) * self.settings.clutter_loss_db_per_m

        forward = self._direction(
            source, target, profile, diffraction.loss_db, clutter_loss, source_radio, target_radio
        )
        reverse = self._direction(
            target, source, profile, diffraction.loss_db, clutter_loss, target_radio, source_radio
        )
        if self.settings.validation_mode == ValidationMode.STRICT_LOS:
            valid = forward.valid and reverse.valid and los_clear and fresnel_clear
        else:
            valid = forward.valid and reverse.valid
        return LinkResult(
            source.id,
            target.id,
            source.distance_to(target),
            forward,
            reverse,
            valid,
            los_clear,
            fresnel_clear,
            float(profile.clearance_m[min_clearance_index]),
            min_ratio,
            float(profile.distances_m[min_ratio_index]),
            float(np.max(profile.fresnel_radius_m)),
            diffraction.loss_db,
            diffraction.obstacles,
            profile if include_profile else None,
        )

    def _direction(
        self,
        source: Site,
        target: Site,
        profile: TerrainProfile,
        diffraction_loss_db: float,
        clutter_loss_db: float,
        source_radio: RadioBudget | None = None,
        target_radio: RadioBudget | None = None,
    ) -> DirectionResult:
        distance = source.distance_to(target)
        departure = elevation_angle_deg(
            distance, source.antenna_absolute_elevation_m, target.antenna_absolute_elevation_m
        )
        arrival = elevation_angle_deg(
            distance, target.antenna_absolute_elevation_m, source.antenna_absolute_elevation_m
        )
        tx = source_radio.antenna if source_radio else self.antenna_settings(source)
        rx = target_radio.antenna if target_radio else self.antenna_settings(target)
        tx_gain = self.pattern(tx).gain_at(departure)
        rx_gain = self.pattern(rx).gain_at(arrival)
        fspl = free_space_path_loss_db(distance, self.settings.frequency_mhz)
        total_path_loss = fspl + diffraction_loss_db + clutter_loss_db
        power = received_power_dbm(
            source_radio.tx_power_dbm if source_radio else self.settings.tx_power_dbm,
            tx_gain,
            tx.feed_loss_db,
            total_path_loss,
            rx_gain,
            rx.feed_loss_db,
            source_radio.miscellaneous_loss_db if source_radio else self.settings.miscellaneous_loss_db,
        )
        sensitivity = (
            target_radio.sensitivity_dbm
            if target_radio
            else self.settings.effective_sensitivity_dbm
        )
        raw_margin = power - sensitivity
        usable_margin = raw_margin - self.settings.fade_margin_db
        return DirectionResult(
            source.id,
            target.id,
            distance,
            fspl,
            diffraction_loss_db,
            clutter_loss_db,
            total_path_loss,
            tx_gain,
            rx_gain,
            power,
            sensitivity,
            raw_margin,
            usable_margin,
            departure,
            arrival,
            usable_margin >= 0.0,
            source_radio.tx_power_dbm if source_radio else self.settings.tx_power_dbm,
            tx.feed_loss_db,
            rx.feed_loss_db,
            source_radio.miscellaneous_loss_db if source_radio else self.settings.miscellaneous_loss_db,
        )
