use std::collections::{BTreeMap, HashSet};

use repeaternet_optimizer::{
    CandidateSettings, InfrastructurePolicy, OptimizationPriority, RoadAccessClasses, RouterPolicy,
    SearchEffort, SiteOrigin,
};
use repeaternet_rf_core::{
    AntennaPattern, AntennaSettings, LoRaSettings, RfSettings, SiteKind, ValidationMode,
};
use repeaternet_terrain::LonLat;
use serde_json::{Value, json};

use crate::api::{ApiError, ApiResult};

pub struct Location {
    pub id: String,
    pub name: String,
    pub coordinate: LonLat,
    pub kind: SiteKind,
    pub origin: SiteOrigin,
    pub policy: RouterPolicy,
    pub height: f64,
    pub height_override: bool,
}

pub struct RouteInputs {
    pub plan: Value,
    pub locations: Vec<Location>,
    pub rf: RfSettings,
    pub rf_payload: Value,
    pub candidates: CandidateSettings,
}

fn merge(default: &mut Value, overrides: &Value) -> ApiResult<()> {
    let fields = overrides
        .as_object()
        .ok_or_else(|| ApiError::invalid("Settings must be a JSON object"))?;
    for (key, value) in fields {
        let old = default
            .get_mut(key)
            .ok_or_else(|| ApiError::invalid(format!("Unknown setting: {key}")))?;
        if old.is_object() && key != "snr_thresholds_db" {
            merge(old, value)?;
            continue;
        }
        if key == "pattern_csv" && (value.is_null() || value.as_str() == Some("")) {
            *old = value.clone();
            continue;
        }
        if key == "pattern_csv" {
            return Err(ApiError::invalid(
                "Server filesystem antenna paths are not supported",
            ));
        }
        let valid = match key.as_str() {
            "maximum_link_distance_m" => value.is_null() || number(value).is_some(),
            "snr_thresholds_db" => value.as_object().is_some_and(|fields| {
                fields
                    .iter()
                    .all(|(key, value)| key.parse::<u8>().is_ok() && number(value).is_some())
            }),
            _ if old.is_boolean() => value.is_boolean(),
            _ if old.is_string() => value.is_string(),
            _ if old.is_u64() => integer(value).is_some(),
            _ => number(value).is_some(),
        };
        if !valid {
            return Err(ApiError::invalid(format!("Invalid value for {key}")));
        }
        *old = value.clone();
    }
    Ok(())
}

fn number(value: &Value) -> Option<f64> {
    value.as_f64().filter(|v| v.is_finite())
}

fn integer(value: &Value) -> Option<usize> {
    number(value)
        .filter(|v| *v >= 0.0 && *v < usize::MAX as f64 && v.fract() == 0.0)
        .map(|v| v as usize)
}

macro_rules! choice {
    ($value:expr, $key:literal, {$($wire:literal => $variant:expr),+ $(,)?}) => {
        match $value[$key].as_str() {
            $(Some($wire) => $variant,)+
            _ => return Err(ApiError::invalid(concat!("Invalid ", $key))),
        }
    };
}

fn antenna(value: &Value) -> ApiResult<AntennaSettings> {
    let height_agl_m = value["height_agl_m"].as_f64().unwrap();
    if height_agl_m <= 0.0 || height_agl_m > 1000.0 {
        return Err(ApiError::invalid(
            "Antenna height must be between 0 and 1000 m",
        ));
    }
    Ok(AntennaSettings {
        pattern: AntennaPattern::ConstantGain(value["gain_dbi"].as_f64().unwrap()),
        feed_loss_db: value["feed_loss_db"].as_f64().unwrap(),
        height_agl_m,
    })
}

