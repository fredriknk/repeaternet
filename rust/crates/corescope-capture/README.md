# CoreScope capture

`corescope-capture` records CoreScope RF receptions and topology in SQLite for
later validation of RepeaterNet link predictions. It is a separate executable
so it can run and be scheduled independently of the planner.

The tool reads packet IDs and hashes, observer identities, reception times,
RSSI, SNR, direction, route and payload types, scope, relay prefixes and
resolved relay public keys. For public MeshCore adverts it also records the
sender public key, advertised coordinates and name, and the raw role flags
byte. Node, observer and neighbor snapshots retain the upstream metadata as
JSON, including observer radio settings when supplied. The database excludes
packet bytes, decoded message text and channel messages. These fields can
contain private communications and are unnecessary for link validation. The
tool only makes GET requests to the capture endpoints. Backups, channels and
administrative controls are outside its scope.

## Start a continuous capture

From `rust/`, start the capture and leave the terminal open.

```sh
cargo run --release --locked -p corescope-capture -- watch
```

On Windows, you can build once with
`cargo build --release --locked -p corescope-capture` and run
`.\target\release\corescope-capture.exe watch` from PowerShell. On Linux or macOS,
run `./target/release/corescope-capture watch`.

The terminal prints the database path and the dashboard URL. Each cycle reports
new receptions, distinct directed pairs, requests this cycle and in the last
24 hours, the next packet poll and any backoff. Press **Ctrl+C** to finish the
current bounded request or SQLite write, close the dashboard and save the
cursor. Restart with the same database to resume.

