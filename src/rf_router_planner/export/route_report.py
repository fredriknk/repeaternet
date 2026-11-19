"""Self-contained, printable reports for the selected RF route."""

from __future__ import annotations

import html
import json
import math
import re
from collections.abc import Iterable
from dataclasses import replace
from datetime import UTC, datetime
from functools import lru_cache
from pathlib import Path

import numpy as np

from rf_router_planner.models.link import LinkResult
from rf_router_planner.models.network import NetworkSolution
from rf_router_planner.models.settings import CandidateSettings, RFSettings
from rf_router_planner.models.site import HeightReference, Site, SiteKind, SiteOrigin
from rf_router_planner.optimization.optimizer import OptimizationResult

from .coverage_report import coverage_legend
from .route_report_coverage import ReportCoverage, coverage_section

MAX_PROFILE_POINTS = 1_000
ROLE_LABELS = {
    SiteKind.ENDPOINT_A: "Endpoint A",
    SiteKind.ENDPOINT_B: "Endpoint B",
    SiteKind.CLIENT: "Client",
}
LINK_COLORS = ("#ae4037", "#aa680e", "#217250")


def _escape(value: object) -> str:
    return html.escape(str(value), quote=True)


def _number(value: float | None, digits: int = 1, unit: str = "") -> str:
    return f"{value:.{digits}f}{unit}" if value is not None and math.isfinite(value) else "—"


def _km(value: float) -> str:
    return _number(value / 1_000, 3)


def _table(
    columns: Iterable[str],
    rows: Iterable[Iterable[object]],
    *,
    label: str = "",
) -> str:
    caption = f"<caption>{_escape(label)}</caption>" if label else ""
    headings = "".join(f'<th scope="col">{_escape(column)}</th>' for column in columns)
    body = "".join(
        "<tr>" + "".join(f"<td>{_escape(value)}</td>" for value in row) + "</tr>" for row in rows
    )
    return f"<table>{caption}<thead><tr>{headings}</tr></thead><tbody>{body}</tbody></table>"


def _pairs(pairs: Iterable[tuple[str, object]]) -> str:
    return (
        "<dl>"
        + "".join(
            f"<div><dt>{_escape(label)}</dt><dd>{_escape(value)}</dd></div>"
            for label, value in pairs
        )
        + "</dl>"
    )


def _distance(links: Iterable[LinkResult]) -> float:
    return sum(link.distance_m for link in links)


def _weakest(links: Iterable[LinkResult]) -> float:
    return min((link.worst_margin_db for link in links), default=math.nan)


def _route_order(
    links: list[LinkResult], route_ids: list[str], solution: NetworkSolution | None
) -> tuple[list[LinkResult], int]:
    remaining = {frozenset((link.source_id, link.target_id)): link for link in links}
    ordered = []
    paths = [route_ids]
    if solution:
        paths.extend(
            path for alternatives in solution.client_paths.values() for path in alternatives
        )
    primary_hops = 0
    for index, path in enumerate(paths):
        for source, target in zip(path, path[1:], strict=False):
            link = remaining.pop(frozenset((source, target)), None)
            if link is not None:
                ordered.append(_orient(link, source))
        if index == 0:
            primary_hops = len(ordered)
    ordered.extend(remaining.values())
    return ordered, primary_hops


