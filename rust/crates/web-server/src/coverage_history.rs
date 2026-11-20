use serde_json::{Map, Value, json};

pub(crate) fn job_summary(job: Value, terrain: Option<&Value>) -> Value {
    const KEYS: &[&str] = &[
        "job_id",
        "project_id",
        "state",
        "created_at",
        "alternative_id",
        "route_search_complete",
        "achieved_path_count",
        "requested_path_count",
        "effective_cell_size_m",
        "rows",
        "columns",
        "area_bounds_wgs84",
        "terrain_fingerprint",
        "radio_fingerprint",
        "model_version",
    ];
    let mut summary = select(&job, KEYS);
    for (key, default) in [
        ("source_ids", json!([])),
        ("router_ids", json!([])),
        ("source_height_overrides", json!({})),
        ("settings", json!({})),
        (
            "requested_cells",
            job.get("total").cloned().unwrap_or(json!(0)),
        ),
        ("covered_cells", json!(0)),
        ("evaluated_cells", json!(0)),
        ("unknown_cells", json!(0)),
        ("unresolved_cells", json!(0)),
        ("outside_area_cells", json!(0)),
        ("done", json!(0)),
    ] {
        summary.insert(key.into(), job.get(key).cloned().unwrap_or(default));
    }
    summary.insert("stale".into(), job["stale"].clone());
    summary.insert("route_current".into(), job["route_current"].clone());
    summary.insert(
        "terrain_current".into(),
        json!(terrain.is_some() && terrain == job.get("terrain_fingerprint")),
    );
    Value::Object(summary)
}

fn select(record: &Value, keys: &[&str]) -> Map<String, Value> {
    keys.iter()
        .map(|key| ((*key).to_owned(), record[*key].clone()))
        .collect()
}
