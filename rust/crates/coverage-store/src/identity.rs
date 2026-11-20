use crate::legacy::{self, PYTHON_MODEL_VERSION};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Native fingerprint domain. Legacy identities use a separate domain.
pub const FORMAT: &str = "repeaternet-native-coverage-v1";

pub const REUSE_FIELDS: [&str; 17] = [
    "model_version",
    "project_id",
    "route_fingerprint",
    "terrain_fingerprint",
    "terrain_crs",
    "settings_fingerprint",
    "radio_fingerprint",
    "source_ids",
    "source_sites",
    "source_height_overrides",
    "alternative_id",
    "snapshot_version",
    "grid_bounds_projected",
    "rows",
    "columns",
    "requested_cells",
    "effective_cell_size_m",
];

/// SHA-256 over the native domain and compact, recursively key-sorted JSON.
pub fn fingerprint(value: &Value) -> String {
    let mut hash = Sha256::new();
    hash.update(FORMAT);
    hash.update([0]);
    hash.update(canonical(value).to_string());
    crate::lower_hex(&hash.finalize())
}

fn canonical(value: &Value) -> Value {
    match value {
        Value::Object(fields) => {
            let sorted: std::collections::BTreeMap<_, _> = fields
                .iter()
                .map(|(key, item)| (key, canonical(item)))
                .collect();
            serde_json::to_value(sorted).expect("JSON values serialize")
        }
        Value::Array(items) => Value::Array(items.iter().map(canonical).collect()),
        _ => value.clone(),
    }
}

pub fn route_fingerprint(plan: &Value) -> Option<String> {
    let mut route = plan.as_object()?.clone();
    for field in ["coverage", "coverage_targets", "resolved_search"] {
        route.remove(field);
    }
    Some(fingerprint(&Value::Object(route)))
}

/// The live certified route, terrain, settings and radio of a project.
///
/// Optional `model_version` binds currency to one native model. When omitted,
/// any native model version other than the Python one can be current.
#[derive(Clone, Debug, Serialize)]
#[serde(transparent)]
pub struct CurrentIdentity(Value);

impl CurrentIdentity {
    /// Supply the active certified route snapshot and current terrain/settings/radio.
    pub fn new(value: Value) -> Option<Self> {
        let project = value["project_id"].as_str()?;
        let alternative = value["alternative_id"].as_str()?;
        value["snapshot_version"].as_u64()?;
        value["terrain_fingerprint"].as_array()?;
        if project.is_empty()
            || alternative.is_empty()
            || ![
                "route_fingerprint",
                "settings_fingerprint",
                "radio_fingerprint",
            ]
            .iter()
            .all(|key| digest(&value[key]))
            || value.get("model_version").is_some_and(|model| {
                model
                    .as_str()
                    .is_none_or(|model| model.is_empty() || model == PYTHON_MODEL_VERSION)
            })
        {
            return None;
        }
        Some(Self(value))
    }

    /// Why a run is not current for the route and terrain, ignoring settings and radio.
    pub fn route_stale_reason(&self, job: &Value) -> Option<&'static str> {
        if job["coverage_store_format"] != FORMAT {
            return Some(if legacy::is_legacy(job) {
                reason::LEGACY_ROUTE_NOT_CURRENT
            } else {
                reason::UNSUPPORTED_FORMAT
            });
        }
        if job["stale"] != false {
            return Some(reason::PERSISTED_STALE);
        }
        if !native_model(job) {
            return Some(reason::MODEL_VERSION);
        }
        if self
            .0
            .get("model_version")
            .is_some_and(|model| job["model_version"] != *model)
        {
            return Some(reason::MODEL_VERSION);
        }
        for (key, why) in [
            ("project_id", reason::PROJECT),
            ("route_fingerprint", reason::ROUTE),
            ("alternative_id", reason::ALTERNATIVE),
            ("snapshot_version", reason::SNAPSHOT),
            ("terrain_fingerprint", reason::TERRAIN),
        ] {
            if job[key] != self.0[key] {
                return Some(why);
            }
        }
        None
    }

    /// Why a run is not current. `None` means every identity field matches.
    pub fn stale_reason(&self, job: &Value) -> Option<&'static str> {
        if let Some(reason) = self.route_stale_reason(job) {
            return Some(reason);
        }
        if job["settings_fingerprint"] != self.0["settings_fingerprint"] {
            return Some(reason::SETTINGS);
        }
        if job["radio_fingerprint"] != self.0["radio_fingerprint"] {
            return Some(reason::RADIO);
        }
        if !embedded_settings_match(job) {
            return Some(reason::EMBEDDED);
        }
        None
    }

    pub fn route_matches(&self, job: &Value) -> bool {
        self.route_stale_reason(job).is_none()
    }

    pub fn matches(&self, job: &Value) -> bool {
        self.stale_reason(job).is_none()
    }

    /// The live terrain fingerprint, for `Store::comparison`.
    pub fn terrain_fingerprint(&self) -> &Value {
        &self.0["terrain_fingerprint"]
    }
}