def _orient(link: LinkResult, source: str) -> LinkResult:
    if source == link.source_id:
        return link
    profile = link.profile
    if profile is not None:
        profile = replace(
            profile,
            distances_m=link.distance_m - profile.distances_m[::-1],
            x=profile.x[::-1],
            y=profile.y[::-1],
            dtm_elevation_m=profile.dtm_elevation_m[::-1],
            surface_elevation_m=profile.surface_elevation_m[::-1],
            earth_bulge_m=profile.earth_bulge_m[::-1],
            effective_obstruction_m=profile.effective_obstruction_m[::-1],
            los_elevation_m=profile.los_elevation_m[::-1],
            fresnel_radius_m=profile.fresnel_radius_m[::-1],
            clearance_m=profile.clearance_m[::-1],
            fresnel_clearance_ratio=profile.fresnel_clearance_ratio[::-1],
        )
    return replace(
        link,
        source_id=link.target_id,
        target_id=link.source_id,
        forward=link.reverse,
        reverse=link.forward,
        minimum_clearance_distance_m=link.distance_m - link.minimum_clearance_distance_m,
        dominant_obstacles=[
            {**point, "distance_m": link.distance_m - point["distance_m"]}
            for point in link.dominant_obstacles
        ],
        profile=profile,
    )


def _coordinates(latitude: float | None, longitude: float | None) -> str:
    return f"{_number(latitude, 5)}, {_number(longitude, 5)}"


def _site_fields(site: Site, nearest: dict[str, float | None]) -> tuple[str, str, str, str, str]:
    role = ROLE_LABELS.get(site.kind, "Repeater")
    mast = _number(site.antenna_height_m, unit=" m")
    if site.height_reference == HeightReference.SURFACE_DOM:
        mast += " above surface"
    road = (
        "Beyond mapped road radius"
        if site.id in nearest and nearest[site.id] is None
        else (_number(nearest.get(site.id), 0, " m"))
    )
    installation = (
        ("Existing" if site.origin == SiteOrigin.KNOWN else "Proposed")
        if role == "Repeater"
        else "—"
    )
    return role, _number(site.ground_elevation_m, unit=" m"), mast, installation, road


def _assumptions(
    rf: RFSettings, candidates: CandidateSettings, resolution: float | None, surface: bool | None
) -> str:
    maximum = candidates.maximum_link_distance_m
    values = _pairs(
        (
            ("Frequency", _number(rf.frequency_mhz, 3, " MHz")),
            ("TX power", _number(rf.tx_power_dbm, unit=" dBm")),
            ("Receiver sensitivity", _number(rf.effective_sensitivity_dbm, unit=" dBm")),
            ("Fade margin", _number(rf.fade_margin_db, unit=" dB")),
            ("Earth k-factor", _number(rf.k_factor, 3)),
            ("Fresnel clearance target", _number(100 * rf.required_fresnel_clearance, unit="%")),
            (
                "Validation mode",
                "Strict LOS" if rf.validation_mode == "strict_los" else "RF propagation",
            ),
            ("Maximum hop length", "Unlimited" if maximum is None else f"{_km(maximum)} km"),
            (
                "Surface data (DOM)",
                "Unknown" if surface is None else "Used" if surface else "Unavailable",
            ),
            ("Terrain resolution", _number(resolution, unit=" m")),
        )
    )
    antennas = _table(
        ("Antenna role", "Gain (dBi)", "Feed loss (dB)", "Default height (m AGL)"),
        (
            (
                role,
                _number(antenna.gain_dbi, 2),
                _number(antenna.feed_loss_db, 2),
                _number(antenna.height_agl_m),
            )
            for role, antenna in (
                ("Endpoint A", rf.endpoint_a),
                ("Endpoint B", rf.endpoint_b),
                ("Repeater", rf.router),
            )
        ),
    )
    return f"<section><h2>Planning assumptions</h2>{values}{antennas}</section>"


def _link_color(link: LinkResult) -> str:
    margin = link.worst_margin_db
    return LINK_COLORS[0] if margin < 3 else LINK_COLORS[1] if margin <= 10 else LINK_COLORS[2]


def _hop_name(link: LinkResult) -> str:
    return f"{link.source_id} → {link.target_id}"


