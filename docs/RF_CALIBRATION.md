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

Run Rust stages from `rust/` with
`cargo run --release -p repeaternet-rf-calibration -- <stage>` and Python
stages from the repository root. Every stage reads the previous stage's files
from the **work directory** and writes new files without editing its inputs.

| Stage | Command | Output in the work directory |
| --- | --- | --- |
| 1. Snapshot | `rf-calibration snapshot` | `snapshots/*.sqlite3`, copied with the SQLite backup API from read-only sources |
| 2. Extract | `rf-calibration extract` | `dataset/links.jsonl`, `dataset/nodes.jsonl`, `dataset/summary.json` |
| 3. Profiles | `rf-calibration profiles` | `profiles.jsonl` and `path_features.jsonl` |
| 4. Predict (Rust) | `rf-calibration predict` | `predictions/<engine>.jsonl` |
| 5. Predict (reference) | `python -I tools/rf-calibration/reference_engines.py` | `predictions/itm_p2p.jsonl`, `predictions/itm_p2p_t90.jsonl`, `predictions/p1812.jsonl`, `predictions/py1812_delta_bullington.jsonl` |
| 6. Fit and report | `python -I tools/rf-calibration/calibrate.py` | `report/` tables, figures and `results.json` |

The work directory defaults to `%LOCALAPPDATA%\RepeaterNet\calibration`
(override with `--work-dir` or `RF_CALIBRATION_DIR`). Terrain is fetched only
through the providers crate into the shared cache
(`RF_PLANNER_CACHE_DIR`, default `%LOCALAPPDATA%\RepeaterNet\test-data\shared-cache`),
so the planner reuses every tile. Nothing here prunes or replaces a cache, and
no stage may read or write `%LOCALAPPDATA%\RepeaterNet\data`.

### Terrain profiles and path features

`profiles` and `predict` accept `--links <path>` and default to
`<work>/dataset/links.jsonl`. Profiles exclude distances outside 30 m to 150 km,
self-links, and links or endpoints flagged `over_300km`, `default_position`,
`self_link` or `mqtt_suspect`. CoreScope non-detections are eligible. The
lexically ordered node pair forms `path_id = a~b`. If positions differ
between eligible records for a pair, the record with the most receptions
supplies the endpoints, with ties broken by link ID.

The midpoint of each path selects the planner's Norwegian UTM zone
(EPSG:25832, 25833 or 25835) or the local WGS84 UTM zone. The downloader
unions 300 m half-width corridors for paths in the same region and CRS,
splitting large plans as needed. It requests pinned Kartverket NHM DTM and
DOM at 10 m in Norway and pinned Copernicus GLO-30 at 30 m elsewhere.
GLO-30 is a surface model with some vegetation and buildings already in
the elevations. It supplies only the harness's `dtm` input. The terrain
estimate sums uncached raw tile sizes with the provider's 2× planning
allowance. It is compared with `--max-download-gb` (5 decimal GB by default)
before any download. Fetches are sequential and the run reports upstream
response bytes and composed cache hits.

`profiles.jsonl` is sorted by `path_id` and replaced atomically. Each row
stores the projected endpoints, sorted `link_ids`, target `resolution_m`,
actual `step_m` and projected `distance_m`, with at least two equal
intervals and endpoints included. The `dtm` and optional `dom` arrays hold
raw elevations rounded to 0.01 m. `missing_fraction` counts positions
missing from either available layer. A wholly unavailable DOM is
`null`. A path with over 2% missing positions or a gap of more than five
samples is dropped. Smaller gaps are interpolated linearly. Boundary
gaps use the nearest elevation. A rerun reuses samples when endpoints
and `terrain_fingerprint` match. The fingerprint includes the sampling
method and intersecting cache tile identities, sizes and modification times.

