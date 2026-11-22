"""Measure received SondeHub links using the Rust calibration export."""

import argparse
from collections import Counter, defaultdict
import json
import os
from pathlib import Path
import tempfile

import numpy as np
from scipy.stats import spearmanr


EARTH_M = 6_371_000.0
C = 299_792_458.0
ELEVATION_EDGES = (5.0, 15.0, 30.0, 60.0)
ELEVATION_LABELS = ("2–5", "5–15", "15–30", "30–60", "60–90")
MIN_FLIGHTS = 2


def read_jsonl(path):
    with path.open(encoding="utf-8") as stream:
        return [json.loads(line) for line in stream if line.strip()]


def atomic_text(path, content):
    stream = tempfile.NamedTemporaryFile(
        mode="w", encoding="utf-8", newline="\n", dir=path.parent, delete=False
    )
    staged = Path(stream.name)
    try:
        with stream:
            stream.write(content)
            stream.flush()
            os.fsync(stream.fileno())
        staged.replace(path)
    finally:
        staged.unlink(missing_ok=True)


def finite(value):
    return value is not None and bool(np.isfinite(value))


def usable(row):
    return row["quality_flags"] & 15 == 0


def clear(row):
    elevation = row["elevation_k1_deg"]
    horizon = row["terrain_k1_deg"]
    agl = row["tx_agl_m"]
    return (
        usable(row)
        and finite(elevation)
        and finite(horizon)
        and elevation > max(2.0, horizon)
        and finite(agl)
        and agl > 1000.0
    )


def fspl_db(distance, frequency):
    return 20.0 * np.log10(4.0 * np.pi * distance * frequency / C)


def signal_offset(row, metric):
    power = row["tx_power_mw"]
    distance = row["slant_m"]
    signal = row[metric]
    if (
        signal is None or not finite(power) or power <= 0
        or not finite(distance) or distance <= 0
        or (metric == "rssi" and row["quality_flags"] & 128)
    ):
        return None
    return float(signal["mean"] + fspl_db(distance, row["frequency_hz"]) - 10.0 * np.log10(power))


def sigma(values, weights=None):
    if not len(values):
        return None
    values = np.asarray(values)
    mean = np.average(values, weights=weights)
    return float(np.sqrt(np.average((values - mean) ** 2, weights=weights)))


def elevation_pattern(elevations, residuals, weights, cells):
    bands = np.searchsorted(ELEVATION_EDGES, elevations, side="right")
    indicators = np.eye(len(ELEVATION_LABELS))[bands]
    # Within-pair comparison limits receiver gain and flight TX differences.
    design = indicators.copy()
    response = residuals.copy()
    for indices in cells.values():
        design[indices] -= np.mean(indicators[indices], axis=0)
        response[indices] -= np.mean(residuals[indices])
    scale = np.sqrt(weights)
    reference = 1  # 5–15 degrees
    coefficients, _, _, _ = np.linalg.lstsq(
        np.delete(design, reference, axis=1) * scale[:, None], response * scale, rcond=None
    )
    coefficients = np.insert(coefficients, reference, 0.0)

    # A contrast is identified only if bands co-occur in a path from the reference.
    connected = {reference} if np.any(bands == reference) else set()
    observed = [set(bands[indices]) for indices in cells.values()]
    for _ in ELEVATION_LABELS:
        for group in observed:
            if connected & group:
                connected.update(group)

    results = []
    for band, label in enumerate(ELEVATION_LABELS):
        mask = bands == band
        results.append({
            "elevation_deg": label,
            "bins": int(mask.sum()),
            "receiver_flights": sum(bool(mask[indices].any()) for indices in cells.values()),
            "relative_mean_db": float(coefficients[band]) if band in connected else None,
            "residual_sigma_db": sigma(residuals[mask], weights[mask]),
        })
    return results