Open [the local dashboard](http://127.0.0.1:8790/) to see measured last hops from
the final resolved relay or a zero-hop advert sender to an observer. Click a
line to see its direction, observation count, median SNR and RSSI, distance,
and last seen time. Filter by sample count, time window and last-hop type.
Capture totals cover the whole database for the selected CoreScope endpoint.
Lines require a position for each endpoint. The dashboard refreshes every
minute using SQLite. It serves Leaflet locally and loads OpenStreetMap tiles
from OSM. Use `--dashboard-port <port>` to change the loopback port or
`--no-dashboard` to run the terminal capture alone.

Packet capture covers all regions reported by CoreScope, including
Scandinavia and Finland. Per-node neighbor lookups default to
`--bbox 54,71.5,4,31.7` (south, north, west, east in WGS84 degrees), covering
Scandinavia, including Denmark, and Finland. The bounds set the initial map
view until measured links are available, and define the within-bounds pair
count. RepeaterNet currently supplies terrain for Norway only. Analysis
elsewhere requires other terrain data, such as Copernicus GLO-30 or a
national DEM.

## Request pacing

CoreScope is volunteer-run. Watch uses one HTTP client, sends requests
sequentially, and identifies itself as RepeaterNet.

- Packets are polled every 15 minutes by default (`--packet-minutes`, minimum 5).
  A fresh watch starts with the last 15 minutes. Subsequent polls resume the
  cursor and overlap recent traffic to pick up late receptions.
- Nodes, observers and the global neighbor graph are captured every 6 hours
  (`--snapshot-minutes`, minimum 60). A failed topology cycle can try again
  after an hour. A restart reuses a recent complete snapshot. The client uses
  conditional requests during the session when CoreScope supplies validators.
- Cycle intervals vary by ±10% while respecting the minimums. The default
  `--rate 0.5` spaces request starts at least two seconds apart. The fastest
  accepted setting is `--rate 1`.
- Watch attempts up to 10 in-bounds node neighbor requests at the end of each
  packet cycle (`--neighbor-batch 0..20`). It tries nodes with no prior attempt
  first, then the oldest attempts. A node's neighbors are fetched again only
  after 7 days (`--neighbor-refresh-hours`, minimum 24). There is also a
  rolling cap of 300 per-node lookups per day to bound traffic as the inventory
  grows.
- A failed cycle pauses upstream requests. Consecutive failures back off from
  one minute to an hour. A longer server `Retry-After` takes precedence.
  This covers HTTP 429, 5xx, timeouts and other request failures.

Expect **about 350 requests per day** at defaults once every in-bounds node has
been looked up once: roughly 200 packet requests, 20 topology requests and
about 110 neighbor lookups (around 750 located nodes refreshed weekly). The
first two to three days work through the initial neighbor backlog at up to 300
lookups per day, so expect up to about 520 requests per day then. The packet
estimate assumes two 250-packet pages per poll, including the overlap. Traffic
and page counts vary. With the default eight-page packet cap, even sustained
capped polls stay under about 1,100 requests per day.

## Database and one-shot captures

- `--db <file>` overrides the default capture file. The default is
  `%LOCALAPPDATA%/RepeaterNet/corescope-capture.sqlite3` on Windows and
  `$XDG_DATA_HOME/RepeaterNet/corescope-capture.sqlite3` on Unix
  (`~/.local/share` if XDG is unset). The file must live outside the
  RepeaterNet checkout and `%LOCALAPPDATA%/RepeaterNet/data`. Run one
  capture process per file.
- `snapshot` takes one topology snapshot and one bounded batch of node
  neighbor lookups. `packets` does one packet scan. On a fresh file, it
  starts at the latest packet page and walks backward through the available
  feed. `run` does both once.
- `--base-url <url>` overrides `https://corescope.eth0.no`.
- `--max-pages <count>` defaults to eight for watch packet cycles and 40 for
  node snapshots and one-shot packet scans. Packets request 250 transmissions
  per page and nodes request 500. Capped packet scans resume from the last
  committed page.

## SQLite schema

The database uses `PRAGMA user_version = 2` and WAL mode so the dashboard
can read while capture writes. Existing version 1 files are upgraded in place.

| Table | Contents |
| --- | --- |
| `observations` | One row per CoreScope reception, keyed by endpoint, packet ID and observation ID. For a representative-only response, the observer identity is the fallback key. Expanded observations can include several real receptions of the same transmission at one observer. Path columns contain JSON arrays with null placeholders for unresolved positions. |
| `nodes_snapshot`, `observers_snapshot` | Timestamped JSON records keyed by capture run and public key or observer ID, with location columns for joins. |
| `neighbors_snapshot` | Timestamped JSON responses keyed by capture run and source public key. An empty source key denotes the global graph. |
| `capture_runs` | CoreScope endpoint and version, mode, start and end, status, request and row counts, page-cap flag and errors. |
| `packet_cursor` | Per-endpoint time window and last committed packet ID/offset. The cursor and observations advance together in a SQLite transaction. |
| `request_log` | One row per outbound HTTP attempt, with a timestamp and a node key for neighbor lookups. The rolling 24-hour count begins when version 2 is installed. |
| `watch_state` | Per-endpoint poll and backoff status. |
| `measured_hops` | View of received last hops used to aggregate directed pairs for the dashboard. |

Packet time filters use the transmission's first-seen time. CoreScope
v3.12.0 orders the results by ingest ID. The capture freezes the upper time
bound across capped runs and verifies an overlapping ID between pages.
Each completed scan rewinds the lower time bound by 15 minutes on its next
poll to collect late arrivals and receptions added to existing transmissions.
CoreScope exposes offset pagination and a finite, server-configured history.
The default server observed in November 2026 capped packet pages at 10,000
and returned about seven days from its active feed. An explicitly older
`since` reached about 30 days through CoreScope's disk-backed query, which
returns representative rows without expanded receptions in v3.12.0.
Packets that age out during a scan or arrive with first-seen times older
than the overlap can be missed. Fresh snapshots are historical records, so a
second snapshot creates a new set of timestamped rows.

## Last-hop observations

This example selects received observations with RSSI and SNR. The final
position in `resolved_path` must contain a public key for a relayed packet.
For an explicitly empty prefix path, a public advert's sender is the
transmitter. CoreScope may report missing paths or signal values, and the
query treats zero RSSI as unavailable.

```sql
WITH pairs AS (
    SELECT endpoint, packet_id, observation_id, timestamp,
           CASE
               WHEN json_array_length(path_prefixes) = 0 THEN sender_pubkey
               ELSE json_extract(resolved_path, '$[#-1]')
           END AS transmitter_pubkey,
           observer_id, rssi, snr
    FROM observations
    WHERE direction = 'rx' AND rssi < 0 AND snr IS NOT NULL
)
SELECT *
FROM pairs
WHERE transmitter_pubkey IS NOT NULL AND observer_id IS NOT NULL
  AND transmitter_pubkey <> observer_id
ORDER BY timestamp DESC;
```

Join public keys to the relevant snapshot run for locations. An observer
with `(0, 0)` has an unknown location. A matching located node public key
can supply a candidate location for that observer. Missing or ambiguous
relay resolutions limit the set of samples that can be linked to terrain.
