use std::collections::{BTreeMap, HashSet};

use repeaternet_optimizer::{
    CandidateSettings, InfrastructurePolicy, OptimizationPriority, RoadAccessClasses, RouterPolicy,
    SearchEffort, SiteOrigin,
};
use repeaternet_rf_core::{
    AntennaPattern, AntennaSettings, DiffractionModel, LoRaSettings, ObstacleSurface, RfSettings,
    SiteKind, SiteRadioOverrides, TerminalClutter, ValidationMode,
    calibration::CERTIFICATION_MAX_STEP_M,
};
use repeaternet_terrain::{LonLat, Point, Transformer};
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
    pub terminal_clutter: TerminalClutter,
}

pub struct RouteInputs {
    pub plan: Value,
    pub mesh: bool,
    pub client_count: usize,
    pub locations: Vec<Location>,
    pub rf: RfSettings,
    pub rf_payload: Value,
    pub candidates: CandidateSettings,
}

pub fn project_exclusions(plan: &Value, transform: &Transformer) -> ApiResult<Vec<Vec<Point>>> {
    let Some(value) = plan.get("exclusions_wgs84") else {
        return Ok(Vec::new());
    };
    let invalid = || {
        ApiError::invalid(
            "Exclusions must be polygons with at least three latitude and longitude pairs",
        )
    };
    value
        .as_array()
        .ok_or_else(invalid)?
        .iter()
        .map(|polygon| {
            polygon
                .as_array()
                .filter(|points| points.len() >= 3)
                .ok_or_else(invalid)?
                .iter()
                .map(|point| {
                    let pair = point
                        .as_array()
                        .filter(|pair| pair.len() == 2)
                        .ok_or_else(invalid)?;
                    let location = coordinate(&pair[0], &pair[1], "exclusion point")?;
                    transform
                        .forward(location)
                        .map_err(crate::terrain_http::terrain_invalid)
                })
                .collect()
        })
        .collect()
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
        terminal_clutter: terminal_clutter(&value["terminal_clutter"])?,
    })
}

fn terminal_clutter(value: &Value) -> ApiResult<TerminalClutter> {
    match value.as_str() {
        Some("none") => Ok(TerminalClutter::None),
        Some("p2108_dom") => Ok(TerminalClutter::P2108Dom),
        _ => Err(ApiError::invalid(
            "terminal_clutter must be none or p2108_dom",
        )),
    }
}

pub(crate) fn router_clutter_overrides(plan: &Value) -> ApiResult<BTreeMap<&str, TerminalClutter>> {
    let mut overrides = BTreeMap::new();
    for key in ["manual_routers", "known_routers"] {
        let Some(items) = plan.get(key) else { continue };
        for item in items
            .as_array()
            .ok_or_else(|| ApiError::invalid(format!("{key} must be a list")))?
        {
            if let Some(value) = item.get("terminal_clutter") {
                let model = terminal_clutter(value)?;
                let id = item["id"].as_str().ok_or_else(|| {
                    ApiError::invalid(format!("Each {key} entry needs a valid id"))
                })?;
                overrides.insert(id, model);
            }
        }
    }
    Ok(overrides)
}

pub(crate) fn rf(value: &Value) -> ApiResult<RfSettings> {
    let n = |key: &str| value[key].as_f64().unwrap();
    let validation_mode = choice!(value, "validation_mode", {
        "rf_propagation" => ValidationMode::Propagation, "strict_los" => ValidationMode::StrictLos,
    });
    let diffraction_model = choice!(value, "diffraction_model", {
        "bullington" => DiffractionModel::Bullington,
        "delta_bullington" => DiffractionModel::DeltaBullington,
        "deygout3" => DiffractionModel::Deygout3,
    });
    let obstacle_surface = choice!(value, "obstacle_surface", {
        "ground" => ObstacleSurface::Ground, "surface" => ObstacleSurface::Surface,
    });
    let model_correction_db = number(&value["model_correction_db"])
        .filter(|db| (0.0..=20.0).contains(db))
        .ok_or_else(|| ApiError::invalid("model_correction_db must be between 0 and 20 dB"))?;
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
        diffraction_model,
        obstacle_surface,
        model_correction_db,
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
        site_overrides: site_radio_overrides(value.get("site_overrides"))?,
    };
    settings
        .effective_sensitivity_dbm()
        .map_err(|e| ApiError::invalid(e.to_string()))?;
    Ok(settings)
}