def _link_table(links: list[LinkResult], primary_hops: int) -> str:
    columns = (
        "Hop",
        "km",
        "Usable margin (dB)",
        "Raw margin (dB)",
        "RX power (dBm)",
        "FSPL (dB)",
        "Diffraction (dB)",
        "Clutter (dB)",
        "LOS",
        "Minimum Fresnel clearance",
        "Minimum clearance (m)",
    )
    headings = "".join(f'<th scope="col">{_escape(column)}</th>' for column in columns)
    rows = []
    for number, link in enumerate(links, 1):
        bidirectional = (
            " / ".join(
                _number(getattr(direction, field), 2) for direction in (link.forward, link.reverse)
            )
            for field in (
                "usable_margin_db",
                "raw_margin_db",
                "received_power_dbm",
                "fspl_db",
                "diffraction_loss_db",
                "clutter_loss_db",
            )
        )
        ratio = _number(100 * link.minimum_fresnel_clearance_ratio, unit="%")
        if math.isfinite(link.minimum_fresnel_clearance_ratio):
            ratio += f" at {_km(link.minimum_clearance_distance_m)} km"
        cells = (
            _km(link.distance_m),
            *bidirectional,
            "Clear" if link.los_clear else "Blocked",
            ratio,
            _number(link.minimum_clearance_m, 2),
        )
        attributes = ' class="weak-hop"' if link.worst_margin_db < 3 else ""
        rows.append(
            f'<tr{attributes}><th scope="row"><a href="#hop-{number}">{number}. '
            f"{_escape(_hop_name(link))}</a></th>"
            + "".join(f"<td>{_escape(cell)}</td>" for cell in cells)
            + "</tr>"
        )
    order = f"Primary route first ({primary_hops} hops). " if primary_hops else ""
    return (
        "<section><h2>Links</h2>"
        f'<p class="muted">{order}Paired values show forward / reverse.</p>'
        f'<div class="table-scroll"><table><thead><tr>{headings}</tr></thead>'
        f"<tbody>{''.join(rows)}</tbody></table></div></section>"
    )