pub(crate) fn rf(value: &Value) -> ApiResult<RfSettings> {
    let n = |key: &str| value[key].as_f64().unwrap();
    let validation_mode = choice!(value, "validation_mode", {
        "rf_propagation" => ValidationMode::Propagation, "strict_los" => ValidationMode::StrictLos,
    });
    if value["diffraction_model"] != "bullington" {
        return Err(ApiError::invalid("Invalid diffraction_model"));
    }
    for key in ["frequency_mhz", "k_factor"] {
        if n(key) <= 0.0 {
            return Err(ApiError::invalid(format!("{key} must be positive")));
        }
    }
    if !(0.0..=1.0).contains(&n("required_fresnel_clearance")) {
        return Err(ApiError::invalid(
            "Fresnel clearance must be between 0 and 1",
        ));
    }
    let lora = &value["lora"];
    let bandwidth_hz = lora["bandwidth_hz"].as_f64().unwrap();
    if bandwidth_hz <= 0.0 {
        return Err(ApiError::invalid("LoRa bandwidth must be positive"));
    }
    let settings = RfSettings {
        frequency_mhz: n("frequency_mhz"),
        tx_power_dbm: n("tx_power_dbm"),
        receiver_sensitivity_dbm: n("receiver_sensitivity_dbm"),
        fade_margin_db: n("fade_margin_db"),
        miscellaneous_loss_db: n("miscellaneous_loss_db"),
        k_factor: n("k_factor"),
        required_fresnel_clearance: n("required_fresnel_clearance"),
        validation_mode,
        endpoint_a: antenna(&value["endpoint_a"])?,
        endpoint_b: antenna(&value["endpoint_b"])?,
        router: antenna(&value["router"])?,
        lora: LoRaSettings {
            enabled: lora["enabled"].as_bool().unwrap(),
            bandwidth_hz,
            spreading_factor: u8::try_from(integer(&lora["spreading_factor"]).unwrap())
                .map_err(|_| ApiError::invalid("Invalid spreading_factor"))?,
            coding_rate: lora["coding_rate"].as_str().unwrap().to_owned(),
            noise_figure_db: lora["noise_figure_db"].as_f64().unwrap(),
            manual_sensitivity_override: lora["manual_sensitivity_override"].as_bool().unwrap(),
            snr_thresholds_db: lora["snr_thresholds_db"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(key, value)| (key.parse().unwrap(), value.as_f64().unwrap()))
                .collect::<BTreeMap<_, _>>(),
        },
        clutter_loss_enabled: value["clutter_loss_enabled"].as_bool().unwrap(),
        clutter_loss_db_per_m: n("clutter_loss_db_per_m"),
    };
    settings
        .effective_sensitivity_dbm()
        .map_err(|e| ApiError::invalid(e.to_string()))?;
    Ok(settings)
}

fn candidates(value: &Value) -> ApiResult<CandidateSettings> {
    let n = |key: &str| value[key].as_f64().unwrap();
    let i = |key: &str| integer(&value[key]).unwrap();
    let flag = |key: &str| value[key].as_bool().unwrap();
    for key in [
        "grid_spacing_m",
        "cell_size_m",
        "coarse_sample_step_m",
        "medium_sample_step_m",
        "final_sample_step_m",
        "refine_step_m",
        "router_height_step_m",
        "corridor_width_m",
    ] {
        if n(key) <= 0.0 {
            return Err(ApiError::invalid(format!("{key} must be positive")));
        }
    }
    for (key, min, max) in [
        ("maximum_candidates", 2.0, 2000.0),
        ("maximum_neighbors_per_site", 1.0, 200.0),
        ("refine_radius_m", 0.0, 2000.0),
        ("maximum_road_distance_m", 10.0, 20_000.0),
    ] {
        if !(min..=max).contains(&n(key)) {
            return Err(ApiError::invalid(format!(
                "{key} must be between {min} and {max}"
            )));
        }
    }
    if n("minimum_router_height_m") > n("maximum_router_height_m") {
        return Err(ApiError::invalid("Minimum height exceeds maximum height"));
    }
    let parallel_workers = i("parallel_workers");
    if parallel_workers > 64 {
        return Err(ApiError::invalid(
            "parallel_workers must be between 0 and 64",
        ));
    }
    let settings = CandidateSettings {
        infrastructure_policy: choice!(value, "infrastructure_policy", {
            "existing_only" => InfrastructurePolicy::ExistingOnly,
            "proposed_only" => InfrastructurePolicy::ProposedOnly,
            "existing_and_proposed" => InfrastructurePolicy::ExistingAndProposed,
        }),
        corridor_width_m: n("corridor_width_m"),
        unrestricted_bounding_area: flag("unrestricted_bounding_area"),
        grid_spacing_m: n("grid_spacing_m"),
        cell_size_m: n("cell_size_m"),
        candidates_per_cell: i("candidates_per_cell"),
        maximum_candidates: i("maximum_candidates"),
        maximum_neighbors_per_site: i("maximum_neighbors_per_site"),
        maximum_solution_routers: i("maximum_solution_routers"),
        reliability_paths: i("reliability_paths"),
        parallel_workers,
        coarse_sample_step_m: n("coarse_sample_step_m"),
        medium_sample_step_m: n("medium_sample_step_m"),
        final_sample_step_m: n("final_sample_step_m"),
        maximum_link_distance_m: number(&value["maximum_link_distance_m"]),
        refine_radius_m: n("refine_radius_m"),
        refine_step_m: n("refine_step_m"),
        optimize_heights: flag("optimize_heights"),
        minimum_router_height_m: n("minimum_router_height_m"),
        maximum_router_height_m: n("maximum_router_height_m"),
        router_height_step_m: n("router_height_step_m"),
        priority: choice!(value, "priority", {
            "minimum_routers" => OptimizationPriority::MinimumRouters,
            "fewest_new_installations" => OptimizationPriority::FewestNewInstallations,
            "minimum_infrastructure" => OptimizationPriority::MinimumInfrastructure,
            "maximum_reliability" => OptimizationPriority::MaximumReliability,
        }),
        road_access_enabled: flag("road_access_enabled"),
        maximum_road_distance_m: n("maximum_road_distance_m"),
        road_access_classes: choice!(value, "road_access_classes", {
            "public_roads" => RoadAccessClasses::PublicRoads,
            "all_drivable" => RoadAccessClasses::AllDrivable,
        }),
    };
    if settings.candidates_per_cell == 0
        || settings.reliability_paths == 0
        || settings
            .maximum_link_distance_m
            .is_some_and(|distance| distance <= 0.0)
        || (settings.optimize_heights
            && (settings.minimum_router_height_m <= 0.0
                || settings.maximum_router_height_m > 1000.0
                || (settings.maximum_router_height_m - settings.minimum_router_height_m)
                    / settings.router_height_step_m
                    > 2000.0))
    {
        return Err(ApiError::invalid(
            "Invalid candidate search or height limits",
        ));
    }
    Ok(settings)
}

