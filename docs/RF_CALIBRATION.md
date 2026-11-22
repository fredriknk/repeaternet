# RF calibration against measured links

Status: in progress (started 2026-11-22). This page describes the method, the
file contracts between stages and, once available, the results. It records
measurements of RepeaterNet's path-loss predictions against real receptions.
[Wired into rf-core](#wired-into-rf-core) describes how the planner now uses
the results.

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

SondeHub paths are mostly air to ground. The independent analysis below
checks receiver spread and refraction, with a small terrain-shadow comparison.

Observers log every copy of a flood packet they hear, one per relay. A relay
that is known to have forwarded a packet, because another observer heard that
copy, gave the observer an opportunity to hear it. This turns repeated
silence into **non-detection evidence** instead of missing data.

## Dataset

Extract reads the CoreScope and Meshtastic SQLite snapshots in
`<work>/snapshots/` in read-only mode. It stages each output in the dataset
directory with a `.partial` suffix and replaces each file by rename, publishing
the summary last. The snapshot layout has one file per capture source.
The terrestrial extraction uses CoreScope and Meshtastic. The SondeHub
subcommand reads its snapshot separately.

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
transmission to `sender_pubkey` (`corescope_zero_hop`). MeshCore sends a
zero-hop advert as a direct packet with an empty path, so this holds for
any route type. Otherwise the packet must be a flood (route type 0 or 1)
and the final element of `resolved_path` must identify the transmitter
(`corescope_last_hop`). A flood packet accumulates the relays that already
forwarded it, so its last entry transmitted the copy the observer heard.
A direct packet (route type 2 or 3) carries the route ahead, and each relay
strips itself, so its transmitter is unknown and it is not attributed.
Before this rule, direct-routed pairs had measured excess loss
anti-correlated with every engine (r = −0.46). The receiver is
`observer_id`. Keys are case-insensitive.

`short_relay_prefix` marks links where the relay was resolved from a
one-byte hash, which is often ambiguous. `implausible_signal` marks links
received more strongly than free space allows with a 42 dBm budget
(30 dBm into 6 dBi antennas at both ends). Those have a wrong position or a
misattributed relay, and the fit excludes them.

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

- Engines are compared on the same links. A configuration is
  `engine · surface · clutter · step · k`. Antenna heights are fitted per node
  in-sample and set from the role prior for held-out predictions.
- **Held-out prediction.** Nodes are assigned to 20 km blocks, and blocks to
  five folds. For each pair of folds {k, j}, the test links are those whose
  endpoints lie in folds k and j, and the fit uses only links touching neither
  fold. No test node appears in training, so a prediction has a fitted budget
  constant but no node offsets, which is what a planner faces at a new site.
- **Primary score.** This is the mean held-out log predictive density per link
  under N(μ, σ² + σ_tx² + σ_rx²). Heard links contribute a density, and
  never-heard pairs contribute `log Φ((bound − μ)/s)`. Scores are reported as
  paired differences against the planner's current configuration. The standard
  error ignores clustering by node, so read it as a lower bound.
- **Also reported:** AUC for heard against never-heard links, and bias, RMSE
  and sd of held-out errors on heard links. These are truncated, because only
  strong links are heard. Bias "as configured" uses the planner defaults
  (24.3 dB budget, 3 m antennas) with no fitting at all. The excess scale `a`
  in `FSPL + a · excess` is fitted with censoring on the training folds.
- Norway (Kartverket 10 m DTM and DOM) and world (Copernicus GLO-30, a surface
  model, at 30 m) are reported separately.

## Results

First run on 2026-11-22. The data comes from captures over 20–30 hours:
CoreScope from 2026-11-20 23:00 to 2026-11-21 19:10 UTC, plus the Meshtastic
and other snapshots available that day. All numbers are dB unless stated.
Treat this as a first measurement, not a final calibration: the Norway set has
101 heard links.

### Dataset

| Region | Evidence | Links | Usable | Usable with n ≥ 5 | Receptions |
| --- | --- | ---: | ---: | ---: | ---: |
| Norway | MeshCore flood last hop | 114 | 109 | 97 | 37,693 |
| Norway | MeshCore zero-hop advert | 67 | 64 | 21 | 265 |
| Norway | MeshCore never heard | 1,185 | 1,185 | — | — |
| Norway | Meshtastic traceroute (SNR) | 426 | 117 | 75 | 7,980 |
| Norway | Meshtastic gateway (RSSI and SNR) | 93 | 29 | 16 | 776 |
| World | MeshCore flood last hop (Benelux) | 179 | 175 | 155 | 66,128 |
| World | MeshCore zero-hop advert | 91 | 86 | 28 | 380 |
| World | MeshCore never heard | 2,235 | 2,233 | — | — |
| World | Meshtastic traceroute (SNR) | 1,575 | 830 | 91 | 4,670 |
| World | Meshtastic neighbour (SNR) | 7,429 | 3,982 | 14 | 10,095 |

"Usable" excludes implausible signal, MQTT-suspect links, default
positions and moved endpoints. MeshCore positions are exact, and links carry
many packets. Most public Meshtastic positions are deliberately coarse:
409 of 426 Norway traceroute links have a coarse endpoint and 284 have a
moved endpoint. Within one link, signal varies little: the median
within-link σ_t is 2.5 dB in Norway (P90 4.4) and 2.0 dB in the world set
(P90 4.6).

The Norway run uses CoreScope only (never-heard pairs up to 40 km). Terrain
for the nationwide Meshtastic links would add about 5.9 GB for about 146
coarse links. The world run uses CoreScope Benelux plus Meshtastic gateway and
traceroute links on GLO-30. Worldwide Meshtastic neighbour reports were not
profiled in the first run. See [Full dataset rerun](#full-dataset-rerun).

### Norway (Kartverket 10 m)

101 heard links, 389 never-heard pairs and 88 held-out heard links. Δ is the
held-out log score per link against the planner's current engine
(Bullington on DTM, 10 m, k = 1). Bias is measured minus predicted signal, so
negative means the planner is optimistic.

| Configuration | Δ log score (±SE) | AUC heard / unheard | Bias as configured | Held-out bias | Held-out sd | Excess scale |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Deygout, DTM + DOM P.2108 terminal clutter | +0.088 ± 0.015 | 0.89 | +4.8 | +2.8 | 23.0 | 0.57 |
| ITM 90% time, DTM | +0.084 ± 0.016 | 0.87 | −1.6 | −0.5 | 25.2 | 0.46 |
| Giovaneli, DTM + DOM P.2108 | +0.081 ± 0.014 | 0.89 | +4.8 | +3.1 | 22.4 | 0.65 |
| Deygout, DTM | +0.075 ± 0.011 | 0.89 | −2.0 | −0.1 | 21.6 | 0.58 |
| ITM 50%, DTM | +0.073 ± 0.016 | 0.87 | −2.2 | −0.9 | 25.0 | 0.47 |
| Giovaneli, DTM | +0.072 ± 0.011 | 0.89 | −2.1 | +0.3 | 20.5 | 0.66 |
| Bullington, DTM + DOM P.2108 | +0.023 ± 0.008 | 0.89 | +1.6 | +0.1 | 21.8 | 0.62 |
| Epstein–Peterson, DTM | +0.023 ± 0.012 | 0.87 | −4.0 | −0.9 | 19.6 | 0.71 |
| P.526 delta-Bullington, DTM | +0.009 ± 0.002 | 0.89 | −4.8 | −2.5 | 20.6 | 0.62 |
| P.1812, DTM | +0.002 ± 0.002 | 0.89 | −5.3 | −2.9 | 20.4 | 0.64 |
| **Bullington, DTM (planner)** | 0 | 0.89 | **−5.2** | −2.8 | 20.3 | 0.63 |
| Bullington, DOM as obstacle | +0.013 ± 0.048 | 0.85 | +21.0 | +13.8 | 23.4 | 0.66 |
| Bullington, DTM, 50 m step | −0.014 ± 0.004 | 0.89 | −6.8 | −3.6 | 20.1 | 0.62 |
| Bullington, DTM, 200 m step | −0.036 ± 0.009 | 0.88 | −9.4 | −4.7 | 19.8 | 0.60 |
| ITM 50%, DOM | −0.036 ± 0.063 | 0.81 | +38.3 | +24.6 | 31.2 | 0.47 |
| Free space | −0.728 ± 0.027 | 0.71 | −24.2 | −19.4 | 18.2 | — |

- **Terrain matters.** Every terrain engine beats free space by about
  0.7 nats per link, and the AUC rises from 0.71 to 0.85–0.89.
- **The planner is about 5 dB optimistic** with its defaults (22 dBm,
  2.15 dBi, 1 dB feed loss, 3 m antennas, Bullington on DTM).
- **Coarse profile steps make it worse.** The 50 m and 200 m steps add
  1.6 and 4.2 dB of optimism and score lower, confirming the ridge-skipping
  bias in coverage and optimizer screening.
- **DOM as a solid obstacle surface is about 21 dB too pessimistic** for
  every engine. Trees are not knife edges at 869 MHz.
- **Multi-edge methods (Deygout, Giovaneli) beat Bullington.** Adding the
  P.2108 §3.1 terminal correction with clutter height from DOM − DTM helps
  further. P.833 terminal woodland does not help.
- **The engines overstate terrain loss.** The censored fit supports about
  0.6 of each engine's excess loss. A fitted scale did not improve held-out
  scores, though, so it is not recommended as a correction yet.
- **Bias with v.** For Bullington on DTM, the held-out bias is negative for
  line-of-sight links (−9 dB for v < −0.78) and positive for strongly
  obstructed ones (+8 dB for v 3–6). Survivorship inflates that last number,
  because only the strong obstructed links are heard.

### Norway sensitivity checks

Same configurations and held-out test as the Norway table.

| Subset | Heard / never heard | Best DTM configurations (Δ log score) | Planner bias as configured | DOM as obstacle |
| --- | --- | --- | ---: | --- |
| All CoreScope (main table) | 101 / 389 | Deygout + P.2108 +0.088 ± 0.015, Giovaneli + P.2108 +0.081 ± 0.014 | −5.2 | +21 dB bias |
| Without one-byte relay prefixes | 81 / 389 | Deygout + P.2108 +0.111 ± 0.015, Giovaneli + P.2108 +0.106 ± 0.013 | −2.4 | +24 dB bias; best AUC-driven score (+0.15 ± 0.03) |
| Zero-hop adverts only | 52 / 0 | All DTM engines within ±0.05 of Bullington; P.2108 slightly worse (−0.10 ± 0.07) | −1.5 | −0.47 to −0.53, +25 dB bias |

- **Holds in every subset:** diffract over DTM rather than DOM. Coarse steps
  are worse (200 m: −0.04). Delta-Bullington is never worse than Bullington.
- **The planner's optimism is partly attribution noise.** It falls from about
  5 dB on all links to 1.5–2.5 dB on the cleanest subsets.
- **The P.2108 terminal correction helps relay-to-observer links** (rooftop
  and mast repeaters) but not zero-hop links, where the sender is often a
  companion device near the observer. Make it a per-site option rather than a
  global default.

### World (Copernicus GLO-30, 30 m)

CoreScope Benelux: 139 heard links and 2,211 never-heard pairs. Meshtastic:
24 gateway links and 678 traceroute links (SNR only).

| Configuration | CoreScope Δ log score | AUC | Bias as configured | Meshtastic SNR Δ log score |
| --- | ---: | ---: | ---: | ---: |
| P.526 delta-Bullington | +0.080 ± 0.002 | 0.95 | −13.5 | +0.003 ± 0.010 |
| ITM 90% time | +0.064 ± 0.003 | 0.95 | −10.0 | −0.468 ± 0.029 |
| ITM 50% | +0.043 ± 0.003 | 0.95 | −10.9 | −0.415 ± 0.026 |
| **Bullington (planner)** | 0 | 0.94 | −17.0 | 0 |
| Deygout | −0.001 ± 0.002 | 0.94 | −15.1 | −0.272 ± 0.034 |
| Giovaneli | −0.011 ± 0.002 | 0.94 | −15.3 | −0.211 ± 0.035 |
| Bullington, 200 m step | −0.025 ± 0.002 | 0.94 | −21.4 | +0.007 ± 0.011 |
| Free space | −0.773 ± 0.015 | 0.92 | −39.2 | +0.565 ± 0.036 |

- **Delta-Bullington's smooth-earth term matters on flat, long Benelux
  paths** (+0.08). It is never worse than Bullington in Norway.
- **GLO-30 is a surface model,** so trees and buildings act as knife edges,
  as DOM does in Norway. The fit supports only 0.1–0.2 of the excess, and the
  planner is 10–17 dB optimistic overall. That points at urban clutter and
  indoor receivers the terrain cannot see.
- **With coarse Meshtastic positions, free space scores best** on SNR. Fuzzed
  positions (often more than 1 km) make terrain-specific predictions noise.
  These links are not suitable for engine selection.

### Full dataset rerun

After the first run, the remaining terrain was downloaded:
- nationwide Norway Meshtastic;
- Norway never-heard pairs beyond 40 km;
- worldwide Meshtastic neighbour reports.

That gave 8,310 profiles, with 6.3 GB fetched against a 9.9 GB estimate, and
the engines ran on 9,983 directed links. Held-out Δ log score against the
planner's Bullington (DTM, native step, k = 1):

| Configuration | Norway CoreScope (147 heard, 1,167 never heard) | World CoreScope (139 heard tested, 2,211 never heard) | World Meshtastic SNR (3,368 tested) |
| --- | ---: | ---: | ---: |
| Deygout + DOM P.2108 | +0.048 ± 0.008 | — | — |
| ITM 90% time, DTM | +0.049 ± 0.006 | +0.077 ± 0.003 | −0.329 ± 0.010 |
| Deygout, DTM | +0.035 ± 0.005 | +0.014 ± 0.002 | −0.143 ± 0.011 |
| Giovaneli, DTM | +0.029 ± 0.004 | −0.000 ± 0.002 | −0.090 ± 0.011 |
| P.526 delta-Bullington, DTM | +0.011 ± 0.001 | +0.076 ± 0.002 | −0.067 ± 0.014 |
| **Bullington (planner)** | 0 | 0 | 0 |
| Bullington, 50 m step | −0.006 ± 0.001 | −0.007 ± 0.001 | −0.004 ± 0.002 |
| Bullington, 200 m step | −0.018 ± 0.003 | −0.021 ± 0.002 | +0.002 ± 0.006 |
| Free space | −0.852 ± 0.020 | −0.642 ± 0.014 | +0.435 ± 0.016 |

- **The MeshCore conclusions hold on the larger set.** Multi-edge methods and
  ITM lead in Norway, and delta-Bullington leads in Benelux. Coarse steps are
  worse, and DOM as an obstacle surface carries a bias of about +21 dB. With
  long never-heard pairs included, heard and unheard links separate easily
  (AUC 0.94–0.96).
- **Meshtastic links cannot rank engines.** With deliberately coarse public
  positions, free space predicts SNR best in both regions, and the
  terrain-specific engines add noise. In Norway only 44 Meshtastic SNR links
  and 2 gateway links passed the position checks. Use Meshtastic data for
  budget checks, not for engine choice, unless exact positions are known.
- **The fitted excess scale is not meaningful here.** One scale is fitted
  across blocks, so in the world set the Meshtastic links dominate it, and it
  comes out near 0. It should be fitted per block in a later revision.

### Variability and fade margin

| Component | Estimate | 90% margin | 95% margin |
| --- | --- | ---: | ---: |
| Time, within a link (median link) | σ_t = 2.5 dB Norway, 2.0 dB world | 3.2 | 4.1 |
| Time, within a link (P90 link) | σ_t = 4.4 Norway, 4.6 world | 5.6 | 7.2 |
| Location, known sites (node offsets fitted) | σ ≈ 11 dB Norway (in-sample) | 14 | 18 |
| Location, new sites (held out) | sd ≈ 20–23 dB Norway; P10 of error −27 dB | ≈ 27 | — |

Receiver siting and hardware dominate. In Norway the receive-side offset
spread is σ_rx ≈ 14–16 dB, the transmit side about 2 dB. The heard-link
spread is truncated, so new-site figures are lower bounds. The configured
10 dB fade margin covers time fading on a known link. It does not cover
location or siting uncertainty, which a single margin cannot carry honestly.

### SondeHub air-to-ground check

From `rust/`, run the estimate, then sample terrain if the estimate fits.
Run the Python report from the repository root.

```sh
cargo run --release --locked -p repeaternet-rf-calibration --target-dir target/sondehub -- sondehub --estimate-only
cargo run --release --locked -p repeaternet-rf-calibration --target-dir target/sondehub -- sondehub
python -I tools/rf-calibration/sondehub_check.py
```

The source is the read-only `sondehub-import.sqlite3` in the default
calibration snapshot directory. The output location follows `--work-dir` or
`RF_CALIBRATION_DIR`. The Rust command writes versioned `links.jsonl`,
`receivers.jsonl`, `horizons.jsonl` and `profiles.jsonl` under
`<work>/sondehub/`. Link, horizon and profile rows join on `link_id`.
Receiver IDs are the first 16 hex digits of
`SHA-256(salt || "sondehub:" || receiver_key)`, preserving the source key's
case. The existing `<work>/salt.txt` also salts flight IDs. Coordinates and
signal bins stay in the work directory. The Python stage writes
`results.json` with pseudonymised receiver and flight summaries and the
aggregate `report.md` under `<work>/report-sondehub/`.

Each receiver uses its most reported fixed coordinate cell. Membership in
the planner's Kartverket NHM coverage polygon selects the Norway group and
10 m DTM. Outside the polygon, up to 200 receivers selected by hashed ID
use 30 m GLO-30. Receiver terrain is estimated by group, using twice
uncached composed raw size. The 20 GB cap applies before any terrain fetch.
The estimate for extra shadow chords is checked against the remaining budget.
Fetches are sequential through the shared terrain cache. Unavailable tiles
leave incomplete terrain and the command reports the number of failures.
Confirmed download bytes count successful tile fetches.

The receiver horizon samples 0.5° bearings, out to 10 km or the bin's
transmitter distance, at the terrain raster step. A missing terrain sample
makes that ray prefix ineligible for the fit. The receiver antenna elevation
uses reported height as an MSL approximation when it is 2–100 m above DEM
ground, with a 2 m mount above DEM otherwise. Sonde ellipsoid heights are
converted with the importer's EGM96 grid. For unknown sonde datums, a
same-flight fix supplies MSL and AGL when it is within 30 s, 3 km
horizontally and 250 m of the unknown-datum altitude. Raw altitude and height
provenance are retained.

The clear-path fit requires more than 1 km AGL, elevation above 2° and above
the local terrain horizon at k = 1. Nominal TX power and 3-D free-space
loss determine each received bin's offset. Each receiver must have two
qualifying flights. Receiver–flight pairs get equal fit weight, with each
receiver weighted equally for offset spread. Elevation contrasts are estimated
within receiver–flight pairs against 5–15°. RSSI bins at the rdz floor are
excluded. Auto RX offsets combine receive gain, effective noise and
decoder response. The JSON reports the implied effective noise floor at
zero assumed receive gain, which is conditional on that gain assumption.

The horizon check uses the last bin per receiver and flight, whose
representative position can precede its final packet. It requires a
descending sonde, at least 80 km range, elevation within ±1° at k = 1, a
clear sampled receiver horizon and independent reception continuing for
at least two minutes. Each endpoint gives an apparent k by equating its
range to the smooth-earth tangent range. The median on the log scale and
a crossed receiver/flight bootstrap summarize these endpoints. The fit
assumes a horizon-limited endpoint. Finite decoder sensitivity, unknown
receiver activity, distant terrain and uncertain antenna heights can all
cause the last detection to fall away from the radio horizon.

Low-elevation Norwegian shadow candidates have sampled terrain rising at
least 0.1° above the sonde at k = 4/3. At most 48 paths are profiled with
one per receiver–flight pair and up to four per receiver. The RF core
samples complete DTM chords and computes Bullington at k = 1 and k = 4/3.
Measured extra loss is the signal deficit against that receiver's clear-path
offset. Selection is conditional on reception and nominal transmit power.

**Snapshot results (2026-11-22).** The snapshot contains archive objects
from 14–19 November. The run exported 3,572 bins from 13 Norway receivers
and 75 flights, and 71,319 bins from 199 world receivers and 1,583 flights.
The world estimate included one additional receiver without exportable
bins. A complete local horizon was available for all 3,572 Norway bins
and 66,313 world bins.

| Group | Metric | Receivers | Flights | Receiver–flights | Receiver offset σ | Bin residual σ | Receiver–flight residual σ |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Norway | rdz RSSI | 1 | 13 | 13 | — | 2.61 dB | 2.18 dB |
| Norway | Auto RX SNR | 5 | 37 | 46 | 8.83 dB | 3.27 dB | 2.34 dB |
| World | rdz RSSI | 61 | 707 | 1,149 | 8.99 dB | 3.35 dB | 2.51 dB |
| World | Auto RX SNR | 90 | 968 | 1,788 | 6.08 dB | 4.30 dB | 3.57 dB |

The world RSSI within-flight mean was 3.64 dB lower at 30–60° and
9.46 dB lower at 60–90° than at 5–15°. Its residual σ rose from 3.05 dB
at 2–5° to 4.12 dB at 60–90°. Norway Auto RX SNR fell by 3.38 dB at
30–60° across nine receiver–flights. The direction is consistent with
reduced high-angle antenna gain, with sonde orientation and receiver
selection also affecting the contrast. Every qualifying RSSI bin used
matched sonde heights. Norway had one RSSI receiver, so its between-receiver
RSSI spread is unavailable.

| Group | Last-heard receiver–flights | Receivers | Apparent k (95% crossed interval) | Median range / k = 1 horizon | Median range / k = 4/3 horizon |
| --- | ---: | ---: | ---: | ---: | ---: |
| Norway | 3 | 2 | 0.90 (0.74–0.95) | 0.95 | 0.82 |
| World | 350 | 69 | 0.45 (0.37–0.51) | 0.67 | 0.58 |

The fitted horizon puts the median endpoint at roughly 100% of tangent
range in each group by construction. One world endpoint reached beyond
the k = 1 smooth-earth horizon. The low apparent k and short median range
indicate that this last-heard sample rarely reaches the smooth-earth
horizon. The intervals describe sampling variation for these selected
endpoints. They provide insufficient evidence to recalibrate the physical
k = 1 default.

There were four complete Norway shadow profiles from one Auto RX receiver
on four flights. Median measured extra loss was 21.44 dB, versus
36.56 dB from Bullington at k = 1 and 36.33 dB at k = 4/3. Median
absolute errors were 15.07 dB and 14.84 dB respectively. This comparison
covers one receiver and paths where it still decoded the sonde.

The first receiver estimate was 1.088 GB for Norway and 2.755 GB for
world terrain. A Kartverket HTTP 504 interrupted that run. The retry's
receiver estimate was 3.555 GB, plus a later 0.128 GB shadow estimate.
The retry fetched 2.631 GB of upstream terrain with no failed tiles.
The first attempt had confirmed at least 0.043 GB before it stopped.

### Recommendations

1. **Use DTM for diffraction and model trees and buildings as terminal
   clutter, not as obstacles.** DOM as an obstacle surface is about 21 dB too
   pessimistic in every subset. Offer the P.2108 §3.1 height-gain correction,
   with clutter height from DOM − DTM within 30 m of the terminal, as a
   per-site option. It helps relay-to-observer links but not zero-hop links
   from companion devices. Outside Norway, GLO-30 needs the same treatment:
   prefer a bare-earth global DEM where available.
2. **Make P.526 delta-Bullington the default engine.** It is never worse than
   Bullington in any subset (+0.011 in Norway, +0.076 in Benelux on the full
   dataset) and is the complete ITU method. Offer three-edge Deygout as an
   option: it is best in Norway (+0.035, or +0.048 with P.2108) but no better
   in Benelux. Revisit once several weeks of capture exist.
3. **Never certify on coarse steps.** Keep the 10 m final step. Coverage and
   screening at 200 m are about 4 dB more optimistic and score lower in both
   regions, so lower the coverage step or subtract a step-dependent allowance.
4. **Correct the default budget by 2–5 dB.** On all MeshCore links the planner
   is about 5 dB optimistic, and on the cleanest subsets 1.5–2.5 dB. Present
   location and siting uncertainty separately, as a ±15–20 dB band for unknown
   sites, instead of folding it into the fade margin. SondeHub independently
   shows receiver offsets with a 6–9 dB spread.
5. **Keep k = 1.** k = 4/3 never improved a score, and the SondeHub horizon
   check did not support a larger k.
6. **Wiring into `rf-core`.**
   - Add `DiffractionModel { Bullington, DeltaBullington, Deygout3 }` to
     `RfSettings` behind the plan's existing `rf.diffraction_model`, and move
     the ports from `rf-calibration/src/engines/` into `rf-core`.
   - Add a per-site `TerminalClutter { None, P2108Dom }` with the DOM-derived
     height.
   - Include both in the RF model fingerprint, so saved coverage and route
     caches invalidate.
   - Live coverage computes Bullington incrementally on a convex hull.
     Delta-Bullington needs a running least-squares smooth-earth fit per ray,
     which can also be done incrementally. Deygout is not incremental and
     should start in route certification only.

### Wired into rf-core

Recommendations 1–6 are implemented in the planner (2026-11-22). Every
calibrated number lives in one file,
[`rust/crates/rf-core/src/calibration.rs`](../rust/crates/rf-core/src/calibration.rs).
After a rerun, change the values there. The server sends them to the dashboard
as the read-only `calibration` object in `/api/state`, so neither the JSON
defaults nor the dashboard repeat them.

| Setting | Default | Where it applies |
| --- | --- | --- |
| `rf.diffraction_model` | `delta_bullington` (also `bullington`, `deygout3`) | Route search, link checks and saved coverage. Live coverage runs delta-Bullington incrementally and falls back to it when Deygout is selected. |
| `rf.obstacle_surface` | `ground` (DTM). `surface` restores DOM as obstacles | Everywhere. DOM is otherwise used only for terminal clutter. |
| `rf.model_correction_db` | 3 dB, added to path loss | Every budget. |
| `terminal_clutter` per role, with per-router overrides | `none` (`p2108_dom` optional) | P.2108 §3.1 at that end, using the median DOM − DTM within 30 m. The planner samples a disc around the site rather than the path. |
| Final route step | at most 10 m | Route certification. Screening at 200 m and 50 m is unchanged and has no allowance. |
| Coarse-step allowance | 0 / 1.6 / 4.2 dB at 10 / 50 / 200 m, interpolated on log step, relative to native resolution | Saved coverage, targets, point checks and live coverage (about 1 dB at the 30 m live detail). |
| k-factor | 1.0 | Unchanged. |

Analysis shows the prediction spread (σ ≈ 15 dB for a new site) next to the
key figures, separately from the fade margin, which covers time fading
(σ ≈ 2.5 dB). The calibration harness calls the rf-core engines, and its
prediction files are byte-identical to those from the earlier copies. Plans
saved before this change store `"bullington"` without `obstacle_surface` and
now get the calibrated default. Saved coverage and route caches from the old
model are stale (`coverage-rust-v2`, `route-rust-v2`).

Live coverage time per site (HTTP `elapsed_ms`, median of 8 runs after
warm-up, Oslo fjord endpoints, Kartverket 10 m DTM + DOM, 16 threads):

| Site · step · radius | Before (Bullington over DOM) | After, default (delta-Bullington over DTM) | After, Bullington over DTM |
| --- | ---: | ---: | ---: |
| A · 30 m · 20 km (default detail) | 19.5 ms | 25.8 ms | 17.9 ms |
| A · 120 m · 20 km (drag preview) | 3.4 ms | 3.7 ms | 3.1 ms |
| A · 10 m · 10 km | 40.5 ms | 56.2 ms | 40.0 ms |
| B · 30 m · 20 km | 24.2 ms | 28.0 ms | 21.4 ms |
| B · 120 m · 20 km | 3.5 ms | 3.6 ms | 3.2 ms |
| B · 10 m · 10 km | 56 ms | 93.7 ms | 82.2 ms |

The live delta term uses running prefix sums and hull queries for the
smooth-earth fit, a closed-form smooth-profile Bullington, and tabulated
P.526 spherical-earth functions. The measured error is at most 0.0013 dB
against the exact rf-core function, well inside the 0.5 dB display step.
Route certification always uses the exact rf-core evaluator. Most of the
remaining cost at B's 10 m step comes from DTM itself: without trees, more
rays stay in line of sight and take the O(n) Bullington line-of-sight scan.

### Caveats and next steps

- About one day of mesh captures. Let the captures run for several weeks and
  rerun the pipeline unchanged. Report node-clustered bootstrap intervals once
  there are enough nodes; the current standard errors ignore clustering.
- Antenna heights and hardware are unknown, which the fit handles with priors
  and node offsets. Observers registering their antenna height and gain would
  sharpen absolute calibration more than any engine change.
- Public Meshtastic positions are too coarse for engine selection. Use them
  only for budget checks, or where exact positions are known.
- Fit the excess-loss scale per block. A single scale across blocks is
  dominated by whichever source has the most links.
- Profiles with gaps over water are dropped (44 paths). Filling sea with 0 m
  would keep long coastal paths.