def _profile_chart(link: LinkResult, target: float) -> str:
    profile = link.profile
    if profile is None or len(profile.distances_m) < 2:
        return '<p class="muted">Terrain profile unavailable for this hop.</p>'
    distances = profile.distances_m
    lines = [
        ("Ground + earth bulge", "#865631", profile.dtm_elevation_m + profile.earth_bulge_m),
        ("Line of sight", "#24639a", profile.los_elevation_m),
        (
            f"{target:.0%} Fresnel boundary",
            "#a5660e",
            profile.los_elevation_m - target * profile.fresnel_radius_m,
        ),
    ]
    if profile.surface_available:
        lines.insert(1, ("Surface + earth bulge", "#327845", profile.effective_obstruction_m))
    interior = slice(1, -1) if len(distances) > 2 else slice(None)
    minimum_index = int(np.argmin(profile.clearance_m[interior])) + (interior.start or 0)
    fresnel_index = int(np.argmin(abs(distances - link.minimum_clearance_distance_m)))
    annotations = [
        (
            "Minimum clearance",
            "#b44032",
            distances[minimum_index],
            profile.effective_obstruction_m[minimum_index],
        ),
        (
            "Minimum Fresnel clearance",
            "#6437a2",
            distances[fresnel_index],
            profile.effective_obstruction_m[fresnel_index],
        ),
    ]
    if minimum_index == fresnel_index:
        label, color, distance, elevation = annotations[0]
        annotations = [(f"{label} and minimum Fresnel clearance", color, distance, elevation)]
    annotations.extend(
        ("Diffraction obstacle", "#293843", point["distance_m"], point["elevation_m"])
        for point in link.dominant_obstacles
    )
    important = {0, len(distances) - 1, minimum_index, fresnel_index}
    for _, _, values in lines:
        important.update((int(np.argmin(values)), int(np.argmax(values))))
    remaining = MAX_PROFILE_POINTS - len(important)
    regular = np.linspace(0, len(distances) - 1, remaining, dtype=int)
    indices = (
        np.arange(len(distances))
        if len(distances) <= MAX_PROFILE_POINTS
        else np.union1d(regular, list(important)).astype(int)
    )
    low = min(*(float(np.min(values)) for _, _, values in lines), *(a[3] for a in annotations))
    high = max(*(float(np.max(values)) for _, _, values in lines), *(a[3] for a in annotations))
    pad = max(1.0, (high - low) * 0.06)
    low, high = low - pad, high + pad

    def x(distance: float) -> float:
        return 66 + 663 * distance / link.distance_m

    def y(elevation: float) -> float:
        return 230 - 210 * (elevation - low) / (high - low)

    svg = [
        f'<svg class="profile-chart" viewBox="0 0 760 264" role="img" '
        f'aria-label="Elevation profile for {_escape(_hop_name(link))}" '
        f'data-profile-points="{len(indices)}">'
    ]
    for fraction in np.linspace(0, 1, 5):
        elevation = low + fraction * (high - low)
        position = y(elevation)
        svg.append(
            f'<path d="M66 {position:.1f}H729" stroke="#d5dfda"/>'
            f'<text x="59" y="{position + 4:.1f}" text-anchor="end">{elevation:.1f} m</text>'
        )
        distance = fraction * link.distance_m
        svg.append(
            f'<text x="{x(distance):.1f}" y="253" text-anchor="middle">{distance / 1000:.2f} km</text>'
        )
    for label, color, values in lines:
        points = " ".join(f"{x(distances[i]):.1f},{y(values[i]):.1f}" for i in indices)
        svg.append(
            f'<polyline points="{points}" fill="none" stroke="{color}" stroke-width="2">'
            f"<title>{_escape(label)}</title></polyline>"
        )
    for label, color, distance, elevation in annotations:
        px, py = x(distance), y(elevation)
        if label == "Diffraction obstacle":
            marker = f'<path d="M{px:.1f} {py - 6:.1f}l6 6 -6 6 -6 -6Z"'
        else:
            marker = f'<circle cx="{px:.1f}" cy="{py:.1f}" r="4.5"'
        svg.append(
            f'{marker} fill="{color}" stroke="white"><title>{_escape(label)} at '
            f"{_km(distance)} km</title>"
            + ("</path>" if marker.startswith("<path") else "</circle>")
        )
    legend = [(label, color, "line") for label, color, _ in lines]
    legend.extend(
        dict.fromkeys(
            (label, color, "diamond" if label == "Diffraction obstacle" else "dot")
            for label, color, _, _ in annotations
        )
    )
    return f"<figure>{''.join(svg)}</svg>{_legend(legend, 'figcaption')}</figure>"


def _budget(link: LinkResult) -> str:
    fields = (
        ("TX power (dBm)", "tx_power_dbm"),
        ("TX gain (dBi)", "tx_gain_dbi"),
        ("TX feed loss (dB)", "tx_feed_loss_db"),
        ("FSPL (dB)", "fspl_db"),
        ("Diffraction loss (dB)", "diffraction_loss_db"),
        ("Clutter loss (dB)", "clutter_loss_db"),
        ("Total path loss (dB)", "total_path_loss_db"),
        ("RX gain (dBi)", "rx_gain_dbi"),
        ("RX feed loss (dB)", "rx_feed_loss_db"),
        ("Miscellaneous loss (dB)", "miscellaneous_loss_db"),
        ("Received power (dBm)", "received_power_dbm"),
        ("Sensitivity (dBm)", "sensitivity_dbm"),
        ("Raw margin (dB)", "raw_margin_db"),
        ("Usable margin (dB)", "usable_margin_db"),
        ("Departure angle (°)", "departure_angle_deg"),
        ("Arrival angle (°)", "arrival_angle_deg"),
    )
    return _table(
        (
            "Budget item",
            f"Forward · {_hop_name(link)}",
            f"Reverse · {link.target_id} → {link.source_id}",
        ),
        [
            (
                label,
                _number(getattr(link.forward, field), 2),
                _number(getattr(link.reverse, field), 2),
            )
            for label, field in fields
        ]
        + [
            (
                "Fade margin (dB)",
                *(
                    _number(direction.raw_margin_db - direction.usable_margin_db, 2)
                    for direction in (link.forward, link.reverse)
                ),
            ),
            (
                "Usable",
                "Yes" if link.forward.valid else "No",
                "Yes" if link.reverse.valid else "No",
            ),
        ],
        label="Directional link budget",
    )


