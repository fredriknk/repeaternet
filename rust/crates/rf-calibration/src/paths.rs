//! Calibration work directory and guard rails.
//!
//! The work directory holds database snapshots, the extracted dataset,
//! profiles and predictions. It defaults to
//! `%LOCALAPPDATA%/RepeaterNet/calibration` (or
//! `$XDG_DATA_HOME/RepeaterNet/calibration`) and can be set with
//! `--work-dir` or `RF_CALIBRATION_DIR`. Terrain always goes through the
//! shared provider cache (`RF_PLANNER_CACHE_DIR`, default the test data
//! directory's `shared-cache`) so downloads are reused by the planner.

use std::env;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

/// `%LOCALAPPDATA%/RepeaterNet` or the XDG equivalent.
pub fn repeaternet_data_root() -> Result<PathBuf> {
    if let Some(dir) = env::var_os("LOCALAPPDATA") {
        return Ok(PathBuf::from(dir).join("RepeaterNet"));
    }
    if let Some(dir) = env::var_os("XDG_DATA_HOME") {
        return Ok(PathBuf::from(dir).join("RepeaterNet"));
    }
    if let Some(home) = env::var_os("HOME") {
        return Ok(PathBuf::from(home).join(".local/share/RepeaterNet"));
    }
    bail!("cannot locate the RepeaterNet data root; pass --work-dir")
}

/// Resolve the calibration work directory.
pub fn work_dir(explicit: Option<&Path>) -> Result<PathBuf> {
    let dir = match explicit {
        Some(path) => path.to_path_buf(),
        None => match env::var_os("RF_CALIBRATION_DIR") {
            Some(dir) => PathBuf::from(dir),
            None => repeaternet_data_root()?.join("calibration"),
        },
    };
    refuse_user_data(&dir)?;
    Ok(dir)
}

/// Shared terrain cache: `RF_PLANNER_CACHE_DIR`, else the test data
/// directory's `shared-cache`. Never the user's real data directory.
pub fn shared_cache_dir() -> Result<PathBuf> {
    let dir = match env::var_os("RF_PLANNER_CACHE_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => repeaternet_data_root()?
            .join("test-data")
            .join("shared-cache"),
    };
    refuse_user_data(&dir)?;
    Ok(dir)
}

/// Refuse paths inside `<RepeaterNet>/data`, the user's real planner data.
pub fn refuse_user_data(path: &Path) -> Result<()> {
    if let Ok(root) = repeaternet_data_root() {
        let norm = |p: &Path| p.to_string_lossy().replace('\\', "/").to_lowercase();
        let real = norm(&root.join("data"));
        let candidate = norm(path);
        if candidate == real || candidate.starts_with(&format!("{real}/")) {
            bail!(
                "refusing to use {}: it is inside the real RepeaterNet data directory",
                path.display()
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_real_data_directory() {
        let root = repeaternet_data_root().unwrap();
        assert!(refuse_user_data(&root.join("data")).is_err());
        assert!(refuse_user_data(&root.join("data").join("x")).is_err());
        assert!(refuse_user_data(&root.join("database")).is_ok());
        assert!(refuse_user_data(&root.join("calibration")).is_ok());
    }
}