pub(crate) fn coordinate(lat: &Value, lon: &Value, name: &str) -> ApiResult<LonLat> {
    match (number(lat), number(lon)) {
        (Some(lat), Some(lon))
            if (-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon) =>
        {
            Ok(LonLat {
                longitude: lon,
                latitude: lat,
            })
        }
        _ => Err(ApiError::invalid(format!("Invalid {name} coordinates"))),
    }
}

fn locations(
    plan: &Value,
    rf: &RfSettings,
    policy: InfrastructurePolicy,
) -> ApiResult<(Vec<Location>, usize)> {
    let mut locations = Vec::new();
    let mut ids = HashSet::new();
    let mut excluded_known = 0;
    for (key, id, kind, antenna) in [
        ("a", "A", SiteKind::EndpointA, &rf.endpoint_a),
        ("b", "B", SiteKind::EndpointB, &rf.endpoint_b),
    ] {
        let fields = plan[key]
            .as_array()
            .filter(|values| values.len() == 2)
            .ok_or_else(|| {
                ApiError::invalid(format!("{key} must contain latitude and longitude"))
            })?;
        locations.push(Location {
            id: id.into(),
            name: format!("Endpoint {id}"),
            coordinate: coordinate(&fields[0], &fields[1], key)?,
            kind,
            origin: SiteOrigin::Optimized,
            policy: RouterPolicy::Optional,
            height: antenna.height_agl_m,
            height_override: false,
        });
    }
    for (key, prefix, origin, eligible) in [
        (
            "known_routers",
            "K-",
            SiteOrigin::Known,
            policy != InfrastructurePolicy::ProposedOnly,
        ),
        (
            "manual_routers",
            "M-",
            SiteOrigin::Manual,
            policy != InfrastructurePolicy::ExistingOnly,
        ),
    ] {
        let Some(items) = plan.get(key) else { continue };
        let items = items
            .as_array()
            .ok_or_else(|| ApiError::invalid(format!("{key} must be a list")))?;
        for item in items {
            let id = item["id"]
                .as_str()
                .filter(|id| {
                    id.starts_with(prefix)
                        && id.chars().count() <= 64
                        && id
                            .chars()
                            .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
                })
                .ok_or_else(|| ApiError::invalid(format!("Each {key} entry needs a valid id")))?;
            if !ids.insert(id) {
                return Err(ApiError::invalid("Router ids must be unique"));
            }
            let coordinate = coordinate(&item["latitude"], &item["longitude"], "router")?;
            let router_policy = match item.get("policy").and_then(Value::as_str) {
                None if item.get("policy").is_none() => RouterPolicy::Optional,
                Some("optional") => RouterPolicy::Optional,
                Some("required") => RouterPolicy::Required,
                Some("excluded") => RouterPolicy::Excluded,
                _ => {
                    return Err(ApiError::invalid(
                        "Router policy must be optional, required, or excluded",
                    ));
                }
            };
            let height_override = if origin == SiteOrigin::Manual {
                true
            } else {
                match item.get("height_override") {
                    None => false,
                    Some(value) => value.as_bool().ok_or_else(|| {
                        ApiError::invalid("MeshCore height_override must be a boolean")
                    })?,
                }
            };
            let height = if origin == SiteOrigin::Manual {
                match item.get("antenna_height_m") {
                    None => rf.router.height_agl_m,
                    Some(value) => number(value)
                        .filter(|v| *v > 0.0 && *v <= 1000.0)
                        .ok_or_else(|| {
                            ApiError::invalid(
                                "Proposed-router antenna height must be between 0 and 1,000 m",
                            )
                        })?,
                }
            } else if height_override {
                number(&item["antenna_height_m"])
                    .filter(|v| (0.1..=500.0).contains(v))
                    .ok_or_else(|| {
                        ApiError::invalid(
                            "MeshCore antenna-height override must be between 0.1 and 500 m",
                        )
                    })?
            } else {
                rf.router.height_agl_m
            };
            if !eligible || router_policy == RouterPolicy::Excluded {
                excluded_known += usize::from(origin == SiteOrigin::Known);
                continue;
            }
            locations.push(Location {
                id: id.to_owned(),
                name: item["name"]
                    .as_str()
                    .filter(|name| !name.is_empty())
                    .unwrap_or(id)
                    .to_owned(),
                coordinate,
                kind: SiteKind::Router,
                origin,
                policy: router_policy,
                height,
                height_override,
            });
        }
    }
    Ok((locations, excluded_known))
}