def _legend(items: Iterable[tuple[str, str, str]], tag: str = "div") -> str:
    return (
        f'<{tag} class="legend">'
        + "".join(
            f'<span><i class="swatch {shape}" style="--swatch:{color}"></i>{_escape(label)}</span>'
            for label, color, shape in items
        )
        + f"</{tag}>"
    )


@lru_cache(maxsize=1)
def _leaflet_assets() -> tuple[str, str]:
    directory = Path(__file__).resolve().parents[1] / "data" / "leaflet"
    css = (directory / "leaflet.css").read_text(encoding="utf-8")
    # Circle markers and a checkbox use none of Leaflet's default image assets.
    css = re.sub(r"^\s*background-image: url\(images/[^)]+\);", "", css, flags=re.MULTILINE)
    js = (directory / "leaflet.js").read_text(encoding="utf-8").split("//# sourceMappingURL=")[0]
    return css, js


REPORT_CSS = """
:root{color-scheme:light;--ink:#19372d;--muted:#566b61;--line:#d5dfda;--paper:#fff;--bg:#eef2ef}
*{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--ink);font:15px/1.45 "Segoe UI",Arial,sans-serif}
main{max-width:1160px;margin:24px auto;padding:32px 40px;background:var(--paper);box-shadow:0 8px 32px #142d2018}
header{display:flex;align-items:flex-start;justify-content:space-between;gap:12px;border-bottom:2px solid var(--ink);padding-bottom:16px}
h1{font-size:30px;margin:0}h2{font-size:21px;margin:24px 0 10px}p{margin:8px 0}.muted,dt{color:var(--muted)}
dl{display:grid;grid-template-columns:repeat(auto-fit,minmax(150px,1fr));gap:8px;margin:14px 0}
dl div{padding:7px 10px;border:1px solid var(--line);border-radius:6px}dt{font-size:12px}dd{margin:3px 0 0;font-weight:600}
ul{padding-left:1.5em}a{color:#215d81;overflow-wrap:anywhere}button{font:inherit;cursor:pointer;border:1px solid #52786a;background:#19372d;color:white;border-radius:6px;padding:7px 11px}
#route-map{height:390px;background:#e9efed;border:1px solid var(--line);z-index:0}
#route-map .leaflet-image-layer{image-rendering:pixelated}
.legend{display:flex;flex-wrap:wrap;gap:6px 14px;margin:8px 0;font-size:12px}
.swatch{display:inline-block;width:14px;height:11px;margin-right:5px;background:var(--swatch);vertical-align:-1px}
.swatch.line,.swatch.dash{width:16px;height:0;background:none;border-top:3px solid var(--swatch);vertical-align:3px}
.swatch.dash{border-top-style:dashed}.swatch.dot{width:10px;height:10px;border-radius:50%}
.swatch.diamond{width:8px;height:8px;transform:rotate(45deg);margin-right:7px}
.table-scroll{overflow-x:auto}table{width:100%;border-collapse:collapse;font-size:13px}
th,td{text-align:left;padding:6px 8px;vertical-align:top;border-bottom:1px solid var(--line);overflow-wrap:anywhere}
thead th{background:#f1f5f2}tbody th{font-weight:600}caption{text-align:left;font-weight:700;margin:6px 0}
.weak-hop th,.weak-hop td{background:#fff0ea}.weak-hop th{border-left:3px solid #ae4037}
figure{margin:0}.profile-chart{display:block;width:100%;height:auto;border:1px solid var(--line);background:#f9fbfa}
.profile-chart text{font:12px "Segoe UI",Arial,sans-serif;fill:#43574d}
.hop:target h2{color:#215d81}.hop table{max-width:760px}.hop td,.hop th{padding:3px 8px}
@media(max-width:640px){main{padding:16px;margin:0}h1{font-size:24px}#route-map{height:300px}}
@media print{
  @page{margin:11mm}body{background:white}main{padding:0;margin:0;max-width:none;box-shadow:none}
  #route-map{height:74mm;print-color-adjust:exact}.leaflet-control-zoom,.leaflet-control-layers,.print{display:none}
  .map-section,figure,tr{break-inside:avoid}h2,caption{break-after:avoid}.hop{break-before:page}
  .table-scroll{overflow:visible}th,td{padding:3px 4px;font-size:9pt}.swatch,.weak-hop{print-color-adjust:exact}
}
"""

