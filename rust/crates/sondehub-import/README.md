# SondeHub measured-link importer

`sondehub-import` imports worldwide 400–406 MHz radiosonde receptions from
the [SondeHub history archive](https://github.com/projecthorus/sondehub-infra/wiki/SondeHub-Historic-Data).
It reads daily UTC serial indexes at `https://history.sondehub.org/`, then
downloads `serial/{serial}.json.gz` for each listed flight. A serial object
contains the whole flight, including observations across UTC midnight.
Choose **Last week** in the measured-links viewer to allow for archive lag.

## Run and resume

Build from `rust/` and run the executable (add `.exe` on Windows).

```sh
cargo build --release --locked --target-dir target/sondehub
target/sondehub/release/sondehub-import
target/sondehub/release/meshtastic-capture view --port 8796
```

| Option | Default | Behaviour |
| --- | --- | --- |
| `--db-dir DIR` | `%LOCALAPPDATA%/RepeaterNet` or `$XDG_DATA_HOME/RepeaterNet` | Shared capture directory. On Unix the fallback is `~/.local/share/RepeaterNet`. |
| `--since YYYY-MM-DD` | `2021-01-01` | Oldest archive day to list, inclusive. |
| `--until YYYY-MM-DD` | Two UTC days ago | Newest archive day to list, inclusive. The delay allows for late publication. |
| `--total-budget-gb GB` | 50 | Total capture database ceiling in decimal GB. |
| `--reserve-gb GB` | 5 | Leave this space for live captures. |
| `--min-interval-ms MS` | 1000 | Minimum time between HTTP request starts, including terrain and retries. |
| `--low-bin-seconds SECONDS` | 30 | UTC bin width below 3,000 m AGL. |
| `--high-bin-seconds SECONDS` | 300 | UTC bin width at or above 3,000 m AGL and when AGL is unavailable. |
| `--tile-cache-dir DIR` | `RepeaterNet/cache/terrarium` in the user data directory | Override to isolate a test or put the cache on another disk. |
| `--tile-cache-gb GB` | 2 | Disk limit for least-recently-used Terrarium PNG tiles, separate from the database limit. |

One client makes sequential HTTPS requests using RepeaterNet's descriptive
User-Agent. It requests gzip and counts downloaded response bodies before
decompression, including listing and terrain requests. A retryable failure
waits at least 2 or 4 seconds before the next attempt, or longer if
`Retry-After` says so. Three successive retryable failures for a request,
including HTTP 403, 429 or 5xx, stop the run. Missing serial objects are
recorded and retried on a later run. The importer prints bytes, requests,
terrain requests, objects, input and kept row counts, and rejects by reason.

The importer walks backward from the newest day. It commits each serial
object together with its observations and viewer aggregates. Ctrl+C rolls
back an in-progress transaction. Run the same command again to skip
completed objects, including objects with a terminal parse error recorded
as `invalid`. Binning settings are fixed per database. To change them,
start in a different directory. The importer takes an OS lock to prevent
multiple writers and uses SQLite WAL so the viewer can read committed
batches during import.

## Shared disk budget

The size check adds the lengths of all `*.sqlite3`, `*.sqlite3-wal` and
`*.sqlite3-shm` files directly in the selected directory, including
backup files with those suffixes. Before committing it accounts for
possible additional pages in the WAL, SHM and main file. The next batch
rolls back if the estimate would exceed the total limit minus the
reserve. Other processes can grow these files between checks. The
reserve gives them headroom.

`meshtastic-capture run --total-budget-gb 50` checks the same files and
stops each host at the full limit. `corescope-capture` does not enforce
the budget yet. The viewer's **Capture totals** shows usage against the
full limit for the selected directory.

## What gets stored

The importer keeps valid positions, a 400–406 MHz frequency and at least
one finite signal reading. It counts missing or invalid receiver positions,
including `0,0`. Within each flight it deduplicates by receiver, frame
and sonde timestamp. Among usable duplicates it keeps the earliest
receipt time, breaking ties by archive order. UTC bins use the configured
width for the observation's AGL class and are keyed by receiver, decoder
family, bin width and bin start. The earliest sonde timestamp and then
lowest frame number supply the representative observation. Its
positions are an example for the bin. Each signal separately has a
sample count, minimum, maximum and mean across the deduplicated rows.

| Table | Contents |
| --- | --- |
| `source_object` | HTTPS locator, archive day, ETag, SHA-256 of downloaded object bytes, retrieval time, licence, status and per-object row and reject counts. |
| `receiver` | Public receiver callsign as an identifier. The viewer displays it in the map tooltip. |
| `measured_link` | Representative sonde and receiver position, raw altitude and datum, sonde AGL, frequency in Hz and basis, modulation and sonde type where known, nominal TX power and basis where available, decoder and version, receiver antenna and LNA indication, bin statistics and quality flags. Times are UTC Unix milliseconds except bin starts and manifest timestamps, which are seconds. |
| `flight_extent`, `flight_signal` | Flight bounds and sample-weighted viewer aggregates per flight, receiver, metric and AGL band. |
| `import_settings` | The two time-bin widths used in the database. |

An explicit in-band `tx_frequency` takes precedence over the receiver's
`frequency` estimate. A public model specification supplies
`tx_power_basis='nominal_model_spec'` where the model is specific enough.
The value is a calibration assumption. It can differ by transmitter mode
and excludes antenna gain. The table uses 60 mW for RS41-SG and RS41-SGP
(minimum in high-power mode) and iMet-4-AB (low end of the specified range),
100 mW for DFM-17, WxR-301D and iMS-100, 150 mW for M20 and 50 mW for PS-B3.
[Vaisala RS41-SG](https://docs.vaisala.com/v/u/B211321EN-L/en-US),
[Vaisala RS41-SGP](https://docs.vaisala.com/api/khub/documents/Esj71UqX7oDBFLgDCSbsFA/content),
[GRAW DFM-17](https://www.graw.de/products/radiosondes/radiosonde-dfm-17),
[InterMet iMet-4-AB](https://www.intermetsystems.com/wp-content/uploads/2022/12/iMet4-AB.pdf),
[WMO intercomparison, Table 3.2](https://www.gruan.org/gruan/editor/documents/wmo/IOM-143_UAII-2022.pdf).

Receiver antenna descriptions are the only uploader free-text field kept.
Descriptions with apparent email addresses, URLs or phone numbers are
discarded. The LNA flag indicates that the description mentioned an LNA or
preamp. Receiver altitude datum is unknown in the archive. Losses, antenna
gain, receiver bias and signal detection thresholds can vary by site.

### Height and quality flags

AGL uses zoom-9 [AWS Terrain Tiles](https://registry.opendata.aws/terrain-tiles/)
in [Terrarium format](https://github.com/tilezen/joerd/blob/master/docs/formats.md).
The importer samples the containing pixel and uses sea level over negative
terrain pixels. For GPS ellipsoid heights it subtracts EGM96 geoid undulation
before subtracting the terrain height. The public-domain embedded grid
samples every fourth row and column of NGA's `us_nga_egm96_15.tif`
(SHA-256 `db493027562c9b004d7220fa881f5603adada4e1c5029b933fa7de4547b0e78d`),
stores rounded centimetres as little-endian signed 16-bit values, and uses
bilinear interpolation. AGL is an estimate at the DEM and geoid resolution.
It is null when the sonde datum is unknown or terrain is unavailable, for
example outside Web Mercator coverage. [PROJ-data grid and licence](https://github.com/OSGeo/PROJ-data/blob/master/us_nga/us_nga_README.txt).

| Bit value | Meaning |
| ---: | --- |
| 1 | Great-circle path over 800 km. |
| 2 | Receiver moved more than 1 km between positions or from its first position in this flight. |
| 4 | Balloon more than 100 m below the estimated terrain. |
| 8 | SNR of at least 60 dB. |
| 16 | Unknown balloon altitude datum. |
| 32 | No terrain sample for a balloon with a known datum. |
| 64 | Unknown receiver altitude datum. |
| 128 | rdzTTGOsonde RSSI at or below the −127.5 dBm floor. |

A flag applies when any retained sample in the bin meets its condition.
The viewer hides the first four flags by default. It filters by the
representative balloon position, time window and AGL, and draws up to
15,000 link groups. Fully visible flights use precomputed groups and
boundary flights use the measured bins that fall inside the query.
`sonde_snr_auto_rx` uses Auto RX demodulator Eb/N0-style SNR.
`sonde_rssi_rdz` uses rdzTTGOsonde SX127x RSSI in half-dB steps.
Each has a separate map legend and aggregate. Other decoder families,
if present, remain in the database with their own signal kinds.

## Smoke test and storage estimate

The worldwide run for the 2026-11-19 and 2026-11-20 UTC indexes imported
1,362 distinct serial objects, all successfully. The two lists had 193
serials in common. The run downloaded 1,524,701,932 bytes in 2,962 HTTP
requests, including 1,595 terrain requests. It read 54,700,063 rows,
rejected 28,273,721 and subsampled the remaining 26,426,342 into 299,127
stored bins. A repeat run skipped all 1,362 objects and fetched only the
two date listings.

After checkpointing, the SondeHub file was 88,350,720 bytes including
provenance and viewer aggregates, or **295.4 bytes per stored bin**.
That is about **149,600 bins per archive day** and **16.1 GB per year**.
With 5 GB reserved and about 0.41 GB already used by other captures,
the 50 GB limit projects to about **1,009 days (33 months)** of history.
At the measured default pace of about 57 minutes for two days, that is
about **20 days of continuous import**. The estimate uses two recent
days and includes initial terrain downloads. Flight coverage, archive
density, cache reuse and network conditions will change both figures.

## Licence and attribution

SondeHub Radiosonde Telemetry is licensed under
[CC BY-SA 2.0](https://creativecommons.org/licenses/by-sa/2.0/). When
redistributing extracts, credit SondeHub and its contributing receivers,
include the source and retrieval date, identify the transformations, and
distribute the adapted data under the same licence.

> SondeHub Radiosonde Telemetry was accessed on YYYY-MM-DD from the AWS
> Registry of Open Data and SondeHub history archive. RepeaterNet
> deduplicated, subsampled and added terrain estimates to these
> CC BY-SA 2.0 data.

[SondeHub archive registry and citation](https://registry.opendata.aws/sondehub-telemetry/).
For terrain attribution, see the [Terrain Tiles source terms](https://github.com/tilezen/joerd/blob/master/docs/attribution.md).