pub(crate) fn merged(defaults: &Value, plan: &Value, key: &str) -> ApiResult<Value> {
    let mut result = defaults[key].clone();
    if let Some(value) = plan.get(key) {
        merge(&mut result, value)?;
    }
    Ok(result)
}

pub(crate) fn terrain_candidates(plan: &Value, defaults: &Value) -> ApiResult<CandidateSettings> {
    candidates(&merged(defaults, plan, "candidates")?)
}

pub fn readiness_locations(plan: &Value, defaults: &Value) -> ApiResult<Vec<Location>> {
    let candidates = terrain_candidates(plan, defaults)?;
    Ok(locations(
        plan,
        &RfSettings::default(),
        candidates.infrastructure_policy,
    )?
    .0)
}

pub fn parse(mut plan: Value, defaults: &Value) -> ApiResult<RouteInputs> {
    let rf_payload = merged(defaults, &plan, "rf")?;
    let rf = rf(&rf_payload)?;
    let candidate_values = merged(defaults, &plan, "candidates")?;
    let requested = candidates(&candidate_values)?;
    let effort = match plan.get("search_effort").and_then(Value::as_str) {
        Some("quick") => SearchEffort::Quick,
        Some("balanced") => SearchEffort::Balanced,
        Some("thorough") => SearchEffort::Thorough,
        None if plan.get("search_effort").is_none() => SearchEffort::Balanced,
        _ => {
            return Err(ApiError::invalid(
                "search_effort must be quick, balanced, or thorough",
            ));
        }
    };
    let candidates = effort.resolve(&requested);
    let (locations, excluded_known) = locations(&plan, &rf, candidates.infrastructure_policy)?;
    if locations[0].coordinate == locations[1].coordinate {
        return Err(ApiError::invalid("Place endpoints at different locations"));
    }
    let fixed = &locations[2..];
    if fixed.len() > candidates.maximum_candidates {
        return Err(ApiError::invalid(
            "Selected routers exceed the Max candidates setting",
        ));
    }
    if fixed
        .iter()
        .filter(|site| site.policy == RouterPolicy::Required)
        .count()
        > candidates.maximum_solution_routers
    {
        return Err(ApiError::invalid(
            "Required routers exceed the Max total routers setting",
        ));
    }
    let known = fixed
        .iter()
        .filter(|site| site.origin == SiteOrigin::Known)
        .count();
    if plan.get("search_effort").is_none() {
        plan["search_effort"] = json!("balanced");
    }
    plan["resolved_search"] = json!({
        "candidate_limit": candidates.maximum_candidates,
        "requested_candidate_limit": requested.maximum_candidates,
        "neighbor_limit": candidates.maximum_neighbors_per_site,
        "generated_candidate_limit": if candidates.infrastructure_policy == InfrastructurePolicy::ExistingOnly {
            0
        } else {
            candidates.maximum_candidates - fixed.len()
        },
        "known_router_count": known,
        "excluded_router_count": excluded_known,
        "manual_router_count": fixed.len() - known,
        "infrastructure_policy": candidate_values["infrastructure_policy"],
    });
    Ok(RouteInputs {
        plan,
        locations,
        rf,
        rf_payload,
        candidates,
    })
}
