# Meshtastic capture and measured links viewer

Capture public RF evidence for RepeaterNet path-loss calibration. The viewer
reads the resulting SQLite files alongside CoreScope MeshCore captures.

From `rust/`, build the executable in a dedicated target directory.

```powershell
& "$env:USERPROFILE/.cargo/bin/cargo.exe" build --locked -p meshtastic-capture --target-dir target/meshtastic
.\target\meshtastic\debug\meshtastic-capture.exe run --duration 12h --no-dashboard
.\target\meshtastic\debug\meshtastic-capture.exe view
```

The viewer listens at [127.0.0.1:8795](http://127.0.0.1:8795/). Leave the
capture terminal open to see each host's request, measurement, located-link
and backoff totals. **Ctrl+C** stops new requests and lets a bounded in-flight
request or SQLite write finish. Without `--no-dashboard`, `run` also starts
the viewer. Use a separate terminal for `view` when capturing with
`--no-dashboard`.

## Sources and request ceilings

Each enabled host has one sequential HTTP client. All requests are GETs. The
client checks `robots.txt` once at startup, then checks each request URL
against the rules. A missing file (404 or 410) permits requests. Other
unsuccessful robots responses stop the host for that run. Redirects are
treated as unsuccessful responses. Requests advertise gzip and use ETag and
Last-Modified validators on stable snapshot URLs when supplied. The
User-Agent identifies RepeaterNet and the temporary RF calibration dataset.

The same two-second minimum applies on each host, including robots and
reception lookups. Request spacing and polling use ±10% jitter clamped to
their configured minimum. Failures back off from one minute up to an hour,
or longer when required by `Retry-After`. Three API responses in the
403/404/5xx group stop that host for the run. Each database counts requests
against its own rolling 24-hour budget, including across restarts.

| Source | Endpoints and cadence | Maximum requests in 12 h per host |
| --- | --- | ---: |
| `map.868.no` | Capabilities at startup. Nodes at startup and every six hours. Observations every 15 minutes with a rolling 45-minute window and up to three 2,000-row pages. If capped, resume the fixed window on the next poll. | 148 |
| `meshtastic.liamcottle.net`, `karta.meshat.se`, `meshtastic.valzzu.xyz`, `meshmap2.lucifernet.com`, `map.meshnet.si` | One node snapshot at startup and every six hours. The worldwide snapshot can be about 25 MB. Neighbour evidence comes from the snapshots. | 3 each |
| `meshview.format.ovh`, `meshview.lsinfra.de`, `meshview.bayme.sh`, `meshview.socalmesh.org`, `meshview.nyme.sh` | Nodes at startup and every six hours. Newest page of up to 120 packets per port (70 and 71) every 30 minutes with one hour of overlap. Up to five `packets_seen` lookups per port per cycle. | 291 each |
| `meshinfo.cvme.sh` | Nodes active in the last 30 days at startup and every six hours. Up to three cursor pages of 200 recent traceroutes every 30 minutes. | 75 |

The default-settings ceiling is **1,693 requests across all 12 hosts in
12 hours**, including robots checks. Real counts are usually lower.
Meshview's bounded feeds can omit records when traffic exceeds a page.
MeshInfo starts from the newest traceroutes each cycle and bounds the
backfill to three pages. A capped 868.no visit keeps an unfinished cursor
for up to six hours. Capture uses the public HTTP endpoints only.

Contracts are documented by [868.no](https://map.868.no/api-docs.html),
[Liam Cottle's map](https://github.com/liamcottle/meshtastic-map),
[Meshview](https://meshview.lsinfra.de/api.html) and
[MeshInfo](https://github.com/MeshAddicts/meshinfo/blob/main/API.md).

## Commands and flags

- `run --only HOST` enables the specified host. Repeat it to select a group.
  `--skip HOST` disables a host and can also be repeated.
- `run --once --no-dashboard --db-dir DIR` attempts one startup cycle per
  enabled host. A backoff ends that host's one-shot cycle and prints the
  reason. `--duration` accepts whole seconds, minutes, hours or days such as
  `45s`, `20m`, `12h` or `1d`.
- `run --daily-budget 300` sets the rolling request ceiling separately
  applied to each host. `--min-interval-ms` can increase the common minimum
  above 2,000 ms.
- `run --dashboard-port 8795` and `view --port 8795` set the loopback port.
  `--no-dashboard` disables the viewer in run mode.
- `view --db-dir DIR` reads `*capture*.sqlite3` in a different directory.
  Repeat `view --glob PATTERN` options to read from several directories.
  Quote the patterns in the shell. The default searches the capture
  directory. Files named with `backup` are skipped.

## SQLite and calibration

Each host writes `meshtastic-capture-<host>.sqlite3` under
`%LOCALAPPDATA%/RepeaterNet` on Windows, or `$XDG_DATA_HOME/RepeaterNet` on
Unix (default `~/.local/share/RepeaterNet`). `run --db-dir` overrides the
directory. Each file is tied to its host and has a companion OS lock file
so only one process can capture into it at a time. SQLite runs in WAL mode.
The viewer opens the database files read-only and refreshes once per
minute through the loopback server. Leaflet is embedded. Map tiles come
from OpenStreetMap.

| Table | Contents |
| --- | --- |
| `measurements` | Allowlisted evidence keyed by host and observation ID. The key includes the evidence kind and leg or neighbour when the upstream record contains multiple observations. UTC is stored as Unix milliseconds. Relayed gateway receptions without a confirmed transmitter have a null `tx`. |
| `positions` | Timestamped coordinates, altitude, precision bits or radius, and location source. If a source supplies no position timestamp, the tool uses the snapshot time. |
| `radios` | Timestamped region, modem preset, frequency or slot, SF, BW, CR, TX power, hardware, firmware and role where supplied. |
| `meshtastic_instance`, `requests`, `poll_state` | Host identity, rolling request counts and bounded polling checkpoints. |

The collector stores no message text, raw payloads, channel names, or
decrypted application content. It reads only the numeric RF fields from
Meshview's decoded traceroute and neighbour reports. The parser fixtures
retain only relevant fields from live samples.

The viewer groups located measurements by directed pair, evidence, band
and preset within the selected time window and shows mean SNR and RSSI.
Per-source totals cover all time. For MeshCore, the latest observer radio
configuration supplies the frequency band and SF/BW/CR when available.
For Meshtastic, a reported region or latest endpoint radio configuration
supplies the filter values. The US-based Meshview instances and Central
Valley MeshInfo use US as a regional hint where no region was reported.
Treat band and preset as context when they come from a snapshot. They can
differ from the configuration at the time of reception.

The map flags neighbour reports with zero SNR, reporting firmware older
than 2.3.2, or endpoints over 300 km apart. The viewer uses last known
coordinates for that distance test. Positions with an estimated radius
over 100 m are coarse. When neither the source nor the position report
provides precision, the viewer shows it as unknown. Both flags are exposed
in `measurement_flags` and its located subset `located_measurements`.
For external SQL clients, register `capture_over_300_km(lat1, lon1, lat2, lon2)`
as a great-circle distance test returning false for missing coordinates, or
read the underlying tables and join them with positions appropriate to the
measurement time.

868.no provides traceroute SNR in dB. Meshview and MeshInfo values are
converted from quarter-dB units. Unknown traceroute hops and the −128
sentinel do not create measured legs. A gateway's receiver-side SNR can
describe the last relay, so the viewer needs a confirmed transmitter
before drawing the link. Public map positions can be stale or purposely
imprecise. Firmware before 2.3.2 can report MQTT-only neighbours. Preset
and band affect sensitivity and propagation, and unknown TX power,
antenna gain, and cable loss limit absolute path-loss estimates.