/// Stable machine-readable stale and comparison reasons.
pub mod reason {
    pub const CURRENT_UNAVAILABLE: &str = "current_identity_unavailable";
    pub const UNSUPPORTED_FORMAT: &str = "unsupported_legacy_model_or_format";
    pub const PERSISTED_STALE: &str = "persisted_stale";
    pub const MODEL_VERSION: &str = "model_version_changed";
    pub const PROJECT: &str = "project_changed";
    pub const ROUTE: &str = "route_changed";
    pub const ALTERNATIVE: &str = "alternative_changed";
    pub const SNAPSHOT: &str = "snapshot_changed";
    pub const TERRAIN: &str = "terrain_changed";
    pub const SETTINGS: &str = "settings_changed";
    pub const RADIO: &str = "radio_changed";
    pub const EMBEDDED: &str = "embedded_settings_invalid";
    pub const LEGACY_IDENTITY_UNAVAILABLE: &str = "legacy_identity_unavailable";
    pub const LEGACY_PROJECT: &str = "legacy_project_changed";
    pub const LEGACY_TERRAIN: &str = "legacy_terrain_changed";
    pub const LEGACY_EMBEDDED: &str = "legacy_embedded_settings_invalid";
    /// Python keeps the certified route and snapshot counter in process memory,
    /// so no later process can prove a Python run belongs to the live route.
    pub const LEGACY_ROUTE_NOT_CURRENT: &str = "legacy_python_route_not_current";
    pub const INCOMPLETE: &str = "comparison_run_incomplete";
    pub const DOMAIN: &str = "comparison_fingerprint_domain_mismatch";
    pub const COMPARISON_MODEL: &str = "comparison_model_version_mismatch";
    pub const COMPARISON_TERRAIN: &str = "comparison_terrain_changed";
    pub const COMPARISON_RADIO: &str = "comparison_radio_mismatch";
    pub const COMPARISON_PROJECT: &str = "comparison_project_mismatch";
    pub const COMPARISON_EMBEDDED: &str = "comparison_embedded_settings_invalid";
    pub const COMPARISON_GRID: &str = "comparison_grid_invalid";
}

/// Status of any stored run against the live identities, native or Python.
pub fn job_stale_reason(
    job: &Value,
    current: Option<&CurrentIdentity>,
    legacy: Option<&legacy::LegacyIdentity>,
) -> Option<&'static str> {
    if job["coverage_store_format"] == FORMAT {
        return match current {
            Some(current) => current.stale_reason(job),
            None => Some(reason::CURRENT_UNAVAILABLE),
        };
    }
    if !legacy::is_legacy(job) {
        return Some(reason::UNSUPPORTED_FORMAT);
    }
    Some(match legacy {
        Some(legacy) => legacy.stale_reason(job),
        None => reason::LEGACY_IDENTITY_UNAVAILABLE,
    })
}

