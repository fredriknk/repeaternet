# RF calibration against measured links

Status: in progress (started 2026-11-22). This page describes the method, the
file contracts between stages and, once available, the results. It records
measurements of RepeaterNet's path-loss predictions against real receptions. It
does not describe a feature of the planner.

## Question

How well does RepeaterNet's predicted path loss at 869.5 MHz (FSPL plus
Bullington diffraction over Kartverket DTM or DOM) match MeshCore and
Meshtastic receptions? Which engine and correction terms should the planner
use, and how large must the fade margin be to cover location and time
variability?

## Pipeline

All stages are run from `rust/` with
`cargo run --release -p repeaternet-rf-calibration -- <stage>`. Python stages
live in [`tools/rf-calibration/`](../tools/rf-calibration). Every stage reads
the previous stage's files from the **work directory** and writes new files.
It never edits its inputs.

| Stage | Command | Output in the work directory |
| --- | --- | --- |
| 1. Snapshot | `rf-calibration snapshot` | `snapshots/*.sqlite3`, copied with the SQLite backup API from read-only sources |
| 2. Extract | `rf-calibration extract` | `dataset/links.jsonl`, `dataset/nodes.jsonl`, `dataset/summary.json` |
| 3. Profiles | `rf-calibration profiles` | `profiles.jsonl` (DTM and DOM, or a global DEM) |
| 4. Predict (Rust) | `rf-calibration predict` | `predictions/<engine>.jsonl` |
| 5. Predict (reference) | `python -I tools/rf-calibration/reference_engines.py` | `predictions/itm_p2p.jsonl`, `predictions/p1812.jsonl` |
| 6. Fit and report | `python -I tools/rf-calibration/calibrate.py` | `report/` tables, figures and `results.json` |

The work directory defaults to `%LOCALAPPDATA%\RepeaterNet\calibration`
(override with `--work-dir` or `RF_CALIBRATION_DIR`). Terrain is fetched only
through the providers crate into the shared cache
(`RF_PLANNER_CACHE_DIR`, default `%LOCALAPPDATA%\RepeaterNet\test-data\shared-cache`),
so the planner reuses every tile. Nothing here prunes or replaces a cache, and
no stage may read or write `%LOCALAPPDATA%\RepeaterNet\data`.

## Privacy

Capture databases contain node names and public keys. The dataset replaces
every identity with `SHA-256(salt || namespace || ":" || id)` truncated to
16 hex digits. The salt is in `<work>/salt.txt` and is never committed. Exact
coordinates stay in the work directory. Committed docs, tests and fixtures
contain only aggregates or synthetic data.

## Data sources

| Source | Evidence | Signal | Radio |
| --- | --- | --- | --- |
| CoreScope (`corescope.eth0.no`, Norway, and `analyzer.on8ar.eu`, Benelux) | Observer receptions. The final entry in `resolved_path` is the last relay. For an empty path, the advert sender is the transmitter (zero hop). | RSSI and SNR per reception | Observer snapshot `radio` string `freq,bw,sf,cr` (869.618 MHz, 62.5 kHz, SF8 in Norway) and `noise_floor` |
| Meshtastic map captures (`map.868.no` Norway, others worldwide) | Gateway receptions with a confirmed transmitter, traceroute legs, neighbour reports | Gateway RSSI and SNR. Traceroutes and neighbours have SNR only. | Node `radios` history (preset, region, sometimes TX power) |
| SondeHub | Radiosonde to ground station, 400–406 MHz | RSSI or SNR | Sonde type, sometimes TX power |

SondeHub paths are mostly air to ground. They are used only to check the
receiver and refraction side, not terrain diffraction.

Observers log every copy of a flood packet they hear, one per relay. A relay
that is known to have forwarded a packet, because another observer heard that
copy, gave the observer an opportunity to hear it. This turns repeated
silence into **non-detection evidence** instead of missing data.

## Dataset (`dataset/links.jsonl`)

One JSON object per directed link `tx → rx` and evidence type. The
authoritative definition is
[`dataset.rs`](../rust/crates/rf-calibration/src/dataset.rs). In summary:

- `link_id`, `source`, `evidence` (`corescope_last_hop`, `corescope_zero_hop`,
  `corescope_non_detection`, `meshtastic_gateway_rx`, `meshtastic_traceroute`,
  `meshtastic_neighbor`), `region` (`norway` when both ends are inside the
  Kartverket coverage polygon, else `world`).
- `tx` and `rx` endpoints, each with: `node` (pseudonymous), `lat`, `lon`,
  `pos_source`, `pos_precision_m`, `altitude_m`, `role` and `flags`.
- `distance_m` and `radio`, where every value is either known with a `basis`
  or `null`.
