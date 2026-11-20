//! Two-way terrain-aware link evaluation and optimistic routing margins.

use crate::{
    bullington_loss_db, elevation_angle_deg, free_space_path_loss_db, received_power_dbm,
    AntennaSettings, DirectionResult, LinkResult, RadioBudget, RfError, RfSettings, Site,
    TerrainSource, ValidationMode,
};

use crate::profile::sample_path;

/// Link-specific sampling options and radio overrides.
#[derive(Clone, Copy, Debug)]
pub struct EvaluationOptions<'a> {
    pub sample_step_m: Option<f64>,
    /// Retain sampled terrain and clearance arrays in the result.
    pub include_profile: bool,
    pub source_radio: Option<&'a RadioBudget>,
    pub target_radio: Option<&'a RadioBudget>,
}

impl Default for EvaluationOptions<'_> {
    fn default() -> Self {
        Self {
            sample_step_m: None,
            include_profile: true,
            source_radio: None,
            target_radio: None,
        }
    }
}

/// Evaluates links against a shared terrain source and RF configuration.
pub struct LinkEvaluator<'a, T: TerrainSource + ?Sized> {
    pub terrain: &'a T,
    pub settings: &'a RfSettings,
}

impl<'a, T: TerrainSource + ?Sized> LinkEvaluator<'a, T> {
    pub fn new(terrain: &'a T, settings: &'a RfSettings) -> Self {
        Self { terrain, settings }
    }

    /// Free-space margin using peak antenna gains and the shared radio settings.
    /// Used for routing screens before sampling terrain.
    pub fn optimistic_margin_db(&self, source: &Site, target: &Site) -> Result<f64, RfError> {
        let tx = self.settings.antenna_settings(source.kind);
        let rx = self.settings.antenna_settings(target.kind);
        let fspl =
            free_space_path_loss_db(source.distance_to(target), self.settings.frequency_mhz)?;
        let power = received_power_dbm(
            self.settings.tx_power_dbm,
            tx.pattern.peak_gain_dbi(),
            tx.feed_loss_db,
            fspl,
            rx.pattern.peak_gain_dbi(),
            rx.feed_loss_db,
            self.settings.miscellaneous_loss_db,
        );
        Ok(power - self.settings.effective_sensitivity_dbm()? - self.settings.fade_margin_db)
    }

    /// Evaluate both directions using one sampled path and a shared propagation loss.
    pub fn evaluate(
        &self,
        source: &Site,
        target: &Site,
        options: EvaluationOptions<'_>,
    ) -> Result<LinkResult, RfError> {
        let samples = sample_path(
            self.terrain,
            source,
            target,
            self.settings.frequency_mhz,
            self.settings.k_factor,
            options.sample_step_m,
        )?;
        let clutter_loss_db = if self.settings.clutter_loss_enabled {
            samples
                .clutter_excess()
                .map_or(0.0, |excess| excess * self.settings.clutter_loss_db_per_m)
        } else {
            0.0
        };
        let (distance_m, metrics, diffraction, profile) = if options.include_profile {
            let profile = samples.into_profile()?;
            let diffraction = bullington_loss_db(
                &profile.distances_m,
                &profile.effective_obstruction_m,
                &profile.los_elevation_m,
                self.settings.frequency_mhz,
            )?;
            (
                *profile.distances_m.last().unwrap(),
                profile.clearance_metrics(),
                diffraction,
                Some(profile),
            )
        } else {
            let scalar = samples.into_scalar()?;
            let diffraction = bullington_loss_db(
                &scalar.distances,
                &scalar.obstructions,
                &scalar.los,
                self.settings.frequency_mhz,
            )?;
            (
                *scalar.distances.last().unwrap(),
                scalar.metrics,
                diffraction,
                None,
            )
        };
        let los_clear = metrics.clearance >= 0.0;
        let fresnel_clear = metrics.ratio >= self.settings.required_fresnel_clearance;
        let path = LinkPath {
            distance_m,
            fspl_db: free_space_path_loss_db(distance_m, self.settings.frequency_mhz)?,
            diffraction_loss_db: diffraction.loss_db,
            clutter_loss_db,
        };
        let departure = elevation_angle_deg(
            distance_m,
            source.antenna_absolute_elevation_m(),
            target.antenna_absolute_elevation_m(),
        )?;
        let source_radio = self.site_radio(source, options.source_radio)?;
        let target_radio = self.site_radio(target, options.target_radio)?;
        let forward = path.direction(
            &source_radio,
            &target_radio,
            departure,
            self.settings.fade_margin_db,
        );
        let reverse = path.direction(
            &target_radio,
            &source_radio,
            -departure,
            self.settings.fade_margin_db,
        );
        let valid = forward.valid
            && reverse.valid
            && (self.settings.validation_mode == ValidationMode::Propagation
                || (los_clear && fresnel_clear));
        Ok(LinkResult {
            source_id: source.id.clone(),
            target_id: target.id.clone(),
            distance_m,
            forward,
            reverse,
            valid,
            los_clear,
            fresnel_clear,
            minimum_clearance_m: metrics.clearance,
            minimum_fresnel_clearance_ratio: metrics.ratio,
            minimum_clearance_distance_m: metrics.ratio_distance,
            maximum_fresnel_radius_m: metrics.maximum_radius,
            diffraction_loss_db: diffraction.loss_db,
            dominant_obstacles: diffraction.obstacles,
            profile,
        })
    }

