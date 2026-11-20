use std::{
    borrow::Borrow,
    collections::BTreeSet,
    io::{BufRead, Read},
};

use serde::{Deserialize, Serialize};

use crate::{
    analyze_node_failures, invalid, network::usable_sources, CoverageCell, CoverageError,
    CoverageNetwork, CoverageSettings, CoverageState, MapBounds, MAX_CELLS,
};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ComparisonRun {
    pub job_id: String,
    pub project_id: String,
    pub state: String,
    pub terrain_fingerprint: String,
    pub radio_fingerprint: String,
    pub model_version: String,
    pub settings: CoverageSettings,
    pub rows: usize,
    pub columns: usize,
    pub effective_cell_size_m: f64,
    pub area_bounds_wgs84: MapBounds,
    #[serde(default)]
    pub requested_cells: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<usize>,
    #[serde(flatten)]
    pub network: CoverageNetwork,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoverageChange {
    None,
    Retained,
    Gained,
    Lost,
    Unknown,
    Unresolved,
    OutsideArea,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CellChange {
    pub index: usize,
    pub latitude: f64,
    pub longitude: f64,
    pub local_change: CoverageChange,
    pub reference_connected_change: CoverageChange,
    pub baseline_source_count: usize,
    pub scenario_source_count: usize,
    pub margin_delta_db: Option<f64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ComparisonCounts {
    pub gained_local: usize,
    pub lost_local: usize,
    pub retained_local: usize,
    pub unchanged_uncovered_local: usize,
    pub gained_reference_connected: usize,
    pub lost_reference_connected: usize,
    pub retained_reference_connected: usize,
    pub unchanged_disconnected: usize,
    pub unknown: usize,
    pub unresolved: usize,
    pub outside_area: usize,
    pub margin_comparable: usize,
    pub evaluated_cells: usize,
    pub outside_area_cells: usize,
    pub baseline_covered_cells: usize,
    pub scenario_covered_cells: usize,
    pub baseline_coverage_percent: Option<f64>,
    pub scenario_coverage_percent: Option<f64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct GridComparison {
    pub counts: ComparisonCounts,
    pub cells: Vec<CellChange>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunComparison {
    pub baseline_job_id: String,
    pub scenario_job_id: String,
    pub reference_id: String,
    pub baseline_reference_available: bool,
    pub scenario_reference_available: bool,
    pub baseline_connected_source_ids: Vec<String>,
    pub scenario_connected_source_ids: Vec<String>,
    pub baseline_unresolved_backbone_edges: usize,
    pub scenario_unresolved_backbone_edges: usize,
    pub effective_cell_size_m: f64,
    pub area_bounds_wgs84: MapBounds,
    pub approximate_sampled_area_km2: f64,
    #[serde(flatten)]
    pub grid: GridComparison,
}

impl ComparisonRun {
    fn cell_count(&self) -> usize {
        if self.requested_cells == 0 {
            self.total.unwrap_or(0)
        } else {
            self.requested_cells
        }
    }

    pub fn check_compatibility(
        &self,
        scenario: &Self,
        current_terrain_fingerprint: &str,
        reference_id: &str,
    ) -> Result<(), CoverageError> {
        if self.state != "complete" || scenario.state != "complete" {
            return Err(invalid("Both coverage runs must be complete"));
        }
        if self.project_id != scenario.project_id {
            return Err(invalid("Coverage run not found in this project workspace"));
        }
        if self.terrain_fingerprint != scenario.terrain_fingerprint
            || self.terrain_fingerprint != current_terrain_fingerprint
        {
            return Err(invalid(
                "Runs use different or changed terrain; recalculate on the same terrain",
            ));
        }
        if self.radio_fingerprint != scenario.radio_fingerprint {
            return Err(invalid("Runs use different radio or propagation settings"));
        }
        if self.model_version != scenario.model_version {
            return Err(invalid(
                "Runs were calculated with different coverage model versions",
            ));
        }
        if self.settings.profile_step_m != scenario.settings.profile_step_m
            || self.settings.maximum_profile_samples != scenario.settings.maximum_profile_samples
            || self.settings.client != scenario.settings.client
        {
            return Err(invalid(
                "Runs use different client radios or profile resolution",
            ));
        }
        if self.rows != scenario.rows
            || self.columns != scenario.columns
            || self.effective_cell_size_m != scenario.effective_cell_size_m
            || self.area_bounds_wgs84 != scenario.area_bounds_wgs84
            || !self.effective_cell_size_m.is_finite()
            || self.effective_cell_size_m <= 0.0
            || !self.area_bounds_wgs84.iter().all(|value| value.is_finite())
        {
            return Err(invalid("Runs use different grid bounds or cell spacing"));
        }
        for run in [self, scenario] {
            let selected = |ids: &[String]| ids.iter().any(|id| id == reference_id);
            if !selected(&run.network.router_ids) || !selected(&run.network.source_ids) {
                return Err(invalid(
                    "Reference router must be a selected repeater in both runs",
                ));
            }
        }
        let cell_count = self.cell_count();
        if !(1..=MAX_CELLS).contains(&cell_count)
            || cell_count != scenario.cell_count()
            || self.rows.checked_mul(self.columns) != Some(cell_count)
        {
            return Err(invalid(
                "Coverage runs are incomplete and cannot be compared",
            ));
        }
        Ok(())
    }
}

/// Compare immutable runs. The caller supplies the active terrain fingerprint and authorizes both runs.
pub fn compare_runs<L: Borrow<CoverageCell>, R: Borrow<CoverageCell>>(
    baseline: &ComparisonRun,
    scenario: &ComparisonRun,
    current_terrain_fingerprint: &str,
    reference_id: &str,
    baseline_cells: impl IntoIterator<Item = Result<L, CoverageError>>,
    scenario_cells: impl IntoIterator<Item = Result<R, CoverageError>>,
) -> Result<RunComparison, CoverageError> {
    baseline.check_compatibility(scenario, current_terrain_fingerprint, reference_id)?;
    let left = analyze_node_failures(
        std::iter::empty::<CoverageCell>(),
        &baseline.network,
        &[],
        reference_id,
    )?;
    let right = analyze_node_failures(
        std::iter::empty::<CoverageCell>(),
        &scenario.network,
        &[],
        reference_id,
    )?;
    let grid = compare_coverage_streams(
        baseline_cells.into_iter().map(check_complete_cell),
        scenario_cells.into_iter().map(check_complete_cell),
        &left
            .connected_source_ids
            .iter()
            .map(String::as_str)
            .collect(),
        &right
            .connected_source_ids
            .iter()
            .map(String::as_str)
            .collect(),
        baseline.cell_count(),
    )?;
    Ok(RunComparison {
        baseline_job_id: baseline.job_id.clone(),
        scenario_job_id: scenario.job_id.clone(),
        reference_id: reference_id.to_owned(),
        baseline_reference_available: left.reference_available,
        scenario_reference_available: right.reference_available,
        baseline_connected_source_ids: left.connected_source_ids,
        scenario_connected_source_ids: right.connected_source_ids,
        baseline_unresolved_backbone_edges: left.unresolved_links,
        scenario_unresolved_backbone_edges: right.unresolved_links,
        effective_cell_size_m: scenario.effective_cell_size_m,
        area_bounds_wgs84: scenario.area_bounds_wgs84,
        approximate_sampled_area_km2: grid.counts.evaluated_cells as f64
            * baseline.effective_cell_size_m.powi(2)
            / 1_000_000.0,
        grid,
    })
}

fn check_complete_cell<C: Borrow<CoverageCell>>(
    row: Result<C, CoverageError>,
) -> Result<C, CoverageError> {
    let row = row?;
    let cell = row.borrow();
    if cell.state == CoverageState::NotEvaluated || !cell.x.is_finite() || !cell.y.is_finite() {
        return Err(invalid("Coverage runs contain incomplete or invalid cells"));
    }
    Ok(row)
}

pub fn compare_coverage_cells(
    baseline: &[CoverageCell],
    scenario: &[CoverageCell],
    baseline_connected: &BTreeSet<&str>,
    scenario_connected: &BTreeSet<&str>,
) -> Result<GridComparison, CoverageError> {
    compare_coverage_streams(
        baseline.iter().map(Ok),
        scenario.iter().map(Ok),
        baseline_connected,
        scenario_connected,
        baseline.len(),
    )
}

pub fn compare_coverage_streams<L: Borrow<CoverageCell>, R: Borrow<CoverageCell>>(
    baseline: impl IntoIterator<Item = Result<L, CoverageError>>,
    scenario: impl IntoIterator<Item = Result<R, CoverageError>>,
    baseline_connected: &BTreeSet<&str>,
    scenario_connected: &BTreeSet<&str>,
    expected_count: usize,
) -> Result<GridComparison, CoverageError> {
    if expected_count > MAX_CELLS {
        return Err(invalid("Coverage comparison exceeds the grid cell limit"));
    }
    let mut baseline = baseline.into_iter();
    let mut scenario = scenario.into_iter();
    let mut result = GridComparison {
        cells: Vec::with_capacity(expected_count),
        ..GridComparison::default()
    };
    let count_error = || invalid("Coverage grids contain different cell counts");
    for index in 0..expected_count {
        let left = baseline.next().transpose()?.ok_or_else(count_error)?;
        let right = scenario.next().transpose()?.ok_or_else(count_error)?;
        let (left, right) = (left.borrow(), right.borrow());
        if left.index != index || right.index != index {
            return Err(invalid("Coverage grids have different cell ordering"));
        }
        for (a, b) in [
            (left.latitude, right.latitude),
            (left.longitude, right.longitude),
        ] {
            if !a.is_finite() || !b.is_finite() || (a - b).abs() > 1e-6 {
                return Err(invalid(
                    "Coverage grids do not share the same sample coordinates",
                ));
            }
        }
        for (a, b) in [(left.x, right.x), (left.y, right.y)] {
            if !a.is_finite() || !b.is_finite() || (a - b).abs() > 1e-3 {
                return Err(invalid(
                    "Coverage grids do not share the same projected sample coordinates",
                ));
            }
        }
        let states = [left.state, right.state];
        let mut row = CellChange {
            index,
            latitude: left.latitude,
            longitude: left.longitude,
            local_change: CoverageChange::None,
            reference_connected_change: CoverageChange::None,
            baseline_source_count: 0,
            scenario_source_count: 0,
            margin_delta_db: None,
        };
        if states.contains(&CoverageState::OutsideArea) {
            result.counts.outside_area += 1;
            row.local_change = CoverageChange::OutsideArea;
            row.reference_connected_change = CoverageChange::OutsideArea;
        } else {
            let left_sources = usable_sources(left);
            let right_sources = usable_sources(right);
            row.baseline_source_count = left_sources.len();
            row.scenario_source_count = right_sources.len();
            if states.contains(&CoverageState::Unresolved) {
                result.counts.unresolved += 1;
                row.local_change = CoverageChange::Unresolved;
                row.reference_connected_change = CoverageChange::Unresolved;
            } else if states.contains(&CoverageState::UnknownTerrain)
                || states.contains(&CoverageState::NotEvaluated)
            {
                result.counts.unknown += 1;
                row.local_change = CoverageChange::Unknown;
                row.reference_connected_change = CoverageChange::Unknown;
            } else {
                result.counts.evaluated_cells += 1;
                row.local_change = change(!left_sources.is_empty(), !right_sources.is_empty());
                row.reference_connected_change = change(
                    !left_sources.is_disjoint(baseline_connected),
                    !right_sources.is_disjoint(scenario_connected),
                );
                match row.local_change {
                    CoverageChange::Retained => result.counts.retained_local += 1,
                    CoverageChange::Gained => result.counts.gained_local += 1,
                    CoverageChange::Lost => result.counts.lost_local += 1,
                    _ => result.counts.unchanged_uncovered_local += 1,
                }
                match row.reference_connected_change {
                    CoverageChange::Retained => result.counts.retained_reference_connected += 1,
                    CoverageChange::Gained => result.counts.gained_reference_connected += 1,
                    CoverageChange::Lost => result.counts.lost_reference_connected += 1,
                    _ => result.counts.unchanged_disconnected += 1,
                }
                if let (Some(before), Some(after)) = (best_margin(left), best_margin(right)) {
                    row.margin_delta_db = Some(after - before);
                    result.counts.margin_comparable += 1;
                }
            }
        }
        result.cells.push(row);
    }
    if baseline.next().transpose()?.is_some() || scenario.next().transpose()?.is_some() {
        return Err(count_error());
    }
    let counts = &mut result.counts;
    counts.outside_area_cells = counts.outside_area;
    counts.baseline_covered_cells = counts.lost_local + counts.retained_local;
    counts.scenario_covered_cells = counts.gained_local + counts.retained_local;
    if counts.evaluated_cells > 0 {
        let percent = |covered: usize| 100.0 * covered as f64 / counts.evaluated_cells as f64;
        counts.baseline_coverage_percent = Some(percent(counts.baseline_covered_cells));
        counts.scenario_coverage_percent = Some(percent(counts.scenario_covered_cells));
    }
    Ok(result)
}

fn change(before: bool, after: bool) -> CoverageChange {
    match (before, after) {
        (true, true) => CoverageChange::Retained,
        (false, true) => CoverageChange::Gained,
        (true, false) => CoverageChange::Lost,
        (false, false) => CoverageChange::None,
    }
}

fn best_margin(cell: &CoverageCell) -> Option<f64> {
    cell.sources
        .iter()
        .filter(|row| row.valid_two_way)
        .filter_map(|row| row.two_way_margin_db)
        .filter(|margin| margin.is_finite())
        .reduce(f64::max)
}

/// Decode one legacy JSONL chunk at a time, with a caller-supplied byte cap per line.
pub fn read_cell_stream(
    mut input: impl BufRead,
    maximum_chunk_bytes: usize,
) -> impl Iterator<Item = Result<CoverageCell, CoverageError>> {
    let mut pending = Vec::new().into_iter();
    let mut line = Vec::new();
    let mut terminal = false;
    std::iter::from_fn(move || {
        while !terminal {
            if let Some(cell) = pending.next() {
                return Some(Ok(cell));
            }
            line.clear();
            let mut bounded = (&mut input).take((maximum_chunk_bytes as u64).saturating_add(1));
            let decoded = match bounded.read_until(b'\n', &mut line) {
                Ok(0) => {
                    terminal = true;
                    return None;
                }
                Ok(len) if len > maximum_chunk_bytes => {
                    Err(invalid("Coverage result chunk exceeds its byte limit"))
                }
                Ok(_) => {
                    serde_json::from_slice::<Vec<CoverageCell>>(&line).map_err(CoverageError::from)
                }
                Err(error) => Err(error.into()),
            };
            match decoded {
                Ok(chunk) => pending = chunk.into_iter(),
                Err(error) => {
                    terminal = true;
                    return Some(Err(error));
                }
            }
        }
        None
    })
}
