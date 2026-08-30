# RF Router Planner

RF Router Planner is a Python desktop engineering tool that searches for the smallest practical set of RF repeaters between two points. It combines terrain and surface elevation, bidirectional RF budgets, Fresnel clearance, Earth curvature, diffraction, antenna patterns, and graph optimization. The default preset targets EU868 LoRa / MeshCore, but all RF inputs are editable.

> RF predictions are planning estimates. They do not replace site surveys, spectrum coordination, antenna measurements, or field link tests.

## Capabilities

- Leaflet map embedded in QtWebEngine: click or type endpoints, drag sites, inspect links, and optionally show candidate sites.
- Local tiled DTM and optional DOM GeoTIFF loading. Raster CRS, transform, resolution, nodata, and bounds are read from the files; samples are taken from raster windows rather than a nationwide in-memory mosaic.
- Configurable Kartverket WCS retrieval for EPSG:25832, 25833, and 25835 with corridor-aware tiling, automatic resolution, estimates, pixel budgets, and disk caching.
- DTM as ground/base elevation; DOM as the RF obstruction surface. Without DOM the UI explicitly reports `Surface obstruction data unavailable — terrain only`.
- FSPL, effective-Earth curvature, first Fresnel zone, dominant/multiple knife-edge diffraction, bidirectional budgets, and optional elevation-pattern antenna gains.
- RF propagation and strict LOS/Fresnel validation modes.
- Candidate generation from a spatially thinned terrain grid using elevation, local slope, and DOM−DTM obstruction quality. The sampling grid is no coarser than the configured local-refinement radius, so useful summits are not stranded outside the later search. Search normally stays inside a configurable A–B corridor.
- Progressive coarse/medium/final validation, an exact endpoint-frontier search for zero-, one-, and two-repeater solutions, bounded nearest-neighbor fallback for longer routes, local coordinate refinement, and optional mast-height reduction.
- Lexicographic minimum-router routing by default. Equal-hop routes maximize bottleneck margin, then Fresnel clearance, lower mast requirement, site quality, and shorter RF distance. Alternative infrastructure and reliability priorities are isolated in `optimization/graph.py`.
- Background optimization and Kartverket download workers with progress reporting; optimization can be cancelled.
- Manual router add, drag, delete, lock, local re-optimization, and coordinate copying.
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

1. Start the application and load one or more DTM GeoTIFF tiles. Optionally select matching DOM tiles. Alternatively, set A and B and use **Download terrain**.
2. Click **Set A** and **Set B**, then click the map. Markers remain draggable. Coordinates can also be entered on the left.
3. Choose RF, antenna, propagation, and optimizer settings. Manual sensitivity is the default. Checking **Calculate LoRa sensitivity** uses bandwidth, spreading factor, noise figure, and the configurable threshold table.
4. Click **Optimize**. Candidate markers are hidden unless **Show candidate sites** is enabled.
5. For a large-area coarse search, click **Validate route detail** to download a narrow high-resolution DTM/DOM strip following the selected multi-hop route and recalculate every hop.
6. Select a link in the table or map to inspect terrain, curvature, Fresnel boundaries, obstruction, diffraction, and budget terms.
7. Adjust routers manually if useful, save the project, or export CSV/GeoJSON.

The map control and local Leaflet library work offline. The OpenStreetMap basemap is a network tile layer, so an offline session displays the map canvas and planning overlays without background tiles. All RF planning with local GeoTIFFs remains offline.

## Kartverket terrain

Kartverket uses these terms:

- **DTM (digital terrengmodell):** bare-earth ground elevation. It determines site ground elevation, terrain slope, candidate quality, and the normal antenna base.
- **DOM (digital overflatemodell):** surface elevation that can include trees and buildings. It is used as the obstruction profile.

When both exist, `obstruction_height = max(0, DOM - DTM)`. A normal ground-mounted antenna is `DTM + height AGL`; it is never silently placed on top of DOM. The site model also supports a `surface_dom` height reference for explicit rooftop installations.

Local tiles must share one CRS. The application samples across all supplied tiles on demand. GeoTIFF boundaries and nodata are respected.