/// Python comparison identity: both runs complete, from one fingerprint domain
/// and model, on the live terrain, with one radio and intact embedded settings.
/// Settings compatibility, grid alignment and the reference router stay with
/// the caller, as does verifying the stored grids (see `Store::comparison`).
pub fn comparison_mismatch(
    project_id: &str,
    baseline: &Value,
    scenario: &Value,
    current_terrain: &Value,
) -> Option<&'static str> {
    let native = |job: &Value| job["coverage_store_format"] == FORMAT && native_model(job);
    let domain = if native(baseline) && native(scenario) {
        embedded_settings_match
    } else if legacy::is_legacy(baseline) && legacy::is_legacy(scenario) {
        legacy::embedded_settings_match
    } else {
        return Some(reason::DOMAIN);
    };
    if [baseline, scenario]
        .iter()
        .any(|job| job["state"] != "complete")
    {
        return Some(reason::INCOMPLETE);
    }
    if [baseline, scenario]
        .iter()
        .any(|job| job["project_id"] != project_id)
    {
        return Some(reason::COMPARISON_PROJECT);
    }
    if baseline["model_version"] != scenario["model_version"] {
        return Some(reason::COMPARISON_MODEL);
    }
    if !current_terrain.is_array()
        || baseline["terrain_fingerprint"] != *current_terrain
        || scenario["terrain_fingerprint"] != *current_terrain
    {
        return Some(reason::COMPARISON_TERRAIN);
    }
    if !baseline["radio_fingerprint"].is_string()
        || baseline["radio_fingerprint"] != scenario["radio_fingerprint"]
    {
        return Some(reason::COMPARISON_RADIO);
    }
    if !domain(baseline) || !domain(scenario) {
        return Some(reason::COMPARISON_EMBEDDED);
    }
    if grid_size(baseline).is_none() || grid_size(scenario).is_none() {
        return Some(reason::COMPARISON_GRID);
    }
    None
}

pub(crate) fn native_model(job: &Value) -> bool {
    job["model_version"]
        .as_str()
        .is_some_and(|model| !model.is_empty() && model != PYTHON_MODEL_VERSION)
}

fn digest(value: &Value) -> bool {
    value
        .as_str()
        .is_some_and(|s| s.len() == 64 && s.bytes().all(super::hex_digit))
}

pub(crate) fn grid_size(job: &Value) -> Option<u64> {
    let count = job["rows"]
        .as_u64()?
        .checked_mul(job["columns"].as_u64()?)?;
    (count > 0 && count <= 16_384 && Some(count) == job["requested_cells"].as_u64())
        .then_some(count)
}

/// Native reuse identity. Python runs, the Python model version, and any
/// incomplete or inconsistent identity return `None`.
pub fn reuse_fingerprint(job: &Value) -> Option<String> {
    if job["coverage_store_format"] != FORMAT || job["stale"] != false || !native_model(job) {
        return None;
    }
    CurrentIdentity::new(job.clone())?;
    grid_size(job)?;
    let bounds = job["grid_bounds_projected"].as_array()?;
    let coords: Vec<_> = bounds.iter().filter_map(Value::as_f64).collect();
    let ids = job["source_ids"].as_array()?;
    let sites = job["source_sites"].as_array()?;
    if coords.len() != 4
        || bounds.len() != 4
        || coords[0] >= coords[2]
        || coords[1] >= coords[3]
        || job["effective_cell_size_m"].as_f64()? <= 0.0
        || job["terrain_crs"].as_str()?.is_empty()
        || !job["source_height_overrides"].is_object()
        || ids.is_empty()
        || ids.len() > 64
        || ids.len() != sites.len()
        || !ids
            .iter()
            .zip(sites)
            .all(|(id, site)| id.as_str().is_some_and(|id| !id.is_empty()) && site["id"] == *id)
        || !embedded_settings_match(job)
    {
        return None;
    }
    let fields = REUSE_FIELDS
        .iter()
        .map(|&key| (key.to_owned(), job[key].clone()))
        .collect();
    Some(fingerprint(&Value::Object(fields)))
}

fn embedded_settings_match(job: &Value) -> bool {
    job["settings"].is_object()
        && job["radio_settings"].is_object()
        && job["settings_fingerprint"] == fingerprint(&job["settings"])
        && job["radio_fingerprint"] == fingerprint(&job["radio_settings"])
}
