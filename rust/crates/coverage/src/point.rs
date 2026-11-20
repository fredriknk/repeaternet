use repeaternet_rf_core::{
    EvaluationOptions, LinkResult, Point, Site, TerrainLayer, TerrainSource,
};
use repeaternet_terrain::LonLat;
use serde::{Deserialize, Serialize};

use crate::{
    engine::client_site, invalid, ClientRadioProfile, CoverageError, CoverageEvaluator,
    CoverageSettings, CoverageSourceResult, Rejection,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PointState {
    UnknownTerrain,
    UnknownLinks,
    Unresolved,
    Evaluated,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PointSourceResult {
    pub distance_m: f64,
    #[serde(flatten)]
    pub result: CoverageSourceResult,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PointCoverage {
    pub latitude: f64,
    pub longitude: f64,
    pub state: PointState,
    pub client: ClientRadioProfile,
    pub surface_available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ground_elevation_m: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub surface_sample_available: Option<bool>,
    pub surface_elevation_m: Option<f64>,
    pub selected_source_id: Option<String>,
    pub message: Option<String>,
    pub sources: Vec<PointSourceResult>,
}

#[derive(Clone, Copy)]
pub struct ExactPointOptions<'a> {
    /// Route candidate settings' final profile step, in metres.
    pub final_sample_step_m: f64,
    pub include_serving_profile: bool,
    pub cancelled: &'a dyn Fn() -> bool,
}

pub struct ExactPointResult {
    pub coverage: PointCoverage,
    /// Converted by the caller's shared route-link DTO for an HTTP inspection response.
    pub profile_link: Option<LinkResult>,
}

impl<T: TerrainSource + ?Sized> CoverageEvaluator<'_, T> {
    pub fn evaluate_point(
        &self,
        location: LonLat,
        sources: &[Site],
        settings: &CoverageSettings,
        options: ExactPointOptions<'_>,
    ) -> Result<ExactPointResult, CoverageError> {
        settings.validate()?;
        if sources.is_empty() || sources.len() > settings.maximum_sources {
            return Err(invalid(
                "Point coverage requires a bounded nonempty source selection",
            ));
        }
        let position = self.transform.forward(location)?;
        self.evaluate_projected_point(position, location, sources, settings, options)
    }

    pub(crate) fn evaluate_projected_point(
        &self,
        position: Point,
        location: LonLat,
        sources: &[Site],
        settings: &CoverageSettings,
        options: ExactPointOptions<'_>,
    ) -> Result<ExactPointResult, CoverageError> {
        let cancelled = options.cancelled;
        if cancelled() {
            return Err(CoverageError::Cancelled);
        }
        let ground = self.sample(&[position], TerrainLayer::GroundDtm)?[0];
        let mut coverage = PointCoverage {
            latitude: location.latitude,
            longitude: location.longitude,
            state: PointState::UnknownTerrain,
            client: settings.client.clone(),
            surface_available: self.terrain.has_surface(),
            ground_elevation_m: ground.is_finite().then_some(ground),
            surface_sample_available: None,
            surface_elevation_m: None,
            selected_source_id: None,
            message: None,
            sources: Vec::new(),
        };
        if cancelled() {
            return Err(CoverageError::Cancelled);
        }
        if !ground.is_finite() {
            coverage.message =
                Some("No ground-elevation data at this location; RF coverage is unknown.".into());
            return Ok(ExactPointResult {
                coverage,
                profile_link: None,
            });
        }
        if coverage.surface_available {
            let value = self.sample(&[position], TerrainLayer::SurfaceDom)?[0];
            coverage.surface_elevation_m = value.is_finite().then_some(value);
        }
        coverage.surface_sample_available = Some(coverage.surface_elevation_m.is_some());
        let client = settings.client.as_radio_budget();
        let target = client_site(
            "coverage-inspection-client".into(),
            position,
            ground,
            coverage.surface_elevation_m,
            &client,
        );
        for source in sources {
            if cancelled() {
                return Err(CoverageError::Cancelled);
            }
            let (mut result, _) = self.evaluate_source(
                source,
                &target,
                EvaluationOptions {
                    sample_step_m: Some(options.final_sample_step_m),
                    include_profile: false,
                    target_radio: Some(&client),
                    ..EvaluationOptions::default()
                },
                settings.maximum_profile_samples,
                cancelled,
            )?;
            result.rejection = match result.rejection {
                Some(Rejection::Validation) => Some(Rejection::LosFresnelValidation),
                None if !result.valid_two_way => Some(Rejection::RadioMargin),
                rejection => rejection,
            };
            coverage.sources.push(PointSourceResult {
                distance_m: source.distance_to(&target),
                result,
            });
        }
        coverage.sources.sort_by(|a, b| {
            b.result
                .valid_two_way
                .cmp(&a.result.valid_two_way)
                .then_with(|| {
                    b.result
                        .two_way_margin_db
                        .unwrap_or(f64::NEG_INFINITY)
                        .total_cmp(&a.result.two_way_margin_db.unwrap_or(f64::NEG_INFINITY))
                })
                .then_with(|| a.result.source_id.cmp(&b.result.source_id))
        });
        let all_rejected = |kind| {
            !coverage.sources.is_empty()
                && coverage
                    .sources
                    .iter()
                    .all(|row| row.result.rejection == Some(kind))
        };
        coverage.state = if all_rejected(Rejection::ProfileSampleLimit) {
            coverage.message = Some("No candidate router profile fits the configured terrain-sample limit. Increase the profile step explicitly or raise the cap.".into());
            PointState::Unresolved
        } else if all_rejected(Rejection::UnknownTerrain) {
            coverage.message = Some("Ground is known here, but terrain data is missing along every candidate path; RF coverage is unknown.".into());
            PointState::UnknownLinks
        } else {
            if coverage
                .sources
                .iter()
                .any(|row| row.result.rejection == Some(Rejection::ProfileSampleLimit))
            {
                coverage.message = Some("Some candidate paths exceed the configured terrain-profile sample limit; those links are unresolved.".into());
            }
            PointState::Evaluated
        };
        let mut profile_link = None;
        if let Some(best) = coverage
            .sources
            .first()
            .filter(|row| row.result.two_way_margin_db.is_some())
        {
            coverage.selected_source_id = Some(best.result.source_id.clone());
            if options.include_serving_profile {
                if cancelled() {
                    return Err(CoverageError::Cancelled);
                }
                let source = sources
                    .iter()
                    .find(|site| site.id == best.result.source_id)
                    .expect("selected source was evaluated");
                profile_link = self
                    .evaluate_source(
                        source,
                        &target,
                        EvaluationOptions {
                            sample_step_m: Some(options.final_sample_step_m),
                            target_radio: Some(&client),
                            ..EvaluationOptions::default()
                        },
                        settings.maximum_profile_samples,
                        cancelled,
                    )?
                    .1;
            }
        }
        if cancelled() {
            return Err(CoverageError::Cancelled);
        }
        Ok(ExactPointResult {
            coverage,
            profile_link,
        })
    }
}
