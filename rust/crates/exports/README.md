# RepeaterNet exports

This crate renders saved coverage manifests and the active route wire DTO using Rust. Python is used only to regenerate the checked-in test oracles.

## Coverage downloads

- Build a `CoverageGrid` with the saved manifest and its projected-to-WGS84 and WGS84-to-projected transforms. Both callbacks take XY, so geographic coordinates are longitude first. Saved map bounds remain south, west, north, east.
- Open an ordered cell iterator and call `summarize`. Pass the counts and the authorized stale flag, target report and optional export timestamp to `metadata`. Open a fresh iterator over the same saved snapshot and pass it to `write_coverage`. Each item is `Result<serde_json::Value, ExportError>`. The grid supports up to 16,384 cells and reconstructs absent cells as outside-area or not-evaluated. Unknown terrain and unresolved profiles retain independent states.
- `CoverageAttachments` accepts a target report, a compatible saved height comparison and a router-failure scenario summary. JSON keeps all cell and source details and optional scenario data. GeoJSON includes grid footprints, network and area context, and target geometry and outcomes. HTML includes these context layers and summary tables and uses inline SVG without remote assets.
- Use `CoverageFormat::filename` and `media_type` for response headers. Render into `spool(|file| ...)` before sending headers. Successful files are rewound, and the temporary file is reclaimed on drop. A caller-supplied writer can receive a prefix on failure. `ExportError::LimitExceeded` maps to HTTP 413. Buffer file writes if appropriate and flush the buffer before returning from the spool callback.

## Route downloads

- `write_route_csv` streams the flat link DTO in active link order. `write_route_geojson` emits the active sites and links using stored WGS84 coordinates, then an optional inverse transform, then synthetic XY if the caller supplies no transform.
- `write_route_html` uses the selected alternative's primary path, followed by optional ordered `client_paths` and remaining links. Supply the original endpoint coordinates in latitude/longitude order, UTC timestamp, terrain resolution, surface flag and captured candidate settings in `RouteReportOptions`. Supply the effective receiver sensitivity as `effective_sensitivity_dbm` in the RF settings for LoRa-derived sensitivity. Include alternative diagnostics on the selected alternative if they are stored separately. Null budget or profile values render as unknown, and SVG profiles retain extrema and fit within 1,000 points per series. Leaflet, CSS, profiles, and the route itself are embedded. Basemap tiles need internet access.
- Build the optional route coverage section with `build_route_overlay` using the saved grid, counts-derived metadata, a fresh iterator, and its authorized target report. It requires a complete, current grid and embeds an indexed PNG of up to 384 pixels on its longest side. Use `CoverageOmission` when the server finds no eligible overlay or detects that the selected route changed. Coverage downloads and the route HTML writer enforce a 100 MiB cap.

## Integration boundary and verification

The server owns workspace and project access, manifest and report selection, snapshot consistency, stale/partial opt-ins, comparison compatibility, HTTP details and dashboard wiring. The renderer only sees the DTOs it is given. The route wire DTO exposes the primary path. Pass additional paths and alternative diagnostics separately when available. Scenario and target assessments are computed upstream. The optional failure summary extends the Python JSON export, which only takes a scenario comparison.

The integration tests use captured Python JSON, GeoJSON, CSV and route map data. They also exercise escaped HTML and script data, partial/empty grids, nonfinite numbers, embedded route coverage, large profile sampling and rejection at the 100 MiB export limit. To refresh fixtures from the repository root, run `python rust/crates/exports/tests/generate_oracles.py` in the development environment with `src` on `PYTHONPATH`. Run `cargo test -p repeaternet-exports` and `cargo clippy -p repeaternet-exports --all-targets -- -D warnings` once the crate is registered in the workspace. Before registration, the crate can be copied out of the workspace and tested using its own manifest.