def fit_signal(rows, metric):
    cells = defaultdict(list)
    for row in rows:
        if clear(row) and (value := signal_offset(row, metric)) is not None:
            cells[row["receiver"], row["flight"]].append((row["elevation_k1_deg"], value))
    counts = Counter(receiver for receiver, _ in cells)
    retained = {key: observations for key, observations in cells.items() if counts[key[0]] >= MIN_FLIGHTS}
    if not retained:
        return {"bins": 0, "receivers": 0, "flights": 0, "receiver_flights": 0}, {}

    receiver_means = defaultdict(list)
    for (receiver, _), observations in retained.items():
        receiver_means[receiver].append(float(np.mean([value for _, value in observations])))
    offsets = {receiver: float(np.mean(averages)) for receiver, averages in receiver_means.items()}

    elevations, residuals, weights, cell_means = [], [], [], []
    indices = {}
    flight_residuals = defaultdict(list)
    for (receiver, flight), observations in retained.items():
        indices[receiver, flight] = list(range(len(residuals), len(residuals) + len(observations)))
        errors = [value - offsets[receiver] for _, value in observations]
        elevations.extend(angle for angle, _ in observations)
        residuals.extend(errors)
        weights.extend([1.0 / len(errors)] * len(errors))
        cell_means.append(np.mean(errors))
        flight_residuals[flight].append(cell_means[-1])

    weights, residuals = np.asarray(weights), np.asarray(residuals)
    result = {
        "bins": len(residuals),
        "same_flight_height_bins": sum(
            row["tx_height_basis"] == "same_flight_fix"
            and (row["receiver"], row["flight"]) in retained
            and clear(row) and signal_offset(row, metric) is not None for row in rows
        ),
        "receivers": len(offsets),
        "flights": len(flight_residuals),
        "receiver_flights": len(retained),
        "receiver_offset_sigma_db": sigma(list(offsets.values())) if len(offsets) >= 2 else None,
        "residual_sigma_db": sigma(residuals, weights),
        "receiver_flight_residual_sigma_db": sigma(cell_means),
        "flight_mean_residual_sigma_db": sigma([np.mean(values) for values in flight_residuals.values()]),
        "elevation": elevation_pattern(elevations, residuals, weights, indices),
        "by_receiver": {
            receiver: {
                "flights": len(receiver_means[receiver]),
                "offset_db": offset,
                **({"effective_noise_dbm_at_zero_gain": -offset} if metric == "snr" else {}),
            }
            for receiver, offset in sorted(offsets.items())
        },
        "by_flight": {
            flight: {"receivers": len(values), "mean_residual_db": float(np.mean(values))}
            for flight, values in sorted(flight_residuals.items())
        },
    }
    return result, offsets


def smooth_horizon_m(k, rx, tx):
    # Small-height expansion of the sum of the two tangent arc lengths.
    return np.sqrt(2.0 * k * EARTH_M) * (np.sqrt(rx) + np.sqrt(tx))


def crossed_interval(log_k, rx_ids, flight_ids):
    _, rx = np.unique(rx_ids, return_inverse=True)
    _, flight = np.unique(flight_ids, return_inverse=True)
    n_rx, n_flight = int(rx.max()) + 1, int(flight.max()) + 1
    if min(n_rx, n_flight) < 2:
        return None
    random = np.random.default_rng(220)
    order = np.argsort(log_k)
    draws = []
    for _ in range(2000):
        rx_count = np.bincount(random.integers(n_rx, size=n_rx), minlength=n_rx)
        flight_count = np.bincount(random.integers(n_flight, size=n_flight), minlength=n_flight)
        weights = (rx_count[rx] * flight_count[flight])[order]
        if weights.sum():
            draws.append(np.median(np.repeat(log_k[order], weights)))
    return np.exp(np.quantile(draws, [0.025, 0.975])).tolist()


def horizon_fit(rows):
    paths = defaultdict(list)
    for row in rows:
        # Last heard comes from all exported bins, even if the final geometry is invalid.
        paths[row["receiver"], row["flight"]].append(row)
    endpoints = []
    for (receiver, flight), bins in paths.items():
        bins.sort(key=lambda row: (row["observed_at_ms"], row["link_id"]))
        last = bins[-1]
        tx, rx = last["tx_msl_m"], last["antenna_msl_m"]
        angle, terrain = last["elevation_k1_deg"], last["terrain_k1_deg"]
        earlier = [
            row["tx_msl_m"] for row in bins[:-1] if usable(row)
            and finite(row["tx_msl_m"])
            and 0 < last["observed_at_ms"] - row["observed_at_ms"] <= 1_200_000
        ]
        if not (
            usable(last) and finite(tx) and tx > 0.0 and finite(rx) and rx > 0.0
            and finite(angle) and -1.0 <= angle <= 1.0
            and finite(terrain) and terrain + 0.1 < angle
            and last["distance_m"] >= 80_000.0
            and (last["flight_witness_after_ms"] or 0) >= 120_000
            and earlier and max(earlier) > tx + 100.0
        ):
            continue
        endpoints.append((receiver, flight, last["distance_m"] / smooth_horizon_m(1.0, rx, tx)))

    result = {
        "candidate_receiver_flights": len(paths),
        "receiver_flights": len(endpoints),
        "receivers": len({rx for rx, _, _ in endpoints}),
        "flights": len({flight for _, flight, _ in endpoints}),
        "constrains_physical_k": False,
    }
    if endpoints:
        rx, flight, ratio = zip(*endpoints)
        ratio = np.asarray(ratio)
        log_k = 2.0 * np.log(ratio)
        center = np.median(log_k)
        fitted = float(np.exp(center))
        result.update({
            "apparent_k": fitted,
            "cluster_interval_95": crossed_interval(log_k, rx, flight),
            "median_range_to_horizon": {
                "k1": float(np.median(ratio)),
                "k4_3": float(np.median(ratio / np.sqrt(4.0 / 3.0))),
                "fitted": float(np.median(ratio / np.sqrt(fitted))),
            },
            "heard_beyond_smooth_horizon": {
                "k1": int(np.sum(ratio > 1.0)),
                "k4_3": int(np.sum(ratio > np.sqrt(4.0 / 3.0))),
                "fitted": int(np.sum(log_k > center)),
            },
        })
    return result