Online service definitions are in [`config/kartverket_wcs.json`](config/kartverket_wcs.json) and in the installed package data. They were verified against the official GetCapabilities documents on 2026-08-29 and are deliberately not hardcoded in the provider. Kartverket currently publishes separate NHM DTM and DOM services for ETRS89 / UTM zones 32, 33, and 35. The provider uses WCS 1.0 GetCoverage because the current ArcGIS-backed capabilities advertise it consistently. Update the JSON if Kartverket changes endpoints or coverage identifiers. Official references: [Kartverket terrain data](https://kartverket.no/api-og-data/terrengdata), [Geonorge elevation catalog](https://kartkatalog.geonorge.no/metadata/?organizations=Kartverket&theme=H%C3%B8ydedata&type=service), and [data.norge.no service record](https://data.norge.no/en/datasets/8c62e33e-76ba-3c00-9db6-3a10e44135bc/hoydedata-laser).

Downloaded rasters use content-addressed filenames and are reused on an exact request cache hit. The square-kilometre setting is a per-request tile limit, not a total-area limit. Large corridors are split into aligned tiles and tiles outside the buffered A-B corridor are skipped. Automatic mode raises the requested resolution only when necessary to fit the total pixel budget; the confirmation dialog shows tile count, effective resolution, pixel count, raw-memory estimate, and cache hits before network work begins. A practical national-scale workflow is a 25-50 m broad search followed by a 1-10 m final-route strip.

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

The implementation uses the ITU-R P.526 single knife-edge approximation and a recursive Deygout multiple-edge method. In **RF propagation** mode, obstruction is not automatically fatal: calculated diffraction loss enters the budget. In **strict LOS** mode, clear LOS, required Fresnel clearance, and budget margin must all pass.

DOM already changes diffraction geometry. A simple configurable DOM−DTM clutter hook exists but is disabled by default; no undocumented forest or urban attenuation constants are invented.

### Antennas and LoRa

Antennas use constant gain or an elevation pattern CSV containing `elevation_deg,gain_dbi`. Gains are interpolated at actual departure and arrival angles. The interface is intentionally ready for a future azimuth-dependent model.

Approximate LoRa sensitivity is thermal noise (`-174 dBm/Hz`), noise bandwidth, receiver noise figure, and an SF-dependent required SNR. The editable source table is documented in [`config/lora_snr_thresholds.json`](config/lora_snr_thresholds.json). Hardware datasheets should override these planning defaults. Manual receiver sensitivity always remains available.

## Optimizer

The core problem is a graph search, not a highest-point search:

```text
terrain corridor → candidate sites → cheap geographic/RF screening
→ endpoint-visible frontiers → exact 0/1/2-repeater search
→ bounded candidate graph fallback for longer routes
→ medium/final edge validation with alternate-path retry
→ local site refinement/mast reduction
```

Candidate cells retain only their best few sites, preventing one summit from dominating the graph. Endpoint pairs and endpoint-to-candidate links are always considered. Candidates visible from opposite endpoints are cross-checked explicitly, so a long summit-to-summit hop cannot be lost to the configurable nearest-neighbor cap used for longer-route fallback. Failed medium- or final-resolution edges are removed and the graph is searched again. The default objective first minimizes graph edges and therefore intermediate routers. It then compares the minimum link margin, Fresnel quality, mast metres, site quality, and total RF distance as separate tuple fields rather than collapsing them into an opaque weighted score.

## Project and export formats

Projects are versioned UTF-8 JSON and include endpoints, settings, terrain references, selected routers, locks, and exclusion-area geometry storage. GeoJSON contains Point features for endpoints/routers and LineString features with RF properties. CSV provides one row per hop with forward, reverse, and worst-case results.

## Tests and examples

Run:

```powershell
pytest
ruff check src tests
mypy src/rf_router_planner --ignore-missing-imports
```

Tests cover FSPL, Fresnel radius, Earth bulge, link budget, elevation angle and gain interpolation, knife-edge diffraction, terrain interpolation/DOM use, graph objectives, project round trips, and synthetic DEM routing:

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

- Deygout is an engineering approximation, not a full ITU terrain/climate propagation suite. Troposcatter, ducting, rain, polarization mismatch, and statistically calibrated clutter are not modeled.
- Candidate generation uses terrain grid maxima/quality rather than road, ownership, power, protected-area, or access datasets. Exclusion geometry is represented in the project and honored by the generator API, but polygon drawing is not yet exposed in the first GUI.
- Local refinement is a deterministic grid search, not continuous optimization.
- The minimum-infrastructure objective uses a documented installation/mast proxy; real costs should be supplied by a future cost model.
- Router edits recalculate adjacent links immediately. “Re-optimize unlocked” locally refines existing unlocked routers; it does not change their count around locked waypoints.
- Basemap tiles are not bundled. A future release could add MBTiles for a fully offline Norwegian background map.
- WCS interoperability varies by server version. The provider targets the currently advertised Kartverket ArcGIS WCS 1.0 interface and keeps all service metadata replaceable.

Logical next work is calibrated Longley–Rice/ITU models, azimuth/3-D antenna patterns, road/property/protection layers, locked-waypoint global re-routing, MBTiles, chunked parallel raster sampling, and field-measurement calibration.
