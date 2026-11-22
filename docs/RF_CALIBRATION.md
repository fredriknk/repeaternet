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
every node identity with `SHA-256(salt || namespace || ":" || lowercase(id))`
truncated to its first 16 hex digits. The namespace is `corescope` for public
keys and `meshtastic` for decimal node numbers, independent of the capture
host. The salt is the trimmed UTF-8 contents of `<work>/salt.txt`. Extract
creates it from 32 cryptographically random bytes encoded as lowercase hex
if the file is missing. Exact coordinates stay in the work directory.
Committed docs, tests and fixtures contain only aggregates or synthetic data.

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

## Dataset

Extract reads the CoreScope and Meshtastic SQLite snapshots in
`<work>/snapshots/` in read-only mode. It stages each output in the dataset
directory with a `.partial` suffix and replaces each file by rename, publishing
the summary last. The snapshot layout has one file per capture source.
Phase 1 covers terrestrial mesh links. The SondeHub snapshot is deferred.

### File contracts

[`dataset.rs`](../rust/crates/rf-calibration/src/dataset.rs) defines the JSONL
records. All records currently carry schema version 1.

- `links.jsonl` has one record per directed transmitter–receiver pair,
  capture source and evidence type, sorted by `link_id`. The source names
  use the host from the capture instance table. Self links and receptions
  without two valid positions are excluded. Endpoint coordinates, roles,
  radio context, distance and region come from the middle reception in
  time order (the later of the two middle receptions for an even count).
  A non-detection uses its first counted opportunity. Distances are
  great-circle distances with a 6,371 km Earth radius. `region` is `norway`
  when both endpoints are inside the built-in `kartverket-nhm` coverage
  polygon, and `world` otherwise.
- Each endpoint carries a pseudonym, `lat`, `lon`, `pos_source`,
  `pos_precision_m`, `altitude_m`, `role` and quality `flags`. On a link,
  `n` counts all retained reception rows. `first_ms` and `last_ms` bound
  their UTC times. `rssi_dbm`, `snr_db`, `signal_dbm` and `noise_dbm` each
  summarize their available per-row values with a count, mean, min, max,
  p10, median, p90 and median absolute deviation (MAD × 1.4826).
  Quantiles interpolate linearly between adjacent sorted samples. A
  quantity with no samples is null.
- `nodes.jsonl` has one record per source and pseudonym, sorted by that
  pair. It uses the latest valid position and latest applicable metadata,
  with the distinct valid coordinate count and maximum pairwise separation
  within that source. Unlocated nodes carry null coordinates. The same
  pseudonym can appear once for each source that knows the node.
- `Radio` includes `lora_region` for the reported Meshtastic regulatory
  region and `field_basis` for each populated field's provenance. The
  existing `basis` lists the distinct provenance labels. Unknown radio
  values remain null.
- `summary.json` contains nonempty region × source × evidence groups with
  link counts, counts with at least five receptions, total receptions,
  distinct endpoint nodes, link and endpoint flag counts, distance
  quantiles (min, p10, p50, p90, max), detection totals and non-detection
  counts. Endpoint flag counts count the two endpoints of each link record.
  The per-source inventory includes node flag counts and input diagnostics,
  including opportunities that did not qualify for a published detection.
  Extract also prints the group counts to stdout.

### CoreScope receptions

Rows with `direction = 'rx'`, negative RSSI and finite SNR supply
measurements. An explicitly empty JSON prefix array attributes the
transmission to `sender_pubkey` (`corescope_zero_hop`). Otherwise the final
element of `resolved_path` must identify the transmitter
(`corescope_last_hop`). The receiver is `observer_id`. Keys are
case-insensitive.

Positions come from timestamped adverts, node snapshots and observer
snapshots. For a reception, use the last valid position at or before its
timestamp, falling forward to the first valid position if necessary.
Nulls, invalid coordinates and `(0,0)` do not supply a location.
Observer snapshots provide frequency, bandwidth, spreading factor,
coding rate, receiver noise floor and role. The node snapshot supplies
its reported role when the observer role is missing. Metadata uses the
most recent snapshot at or before the sample, with the earliest snapshot
as fallback. Link radio fields prefer the receiver's configuration.
MeshCore TX power remains null.

