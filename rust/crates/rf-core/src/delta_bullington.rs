//! P.526-16 §§4.5.2, 3.2 and 3.1.1 in SI units.

use std::f64::consts::PI;

use crate::{
    bullington_loss_db, require_positive, DiffractionObstacle, DiffractionResult, RfError,
    EARTH_MEAN_RADIUS_M, SPEED_OF_LIGHT_M_S,
};

/// Delta-Bullington loss and its constituent diffraction losses.
#[derive(Clone, Debug, PartialEq)]
pub struct DeltaBullington {
    pub loss_db: f64,
    pub bullington_db: f64,
    pub smooth_bullington_db: f64,
    pub spherical_db: f64,
    pub smooth_agl_m: [f64; 2],
    pub obstacles: Vec<DiffractionObstacle>,
}

/// P.526 delta-Bullington on an endpoint-to-endpoint profile in metres.
/// Distances start at zero. Supply raw obstacle elevations with ground DTM at
/// both endpoints, the corresponding Earth bulges and the antenna LOS chord.
pub fn delta_bullington_loss_db(
    distances_m: &[f64],
    obstacle_elevations_m: &[f64],
    earth_bulge_m: &[f64],
    los_elevations_m: &[f64],
    frequency_mhz: f64,
    k_factor: f64,
) -> Result<DeltaBullington, RfError> {
    require_positive(k_factor, "K-factor must be positive")?;
    if distances_m.first() != Some(&0.0)
        || earth_bulge_m.len() != distances_m.len()
        || obstacle_elevations_m.len() != distances_m.len()
    {
        return Err(RfError::InvalidInput(
            "Delta-Bullington requires matching profiles starting at zero",
        ));
    }
    let curved: Vec<_> = obstacle_elevations_m
        .iter()
        .zip(earth_bulge_m)
        .map(|(height, bulge)| height + bulge)
        .collect();
    let actual = bullington_loss_db(distances_m, &curved, los_elevations_m, frequency_mhz)?;
    delta_from_bullington(
        distances_m,
        obstacle_elevations_m,
        earth_bulge_m,
        los_elevations_m,
        frequency_mhz,
        k_factor,
        actual,
    )
}

pub(crate) fn delta_from_bullington(
    x: &[f64],
    h: &[f64],
    earth_bulge_m: &[f64],
    los: &[f64],
    frequency_mhz: f64,
    k: f64,
    actual: DiffractionResult,
) -> Result<DeltaBullington, RfError> {
    let last = x.len() - 1;
    let d = x[last];
    let [st, sr] = smooth_earth_heights(x, h, los);
    let smooth_agl = [los[0] - st, los[last] - sr];
    let smooth_ray: Vec<_> = x
        .iter()
        .map(|&position| smooth_agl[0] + (smooth_agl[1] - smooth_agl[0]) * position / d)
        .collect();
    let smooth_bullington =
        bullington_loss_db(x, earth_bulge_m, &smooth_ray, frequency_mhz)?.loss_db;
    let wavelength = SPEED_OF_LIGHT_M_S / (frequency_mhz * 1e6);
    let spherical = spherical_earth_loss_db(d, smooth_agl, EARTH_MEAN_RADIUS_M * k, wavelength);
    Ok(DeltaBullington {
        loss_db: actual.loss_db + (spherical - smooth_bullington).max(0.0),
        bullington_db: actual.loss_db,
        smooth_bullington_db: smooth_bullington,
        spherical_db: spherical,
        smooth_agl_m: smooth_agl,
        obstacles: actual.obstacles,
    })
}

/// Fit endpoint-clipped smooth terrain to uncurved obstacles, with DTM at both endpoints.
/// Distances must start at zero and increase, with one elevation and chord sample per distance.
pub fn smooth_earth_heights(x: &[f64], h: &[f64], los: &[f64]) -> [f64; 2] {
    let last = x.len() - 1;
    let d = x[last];
    let (v1, v2) = x
        .windows(2)
        .zip(h.windows(2))
        .fold((0.0, 0.0), |(v1, v2), (x, h)| {
            let (a, b) = (x[0], x[1]);
            let (u, v) = (h[0], h[1]);
            (
                v1 + (b - a) * (u + v),
                v2 + (b - a) * (u * (2.0 * a + b) + v * (a + 2.0 * b)),
            )
        });
    let mut st = (2.0 * v1 * d - v2) / d.powi(2);
    let mut sr = (v2 - v1 * d) / d.powi(2);
    // The smooth fit uses raw terrain. Curvature is applied in the Bullington runs.
    let (highest, at, ar) = (1..last).fold((0.0_f64, 0.0_f64, 0.0_f64), |(highest, at, ar), i| {
        let above = h[i] - los[i];
        (
            highest.max(above),
            at.max(above / x[i]),
            ar.max(above / (d - x[i])),
        )
    });
    if highest > 0.0 {
        st -= highest * at / (at + ar);
        sr -= highest * ar / (at + ar);
    }
    [st.min(h[0]), sr.min(h[last])]
}

/// P.526 spherical diffraction over land for a path length, antenna heights above
/// the fitted smooth earth, effective Earth radius and wavelength, all in metres.
pub fn spherical_earth_loss_db(d: f64, [ht, hr]: [f64; 2], radius: f64, wavelength: f64) -> f64 {
    let first_term = |a: f64| {
        // Vertical polarization over land with beta = 1.
        let (eps, sigma) = (22.0_f64, 0.003);
        let q = 60.0 * wavelength * sigma;
        let admittance = (2.0 * PI * a / wavelength).powf(-1.0 / 3.0)
            * ((eps - 1.0).powi(2) + q * q).powf(-0.25)
            * eps.hypot(q);
        let x = (PI / (wavelength * a * a)).cbrt() * d;
        let fx = if x >= 1.6 {
            11.0 + 10.0 * x.log10() - 17.6 * x
        } else {
            -20.0 * x.log10() - 5.6488 * x.powf(1.425)
        };
        let gain = |height: f64| {
            let b = 2.0 * (PI.powi(2) / (wavelength.powi(2) * a)).cbrt() * height;
            let gy = if b > 2.0 {
                17.6 * (b - 1.1).sqrt() - 5.0 * (b - 1.1).log10() - 8.0
            } else {
                20.0 * (b + 0.1 * b.powi(3)).log10()
            };
            gy.max(2.0 + 20.0 * admittance.log10())
        };
        -fx - gain(ht) - gain(hr)
    };

    let root_heights = ht.sqrt() + hr.sqrt();
    if d >= (2.0 * radius).sqrt() * root_heights {
        return first_term(radius);
    }
    let m = d.powi(2) / (4.0 * radius * (ht + hr));
    let c = (ht - hr) / (ht + hr);
    let angle = (1.5 * c * (3.0 * m / (m + 1.0).powi(3)).sqrt())
        .clamp(-1.0, 1.0)
        .acos();
    let b = 2.0 * ((m + 1.0) / (3.0 * m)).sqrt() * (PI / 3.0 + angle / 3.0).cos();
    let d1 = d * (1.0 + b) / 2.0;
    let d2 = d - d1;
    let clearance =
        ((ht - d1.powi(2) / (2.0 * radius)) * d2 + (hr - d2.powi(2) / (2.0 * radius)) * d1) / d;
    let required = 0.552 * (d1 * d2 * wavelength / d).sqrt();
    if clearance >= required {
        return 0.0;
    }
    let marginal_radius = 0.5 * (d / root_heights).powi(2);
    (1.0 - clearance / required) * first_term(marginal_radius).max(0.0)
}
