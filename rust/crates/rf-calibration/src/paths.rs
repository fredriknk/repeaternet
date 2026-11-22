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
use std::path::{Component, Path, PathBuf};

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
        let key = |p: &Path| {
            if cfg!(windows) {
                PathBuf::from(p.to_string_lossy().to_lowercase())
            } else {
                p.to_path_buf()
            }
        };
        let real = absolute_without_parents(&root.join("data"))?;
        let candidate = absolute_without_parents(path)?;
        if key(&candidate).starts_with(key(&real))
            || key(&resolved_path(&candidate)?).starts_with(key(&resolved_path(&real)?))
        {
            bail!(
                "refusing to use {}: it is inside the real RepeaterNet data directory",
                path.display()
            );
        }
    }
    Ok(())
}

fn absolute_without_parents(path: &Path) -> Result<PathBuf> {
    let mut absolute = PathBuf::new();
    for component in std::path::absolute(path)?.components() {
        match component {
            Component::ParentDir => {
                absolute.pop();
            }
            Component::CurDir => {}
            part => absolute.push(part),
        }
    }
    Ok(absolute)
}

fn resolved_path(path: &Path) -> Result<PathBuf> {
    // Resolve junctions/symlinks in any existing ancestor of an output path.
    for ancestor in path.ancestors() {
        match std::fs::canonicalize(ancestor) {
            Ok(resolved) => return Ok(resolved.join(path.strip_prefix(ancestor)?)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_real_data_directory() {
        let root = repeaternet_data_root().unwrap();
        assert!(refuse_user_data(&root.join("data")).is_err());
        assert!(refuse_user_data(&root.join("data").join("x")).is_err());
        assert!(refuse_user_data(&root.join("calibration/../data/x")).is_err());
        assert!(refuse_user_data(&root.join("database")).is_ok());
        assert!(refuse_user_data(&root.join("calibration")).is_ok());
    }
}
