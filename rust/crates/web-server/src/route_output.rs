use repeaternet_optimizer::{
    NetworkSolution, OptimizationResult, PlanningSite, ROUTE_MODEL_VERSION, SiteOrigin,
};
use repeaternet_rf_core::{DirectionResult, HeightReference, LinkResult, SiteKind, TerrainProfile};
use repeaternet_terrain::Transformer;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

#[derive(Serialize, Deserialize)]
pub struct Alternative {
    pub rf_sites: Vec<repeaternet_rf_core::Site>,
    pub client_paths: Vec<Vec<String>>,
    pub summary: Value,
    pub sites: Vec<Value>,
    pub links: Vec<Value>,
}

pub struct RouteOutput {
    pub payload: Value,
    pub alternatives: Vec<Alternative>,
}

fn site(site: &PlanningSite, transform: &Transformer) -> Value {
    let rf = &site.rf;
    let coordinate = transform.reverse(rf.position).ok();
    json!({
        "id": rf.id, "x": rf.position.x, "y": rf.position.y,
        "latitude": coordinate.map(|p| p.latitude), "longitude": coordinate.map(|p| p.longitude),
        "kind": match rf.kind {
            SiteKind::EndpointA => "endpoint_a", SiteKind::EndpointB => "endpoint_b",
            SiteKind::Client => "client", SiteKind::Router => "router", SiteKind::Candidate => "candidate",
        },
        "ground_elevation_m": rf.ground_elevation_m,
        "surface_elevation_m": rf.surface_elevation_m,
        "antenna_height_m": rf.antenna_height_m,
        "terminal_clutter": rf.terminal_clutter,
        "clutter_height_m": rf.clutter_height_m,
        "height_reference": match rf.height_reference {
            HeightReference::GroundDtm => "ground_dtm", HeightReference::SurfaceDom => "surface_dom",
        },
        "terrain_slope": site.terrain_slope, "site_quality": site.site_quality, "locked": site.locked,
        "origin": match site.origin { SiteOrigin::Manual => "manual", SiteOrigin::Known => "known", SiteOrigin::Optimized => "optimized" },
        "required": site.is_required(), "enabled": site.enabled, "height_override": site.height_override,
    })
}

macro_rules! json_fields {
    ($name:expr, $($field:ident),+ $(,)?) => {
        json!({$(stringify!($field): $name.$field),+})
    };
}

fn direction(direction: &DirectionResult) -> Value {
    json_fields!(
        direction,
        source_id,
        target_id,
        distance_m,
        fspl_db,
        diffraction_loss_db,
        clutter_loss_db,
        model_correction_db,
        terminal_clutter_db,
        step_allowance_db,
        total_path_loss_db,
        tx_gain_dbi,
        rx_gain_dbi,
        received_power_dbm,
        sensitivity_dbm,
        raw_margin_db,
        usable_margin_db,
        departure_angle_deg,
        arrival_angle_deg,
        valid,
        tx_power_dbm,
        tx_feed_loss_db,
        rx_feed_loss_db,
        miscellaneous_loss_db
    )
}

fn profile(profile: &TerrainProfile) -> Value {
    let mut value = json_fields!(
        profile,
        distances_m,
        dtm_elevation_m,
        surface_elevation_m,
        earth_bulge_m,
        effective_obstruction_m,
        los_elevation_m,
        fresnel_radius_m,
        clearance_m,
        fresnel_clearance_ratio,
        surface_available
    );
    value["x"] = json!(
        profile
            .points
            .iter()
            .map(|point| point.x)
            .collect::<Vec<_>>()
    );
    value["y"] = json!(
        profile
            .points
            .iter()
            .map(|point| point.y)
            .collect::<Vec<_>>()
    );
    value
}

pub(crate) fn link(link: &LinkResult) -> Value {
    let mut value = json_fields!(
        link,
        source_id,
        target_id,
        distance_m,
        valid,
        los_clear,
        fresnel_clear,
        minimum_clearance_m,
        minimum_fresnel_clearance_ratio,
        minimum_clearance_distance_m,
        maximum_fresnel_radius_m,
        diffraction_model,
        diffraction_loss_db
    );
    value["forward"] = direction(&link.forward);
    value["reverse"] = direction(&link.reverse);
    value["dominant_obstacles"] = json!(
        link.dominant_obstacles
            .iter()
            .map(|item| json_fields!(item, distance_m, elevation_m, v, loss_db))
            .collect::<Vec<_>>()
    );
    value["profile"] = json!(link.profile.as_ref().map(profile));
    value["worst_margin_db"] = json!(link.worst_margin_db());
    value
}

