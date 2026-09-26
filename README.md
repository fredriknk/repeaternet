# RF Router Planner

## Self-hosted web app

The RepeaterNet browser interface runs the existing Python RF engine on your own
server. Start it with Docker:

```sh
docker compose up -d --build
```

Open **http://localhost:8000**. Terrain and the most recently submitted plan are
stored in the persistent `planner-data` volume, separately for each browser.
Results are held in memory; rerun the plan after restarting the server.

For a native installation:

```sh
python -m pip install -e ".[dev]"
python -m rf_router_planner.web
```

Use `--host 0.0.0.0 --port 8000` for LAN access. For Docker LAN access, set
`RF_PLANNER_BIND=0.0.0.0`. Set `RF_PLANNER_TOKEN` to require a shared access token;
use an HTTPS reverse proxy when exposing it beyond a trusted network. Run one
server process: optimization state is managed in that process. Set
`RF_PLANNER_DATA` to change the native data directory (default: `web-data`).

1. Upload DTM GeoTIFF tiles and optional DOM tiles, or estimate and prepare
   corridor terrain from the configured Kartverket WCS services.
2. Click A/B and then the map, drag markers, or enter latitude/longitude. The
   map outlines loaded terrain and highlights sampled nodata gaps.
3. Optionally load nearby CoreScope routers and mark each Optional, Required, or
   Excluded. Choose existing-only, proposed-only, or mixed infrastructure and a
   route objective, then select **Find repeater route**.
4. Select a hop for its terrain/Fresnel profile and RF budget. Export CSV or
   GeoJSON, or save a `.webplan.json` file to reopen settings and endpoints.

Ground and surface tiles can be cleared separately. Clearing tiles permanently
removes those terrain copies from that browser's server workspace. Prepared
Kartverket tiles are cached per workspace and only become available after the
full DTM/optional-DOM generation validates. Plan files do not embed terrain.
Kartverket preparation and basemap tiles need internet access; uploaded terrain
and RF calculations run locally. Browser assets, including Leaflet, are bundled.

The web workflow currently covers two-endpoint planning and existing CoreScope
router candidates. Desktop multi-client network editing, contour generation,
manual router editing, and desktop `.rfplan.json` projects remain available in
the desktop app:

```sh
python -m pip install -e ".[desktop]"
rf-router-planner
```

The two interfaces share the same RF and optimization engine. Qt and Matplotlib
are optional dependencies and are not installed in the web container.

RF Router Planner is a Python desktop engineering tool for designing terrain-aware RF meshes between two or more client sites. It produces the best feasible solution for each exact router count, can require planned or existing routers, and treats reliability as independent-path redundancy rather than router density. It combines terrain and surface elevation, bidirectional RF budgets, Fresnel clearance, Earth curvature, diffraction, antenna patterns, and graph optimization. The default preset targets EU868 LoRa / MeshCore, but all RF inputs are editable.

> RF predictions are planning estimates. They do not replace site surveys, spectrum coordination, antenna measurements, or field link tests.

## Capabilities

- Leaflet map embedded in QtWebEngine: switch between OpenStreetMap and Kartverket topo/hiking backgrounds, generate local DTM contour overlays, add any number of clients and manual routers, drag sites, inspect links, and optionally show candidate sites.
- Local tiled DTM and optional DOM GeoTIFF loading. Raster CRS, transform, resolution, nodata, and bounds are read from the files; samples are taken from raster windows rather than a nationwide in-memory mosaic.
- Configurable Kartverket WCS retrieval for EPSG:25832, 25833, and 25835 with corridor-aware tiling, automatic resolution, estimates, pixel budgets, and disk caching.
- DTM as ground/base elevation; DOM as the RF obstruction surface. Without DOM the UI explicitly reports `Surface obstruction data unavailable — terrain only`.
- FSPL, effective-Earth curvature, first Fresnel zone, dominant/multiple knife-edge diffraction, bidirectional budgets, and optional elevation-pattern antenna gains.
- RF propagation and strict LOS/Fresnel validation modes.
- Candidate generation from a spatially thinned terrain grid using elevation, local slope, and DOM−DTM obstruction quality. The sampling grid is no coarser than the configured local-refinement radius, so useful summits are not stranded outside the later search. Search normally stays inside a configurable A–B corridor.
- One selectable mesh alternative for every feasible exact router count: `1 router`, `2 routers`, `3 routers`, and so on up to the chosen limit.
- Resilient-mesh mode asks for node-independent paths between every client pair and selects the smallest topology that achieves that target. It does not reward routers merely for being close together.
- Every valid RF link between nodes in the selected topology is displayed, with thicker lines marking representative backbone paths.
- Process-based coarse RF evaluation uses all available CPU cores by default for raster-backed searches. The process count remains adjustable under Advanced settings, and optimization can be cancelled.
- Manual routers can be added before optimization and are required in every alternative. Known CoreScope repeaters are shown in gray and can be enabled individually.
- Predicted coverage is calculated in the background from every selected client/router to the terrain candidate grid. Each transmitter has a distinct color and multi-transmitter overlap samples are highlighted in magenta.
- Automatic terrain preparation reuses matching cached tiles without making the user locate cache files. Missing areas are tiled and downloaded after one clear confirmation.
- Terrain/Fresnel plot with hover readout and transparent calculation details.
- JSON project save/load and CSV/GeoJSON route export.