- Aggregates over packets: `n`, `first_ms`, `last_ms`, and `Stat` summaries
  (`n`, `median`, `mean`, `p10`, `p90`, `mad`, `min`, `max`) of `rssi_dbm`,
  `snr_db`, `signal_dbm` and `noise_dbm`.
- `detection`: `heard` out of `opportunities`, with the counting `basis`.
- `flags` for quality.

### Signal power from RSSI and SNR

LoRa packet RSSI is total in-band power. Near and below the noise floor it
flattens at the noise level, while SNR stays informative. Above about +10 dB,
SNR saturates and RSSI carries the information. Both are combined per packet:

- signal `S = RSSI + SNR − 10·log10(1 + 10^(SNR/10))`
- noise `N = RSSI − 10·log10(1 + 10^(SNR/10))`

In the eth0.no capture, mean RSSI is about −112 dBm for SNR −12 to −8 dB and
−41 dBm for SNR +12 dB, which confirms both regimes.

### Quality flags

Endpoint flags: `default_position` (0,0, or a known placeholder such as a
city centroid used by many nodes), `coarse_position` (precision radius
> 100 m or Meshtastic precision bits < 16), `moved` (positions spread
> 200 m over the window), `duplicate_position` (several nodes within 5 m),
`unknown_precision`.

Link flags: `mqtt_suspect` (Meshtastic rules from `measurement_flags`),
`over_300km`, `few_samples` (n < 5), `self_link`, `radio_mismatch` (the
endpoints report different frequency, bandwidth or SF).

## Measured path loss

```
PL_meas = P_tx + G_tx − L_tx + G_rx − L_rx − S
```

`P_tx`, the antenna gains and the feed losses are unknown for almost every
node. They are not hidden constants. The fit has:

- a global offset `c` with a prior centred on 22 dBm + 2 + 2 dBi − 2 dB,
  standard deviation 4 dB;
- per-transmitter and per-receiver random offsets `u_tx` and `u_rx`, which
  absorb power, gain, feed loss and siting;
- for SNR-only evidence, a per-receiver noise floor with a prior from the
  thermal floor `−174 + 10·log10(BW) + NF`, NF = 6 dB, plus observed
  CoreScope `noise_floor` values where known.

Relative engine skill comes from residuals after removing node offsets.
Absolute bias comes from `c`, and its uncertainty is reported with it.

### Censoring

Only packets above the demodulation floor are logged (SNR about −10 dB for
SF8, −17.5 dB for SF11, −20 dB for SF12). Each link likelihood is therefore a
normal density truncated at the floor. Non-detections and detection ratios
add a probit term `P(S > floor)`. Results are also reported on links whose
median SNR is at least 5 dB above the floor, where censoring is negligible.

## Engines

| Engine id | Implementation |
| --- | --- |
| `rfcore_bullington` | `repeaternet-rf-core` `LinkEvaluator`, called from the harness at the native step and at the planner's 200 m screening step |
| `p526_delta_bullington` | Full ITU-R P.526-16 §4.5.2, ported to Rust and cross-checked against Py1812 `dl_delta_bull` |
| `deygout3`, `giovaneli`, `epstein_peterson` | Rust multi-edge methods (P.526 J(v), at most three edges) |
| `itm_p2p` | NTIA ITM v1.3 C++ point to point (`ITM_P2P_CR_Ex`) through ctypes |
| `p1812` | ITU-R P.1812 (Py1812 core) |

Clutter options: `none`, `rfcore_clutter` (the planner's optional surface
clutter), `dom_p2108` (P.2108 §3.1 terminal correction with clutter height
from DOM − DTM around each terminal), `p2108` (statistical, by area type) and
`p833` (vegetation attenuation over the in-canopy path near each terminal).

Antenna heights are unknown. Predictions use a grid of 2, 5, 10 and 20 m above
ground at each end. The fit estimates a per-node height from that grid, with a
role-based prior: repeaters and rooms 6 m, companions and observers 2 m.
Results are reported at the fitted heights and at the planner's default.

## Evaluation

- Report bias, RMSE and the residual standard deviation after node offsets,
  binned by distance, Bullington v, line of sight, terrain roughness Δh,
  DTM versus DOM and source.
- Report Norway (Kartverket 10 m DTM and DOM) and world (Copernicus GLO-30, a
  surface model) separately.
- Use spatial block cross-validation with 20 km blocks, grouped by node, so no
  node appears in both train and test. Fit correction terms only on training
  folds.
- The fade margin is the quantile of the held-out residual distribution: the
  50th, 90th and 95th percentiles of location and time variability, split into
  within-link (time) and between-link (location) components.

## Results

Pending.