fn alternative_id(solution: &NetworkSolution) -> String {
    let mut routers = solution.router_ids.clone();
    routers.sort();
    let signature =
        json!({"paths": solution.client_paths.values().collect::<Vec<_>>(), "routers": routers});
    let hash = crate::catalog::lower_hex(&Sha256::digest(crate::catalog::canonical_json(
        &signature, false,
    )));
    format!("alt-{}", &hash[..16])
}

fn alternative(solution: &NetworkSolution, transform: &Transformer, complete: bool) -> Alternative {
    Alternative {
        rf_sites: solution.sites.iter().map(|site| site.rf.clone()).collect(),
        client_paths: solution.client_paths.values().flatten().cloned().collect(),
        summary: json!({
            "id": alternative_id(solution), "name": solution.name,
            "router_count": solution.router_count(), "existing_router_count": solution.existing_router_count(),
            "proposed_router_count": solution.proposed_router_count(), "minimum_margin_db": solution.minimum_margin_db(),
            "primary_minimum_margin_db": solution.primary_links().map(LinkResult::worst_margin_db).reduce(f64::min),
            "primary_los_blocked_hops": solution.primary_links().filter(|link| !link.los_clear).count(),
            "total_link_distance_m": solution.total_link_distance_m(), "primary_distance_m": solution.primary_distance_m(),
            "requested_path_count": solution.requested_path_count, "achieved_path_count": solution.achieved_path_count,
            "resilient": solution.resilient(), "search_complete": complete,
            "route": solution.primary_route().iter().map(|item| site(item, transform)).collect::<Vec<_>>(),
        }),
        sites: solution
            .sites
            .iter()
            .map(|item| site(item, transform))
            .collect(),
        links: solution.links.iter().map(link).collect(),
    }
}

pub fn build(result: OptimizationResult, transform: &Transformer, rf: &Value) -> RouteOutput {
    let complete = result.search_complete;
    let alternatives: Vec<_> = result
        .alternatives
        .iter()
        .map(|item| alternative(item, transform, complete))
        .collect();
    let mut payload = json!({
        "found": result.found(), "router_count": 0, "existing_router_count": 0, "proposed_router_count": 0,
        "route": [], "links": [], "diagnostics": result.diagnostics,
        "elapsed_seconds": result.elapsed.as_secs_f64(), "search_complete": complete,
        "requested_path_count": 1, "achieved_path_count": 1, "rf_settings": rf,
        "candidates": result.candidates.iter().map(|item| site(item, transform)).collect::<Vec<_>>(),
        "active_alternative_id": null, "alternatives": [],
    });
    if let Some(index) = result.active_index {
        select(&mut payload, &alternatives, index);
    }
    RouteOutput {
        payload,
        alternatives,
    }
}

pub fn select(payload: &mut Value, alternatives: &[Alternative], index: usize) {
    let active = &alternatives[index];
    payload["active_alternative_id"] = active.summary["id"].clone();
    payload["route"] = json!(active.sites);
    payload["links"] = json!(active.links);
    for key in [
        "router_count",
        "existing_router_count",
        "proposed_router_count",
        "requested_path_count",
        "achieved_path_count",
    ] {
        payload[key] = active.summary[key].clone();
    }
    payload["alternatives"] = json!(
        alternatives
            .iter()
            .enumerate()
            .map(|(position, item)| {
                let mut summary = item.summary.clone();
                summary["selected"] = json!(position == index);
                summary
            })
            .collect::<Vec<_>>()
    );
}

pub fn summary(payload: &Value, plan: &str, terrain: &Value) -> Value {
    let mut summary = json!({
        "plan_fingerprint": plan, "terrain_fingerprint": terrain,
        "route_model_version": ROUTE_MODEL_VERSION,
        "saved_at": crate::route_http::now(),
    });
    for key in [
        "found",
        "router_count",
        "existing_router_count",
        "proposed_router_count",
        "elapsed_seconds",
        "search_complete",
        "active_alternative_id",
        "alternatives",
    ] {
        summary[key] = payload[key].clone();
    }
    if let Some(roads) = payload.get("road_polylines") {
        summary["road_polylines"] = roads.clone();
    }
    if let Some(alternatives) = summary["alternatives"].as_array_mut() {
        for item in alternatives {
            item.as_object_mut().unwrap().remove("route");
        }
    }
    summary
}
