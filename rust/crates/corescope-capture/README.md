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

## Usage

From `rust/`:

```sh
cargo build --locked -p corescope-capture
cargo run --locked -p corescope-capture -- snapshot
cargo run --locked -p corescope-capture -- packets --max-pages 3
cargo run --locked -p corescope-capture -- run --loop 15
```

- `snapshot` captures the node inventory, observer inventory and global
  neighbor graph. It fetches per-node neighbors for known nodes within
  `--bbox <south,north,west,east>`. The default box is
  `57.5,71.5,4,31.5` in WGS84 degrees. Use a tighter box for a small
  exploratory snapshot. The per-node request count scales with the box.
- `packets` captures expanded receptions from all regions. The first scan
  starts at the latest packet page and walks backward through the available
  feed. Capped and interrupted scans resume from the last committed page.
- `run` captures packets and takes a snapshot. With `--loop <minutes>` it
  repeats both after waiting at least the given interval after each run. The
  minimum is five minutes. Ctrl+C ends the run after the current bounded
  request or SQLite operation and records its status.
- `--db <file>` overrides the default capture file. The default is
  `%LOCALAPPDATA%/RepeaterNet/corescope-capture.sqlite3` on Windows and
  `$XDG_DATA_HOME/RepeaterNet/corescope-capture.sqlite3` on Unix
  (`~/.local/share` if XDG is unset). The file must live outside the
  RepeaterNet checkout and Windows planner data directory.
- `--base-url <url>` overrides `https://corescope.eth0.no`.
- `--rate <requests-per-second>` defaults to one, including retries. The
  tool identifies itself as RepeaterNet in the User-Agent, retries HTTP 429
  and 5xx up to three times with exponential delays of one, two and four
  seconds, and waits longer when requested by `Retry-After`.
- `--max-pages <count>` defaults to 40 per paginated endpoint per run.
  Packets request 250 transmissions per page and nodes request 500.

## SQLite schema

The database uses `PRAGMA user_version = 1`.

| Table | Contents |
| --- | --- |
| `observations` | One row per CoreScope reception, keyed by endpoint, packet ID and observation ID. For a representative-only response, the observer identity is the fallback key. Expanded observations can include several real receptions of the same transmission at one observer. Path columns contain JSON arrays with null placeholders for unresolved positions. |
| `nodes_snapshot`, `observers_snapshot` | Timestamped JSON records keyed by capture run and public key or observer ID, with location columns for joins. |
| `neighbors_snapshot` | Timestamped JSON responses keyed by capture run and source public key. An empty source key denotes the global graph. |
| `capture_runs` | CoreScope endpoint and version, mode, start and end, status, request and row counts, page-cap flag and errors. |
| `packet_cursor` | Per-endpoint time window and last committed packet ID/offset. The cursor and observations advance together in a SQLite transaction. |

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