MAP_SCRIPT = """
const data = JSON.parse(document.getElementById('route-map-data').textContent);
const map = L.map('route-map', {attributionControl: false, maxZoom: 19});
L.control.attribution({prefix: false}).addTo(map);
L.tileLayer('https://tile.openstreetmap.org/{z}/{x}/{y}.png', {
  attribution: '© <a href="https://www.openstreetmap.org/copyright">OpenStreetMap contributors</a>',
  maxZoom: 19
}).addTo(map);

if (data.coverage) {
  map.createPane('coverage').style.zIndex = 250;
  const overlay = L.imageOverlay(data.coverage.image, data.coverage.bounds, {
    pane: 'coverage', opacity: 0.54, alt: 'Predicted coverage grid'
  }).addTo(map);
  L.control.layers({}, {'Coverage grid': overlay}, {collapsed: false}).addTo(map);
}

L.polyline(data.roads, {color: '#525c62', weight: 2, opacity: 0.85}).addTo(map);
for (const link of data.links) {
  L.polyline(link.coordinates, {color: link.color, weight: 4, dashArray: link.dashed ? '7 6' : null})
    .addTo(map);
}
for (const site of data.sites) {
  const popup = document.createElement('div');
  const title = document.createElement('strong');
  title.textContent = site.id;
  popup.append(title);
  for (const detail of site.details) {
    const line = document.createElement('div');
    line.textContent = detail;
    popup.append(line);
  }
  L.circleMarker(site.coordinates, {
    radius: site.endpoint ? 8 : 6, fillColor: site.endpoint ? '#12395c' : '#6737a2',
    fillOpacity: 1, color: '#fff', weight: 2
  }).addTo(map).bindPopup(popup);
}
const bounds = L.latLngBounds(data.sites.map(site => site.coordinates));
const fitRoute = () => {
  map.invalidateSize({pan: false});
  map.fitBounds(bounds, {padding: [28, 28], maxZoom: 16, animate: false});
};
fitRoute();
window.addEventListener('beforeprint', fitRoute);
window.addEventListener('afterprint', fitRoute);
"""


