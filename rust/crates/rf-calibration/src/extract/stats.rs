use crate::dataset::Stat;

pub(super) fn quantile(sorted: &[f64], fraction: f64) -> f64 {
    let index = fraction * (sorted.len() - 1) as f64;
    sorted[index.floor() as usize]
        + index.fract() * (sorted[index.ceil() as usize] - sorted[index.floor() as usize])
}

pub(super) fn summarize(mut values: Vec<f64>) -> Option<Stat> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    let median = quantile(&values, 0.5);
    let mut deviations: Vec<_> = values.iter().map(|value| (value - median).abs()).collect();
    deviations.sort_by(f64::total_cmp);
    Some(Stat {
        n: values.len() as u32,
        median,
        mean: values.iter().sum::<f64>() / values.len() as f64,
        p10: quantile(&values, 0.1),
        p90: quantile(&values, 0.9),
        mad: 1.4826 * quantile(&deviations, 0.5),
        min: values[0],
        max: values[values.len() - 1],
    })
}

pub(super) fn split_power(rssi: f64, snr: f64) -> (f64, f64) {
    // Factor out the larger term before taking the log to avoid overflow.
    let correction = 10.0 * 10_f64.powf(-snr.abs() / 10.0).ln_1p() / 10_f64.ln();
    (
        rssi + snr.min(0.0) - correction,
        rssi - snr.max(0.0) - correction,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantiles_and_scaled_mad() {
        let stat = summarize(vec![10., 4., 0., 2.]).unwrap();
        assert_eq!(stat.n, 4);
        assert_eq!(stat.median, 3.);
        assert_eq!(stat.mean, 4.);
        assert!((stat.p10 - 0.6).abs() < 1e-12);
        assert!((stat.p90 - 8.2).abs() < 1e-12);
        assert_eq!(stat.mad, 2. * 1.4826);
        assert_eq!(stat.min, 0.);
        assert_eq!(stat.max, 10.);
        assert_eq!(summarize(vec![3.]).unwrap().mad, 0.);
        assert!(summarize(vec![]).is_none());
    }

    #[test]
    fn signal_and_noise_sum_to_total_in_linear_units() {
        for snr in [-20., 0., 12., 1000.] {
            let (signal, noise) = split_power(-100., snr);
            assert!((signal - noise - snr).abs() < 1e-12);
            let power = 10_f64.powf(signal / 10.) + 10_f64.powf(noise / 10.);
            assert!((10. * power.log10() + 100.).abs() < 1e-12);
        }
    }
}
