from __future__ import annotations

import numpy as np

from rf_router_planner.models.link import DirectionResult, LinkResult
from rf_router_planner.models.settings import (
    AntennaSettings,
    DiffractionModel,
    RFSettings,
    ValidationMode,
)
from rf_router_planner.models.site import Site, SiteKind
from rf_router_planner.terrain.raster import TerrainSource
from rf_router_planner.terrain.sampling import TerrainProfile, sample_profile

from .antennas import ConstantGain, ElevationPattern, elevation_angle_deg
from .diffraction import bullington_loss_db, deygout_loss_db
from .link import free_space_path_loss_db, received_power_dbm


class LinkEvaluator:
    def __init__(self, terrain: TerrainSource, settings: RFSettings) -> None:
        self.terrain = terrain
        self.settings = settings
        self._patterns: dict[str, ConstantGain | ElevationPattern] = {}

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
        self, source: Site, target: Site, sample_step_m: float | None = None
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
        diffraction_function = bullington_loss_db if model == DiffractionModel.BULLINGTON else deygout_loss_db
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

        forward = self._direction(source, target, profile, diffraction.loss_db, clutter_loss)
        reverse = self._direction(target, source, profile, diffraction.loss_db, clutter_loss)
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
            profile,
        )

    def _direction(
        self,
        source: Site,
        target: Site,
        profile: TerrainProfile,
        diffraction_loss_db: float,
        clutter_loss_db: float,
    ) -> DirectionResult:
        distance = source.distance_to(target)
        departure = elevation_angle_deg(
            distance, source.antenna_absolute_elevation_m, target.antenna_absolute_elevation_m
        )
        arrival = elevation_angle_deg(
            distance, target.antenna_absolute_elevation_m, source.antenna_absolute_elevation_m
        )
        tx = self.antenna_settings(source)
        rx = self.antenna_settings(target)
        tx_gain = self.pattern(tx).gain_at(departure)
        rx_gain = self.pattern(rx).gain_at(arrival)
        fspl = free_space_path_loss_db(distance, self.settings.frequency_mhz)
        total_path_loss = fspl + diffraction_loss_db + clutter_loss_db
        power = received_power_dbm(
            self.settings.tx_power_dbm,
            tx_gain,
            tx.feed_loss_db,
            total_path_loss,
            rx_gain,
            rx.feed_loss_db,
            self.settings.miscellaneous_loss_db,
        )
        sensitivity = self.settings.effective_sensitivity_dbm
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
            self.settings.tx_power_dbm,
            tx.feed_loss_db,
            rx.feed_loss_db,
            self.settings.miscellaneous_loss_db,
        )
