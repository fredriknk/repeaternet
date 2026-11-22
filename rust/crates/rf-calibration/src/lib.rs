//! RepeaterNet RF calibration harness: measured-link dataset extraction,
//! terrain profiles and pluggable path-loss engines. See
//! `docs/RF_CALIBRATION.md`.

pub mod dataset;
pub mod engines;
pub mod extract;
mod jsonl;
pub mod paths;
pub mod predict;
pub mod profiles;
pub mod snapshot;
pub mod sondehub;