def shadow_check(rows, profiles, offsets, metric):
    values = []
    for row in rows:
        profile = profiles.get(row["link_id"])
        if profile is None or row["receiver"] not in offsets[metric]:
            continue
        offset = signal_offset(row, metric)
        if offset is not None:
            excess = offsets[metric][row["receiver"]] - offset
            values.append((row["receiver"], row["flight"], excess, profile["bullington_k1_db"],
                           profile["bullington_k133_db"]))
    result = {
        "receiver_flights": len(values),
        "receivers": len({row[0] for row in values}),
        "flights": len({row[1] for row in values}),
    }
    if values:
        measured, k1, k133 = np.asarray([row[2:] for row in values]).T
        result["median_measured_extra_db"] = float(np.median(measured))
        for label, predicted in (("k1", k1), ("k4_3", k133)):
            rho = None
            if len(values) >= 3 and np.ptp(predicted) > 0 and np.ptp(measured) > 0:
                rho = float(spearmanr(measured, predicted).statistic)
            result[label] = {
                "median_predicted_db": float(np.median(predicted)),
                "median_absolute_error_db": float(np.median(abs(measured - predicted))),
                "spearman_rho": rho,
            }
    return result


def analyze(links, receivers, horizons, profiles):
    receivers = {row["receiver"]: row for row in receivers}
    horizons = {row["link_id"]: row for row in horizons}
    profiles = {row["link_id"]: row for row in profiles}
    grouped = defaultdict(list)
    for link in links:
        receiver = receivers[link["receiver"]]
        grouped[receiver["region"]].append({
            **link, **horizons[link["link_id"]], "antenna_msl_m": receiver["antenna_msl_m"]
        })

    results = {"schema": 1, "shadow_profiles": len(profiles), "regions": {}}
    for region, rows in sorted(grouped.items()):
        fits, offsets = {}, {}
        for metric in ("rssi", "snr"):
            fits[metric], offsets[metric] = fit_signal(rows, metric)
        results["regions"][region] = {
            "bins": len(rows),
            "receivers": len({row["receiver"] for row in rows}),
            "flights": len({row["flight"] for row in rows}),
            "bins_with_local_horizon": sum(finite(row["terrain_k1_deg"]) for row in rows),
            "signal": fits,
            "horizon": horizon_fit(rows),
            "shadow": {metric: shadow_check(rows, profiles, offsets, metric) for metric in fits},
        }
    return results


def fmt(value, digits=2):
    return "—" if value is None else f"{value:.{digits}f}"


