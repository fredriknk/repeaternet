//! Coverage profile sizing and the shared RF profile implementation.

use crate::TerrainError;

pub use repeaternet_rf_core::{sample_profile, TerrainProfile};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ProfileSampling {
    pub step_m: f64,
    pub required_samples: usize,
    pub maximum_samples: usize,
}

impl ProfileSampling {
    pub fn fits(self) -> bool {
        self.required_samples <= self.maximum_samples
    }
}

/// Resolve the sampling step and report cap overflow without coarsening the profile.
pub fn bounded_profile_step(
    distance_m: f64,
    requested_step_m: f64,
    terrain_resolution_m: f64,
    maximum_samples: usize,
) -> Result<ProfileSampling, TerrainError> {
    if !distance_m.is_finite() || distance_m < 0.0 {
        return Err(TerrainError::InvalidInput(
            "Link distance must be finite and nonnegative",
        ));
    }
    if !requested_step_m.is_finite() || requested_step_m <= 0.0 {
        return Err(TerrainError::InvalidInput(
            "Requested profile step must be finite and positive",
        ));
    }
    if !terrain_resolution_m.is_finite() || terrain_resolution_m <= 0.0 {
        return Err(TerrainError::InvalidInput(
            "Terrain resolution must be finite and positive",
        ));
    }
    if maximum_samples < 3 {
        return Err(TerrainError::InvalidInput(
            "Maximum profile samples must be at least 3",
        ));
    }
    let step_m = requested_step_m.max(terrain_resolution_m);
    let required_intervals = (distance_m / step_m).ceil();
    if required_intervals >= usize::MAX as f64 {
        return Err(TerrainError::InvalidInput(
            "Link profile has too many samples",
        ));
    }
    Ok(ProfileSampling {
        step_m,
        required_samples: (required_intervals as usize + 1).max(3),
        maximum_samples,
    })
}
