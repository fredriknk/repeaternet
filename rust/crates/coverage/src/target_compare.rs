use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{
    invalid, ClientRadioProfile, CoverageError, MapBounds, TargetAssessment, TargetResult,
    TargetState, TargetStats,
};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TargetGridCompatibility {
    pub rows: usize,
    pub columns: usize,
    pub effective_cell_size_m: f64,
    pub area_bounds_wgs84: MapBounds,
}

/// Comparison projection of a persisted target report. Metadata can be supplied by
/// the persistence layer using its existing canonical fingerprint implementation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SavedTargetReport {
    pub report_id: String,
    pub coverage_job_id: String,
    pub project_id: String,
    pub target_fingerprint: String,
    pub terrain_fingerprint: String,
    pub radio_fingerprint: String,
    pub model_version: String,
    pub grid_compatibility: TargetGridCompatibility,
    pub client_profile: ClientRadioProfile,
    pub profile_step_m: f64,
    /// Older Python reports omit this field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub maximum_profile_samples: Option<usize>,
    #[serde(flatten)]
    pub assessment: TargetAssessment,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetChange {
    Improved,
    Regressed,
    Unchanged,
    Uncertain,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TargetComparisonCounts {
    pub improved: usize,
    pub regressed: usize,
    pub unchanged: usize,
    pub uncertain: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TargetDifference {
    pub id: String,
    pub name: String,
    pub baseline_state: TargetState,
    pub scenario_state: TargetState,
    pub change: TargetChange,
    pub baseline_margin_db: Option<f64>,
    pub scenario_margin_db: Option<f64>,
    pub baseline_covered_area_m2: Option<f64>,
    pub scenario_covered_area_m2: Option<f64>,
    pub baseline_covered_length_m: Option<f64>,
    pub scenario_covered_length_m: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TargetReportComparison {
    pub baseline_report_id: String,
    pub scenario_report_id: String,
    pub baseline_coverage_job_id: String,
    pub scenario_coverage_job_id: String,
    pub counts: TargetComparisonCounts,
    pub targets: Vec<TargetDifference>,
}

/// Compare distinct reports after the caller has authorized their project.
pub fn compare_target_reports(
    baseline: &SavedTargetReport,
    scenario: &SavedTargetReport,
) -> Result<TargetReportComparison, CoverageError> {
    if baseline.report_id == scenario.report_id {
        return Err(invalid(
            "Choose two different saved target assessment reports",
        ));
    }
    if baseline.project_id != scenario.project_id {
        return Err(invalid("Target report not found in this project workspace"));
    }
    if baseline.target_fingerprint != scenario.target_fingerprint
        || baseline.terrain_fingerprint != scenario.terrain_fingerprint
        || baseline.radio_fingerprint != scenario.radio_fingerprint
        || baseline.model_version != scenario.model_version
        || baseline.grid_compatibility != scenario.grid_compatibility
        || baseline.client_profile != scenario.client_profile
        || baseline.profile_step_m != scenario.profile_step_m
        || baseline
            .maximum_profile_samples
            .zip(scenario.maximum_profile_samples)
            .is_some_and(|(before, after)| before != after)
        || baseline.assessment.assumptions.road_sample_spacing_m
            != scenario.assessment.assumptions.road_sample_spacing_m
    {
        return Err(invalid("Target reports use different targets, terrain, client radio, resolution, or coverage model"));
    }
    let before = &baseline.assessment.targets;
    let after = &scenario.assessment.targets;
    let mut after_by_id: BTreeMap<_, _> = after.iter().map(|row| (row.id.as_str(), row)).collect();
    let alignment_error = || invalid("Target report contents do not align");
    if after_by_id.len() != before.len() || after_by_id.len() != after.len() {
        return Err(alignment_error());
    }
    let mut counts = TargetComparisonCounts::default();
    let mut targets = Vec::with_capacity(before.len());
    for before in before {
        let after = after_by_id
            .remove(before.id.as_str())
            .ok_or_else(alignment_error)?;
        if before.geometry_type != after.geometry_type
            || before.minimum_margin_db != after.minimum_margin_db
            || before.reference_id != after.reference_id
        {
            return Err(alignment_error());
        }
        let change = match (before.state, after.state) {
            (TargetState::Pass, TargetState::Fail) => {
                counts.regressed += 1;
                TargetChange::Regressed
            }
            (TargetState::Fail, TargetState::Pass) => {
                counts.improved += 1;
                TargetChange::Improved
            }
            (TargetState::Pass, TargetState::Pass) | (TargetState::Fail, TargetState::Fail) => {
                counts.unchanged += 1;
                TargetChange::Unchanged
            }
            _ => {
                counts.uncertain += 1;
                TargetChange::Uncertain
            }
        };
        let (baseline_margin_db, baseline_covered_area_m2, baseline_covered_length_m) =
            metrics(before);
        let (scenario_margin_db, scenario_covered_area_m2, scenario_covered_length_m) =
            metrics(after);
        targets.push(TargetDifference {
            id: before.id.clone(),
            name: after.name.clone(),
            baseline_state: before.state,
            scenario_state: after.state,
            change,
            baseline_margin_db,
            scenario_margin_db,
            baseline_covered_area_m2,
            scenario_covered_area_m2,
            baseline_covered_length_m,
            scenario_covered_length_m,
        });
    }
    Ok(TargetReportComparison {
        baseline_report_id: baseline.report_id.clone(),
        scenario_report_id: scenario.report_id.clone(),
        baseline_coverage_job_id: baseline.coverage_job_id.clone(),
        scenario_coverage_job_id: scenario.coverage_job_id.clone(),
        counts,
        targets,
    })
}

fn metrics(row: &TargetResult) -> (Option<f64>, Option<f64>, Option<f64>) {
    match &row.stats {
        TargetStats::Point(point) => (
            point
                .links
                .as_ref()
                .and_then(|link| link.best_two_way_margin_db),
            None,
            None,
        ),
        TargetStats::Road(road) => (
            road.worst_passing_margin_db,
            None,
            Some(road.covered_length_m),
        ),
        TargetStats::Polygon(polygon) => (None, Some(polygon.covered_area_m2), None),
    }
}
