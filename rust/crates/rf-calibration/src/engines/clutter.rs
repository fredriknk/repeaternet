//! Terminal corrections for paired ground and surface profiles.

use repeaternet_rf_core::{
    EARTH_MEAN_RADIUS_M, Site, calibration::TERMINAL_CLUTTER_RADIUS_M, p2108_terminal_correction_db,
};
use serde::Serialize;

use crate::dataset::{PathFeatures, ProfileRecord};

// P.833-10 Table 1 mixed-forest values at 466.475 and 949 MHz,
// interpolated linearly in log-frequency to 869.5 MHz.
const GAMMA_DB_PER_M: f64 = 0.163_840;
const MAX_WOODLAND_LOSS_DB: f64 = 25.452_880;

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Cover {
    Forest,
    Urban,
}

#[derive(Clone, Copy, Serialize)]
pub struct Terminal {
    pub representative_height_m: f64,
    pub cover: Cover,
    pub canopy_ray_m: f64,
}

pub struct Clutter {
    pub terminals: [Terminal; 2],
    pub p2108_db: Option<f64>,
    pub p833_db: f64,
}

fn p833(canopy_ray_m: f64) -> f64 {
    -MAX_WOODLAND_LOSS_DB * (-canopy_ray_m * GAMMA_DB_PER_M / MAX_WOODLAND_LOSS_DB).exp_m1()
}

fn terminal(
    profile: &ProfileRecord,
    dom: &[f64],
    from: &Site,
    to: &Site,
    representative_height_m: f64,
    k: f64,
    reverse: bool,
) -> Terminal {
    let last = dom.len() - 1;
    let at = |i: usize| if reverse { last - i } else { i };
    let excess = |i: usize| dom[at(i)] - profile.dtm[at(i)];
    let mut nearby =
        (1..=last).take_while(|i| *i as f64 * profile.step_m <= TERMINAL_CLUTTER_RADIUS_M);
    // A variable, sustained obstruction is a rough proxy for canopy.
    let cover = if nearby.any(|i| {
        let (left, right) = (excess(i - 1), excess(i));
        left > 3.0 && right > 3.0 && (right - left).abs() >= 1.0
    }) {
        Cover::Forest
    } else {
        Cover::Urban
    };

    let height = from.antenna_absolute_elevation_m();
    let rise = to.antenna_absolute_elevation_m() - height;
    let distance = profile.distance_m;
    let gap = |i: usize| {
        let d = if i == last {
            distance
        } else {
            i as f64 * profile.step_m
        };
        let bulge = d * (distance - d) / (2.0 * EARTH_MEAN_RADIUS_M * k);
        dom[at(i)] + bulge - (height + rise * d / distance)
    };
    let mut canopy_ray_m = 0.0;
    if matches!(cover, Cover::Forest) && excess(0) > 3.0 && gap(0) > 0.0 {
        let mut above_ray = gap(0);
        let mut cover_height = excess(0);
        let mut horizontal = distance;
        for i in 1..=last {
            let next_above_ray = gap(i);
            let next_cover_height = excess(i);
            let crosses = |left, right, threshold| {
                if right <= threshold {
                    (left - threshold) / (left - right)
                } else {
                    1.0
                }
            };
            if next_above_ray <= 0.0 || next_cover_height <= 3.0 {
                let ray_exit: f64 = crosses(above_ray, next_above_ray, 0.0);
                let cover_exit = crosses(cover_height, next_cover_height, 3.0);
                horizontal = (i as f64 - 1.0 + ray_exit.min(cover_exit)) * profile.step_m;
                break;
            }
            above_ray = next_above_ray;
            cover_height = next_cover_height;
        }
        canopy_ray_m = horizontal * (1.0 + (rise / distance).powi(2)).sqrt();
    }
    Terminal {
        representative_height_m,
        cover,
        canopy_ray_m,
    }
}

pub fn evaluate(
    profile: &ProfileRecord,
    features: &PathFeatures,
    tx: &Site,
    rx: &Site,
    freq: f64,
    k: f64,
) -> Option<Clutter> {
    let dom = profile.dom.as_deref()?;
    let reverse = tx.position.x > rx.position.x;
    let (a, b) = (features.a_clutter_height_m?, features.b_clutter_height_m?);
    let [tx_r, rx_r] = if reverse { [b, a] } else { [a, b] };
    let tx_clutter = terminal(profile, dom, tx, rx, tx_r, k, reverse);
    let rx_clutter = terminal(profile, dom, rx, tx, rx_r, k, !reverse);
    let p2108_db = (30.0..=3000.0).contains(&freq).then(|| {
        p2108_terminal_correction_db(freq, tx.antenna_height_m, tx_r)
            + p2108_terminal_correction_db(freq, rx.antenna_height_m, rx_r)
    });
    // A continuous woodland ray can belong to both ends. Count its length once.
    let ray_distance = tx
        .distance_to(rx)
        .hypot(tx.antenna_absolute_elevation_m() - rx.antenna_absolute_elevation_m());
    let p833_db = if tx_clutter.canopy_ray_m + rx_clutter.canopy_ray_m >= ray_distance {
        p833(ray_distance)
    } else {
        p833(tx_clutter.canopy_ray_m) + p833(rx_clutter.canopy_ray_m)
    };
    Some(Clutter {
        terminals: [tx_clutter, rx_clutter],
        p2108_db,
        p833_db,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn woodland_correction_is_bounded() {
        assert_eq!(p833(0.0), 0.0);
        assert!((p833(0.001) / 0.001 - GAMMA_DB_PER_M).abs() < 1e-6);
        assert_eq!(p833(10000.0), MAX_WOODLAND_LOSS_DB);
    }
}