## Installation

Python 3.12 or newer is required. QtWebEngine is included with PySide6 but is a large dependency.

```powershell
py -3.12 -m venv .venv
.venv\Scripts\Activate.ps1
python -m pip install -e ".[dev]"
rf-router-planner
```

Or run `python -m rf_router_planner`. Use `--debug` for detailed logs and `--log-file planner.log` to retain them.

## Typical workflow

1. Click **+ Client** and place at least two client sites. Repeat for C3, C4, and further clients. A and B remain the first two clients for compatibility with older projects.
2. Optionally click **+ Router** to place planned infrastructure that every solution must use. Import CoreScope repeaters from **Tools**, then click a gray repeater and enable it if it should participate in the plan.
3. Pick an everyday goal: **Fewest routers**, **Balanced**, or **Resilient mesh**. Choose how many exact-count alternatives to produce and, for resilient mode, how many independent paths are required.
4. Click **Optimize**. With **Prepare terrain automatically** enabled, matching cached tiles are loaded immediately; otherwise the app proposes a tiled Kartverket download covering all planning sites.
5. Use the solution dropdown above the result table to compare exact router counts. All viable selected-node links are visible, while the stronger lines show representative client-to-client paths.
6. Turn on **View → Predicted coverage** to calculate colored client/router reach and highlight overlap candidates in magenta. Map layers and their colors are explained by the on-map legend.
7. For a large-area coarse search, choose **Terrain → Validate selected network in detail** to download a narrow high-resolution DTM/DOM strip and recalculate every selected link.
8. Select a link to inspect its terrain/Fresnel profile, then save the project or export CSV/GeoJSON.

Frequently used controls stay on the toolbar and the three-step planning panel. Radio, antenna, search, terrain-download, cache, and Earth-model settings live in the expandable Advanced section. The default cache is stored under the operating system's local application-data directory and is read-only in the normal workflow.

The map control, local Leaflet library, DTM contours, and planning overlays work offline. OpenStreetMap and Kartverket backgrounds are network tile layers, so an offline session displays the map canvas and local overlays without background tiles. All RF planning with local GeoTIFFs remains offline.

## Kartverket terrain

Kartverket uses these terms:

- **DTM (digital terrengmodell):** bare-earth ground elevation. It determines site ground elevation, terrain slope, candidate quality, and the normal antenna base.
- **DOM (digital overflatemodell):** surface elevation that can include trees and buildings. It is used as the obstruction profile.

When both exist, `obstruction_height = max(0, DOM - DTM)`. A normal ground-mounted antenna is `DTM + height AGL`; it is never silently placed on top of DOM. The site model also supports a `surface_dom` height reference for explicit rooftop installations.

Local tiles must share one CRS. The application samples across all supplied tiles on demand. GeoTIFF boundaries and nodata are respected.