def _map_parts(
    sites: list[Site],
    links: list[LinkResult],
    nearest: dict[str, float | None],
    roads: list[tuple[tuple[float, float], ...]],
    coverage: ReportCoverage | None,
) -> tuple[str, str]:
    positions = {
        site.id: (site.latitude, site.longitude)
        for site in sites
        if site.latitude is not None
        and site.longitude is not None
        and math.isfinite(site.latitude)
        and math.isfinite(site.longitude)
    }
    if not positions:
        return "<section><h2>Route map</h2><p>No site coordinates available.</p></section>", ""
    markers = []
    for site in sites:
        if site.id not in positions:
            continue
        role, ground, mast, _, road = _site_fields(site, nearest)
        markers.append(
            {
                "id": site.id,
                "coordinates": positions[site.id],
                "endpoint": role != "Repeater",
                "details": [
                    role,
                    _coordinates(*positions[site.id]),
                    f"Ground elevation · {ground}",
                    f"Mast height · {mast}",
                    f"Nearest road · {road}",
                ],
            }
        )
    data = {
        "sites": markers,
        "roads": roads,
        "links": [
            {
                "coordinates": [positions[link.source_id], positions[link.target_id]],
                "color": _link_color(link),
                "dashed": not link.los_clear,
            }
            for link in links
            if link.source_id in positions and link.target_id in positions
        ],
        "coverage": {"image": coverage.image, "bounds": coverage.bounds} if coverage else None,
    }
    # An escaped less-than sign prevents embedded strings from closing the script element.
    data_json = json.dumps(data, separators=(",", ":"), allow_nan=False).replace("<", r"\u003c")
    labels = [
        ("Endpoints", "#12395c", "dot"),
        ("Repeaters", "#6737a2", "dot"),
        ("Under 3 dB", LINK_COLORS[0], "line"),
        ("3–10 dB", LINK_COLORS[1], "line"),
        ("Over 10 dB", LINK_COLORS[2], "line"),
        ("Blocked LOS", "#52675d", "dash"),
    ]
    if roads:
        labels.append(("Nearby roads", "#525c62", "line"))
    legend = _legend(labels)
    if coverage:
        legend += _legend(
            (label, color, "square") for label, color in coverage_legend(coverage.mode)
        )
    map_markup = (
        '<section class="map-section"><h2>Route map</h2>'
        '<div id="route-map" role="region" aria-label="Interactive route map"></div>'
        f'{legend}<p class="muted">Basemap tiles require internet access.</p></section>'
    )
    scripts = (
        f'<script id="route-map-data" type="application/json">{data_json}</script>'
        f"<script>{_leaflet_assets()[1]}</script><script>{MAP_SCRIPT}</script>"
    )
    return map_markup, scripts


