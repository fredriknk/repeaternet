use std::path::Path;

use repeaternet_rf_core::{
    EvaluationOptions, LinkEvaluator, RfSettings, SPEED_OF_LIGHT_M_S, bullington_loss_db,
    earth_bulge_m, sample_profile,
};
use serde::Deserialize;

use super::*;
use crate::dataset::SCHEMA_VERSION;

const FREQ: f64 = 869.5;

fn sampled(distance: f64, ground: Vec<f64>, tx: f64, rx: f64) -> ProfileRecord {
    let last = ground.len() - 1;
    let step = distance / last as f64;
    let end = |node: &str, x: f64, ground_m| ProfileEnd {
        node: node.into(),
        lat: 0.0,
        lon: 0.0,
        x,
        y: 0.0,
        ground_m,
        surface_m: None,
    };
    ProfileRecord {
        schema: SCHEMA_VERSION,
        path_id: "a~b".into(),
        link_ids: Vec::new(),
        terrain: "synthetic".into(),
        crs_epsg: 25832,
        resolution_m: 10.0,
        step_m: step,
        distance_m: distance,
        a: end("a", 0.0, tx),
        b: end("b", distance, rx),
        dtm: ground,
        dom: None,
        missing_fraction: 0.0,
        terrain_fingerprint: String::new(),
    }
}

fn check(actual: f64, expected: f64, tolerance: f64) {
    assert!(
        (actual - expected).abs() < tolerance,
        "{actual} expected {expected}"
    );
}

#[test]
fn short_flat_path_has_no_excess() {
    let stored = sampled(100.0, vec![0.0; 11], 0.0, 0.0);
    let terrain = StoredTerrain {
        profile: &stored,
        surface: false,
    };
    let a = site(&stored.a, 0.0, 3.0);
    let b = site(&stored.b, stored.distance_m, 3.0);
    let p = sample_profile(&terrain, &a, &b, FREQ, 1.0, None).unwrap();
    let clearance = Clearance::from_profile(&p, FREQ);
    assert!(clearance.los);
    assert!(clearance.direct_v < -0.78);
    assert_eq!(delta::evaluate(&p, FREQ, 1.0, 0.0).unwrap().loss, 0.0);
    let mut z = p.effective_obstruction_m;
    z[0] = 3.0;
    z[10] = 3.0;
    let multi = multi_edge::evaluate(&p.distances_m, &z, FREQ);
    assert!(multi.edges.is_empty());
    assert_eq!(
        [multi.deygout, multi.epstein_peterson, multi.giovaneli],
        [0.0; 3]
    );
}

#[test]
fn single_edge_matches_knife_edge_formula() {
    let x = [0.0, 50.0, 100.0];
    let z = [0.0, 5.0, 0.0];
    let v = 5.0 * (8.0 * FREQ * 1e6 / (100.0 * SPEED_OF_LIGHT_M_S)).sqrt();
    let j = 6.9 + 20.0 * ((v - 0.1).hypot(1.0) + v - 0.1).log10();
    let multi = multi_edge::evaluate(&x, &z, FREQ);
    for loss in [multi.deygout, multi.epstein_peterson, multi.giovaneli] {
        check(loss, j, 1e-10);
    }
    assert_eq!(multi.edges.len(), 1);
    let b = bullington_loss_db(&x, &z, &[0.0; 3], FREQ).unwrap();
    check(b.loss_db, j + (1.0 - (-j / 6.0).exp()) * 10.002, 1e-10);
}

#[test]
fn two_ridges_match_hand_calculation() {
    // Wavelength 1 m. First ridge is primary. EP sees it 12.5 m above
    // the left terminal to second ridge chord. Giovaneli's right support
    // projects to 10 m at the receiver, giving 20 - 10/3 m residual.
    let m = multi_edge::evaluate(
        &[0.0, 1000.0, 2000.0, 3000.0],
        &[0.0, 20.0, 15.0, 0.0],
        SPEED_OF_LIGHT_M_S / 1e6,
    );
    check(m.deygout, 23.291_301_363_541_344, 1e-10);
    check(m.epstein_peterson, 21.265_530_323_549_473, 1e-10);
    check(m.giovaneli, 22.114_694_382_382_07, 1e-10);
    assert_eq!(
        m.edges
            .iter()
            .map(|edge| edge.distance_m)
            .collect::<Vec<_>>(),
        [1000.0, 2000.0]
    );
}