    fn site_radio<'b>(
        &'b self,
        site: &'b Site,
        overrides: Option<&'b RadioBudget>,
    ) -> Result<SiteRadio<'b>, RfError> {
        Ok(match overrides {
            Some(radio) => SiteRadio {
                id: &site.id,
                antenna: &radio.antenna,
                tx_power_dbm: radio.tx_power_dbm,
                sensitivity_dbm: radio.sensitivity_dbm,
                miscellaneous_loss_db: radio.miscellaneous_loss_db,
            },
            None => SiteRadio {
                id: &site.id,
                antenna: self.settings.antenna_settings(site.kind),
                tx_power_dbm: self.settings.tx_power_dbm,
                sensitivity_dbm: self.settings.effective_sensitivity_dbm()?,
                miscellaneous_loss_db: self.settings.miscellaneous_loss_db,
            },
        })
    }
}

struct SiteRadio<'a> {
    id: &'a str,
    antenna: &'a AntennaSettings,
    tx_power_dbm: f64,
    sensitivity_dbm: f64,
    miscellaneous_loss_db: f64,
}

struct LinkPath {
    distance_m: f64,
    fspl_db: f64,
    diffraction_loss_db: f64,
    clutter_loss_db: f64,
}

impl LinkPath {
    fn direction(
        &self,
        source: &SiteRadio<'_>,
        target: &SiteRadio<'_>,
        departure_angle_deg: f64,
        fade_margin_db: f64,
    ) -> DirectionResult {
        let arrival_angle_deg = -departure_angle_deg;
        let tx_gain_dbi = source.antenna.pattern.gain_at(departure_angle_deg);
        let rx_gain_dbi = target.antenna.pattern.gain_at(arrival_angle_deg);
        let total_path_loss_db = self.fspl_db + self.diffraction_loss_db + self.clutter_loss_db;
        let power = received_power_dbm(
            source.tx_power_dbm,
            tx_gain_dbi,
            source.antenna.feed_loss_db,
            total_path_loss_db,
            rx_gain_dbi,
            target.antenna.feed_loss_db,
            source.miscellaneous_loss_db,
        );
        let raw_margin_db = power - target.sensitivity_dbm;
        let usable_margin_db = raw_margin_db - fade_margin_db;
        DirectionResult {
            source_id: source.id.into(),
            target_id: target.id.into(),
            distance_m: self.distance_m,
            fspl_db: self.fspl_db,
            diffraction_loss_db: self.diffraction_loss_db,
            clutter_loss_db: self.clutter_loss_db,
            total_path_loss_db,
            tx_gain_dbi,
            rx_gain_dbi,
            received_power_dbm: power,
            sensitivity_dbm: target.sensitivity_dbm,
            raw_margin_db,
            usable_margin_db,
            departure_angle_deg,
            arrival_angle_deg,
            valid: usable_margin_db >= 0.0,
            tx_power_dbm: source.tx_power_dbm,
            tx_feed_loss_db: source.antenna.feed_loss_db,
            rx_feed_loss_db: target.antenna.feed_loss_db,
            miscellaneous_loss_db: source.miscellaneous_loss_db,
        }
    }
}