def export_route_html(
    result: OptimizationResult,
    rf: RFSettings,
    candidates: CandidateSettings,
    *,
    generated_at: datetime,
    endpoint_coordinates: tuple[tuple[float, float] | None, tuple[float, float] | None] = (
        None,
        None,
    ),
    terrain_resolution_m: float | None = None,
    surface_available: bool | None = None,
    coverage: ReportCoverage | None = None,
    coverage_note: str = "No completed coverage run is available.",
) -> str:
    """Render the selected solution and supplied run assumptions without I/O to the workspace."""
    solution = result.active_solution
    route = solution.primary_route if solution else result.route
    sites = solution.sites if solution else route
    route_ids = [site.id for site in route]
    links, primary_hops = _route_order(
        solution.links if solution else result.links, route_ids, solution
    )
    existing = (
        solution.existing_router_count
        if solution
        else sum(site.origin == SiteOrigin.KNOWN for site in route[1:-1])
    )
    if surface_available is None:
        surfaces = [link.profile.surface_available for link in links if link.profile is not None]
        surface_available = any(surfaces) if surfaces else None
    endpoint_labels = []
    for kind, position, fallback in zip(
        (SiteKind.ENDPOINT_A, SiteKind.ENDPOINT_B), endpoint_coordinates, (0, -1), strict=True
    ):
        if position is None:
            site = next(
                (site for site in sites if site.kind == kind), route[fallback] if route else None
            )
            position = (site.latitude, site.longitude) if site else (None, None)
        endpoint_labels.append((ROLE_LABELS[kind], _coordinates(*position)))
    requested = solution.requested_path_count if solution else candidates.reliability_paths
    achieved = solution.achieved_path_count if solution else int(bool(primary_hops))
    summary = _pairs(
        (
            *endpoint_labels,
            (
                "Repeaters",
                f"{result.router_count} total · {result.router_count - existing} proposed · {existing} existing",
            ),
            ("Hop count", f"{len(links)} total · {primary_hops} primary"),
            ("Total link distance", f"{_km(_distance(links))} km"),
            ("Primary path distance", f"{_km(_distance(links[:primary_hops]))} km"),
            ("Weakest usable margin", _number(_weakest(links), 2, " dB")),
            ("Independent paths", f"{achieved} / {requested} requested"),
            ("Solver status", "Complete" if result.search_complete else "Search incomplete"),
            ("Solver time", _number(result.elapsed_seconds, 2, " s")),
        )
    )
    diagnostics = list(
        dict.fromkeys([*result.diagnostics, *(solution.diagnostics if solution else [])])
    )
    messages = (
        "<ul>" + "".join(f"<li>{_escape(item)}</li>" for item in diagnostics) + "</ul>"
        if diagnostics
        else '<p class="muted">No solver diagnostics recorded.</p>'
    )
    roads = []
    nearest = {}
    if (overlay := result.road_overlay) is not None:
        index = result.active_solution_index
        if 0 <= index < len(overlay.polyline_indices):
            roads = [overlay.polylines[i] for i in overlay.polyline_indices[index]]
        if 0 <= index < len(overlay.nearest_distances_m):
            nearest = overlay.nearest_distances_m[index]
    map_markup, scripts = _map_parts(sites, links, nearest, roads, coverage)
    site_table = _table(
        (
            "Site ID",
            "Role",
            "Latitude",
            "Longitude",
            "Ground elevation",
            "Mast height",
            "Installation",
            "Nearest road",
        ),
        (
            (
                site.id,
                role,
                _number(site.latitude, 5),
                _number(site.longitude, 5),
                ground,
                mast,
                status,
                road,
            )
            for site in sites
            for role, ground, mast, status, road in [_site_fields(site, nearest)]
        ),
    )
    alternatives = [item for item in result.alternatives if item is not solution]
    other_solutions = (
        _table(
            ("Solution", "Repeaters", "Weakest margin (dB)", "Total distance (km)", "Paths"),
            (
                (
                    item.name,
                    item.router_count,
                    _number(item.minimum_margin_db, 2),
                    _km(_distance(item.links)),
                    f"{item.achieved_path_count} / {item.requested_path_count}",
                )
                for item in alternatives
            ),
        )
        if alternatives
        else '<p class="muted">No other solutions recorded.</p>'
    )
    profiles = "".join(
        f'<section class="hop" id="hop-{number}"><h2>Hop {number} · {_escape(_hop_name(link))}</h2>'
        f"{_profile_chart(link, rf.required_fresnel_clearance)}{_budget(link)}</section>"
        for number, link in enumerate(links, 1)
    )
    stamp = generated_at.astimezone(UTC).isoformat(timespec="seconds")
    selected = solution.name if solution else "Selected route" if result.found else "No route found"
    coverage_markup = (
        coverage_section(coverage) if coverage else f'<p class="muted">{_escape(coverage_note)}</p>'
    )
    return (
        '<!doctype html><html lang="en"><head><meta charset="utf-8">'
        '<meta name="viewport" content="width=device-width, initial-scale=1">'
        f"<title>RF route report</title><style>{_leaflet_assets()[0]}\n{REPORT_CSS}</style></head>"
        "<body><main><header><div><h1>RF route report</h1>"
        f'<p>{_escape(selected)}</p><p class="muted">Generated <time datetime="{_escape(stamp)}">'
        f"{_escape(stamp.replace('T', ' ').replace('+00:00', ' UTC'))}</time></p></div>"
        '<button class="print" type="button" onclick="window.print()">Print report</button></header>'
        f"<section><h2>Route summary</h2>{summary}<h3>Solver diagnostics</h3>{messages}</section>"
        f"{_assumptions(rf, candidates, terrain_resolution_m, surface_available)}{map_markup}"
        f'{coverage_markup}<section><h2>Sites</h2><div class="table-scroll">{site_table}</div></section>'
        f"{_link_table(links, primary_hops)}<section><h2>Alternatives</h2>{other_solutions}</section>"
        f"{profiles}</main>{scripts}</body></html>"
    )