#[test]
fn smooth_earth_matches_p526_reference() {
    let stored = sampled(30_000.0, vec![0.0; 3001], 0.0, 0.0);
    let a = site(&stored.a, 0.0, 3.0);
    let b = site(&stored.b, stored.distance_m, 3.0);
    let terrain = StoredTerrain {
        profile: &stored,
        surface: false,
    };
    let p = sample_profile(&terrain, &a, &b, FREQ, 1.0, None).unwrap();
    let b = bullington_loss_db(
        &p.distances_m,
        &p.effective_obstruction_m,
        &p.los_elevation_m,
        FREQ,
    )
    .unwrap();
    let d = delta::evaluate(&p, FREQ, 1.0, b.loss_db).unwrap();
    check(d.smooth_bullington, 19.780, 0.05);
    check(d.spherical, 52.454, 0.05);
    check(d.loss, 52.456, 0.05);
}

#[test]
fn stored_terrain_matches_rfcore_on_the_same_samples() {
    struct NativeDom;
    impl TerrainSource for NativeDom {
        fn resolution_m(&self) -> f64 {
            10.0
        }
        fn has_surface(&self) -> bool {
            true
        }
        fn sample(
            &self,
            points: &[repeaternet_rf_core::Point],
            layer: TerrainLayer,
        ) -> Result<Vec<f64>, RfError> {
            Ok(points
                .iter()
                .map(|p| {
                    let ground = p.x / 20.0;
                    if layer == TerrainLayer::SurfaceDom && (p.x.round() - 100.0).abs() <= 10.0 {
                        ground + 30.0
                    } else {
                        ground
                    }
                })
                .collect())
        }
    }

    let mut stored = sampled(
        200.0,
        (0..=20).map(|i| f64::from(i) / 2.0).collect(),
        0.0,
        10.0,
    );
    let mut dom = stored.dtm.clone();
    for height in &mut dom[9..=11] {
        *height += 30.0;
    }
    stored.dom = Some(dom);
    let a = site(&stored.a, 0.0, 2.0);
    let b = site(&stored.b, 200.0, 10.0);
    let terrain = StoredTerrain {
        profile: &stored,
        surface: true,
    };
    let rf = RfSettings {
        frequency_mhz: FREQ,
        k_factor: 4.0 / 3.0,
        ..RfSettings::default()
    };
    for (tx, rx) in [(&a, &b), (&b, &a)] {
        let options = EvaluationOptions::default();
        let expected = LinkEvaluator::new(&NativeDom, &rf)
            .evaluate(tx, rx, options)
            .unwrap();
        let got = LinkEvaluator::new(&terrain, &rf)
            .evaluate(tx, rx, options)
            .unwrap();
        check(got.diffraction_loss_db, expected.diffraction_loss_db, 1e-10);
        assert_eq!(got.los_clear, expected.los_clear);
        let (got, expected) = (&got.dominant_obstacles[0], &expected.dominant_obstacles[0]);
        check(got.v, expected.v, 1e-10);
        check(got.distance_m, expected.distance_m, 1e-10);
    }
}

#[test]
#[ignore = "requires the local RF review's private terrain fixture"]
fn drammenmesh_link_zero_matches_review() {
    const FIXTURE: &str =
        r"C:\Users\fnk\pythonprojects\repeaternet\rust\target\rf-review\drammenmesh_links.json";

    #[derive(Deserialize)]
    struct Link {
        profile: ReviewProfile,
    }
    #[derive(Deserialize)]
    struct ReviewProfile {
        distances_m: Vec<f64>,
        dtm_elevation_m: Vec<f64>,
        los_elevation_m: Vec<f64>,
    }

    let path = Path::new(FIXTURE);
    if !path.exists() {
        eprintln!("Skipped missing local review fixture");
        return;
    }
    let links: Vec<Link> = serde_json::from_reader(std::fs::File::open(path).unwrap()).unwrap();
    let p = &links[0].profile;
    let x = &p.distances_m;
    let last = x.len() - 1;
    let mut z: Vec<_> = x
        .iter()
        .zip(&p.dtm_elevation_m)
        .map(|(&at, &height)| height + earth_bulge_m(at, x[last], 1.0).unwrap())
        .collect();
    let bull = bullington_loss_db(x, &z, &p.los_elevation_m, FREQ).unwrap();
    z[0] = p.los_elevation_m[0];
    z[last] = p.los_elevation_m[last];
    let m = multi_edge::evaluate(x, &z, FREQ);
    check(bull.loss_db, 35.02, 0.05);
    check(m.deygout, 38.18, 0.05);
    check(m.epstein_peterson, 21.73, 0.05);
    check(m.giovaneli, 35.88, 0.05);
}