`path_features.jsonl` has the same order and one row per profile. Along
with `schema`, `path_id` and `distance_m`, it carries `delta_h_m`, the
90th minus 10th percentile of linearly detrended DTM samples within the
middle 80% of the chord. `a_clutter_height_m` and `b_clutter_height_m`
are nonnegative medians of DOM minus DTM within 30 m of each terminal.
`clutter_fraction_gt_3m` is the length fraction above 3 m of DOM excess,
with linear interpolation across threshold crossings. All three
DOM-derived values are `null` when DOM is unavailable.

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
| `rfcore_bullington` | RF core's `LinkEvaluator` at nominal native (10 or 30 m), 50 m and 200 m steps, with both k = 1 and k = 4/3 |
| `p526_delta_bullington` | `Lbull + max(Lsph − Lbs, 0)` from P.526-16 §4.5.2, with the endpoint-clipped smooth terrain fit, smooth-profile Bullington loss `Lbs` and spherical loss `Lsph` from §§3.2 and 3.1.1. Uses vertical polarization, β = 1, εr = 22 and σ = 0.003 S/m over land. [ITU-R P.526-16](https://www.itu.int/rec/R-REC-P.526-16-202511-I) |
| `deygout3` | Selects the maximum-v point on the full chord, then the maximum-v point among the remaining subpaths. Sums P.526 `J(v)` for up to three selected edges, stopping at v ≤ −0.78. |
| `epstein_peterson` | Uses those same edge positions and sums `J(v)` with each edge measured against the chord through its immediate neighbours. |
| `giovaneli` | Uses those same candidates. Selects the largest residual-height edge against each subpath chord, projects the two supporting rays to the subpath endpoints, and sums the resulting `J(v)` terms recursively. Subpaths with all residual heights ≤ 0 use their neighbouring chords. |
| `itm_p2p` | NTIA ITM v1.3 C++ point to point (`ITM_P2P_CR_Ex`) through ctypes |
| `itm_p2p_t90` | NTIA ITM point to point at 90% time reliability |
| `p1812` | ITU-R P.1812 (Py1812 core) |
| `py1812_delta_bullington` | Py1812 `dl_delta_bull` diffraction-only cross-check |

The Rust ports use the native grid at k = 1. The multi-edge definitions
are the capped variants from the model review. Adjacent samples of a single
crest can be selected as separate edges. The 30 km smooth-earth reference
gives `Lbs = 19.780 dB` and `Lsph = 52.454 dB`, within 0.003 dB of the
Py1812 check. [NTIA TR-26-580, §§3.3 and Appendices B–D](https://its.ntia.gov/publications/download/TR-26-580.pdf) ·
[Py1812 reference implementation](https://github.com/eeveetza/Py1812/tree/6c9061dd022e93c84169059a016643926ce89964)

Every Rust engine uses RF core's curvature convention, adding
`x·(D−x)/(2·k·6,371,000)` metres to each interior obstruction. Both
antenna elevations use their terminal DTM plus antenna height. Coarser
variants interpolate the stored elevations onto RF core's
endpoint-preserving grid. Prediction `step_m` is the nominal request.
The link's `radio.freq_mhz` supplies frequency, defaulting to 869.5 MHz.
FSPL uses RF core's `32.44 + 20·log10(d_km) + 20·log10(f_MHz)`, with
`d_km` derived from the 3-D antenna separation.

Each engine writes a separate `PredictionRecord` JSONL file, sorted by
link ID, surface, clutter, TX height, RX height, k factor and step. The
`line_of_sight` flag indicates geometric clearance on the sampled surface.
For Bullington and delta-Bullington, `v_max` is the equivalent Bullington
edge v when reported and the largest direct-chord v otherwise. For the
multi-edge engines, it is the largest direct-chord v. Diagnostics give
edge positions relative to TX and, for delta-Bullington,
`lbull_db`, `lbs_db`, `lsph_db` and `smooth_agl_m`.

Clutter is additive to `fspl_db + excess_db`. Every surface gets a
`none` row. Other Rust clutter variants use the DTM excess:

- `rfcore_clutter` enables the planner's optional coefficient on the
  maximum sampled positive DOM minus DTM excess. The current default
  coefficient is 0 dB/m, so its predictions have zero additive loss.
- `dom_p2108` sums the height-gain corrections for both ends at 30 MHz
  to 3 GHz, with representative height `R` from the terminal medians
  in `path_features.jsonl`. A terminal is classified as forest if two
  consecutive samples within 30 m both exceed 3 m of DOM minus DTM and
  differ in excess height by at least 1 m. Other terminals are classified
  as urban. Both categories use equation (2a) with a 27 m width.
  For antenna height `h < R`, the correction is `J(v) − 6.03`,
  clamped to zero, where
  `v = 0.342·sqrt(f_GHz·(R−h)·atan_deg((R−h)/27))`.
  The correction is zero at or above `R`.
  [ITU-R P.2108-1, §3.1 and Table 3](https://www.itu.int/rec/R-REC-P.2108-1-202109-I)
- `p833` uses the same forest heuristic. The woodland segment starts
  at a terminal when the ray is below DOM and DOM exceeds DTM by more
  than 3 m. Its end is the first ray crossing of curvature-adjusted DOM
  or the end of the contiguous excess above 3 m, with linear
  interpolation at each crossing. For 3-D in-canopy length `d`,
  `Aev = Am·(1 − exp(−d·γ/Am))`. The fixed 869.5 MHz coefficients
  are γ = 0.163840 dB/m and Am = 25.452880 dB, interpolated in log
  frequency between the 466.475 MHz and 949 MHz mixed-forest
  measurements in Table 1. Add losses for disjoint terminal segments
  and evaluate overlapping segments once over their combined length.
  [ITU-R P.833-10, §2.1 and Table 1](https://www.itu.int/rec/R-REC-P.833-10-202109-I)

The `dom_p2108` and `p833` rows require DOM. Their diagnostics record
the cover classification, representative height and in-canopy distance
for each terminal in TX, RX order.

Antenna heights are unknown. Predictions use a grid of 2, 5, 10 and 20 m above
ground at each end. The fit estimates a per-node height from that grid, with a
role-based prior: repeaters and rooms 6 m, companions and observers 2 m.
Results are reported at the fitted heights and at the planner's default.

### Engines: reference implementations

The Python runner uses raw, uniform DTM or DOM samples in Tx-to-Rx order and
anchors both antennas to DTM ground. The 3-D FSPL is `32.45 + 20 log10(f_MHz)
+ 20 log10(d_km)`, using `radio.freq_mhz` or 869.5 MHz. ITM uses
`ITM_P2P_CR_Ex` from v1.3 at `652b3fa698a3c5026dd5d68ac06a84c254e29dcc`,
continental temperate climate (5), vertical polarisation, εr = 15 and
σ = 0.005 S/m. The sea-level input `N0 = 301 exp(h_sys / 9460)` uses the
middle profile elevation mean after trimming `floor(intervals / 10)` samples
at each end. This gives surface refractivity `Ns = 301` and effective
`k ≈ 1.333`. Confidence is 50% for both outputs. The median uses 50%
reliability and `mdvar=0` (single message, situation only). The 90% variant
uses `mdvar=1` (accidental mode, with time varying independently).
Mode and warning flags are in `diag`. [NTIA ITM v1.3](https://github.com/NTIA/itm/tree/v1.3) · [NTIA ITM warnings](https://github.com/NTIA/itm/blob/v1.3/ERRORS_AND_WARNINGS.md)

P.1812 uses the Py1812 -8 core at `6c9061dd022e93c84169059a016643926ce89964`
with the advised -6 troposcatter term, vertical polarisation, `p = pL = 50%`,
`sigmaL = 0`, outdoor reception and 2.15 dBi at each antenna. The advised
result uses bilinearly interpolated DN50 and N050 at the path centre on the
ITU 1.5° grids. DN50 gives the median effective `k`. The sea proxy marks
contiguous DTM runs of at least 15 samples below 2 m when the run median is
below 0.25 m. Other samples up to 100 m are coastal and the rest inland.
Coast distances use the midpoints of the first and last land–sea transitions,
or zero for an endpoint at sea. DOM adds its height above DTM at interior
samples. The `p1812_woodland` scenario raises interior cover to at least
15 m within 100 m of either terminal. Its increment over the mapped case is
recorded in `diag`, with the joint terrain and cover loss in `excess_db`.
The optional published -7 diagnostic uses the common-volume DN50/N050 and
terrain height with the named Py1812 horizon and scatter angles. The full
height grid is the default. An optional subset is recorded in each row.
[Py1812 pinned source](https://github.com/eeveetza/Py1812/tree/6c9061dd022e93c84169059a016643926ce89964) · [ITU-R P.1812-7 and scatter notice](https://www.itu.int/rec/R-REC-P.1812-7-202308-S) · [ITU-R P.2001-5](https://www.itu.int/rec/R-REC-P.2001-5-202308-S)

The Py1812 delta-Bullington cross-check calls `dl_delta_bull` with the
Py1812 smooth-surface diffraction heights, `k = 1`, vertical polarisation,
no sea fraction and `flag4 = 0`. It adds the diffraction loss to the
calibration FSPL and records actual and smooth Bullington loss and spherical
diffraction separately. [ITU-R P.526-16 §4.5.2](https://www.itu.int/rec/R-REC-P.526-16-202511-I) · [Reference engine setup and verification](../tools/rf-calibration/README.md)

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