def report(results):
    lines = [
        "# SondeHub air-to-ground check", "",
        "Receiver and flight are the reporting units. Bins are weighted to give equal total weight "
        "to each receiver–flight pair. Receiver offsets use nominal TX power and at least two flights. "
        "The receiver spread weights each receiver equally. Auto RX SNR offsets combine receive gain, "
        "noise and demodulator response. RSSI and SNR are analysed separately. "
        "The JSON includes pseudonymised receiver and flight statistics.", "",
        "Clear paths have sonde AGL above 1 km and elevation above 2° and the sampled 10 km local "
        "horizon at k = 1. Elevation effects compare bands within receiver–flight pairs, relative "
        "to 5–15°. The residual spread by band is from the receiver-only model.", "",
        "The last-heard fit uses descending flights with another receiver still hearing the sonde "
        "at least two minutes later. Apparent k is the median on the log scale of the k values "
        "that place those last receptions on the smooth-earth horizon. The interval resamples "
        "receivers and flights. Receiver activity, detection thresholds, distant obstacles and "
        "antenna heights are uncertain. These endpoints cannot identify the physical k.", "",
        f"The export contains {results['shadow_profiles']} complete Bullington profiles.", "",
    ]
    for region, row in results["regions"].items():
        lines.extend([
            f"## {region.title()}", "",
            f"{row['bins']:,} bins from {row['receivers']} receivers and {row['flights']} flights. "
            f"{row['bins_with_local_horizon']:,} bins have a complete sampled local horizon.", "",
            "| Metric | Bins | Receivers | Flights | Receiver–flights | Receiver σ (dB) | Bin residual σ (dB) | Pair residual σ (dB) | Flight residual σ (dB) |",
            "| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |",
        ])
        for metric, fit in row["signal"].items():
            lines.append(
                f"| {metric.upper()} | {fit['bins']} | {fit['receivers']} | {fit['flights']} | "
                f"{fit['receiver_flights']} | {fmt(fit.get('receiver_offset_sigma_db'))} | "
                f"{fmt(fit.get('residual_sigma_db'))} | {fmt(fit.get('receiver_flight_residual_sigma_db'))} | "
                f"{fmt(fit.get('flight_mean_residual_sigma_db'))} |"
            )
        for metric, fit in row["signal"].items():
            if not fit["bins"]:
                continue
            lines.extend([
                "", f"### {metric.upper()} by elevation", "",
                f"Matched same-flight height fixes supply {fit['same_flight_height_bins']} bins.", "",
                "| Elevation (°) | Bins | Receiver–flights | Relative mean (dB) | Residual σ (dB) |",
                "| --- | ---: | ---: | ---: | ---: |",
            ])
            for band in fit["elevation"]:
                lines.append(
                    f"| {band['elevation_deg']} | {band['bins']} | {band['receiver_flights']} | "
                    f"{fmt(band['relative_mean_db'])} | {fmt(band['residual_sigma_db'])} |"
                )
        h = row["horizon"]
        lines.extend(["", "### Last heard", ""])
        if h["receiver_flights"]:
            interval = h["cluster_interval_95"]
            interval_text = f"{fmt(interval[0])}–{fmt(interval[1])}" if interval else "unavailable"
            lines.append(
                f"Apparent k = {fmt(h['apparent_k'])} (95% crossed interval {interval_text}) from "
                f"{h['receiver_flights']} of {h['candidate_receiver_flights']} receiver–flights, "
                f"{h['receivers']} receivers and {h['flights']} flights."
            )
            lines.extend(["", "| k | Median range / horizon | Heard beyond horizon |", "| --- | ---: | ---: |"])
            for key, label in (("k1", "1"), ("k4_3", "4/3"), ("fitted", "Fitted")):
                lines.append(
                    f"| {label} | {fmt(h['median_range_to_horizon'][key])} | "
                    f"{h['heard_beyond_smooth_horizon'][key]} |"
                )
        else:
            lines.append("No receiver–flight endpoints passed the last-heard selection.")
        lines.extend(["", "### Terrain shadow", ""])
        for metric, check in row["shadow"].items():
            n = check["receiver_flights"]
            if n:
                lines.append(
                    f"- {metric.upper()}, {n} receiver–flights from {check['receivers']} receivers "
                    f"and {check['flights']} flights. Median measured extra loss "
                    f"{fmt(check['median_measured_extra_db'])} dB. Median Bullington loss "
                    f"{fmt(check['k1']['median_predicted_db'])} dB at k = 1 and "
                    f"{fmt(check['k4_3']['median_predicted_db'])} dB at k = 4/3. "
                    f"Median absolute errors {fmt(check['k1']['median_absolute_error_db'])} and "
                    f"{fmt(check['k4_3']['median_absolute_error_db'])} dB."
                )
            else:
                lines.append(f"- {metric.upper()} has no complete shadow profiles with a fitted receiver offset.")
        lines.append("")
    return "\n".join(lines) + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    fallback = Path(os.environ.get("LOCALAPPDATA", os.environ.get(
        "XDG_DATA_HOME", Path.home() / ".local/share"
    ))) / "RepeaterNet"
    parser.add_argument("--work-dir", type=Path,
                        default=os.environ.get("RF_CALIBRATION_DIR", fallback / "calibration"))
    args = parser.parse_args()
    source = args.work_dir / "sondehub"
    output = args.work_dir / "report-sondehub"
    protected = (fallback / "data").resolve()
    files = [source / f"{name}.jsonl" for name in ("links", "receivers", "horizons", "profiles")]
    paths = files + [output / "results.json", output / "report.md"]
    if any(path.resolve().is_relative_to(protected) for path in paths):
        parser.error("refusing to use the real RepeaterNet data directory")
    results = analyze(*(read_jsonl(path) for path in files))
    encoded = json.dumps(results, indent=2, allow_nan=False) + "\n"
    output.mkdir(parents=True, exist_ok=True)
    atomic_text(output / "report.md", report(results))
    atomic_text(output / "results.json", encoded)
    print("Wrote report-sondehub/results.json and report-sondehub/report.md")


if __name__ == "__main__":
    main()
