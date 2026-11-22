//! Values from docs/RF_CALIBRATION.md, first run 2026-11-22 on about one day of
//! MeshCore captures. They are expected to change after the next calibration
//! rerun. Change the calibrated values only here.

use crate::{DiffractionModel, ObstacleSurface, TerminalClutter};

/// Delta-Bullington improved scores in both Norway and Benelux.
pub const DEFAULT_DIFFRACTION_MODEL: DiffractionModel = DiffractionModel::DeltaBullington;
/// Diffraction over DOM was about 21 dB too pessimistic.
pub const DEFAULT_OBSTACLE_SURFACE: ObstacleSurface = ObstacleSurface::Ground;
/// P.2108 helped relay-to-observer links, so terminal clutter is opt-in per site.
pub const DEFAULT_TERMINAL_CLUTTER: TerminalClutter = TerminalClutter::None;
/// The planner was about 5 dB optimistic overall and 1.5–2.5 dB on the cleanest subsets.
pub const MODEL_CORRECTION_DB: f64 = 3.0;
/// Coarser sampling misses ridges, so the recommendation retains 10 m for certification.
pub const CERTIFICATION_MAX_STEP_M: f64 = 10.0;
/// Measured optimism above the 10 m baseline at the tested nominal steps.
pub const COARSE_STEP_ALLOWANCE_DB: [(f64, f64); 3] = [(10.0, 0.0), (50.0, 1.6), (200.0, 4.2)];
/// The terminal correction used DOM minus DTM within 30 m of the terminal.
pub const TERMINAL_CLUTTER_RADIUS_M: f64 = 30.0;
/// The P.2108 comparison used the same 27 m street width for forest and urban terminals.
pub const P2108_STREET_WIDTH_M: f64 = 27.0;
/// Median within-link time variability in Norway was 2.5 dB.
pub const TIME_VARIABILITY_SIGMA_DB: f64 = 2.5;
/// Receive-side siting spread was 14–16 dB, with 20–23 dB held-out new-site error.
pub const NEW_SITE_SIGMA_DB: f64 = 15.0;
/// Raising k to 4/3 never improved a calibration score.
pub const K_FACTOR: f64 = 1.0;

/// Extra optimism from coarsening beyond native resolution, based on the 50 m and 200 m findings.
pub fn step_allowance_db(step_m: f64, terrain_resolution_m: f64) -> f64 {
    fn table(step_m: f64) -> f64 {
        let [low, middle, high] = COARSE_STEP_ALLOWANCE_DB;
        if step_m <= low.0 {
            return low.1;
        }
        let ((start, start_db), (end, end_db)) = if step_m <= middle.0 {
            (low, middle)
        } else {
            (middle, high)
        };
        let fraction = (step_m / start).ln() / (end / start).ln();
        (start_db + fraction * (end_db - start_db)).min(8.0)
    }

    (table(step_m.max(terrain_resolution_m)) - table(terrain_resolution_m)).max(0.0)
}