fn site_radio_overrides(value: Option<&Value>) -> ApiResult<BTreeMap<String, SiteRadioOverrides>> {
    let Some(value) = value else {
        return Ok(BTreeMap::new());
    };
    let fields = value
        .as_object()
        .ok_or_else(|| ApiError::invalid("Site radio overrides must be an object"))?;
    fields
        .iter()
        .map(|(id, profile)| {
            let invalid = || ApiError::invalid("Invalid site radio override");
            let field = |name, min, max| {
                profile
                    .get(name)
                    .map(|value| {
                        number(value)
                            .filter(|n| (min..=max).contains(n))
                            .ok_or_else(invalid)
                    })
                    .transpose()
            };
            if id.is_empty() || id.chars().count() > 64 || !profile.is_object() {
                return Err(invalid());
            }
            Ok((
                id.clone(),
                SiteRadioOverrides {
                    gain_dbi: field("gain_dbi", -100.0, 100.0)?,
                },
            ))
        })
        .collect()
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
        ("final_sample_step_m", 0.0, CERTIFICATION_MAX_STEP_M),
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
    if i("maximum_hops") == 0 {
        return Err(ApiError::invalid("Maximum hops must be at least 1"));
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
        maximum_neighbors_per_site: i("maximum_neighbors_per_site"),
        exhaustive_screening: true,
        maximum_new_routers: i("maximum_new_routers"),
        maximum_hops: i("maximum_hops"),
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

pub(crate) fn mesh_mode(plan: &Value) -> ApiResult<bool> {
    match plan.get("mode").and_then(Value::as_str) {
        None if plan.get("mode").is_none() => Ok(false),
        Some("endpoints") => Ok(false),
        Some("mesh") => Ok(true),
        _ => Err(ApiError::invalid("Mode must be endpoints or mesh")),
    }
}

pub(crate) fn validate_plan_mode(plan: &mut Value) -> ApiResult<()> {
    if mesh_mode(plan)? || plan.get("clients").is_some() {
        mesh_clients(plan, &AntennaSettings::default(), false)?;
    }
    if let Some(fields) = plan.as_object_mut() {
        fields.remove("mesh_topology");
    }
    Ok(())
}

pub(crate) fn mesh_clients(
    plan: &Value,
    antenna: &AntennaSettings,
    for_search: bool,
) -> ApiResult<Vec<Location>> {
    let list = plan["clients"]
        .as_array()
        .ok_or_else(|| ApiError::invalid("Clients must be a list"))?;
    if for_search && list.len() < 2 {
        return Err(ApiError::invalid("Add at least two clients to find a mesh"));
    }
    let mut ids = HashSet::new();
    let mut points = HashSet::new();
    list.iter()
        .map(|item| {
            let id = item["id"]
                .as_str()
                .filter(|id| {
                    id.starts_with("CL-")
                        && id.len() > 3
                        && id.len() <= 64
                        && id
                            .bytes()
                            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
                })
                .ok_or_else(|| ApiError::invalid("Each client needs a valid id"))?;
            if !ids.insert(id) {
                return Err(ApiError::invalid("Client ids must be unique"));
            }
            let name = item["name"]
                .as_str()
                .map(str::trim)
                .filter(|name| {
                    !name.is_empty()
                        && name.chars().count() <= 80
                        && !name.chars().any(char::is_control)
                })
                .ok_or_else(|| {
                    ApiError::invalid("Each client needs a name of up to 80 characters")
                })?;
            let coordinate = coordinate(&item["latitude"], &item["longitude"], name)?;
            let key = [coordinate.latitude, coordinate.longitude].map(|v| (v + 0.0).to_bits());
            if !points.insert(key) {
                return Err(ApiError::invalid("Place clients at different locations"));
            }
            let height = number(&item["antenna_height_m"])
                .filter(|v| (0.1..=1000.0).contains(v))
                .ok_or_else(|| {
                    ApiError::invalid(format!(
                        "{name} antenna height must be between 0.1 and 1,000 m"
                    ))
                })?;
            Ok(Location {
                id: id.to_owned(),
                name: name.to_owned(),
                coordinate,
                kind: SiteKind::Client,
                origin: SiteOrigin::Optimized,
                policy: RouterPolicy::Optional,
                height,
                height_override: true,
                terminal_clutter: antenna.terminal_clutter,
            })
        })
        .collect()
}

fn locations(
    plan: &Value,
    rf: &RfSettings,
    policy: InfrastructurePolicy,
    include_excluded: bool,
) -> ApiResult<(Vec<Location>, usize)> {
    let clutter_overrides = router_clutter_overrides(plan)?;
    let mesh = mesh_mode(plan)?;
    let mut locations = if mesh {
        mesh_clients(plan, &rf.endpoint_a, !include_excluded)?
    } else {
        Vec::new()
    };
    let mut ids = HashSet::new();
    let mut excluded_known = 0;
    for (key, id, kind, antenna) in [
        ("a", "A", SiteKind::EndpointA, &rf.endpoint_a),
        ("b", "B", SiteKind::EndpointB, &rf.endpoint_b),
    ]
    .into_iter()
    .filter(|_| !mesh)
    {
        let fields = plan[key]
            .as_array()
            .filter(|values| values.len() == 2)
            .ok_or_else(|| {
                ApiError::invalid(format!("{key} must contain latitude and longitude"))
            })?;
        locations.push(Location {
            id: id.into(),
            name: plan["endpoint_names"][key]
                .as_str()
                .filter(|name| !name.is_empty())
                .map_or_else(|| format!("Endpoint {id}"), str::to_owned),
            coordinate: coordinate(&fields[0], &fields[1], key)?,
            kind,
            origin: SiteOrigin::Optimized,
            policy: RouterPolicy::Optional,
            height: antenna.height_agl_m,
            height_override: false,
            terminal_clutter: antenna.terminal_clutter,
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
            if !include_excluded && (!eligible || router_policy == RouterPolicy::Excluded) {
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
                terminal_clutter: clutter_overrides
                    .get(id)
                    .copied()
                    .unwrap_or(rf.router.terminal_clutter),
            });
        }
    }
    Ok((locations, excluded_known))
}

pub(crate) fn merged(defaults: &Value, plan: &Value, key: &str) -> ApiResult<Value> {
    let mut result = defaults[key].clone();
    if let Some(value) = plan.get(key) {
        if key == "rf" && value.get("site_overrides").is_some() {
            let mut value = without_legacy_setting(key, value).into_owned();
            let overrides = value
                .as_object_mut()
                .unwrap()
                .remove("site_overrides")
                .unwrap();
            merge(&mut result, &value)?;
            result["site_overrides"] = overrides;
        } else {
            merge(&mut result, &without_legacy_setting(key, value))?;
        }
    }
    Ok(result)
}

/// Fall back to current defaults for settings retired from saved plans.
fn without_legacy_setting<'a>(key: &str, value: &'a Value) -> std::borrow::Cow<'a, Value> {
    let field = match key {
        "candidates" if value.get("maximum_solution_routers").is_some() => {
            "maximum_solution_routers"
        }
        // Before RF calibration Bullington was the only model. Those plans have
        // no obstacle_surface and should inherit the calibrated default.
        "rf" if value.get("obstacle_surface").is_none()
            && value.get("diffraction_model") == Some(&json!("bullington")) =>
        {
            "diffraction_model"
        }
        _ => return std::borrow::Cow::Borrowed(value),
    };
    let mut value = value.clone();
    if let Some(fields) = value.as_object_mut() {
        fields.remove(field);
    }
    std::borrow::Cow::Owned(value)
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
        false,
    )?
    .0)
}