Online service definitions are in [`config/kartverket_wcs.json`](config/kartverket_wcs.json) and in the installed package data. They were verified against the official GetCapabilities documents on 2026-08-29 and are deliberately not hardcoded in the provider. Kartverket currently publishes separate NHM DTM and DOM services for ETRS89 / UTM zones 32, 33, and 35. The provider uses WCS 1.0 GetCoverage because the current ArcGIS-backed capabilities advertise it consistently. Update the JSON if Kartverket changes endpoints or coverage identifiers. Official references: [Kartverket terrain data](https://kartverket.no/api-og-data/terrengdata), [Geonorge elevation catalog](https://kartkatalog.geonorge.no/metadata/?organizations=Kartverket&theme=H%C3%B8ydedata&type=service), and [data.norge.no service record](https://data.norge.no/en/datasets/8c62e33e-76ba-3c00-9db6-3a10e44135bc/hoydedata-laser).

Downloaded rasters use content-addressed filenames and are reused on an exact request cache hit. The square-kilometre setting is a per-request tile limit, not a total-area limit. Large corridors are split into aligned tiles and tiles outside the buffered A-B corridor are skipped. Automatic mode raises the requested resolution only when necessary to fit the total pixel budget; the confirmation dialog shows tile count, effective resolution, pixel count, raw-memory estimate, and cache hits before network work begins. A practical national-scale workflow is a 25-50 m broad search followed by a 1-10 m final-route strip.

## CoreScope integration

**Tools → Import known CoreScope routers** reads the public CoreScope node endpoint in a background worker, filters valid Scandinavian repeater coordinates near the current project, and draws them in gray. Import is opt-in. Clicking one and choosing **Enable/disable selected known router** adds or removes it from optimization without silently modifying the plan.

The integration uses CoreScope's public [`GET /api/nodes`](https://corescope.eth0.no/api/nodes) interface; its live [OpenAPI specification](https://corescope.eth0.no/api/spec) and [API documentation](https://corescope.eth0.no/api/docs) are the authoritative schema references. The CoreScope application source is GPL-3.0, but no separate dataset license was found in its published API material when this integration was written. Consequently, fetched catalog data remains session-local and only repeaters explicitly enabled by the user are stored in a project. Confirm data-use terms with the CoreScope operator before redistributing a bulk export.

## Coordinates

The map and exports use WGS84 latitude/longitude (EPSG:4326). Terrain calculations use the loaded raster CRS. For downloads, a suitable Norwegian ETRS89 UTM service is chosen from longitude: 25832, 25833, or 25835. PyProj always uses XY order explicitly.

## RF model

### Link budget

For distance in kilometres and frequency in MHz:

```text
FSPL = 32.44 + 20 log10(distance_km) + 20 log10(frequency_MHz)

received power = TX power + TX pattern gain - TX feed loss
                 - FSPL - diffraction - clutter
                 + RX pattern gain - RX feed loss - miscellaneous loss

raw margin    = received power - receiver sensitivity
usable margin = raw margin - required fade margin
```

Forward and reverse directions are evaluated independently. An undirected graph edge exists only when both directions pass; routing scores use the worse usable margin.

### Curvature and refraction

Terrain obstruction is raised by the effective-Earth bulge
`d1*d2/(2*k*R)`, using mean Earth radius `R = 6,371,000 m`. The default `k = 4/3`; 1.0 and custom positive values are accepted.

### Fresnel clearance

The first-zone radius is calculated at every profile point from wavelength and the distances to both antennas. The result records maximum radius, minimum absolute clearance, minimum clearance ratio, and its location. The default target is 60%.

### Diffraction

New plans use the Bullington terrain component from ITU-R P.526-16 §4.5.1,
with the existing effective-Earth curvature adjustment. This avoids treating
each sample on a smooth terrain shoulder as another diffracting obstacle.
It is not the complete delta-Bullington/spherical-Earth method. Legacy recursive
Deygout remains selectable in desktop propagation settings and through the
`diffraction_model` RF setting (`bullington` or `deygout`). Existing desktop
projects without this field load as `deygout` to preserve their model choice.

In **RF propagation** mode, calculated diffraction loss enters the budget.
In **strict LOS** mode, clear LOS, required Fresnel clearance, and budget margin
must all pass. Results remain planning estimates, not measured coverage.

DOM already changes diffraction geometry. A simple configurable DOM−DTM clutter hook exists but is disabled by default; no undocumented forest or urban attenuation constants are invented.

### Antennas and LoRa

Antennas use constant gain or an elevation pattern CSV containing `elevation_deg,gain_dbi`. Gains are interpolated at actual departure and arrival angles. The interface is intentionally ready for a future azimuth-dependent model.

Approximate LoRa sensitivity is thermal noise (`-174 dBm/Hz`), noise bandwidth, receiver noise figure, and an SF-dependent required SNR. The editable source table is documented in [`config/lora_snr_thresholds.json`](config/lora_snr_thresholds.json). Hardware datasheets should override these planning defaults. Manual receiver sensitivity always remains available.

## Optimizer

The core problem is a constrained mesh-topology search, not a highest-point search:

```text
multi-client area → candidate sites → cheap geographic/RF screening
→ parallel coarse link evaluation → final retry of coarse failures
→ exhaustive low-hop discovery for two-client minimum-router plans
→ best connected topology for each exact router count
→ node-independent client-path measurement
→ final certification of every selected topology edge until stable
```

Every client and every required manual router is mandatory. Disconnected candidate
islands are pruned. Small candidate pools are enumerated exactly; large pools use
a deterministic bounded beam search. Diagnostics distinguish these modes.
Each selected topology retains all induced valid RF links, not only a tree.
Final-invalid edges are removed and alternatives are solved again until all
selected edges are certified; there is no fixed-round escape returning coarse
edges. Cancellation is checked during enumeration, beam expansion and validation.

Minimum-router mode also searches all zero-, one- and two-repeater possibilities
within the generated candidate set, even when the sparse graph already connects.
Larger meshes remain limited by graph screening and heuristic subset search.
Exhaustive subset enumeration describes the current screened graph, not a proof
of global optimality on continuous terrain or an entirely final-evaluated graph.
Coarse/final disagreement can still affect ranking of unselected alternatives.

The default objective minimizes repeater count, then RF quality. Infrastructure
mode minimizes a proxy of 100 installation units plus mast metres per repeater;
it can prefer two short masts over one very tall mast. Reliability mode first
seeks the requested node-independent paths, then the smallest qualifying mesh.
Standalone route scoring resolves bottleneck margin and Fresnel clearance before
additive tie-breakers, with deterministic site-ID ties.

Implementation milestones, regression evidence and deferred work are tracked in
[the RF engine plan](docs/RF_ENGINE_PLAN.md).

Coarse terrain-profile evaluations are independent and therefore run in a `ProcessPoolExecutor` for local raster terrain. Each worker reopens the GeoTIFFs read-only, batches link jobs to reduce inter-process overhead, and returns results in deterministic input order. Small jobs and in-memory synthetic terrains stay single-process because process startup would cost more than it saves.

## Project and export formats

Projects are versioned UTF-8 JSON and include all clients, manual routers, enabled known routers, settings, terrain references, selected routers, locks, and exclusion-area geometry storage. Version-1 two-endpoint projects are migrated when loaded. GeoJSON contains Point features for all selected mesh nodes and LineString features for every viable selected-node RF link. CSV provides one row per selected link with forward, reverse, and worst-case results.

## Tests and examples

Run:

```powershell
pytest
ruff check src tests
mypy src/rf_router_planner --ignore-missing-imports
```

Tests cover FSPL, Fresnel radius, Earth bulge, link budget, elevation angle and gain interpolation, knife-edge diffraction, terrain interpolation/DOM use, graph and mesh objectives, project migration/round trips, process/sequential equivalence, CoreScope parsing/filtering, map setup, and synthetic DEM routing:

- flat clear terrain → zero routers;
- one blocking summit → one router;
- two separated ridges → two routers;
- LOS but insufficient Fresnel clearance;
- diffraction-valid propagation mode versus rejected strict mode;
- minimum router count over shorter geography;
- equal router count resolved by bottleneck margin.

Run `python examples/generate_synthetic_dem.py` to create a demonstration GeoTIFF. [`examples/elevation_pattern.csv`](examples/elevation_pattern.csv) is ready to load in the UI.

## Architecture

```text
models/         deterministic dataclasses and settings
terrain/        raster sources, sampling, Kartverket WCS, candidates
rf/             antennas, FSPL/budget, curvature, Fresnel, diffraction
optimization/   RF graph, objective strategies, progressive optimizer
export/         CSV and GeoJSON
gui/            Qt widgets, Leaflet bridge, workers, plots
```

The RF and optimizer packages have no Qt dependency. `ArrayTerrain` makes the complete calculation pipeline reproducible without external data.

## Known limitations and next improvements

- Neither the Bullington component nor legacy Deygout is a full ITU terrain/climate propagation suite. Troposcatter, ducting, rain, polarization mismatch, and statistically calibrated clutter are not modeled.
- Candidate generation uses terrain grid maxima/quality rather than road, ownership, power, protected-area, or access datasets. Exclusion geometry is represented in the project and honored by the generator API, but polygon drawing is not yet exposed in the first GUI.
- Local refinement is a deterministic grid search, not continuous optimization.
- The minimum-infrastructure objective uses a documented installation/mast proxy; real costs should be supplied by a future cost model.
- Coverage is sampled at the terrain candidate grid within the searched/downloaded region. It is an interactive RF planning overlay, not a continuous calibrated drive-test heatmap.
- Manual routers are required waypoints. Enabled CoreScope routers are currently treated as fixed planning waypoints as well; automatic optional selection from a large imported catalog is future work.
- Basemap tiles are not bundled. A future release could add MBTiles for a fully offline Norwegian background map.
- WCS interoperability varies by server version. The provider targets the currently advertised Kartverket ArcGIS WCS 1.0 interface and keeps all service metadata replaceable.

Logical next work is calibrated Longley–Rice/ITU models, azimuth/3-D antenna patterns, road/property/protection layers, locked-waypoint global re-routing, MBTiles, chunked parallel raster sampling, and field-measurement calibration.