The global graph and per-node responses in `neighbors_snapshot` are
excluded. Upstream computes `avg_snr` from observer measurements assigned
to graph edges, including inferred origin-to-first-relay edges. The graph
keys edges by unordered pair, so one average may also pool both directions.
It cannot establish the SNR measured at the receiving end of a directed hop.
[Graph construction](https://github.com/Kpa-clawbot/CoreScope/blob/efd2aeabb4ecb62de98bbfd7356a01b21a7926be/cmd/server/neighbor_graph.go),
[API mapping](https://github.com/Kpa-clawbot/CoreScope/blob/master/cmd/server/neighbor_api.go).

### CoreScope detection

For each instance, group observations by `packet_hash`. A transmitter
needs a received witness with an explicit zero-hop sender or a fully
resolved path whose final entry names it. A partial path with a known last
hop still supplies signal measurements and can confirm that an observer
heard an otherwise eligible transmission. A transmitter/hash witnessed
only through partial paths is excluded from the opportunity count.

Count at most one opportunity per transmitter, observer and hash. Both
nodes must have a valid position within 100 km of each other at the
earliest recorded `first_seen` for that hash. The distinct receiving
observer must have logged any RX row within ±120 seconds of that first
sighting. Count `heard` if that observer logged a copy attributable to the
same transmitter. If the observer logged the hash with an unknown
transmitter and no copy attributable to this transmitter, exclude that
ambiguous opportunity. Activity and hearing do not require valid signal
metrics.

Publish the pair's pooled `detection` on its last-hop record or, when only
a zero-hop record exists, there. With at least 20 opportunities and zero
heard, publish the tally as `corescope_non_detection` with `n = 0`, null
signal summaries and null reception timestamps. Each pair's tally appears
at most once in the link output.

### Meshtastic measurements

Gateway rows with a transmitter become `meshtastic_gateway_rx`. Flag them
`relayed` if their hop counts differ or directness is unconfirmed. A
`direct` or `own_packet` relay state confirms directness, as do matching
nonzero hop counts when the state is absent. Both traceroute directions
are grouped as `meshtastic_traceroute` using the stored directed leg and
SNR in dB. Neighbour rows become `meshtastic_neighbor`.

Use the valid position nearest the observation time, preferring the
earlier timestamp on equal distance in time. Forward the reported
altitude. Precision uses the reported radius or half the diagonal of the
coordinate cell implied by `precision_bits`. Its half-width in latitude
is `2^(31 - bits) × 0.011132 m`. The longitude component is scaled by
the cosine of latitude.

Radio metadata uses the latest row at or before the sample and falls
back to the earliest. Known numeric and textual device roles are converted
to the same lowercase name. Link fields prefer the transmitter with receiver
fallback. TX power comes only from the transmitter record. A measurement's
reported LoRa region takes precedence over endpoint radio metadata.
Missing spreading factor and bandwidth use the nominal Meshtastic preset
values (`preset_default`), with the firmware's wide bandwidth values
when the radio reports `LORA_24`. Frequency and coding rate require
reported values. [Firmware preset table](https://github.com/meshtastic/firmware/blob/a00675e00c3445927d5e24ce820bc40bde34aaaf/src/mesh/MeshRadio.h).

### Signal power from RSSI and SNR

LoRa packet RSSI is total in-band power. Near and below the noise floor it
flattens at the noise level, while SNR stays informative. Above about +10 dB,
SNR saturates and RSSI carries the information. For a reception with both
metrics, calculate the two powers before aggregation.

- Signal `S = RSSI + SNR − 10·log10(1 + 10^(SNR/10))`.
- Noise `N = RSSI − 10·log10(1 + 10^(SNR/10))`.

In the eth0.no capture, mean RSSI is about −112 dBm for SNR −12 to −8 dB and
−41 dBm for SNR +12 dB, which confirms both regimes.

### Quality flags

Endpoint flags describe the selected position and node history. Across
the snapshot set, an exact coordinate shared by three or more distinct
pseudonyms is `default_position`. An unlocated node that reported
`(0,0)` also receives that flag. A distinct node within 5 m of the
position, including a historical position, triggers `duplicate_position`.
A spread greater than 200 m within a source triggers `moved`.
`coarse_position` means a precision radius above 100 m or fewer than 16
precision bits. Use `unknown_precision` if a radius is unavailable.

A link gets `few_samples` for fewer than five receptions. For the other
link flags, one qualifying reception suffices. `over_300km` uses the
reception-time positions, or the latest-position distance rule for a
Meshtastic neighbour report. `mqtt_suspect` uses the capture's
`measurement_flags` view, which flags neighbour rows with zero SNR,
reporting-receiver firmware older than 2.3.2 (latest firmware at the
reception, otherwise earliest), or latest positions over 300 km apart.
`via_mqtt` records an explicit value of 1. `radio_mismatch` compares
known frequency or bandwidth values (difference above 0.001 in their
respective units) or different spreading factors at both endpoints.
These comparisons also apply to the reference time of non-detections.

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