pub fn parse(plan: Value, defaults: &Value) -> ApiResult<RouteInputs> {
    parse_with_scope(plan, defaults, false)
}

pub fn parse_for_placement(plan: Value, defaults: &Value) -> ApiResult<RouteInputs> {
    parse_with_scope(plan, defaults, true)
}

fn parse_with_scope(mut plan: Value, defaults: &Value, placement: bool) -> ApiResult<RouteInputs> {
    validate_plan_mode(&mut plan)?;
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
    let mesh = mesh_mode(&plan)?;
    let (locations, excluded_known) =
        locations(&plan, &rf, candidates.infrastructure_policy, placement)?;
    if !mesh && !placement && locations[0].coordinate == locations[1].coordinate {
        return Err(ApiError::invalid("Place endpoints at different locations"));
    }
    let client_count = locations
        .iter()
        .take_while(|site| site.kind != SiteKind::Router)
        .count();
    let fixed = &locations[client_count..];
    if !mesh
        && !placement
        && fixed
            .iter()
            .filter(|site| site.policy == RouterPolicy::Required)
            .count()
            >= candidates.maximum_hops
    {
        return Err(ApiError::invalid(
            "Increase Maximum hops to fit all required routers between the endpoints",
        ));
    }
    if let Some(fields) = plan.get_mut("candidates").and_then(Value::as_object_mut) {
        fields.remove("maximum_solution_routers");
    }
    let known = fixed
        .iter()
        .filter(|site| site.origin == SiteOrigin::Known)
        .count();
    if plan.get("search_effort").is_none() {
        plan["search_effort"] = json!("balanced");
    }
    plan["resolved_search"] = json!({
        "candidates_per_cell": candidates.candidates_per_cell,
        "requested_candidates_per_cell": requested.candidates_per_cell,
        "neighbor_limit": candidates.maximum_neighbors_per_site,
        "generates_candidates": candidates.infrastructure_policy != InfrastructurePolicy::ExistingOnly && candidates.maximum_new_routers > 0,
        "known_router_count": known,
        "excluded_router_count": excluded_known,
        "manual_router_count": fixed.len() - known,
        "infrastructure_policy": candidate_values["infrastructure_policy"],
    });
    Ok(RouteInputs {
        plan,
        mesh,
        client_count,
        locations,
        rf,
        rf_payload,
        candidates,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mesh_validates_a_draft_and_uses_each_named_clients_height() {
        let defaults = crate::catalog::dashboard_defaults();
        let mut plan = json!({
            "mode": "mesh",
            "clients": [
                {"id": "CL-1", "name": "Frank", "latitude": 59.9, "longitude": 10.7, "antenna_height_m": 1.5},
                {"id": "CL-2", "name": "Bob", "latitude": 59.95, "longitude": 10.8, "antenna_height_m": 3},
                {"id": "CL-3", "name": "Alice", "latitude": 59.96, "longitude": 10.8, "antenna_height_m": 2}
            ],
            "mesh_topology": {"sites": []}
        });
        let inputs = parse(plan.clone(), &defaults).unwrap();
        assert!(inputs.mesh);
        assert_eq!(inputs.client_count, 3);
        assert_eq!(inputs.locations[1].name, "Bob");
        assert_eq!(inputs.locations[1].height, 3.0);
        assert_eq!(inputs.locations[1].kind, SiteKind::Client);
        assert!(inputs.plan.get("mesh_topology").is_none());
        plan["clients"].as_array_mut().unwrap().truncate(1);
        assert!(validate_plan_mode(&mut plan).is_ok());
        assert!(parse(plan, &defaults).is_err());
    }

    #[test]
    fn legacy_router_limit_loads_with_new_defaults() {
        let defaults = crate::catalog::dashboard_defaults();
        let plan = json!({
            "a": [59.9139, 10.7522],
            "b": [59.95, 10.85],
            "candidates": {"maximum_solution_routers": 0, "refine_radius_m": 100}
        });
        let inputs = parse(plan, &defaults).unwrap();
        assert_eq!(inputs.candidates.maximum_new_routers, 6);
        assert_eq!(inputs.candidates.maximum_hops, 7);
        assert_eq!(inputs.candidates.refine_radius_m, 100.0);
        assert!(
            inputs.plan["candidates"]
                .get("maximum_solution_routers")
                .is_none()
        );
    }
}
