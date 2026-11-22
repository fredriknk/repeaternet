use axum::{Json, extract::State};
use repeaternet_rf_core::{
    EvaluationOptions, HeightReference, LinkEvaluator, Site, SiteKind,
    calibration::CERTIFICATION_MAX_STEP_M,
};
use repeaternet_terrain::{
    TerrainLayer, TerrainSource, Transformer, profile::bounded_profile_step,
};
use serde_json::{Value, json};

use crate::{
    api::{ApiError, ApiResult, ObjectBody, Workspace, active_project},
    catalog::AppState,
    route_inputs, route_output,
    terrain_http::{TerrainContext, terrain_invalid},
};

const MAX_PROFILE_SAMPLES: usize = 65_536;

pub async fn evaluate(
    State(app): State<AppState>,
    workspace: Workspace,
    ObjectBody(body): ObjectBody,
) -> ApiResult<Json<Value>> {
    let source = body.get("source").unwrap_or(&Value::Null);
    let target = body.get("target").unwrap_or(&Value::Null);
    let coordinates = [
        route_inputs::coordinate(&source["latitude"], &source["longitude"], "source")?,
        route_inputs::coordinate(&target["latitude"], &target["longitude"], "target")?,
    ];
    let mut source = site(source, "source")?;
    let mut target = site(target, "target")?;
    let include_profile = match body.get("include_profile") {
        Some(value) => value
            .as_bool()
            .ok_or_else(|| ApiError::invalid("include_profile must be a boolean"))?,
        None => false,
    };
    let project_id = body.get("project_id").cloned();
    let (context, rf, mut source, mut target) = workspace
        .run(app, move |app, key, session| {
            if project_id.is_some_and(|id| id.as_str() != Some(&session.project_id)) {
                return Err(ApiError::conflict("The active project changed"));
            }
            let context = TerrainContext::capture(app, key, session, false)?;
            let plan = active_project(&app.store, key, session)?.plan;
            let rf = route_inputs::rf(&route_inputs::merged(&app.defaults, &plan, "rf")?)?;
            let overrides = route_inputs::router_clutter_overrides(&plan)?;
            for site in [&mut source, &mut target] {
                site.terminal_clutter = overrides
                    .get(site.id.as_str())
                    .filter(|_| site.kind == SiteKind::Router)
                    .copied()
                    .unwrap_or_else(|| rf.antenna_settings(site.kind).terminal_clutter);
            }
            Ok((context, rf, source, target))
        })
        .await?;

    tokio::task::spawn_blocking(move || {
        let terrain = context.open()?;
        let transform = Transformer::new(terrain.crs()).map_err(terrain_invalid)?;
        let points = [
            transform.forward(coordinates[0]).map_err(terrain_invalid)?,
            transform.forward(coordinates[1]).map_err(terrain_invalid)?,
        ];
        let ground = terrain
            .sample(&points, TerrainLayer::GroundDtm)
            .map_err(terrain_invalid)?;
        if ground.iter().any(|value| !value.is_finite()) {
            return Err(ApiError::invalid(
                "Ground terrain is missing at a link site",
            ));
        }
        for ((site, position), elevation) in [&mut source, &mut target]
            .into_iter()
            .zip(points)
            .zip(ground)
        {
            site.position = position;
            site.ground_elevation_m = elevation;
            site.sample_terminal_clutter(&terrain)
                .map_err(terrain_invalid)?;
        }
        if !bounded_profile_step(
            source.distance_to(&target),
            CERTIFICATION_MAX_STEP_M,
            terrain.resolution_m(),
            MAX_PROFILE_SAMPLES,
        )
        .map_err(terrain_invalid)?
        .fits()
        {
            return Err(ApiError::invalid(
                "Link profile exceeds 65,536 terrain samples",
            ));
        }
        let link = LinkEvaluator::new(&terrain, &rf)
            .evaluate(
                &source,
                &target,
                EvaluationOptions {
                    sample_step_m: Some(CERTIFICATION_MAX_STEP_M),
                    include_profile,
                    ..EvaluationOptions::default()
                },
            )
            .map_err(terrain_invalid)?;
        let mut payload = route_output::link(&link);
        payload["required_fresnel_clearance"] = json!(rf.required_fresnel_clearance);
        Ok(Json(payload))
    })
    .await
    .map_err(ApiError::internal)?
}

fn site(value: &Value, role: &str) -> ApiResult<Site> {
    let kind = match value["kind"].as_str() {
        Some("endpoint_a") => SiteKind::EndpointA,
        Some("endpoint_b") => SiteKind::EndpointB,
        Some("router") => SiteKind::Router,
        Some("candidate") => SiteKind::Candidate,
        Some("client") => SiteKind::Client,
        _ => return Err(ApiError::invalid(format!("Invalid {role} site kind"))),
    };
    let height = value["antenna_height_m"]
        .as_f64()
        .filter(|height| *height > 0.0 && *height <= 1000.0)
        .ok_or_else(|| {
            ApiError::invalid(format!(
                "The {role} antenna height must be above 0 m and at most 1,000 m"
            ))
        })?;
    let id = match value.get("id") {
        Some(value) => value
            .as_str()
            .filter(|id| !id.is_empty() && id.chars().count() <= 64)
            .ok_or_else(|| ApiError::invalid(format!("Invalid {role} site ID")))?,
        None => role,
    };
    Ok(Site {
        kind,
        antenna_height_m: height,
        height_reference: HeightReference::GroundDtm,
        ..Site::new(id, 0.0, 0.0)
    })
}
