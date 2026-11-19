"""Rasterize saved coverage cells for the standalone Leaflet route map."""

from __future__ import annotations

import base64
import struct
import zlib
from collections.abc import Iterable
from dataclasses import dataclass
from typing import Any

import numpy as np
from pyproj import Transformer

from .coverage_report import _color, _html, coverage_target_table, iter_complete_grid

MAX_COVERAGE_IMAGE_SIDE = 384


@dataclass(frozen=True, slots=True)
class ReportCoverage:
    image: str
    bounds: tuple[tuple[float, float], tuple[float, float]]
    metadata: dict[str, Any]
    target_report: dict[str, Any] | None

    @property
    def mode(self) -> str:
        return str(self.metadata.get("mode") or "two_way")


def _indexed_png(pixels: np.ndarray, palette: dict[str, int]) -> str:
    def chunk(kind: bytes, content: bytes) -> bytes:
        return (
            struct.pack("!I", len(content))
            + kind
            + content
            + struct.pack("!I", zlib.crc32(kind + content))
        )

    height, width = pixels.shape
    rgb = b"".join(bytes.fromhex(color[1:]) for color in palette)
    scanlines = np.zeros((height, width + 1), dtype=np.uint8)
    scanlines[:, 1:] = pixels
    png = (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack("!IIBBBBB", width, height, 8, 3, 0, 0, 0))
        + chunk(b"PLTE", rgb)
        + chunk(b"tRNS", b"\x00")
        + chunk(b"IDAT", zlib.compress(scanlines.tobytes()))
        + chunk(b"IEND", b"")
    )
    return "data:image/png;base64," + base64.b64encode(png).decode("ascii")


def build_report_coverage(
    cells: Iterable[dict[str, Any]],
    job: dict[str, Any],
    metadata: dict[str, Any],
    target_report: dict[str, Any] | None,
) -> ReportCoverage:
    """Sample a bounded Web Mercator image and retain the full-grid summary."""
    terrain_crs = job["terrain_crs"]
    left, bottom, right, top = job["grid_bounds_projected"]
    cell_size = job["effective_cell_size_m"]
    rows, columns = job["rows"], job["columns"]
    to_mercator = Transformer.from_crs(terrain_crs, 3857, always_xy=True)
    west, south, east, north = to_mercator.transform_bounds(left, bottom, right, top)
    extent_x, extent_y = east - west, north - south
    if not np.all(np.isfinite((west, south, east, north))) or min(extent_x, extent_y) <= 0:
        raise ValueError("Coverage grid has invalid map bounds")
    longest_side = min(MAX_COVERAGE_IMAGE_SIDE, 2 * max(rows, columns))
    pixel_scale = longest_side / max(extent_x, extent_y)
    width = max(1, round(extent_x * pixel_scale))
    height = max(1, round(extent_y * pixel_scale))
    mercator_x, mercator_y = np.meshgrid(
        west + (np.arange(width) + 0.5) * extent_x / width,
        north - (np.arange(height) + 0.5) * extent_y / height,
    )
    # Leaflet draws the image in Web Mercator, so each pixel is sampled back in the terrain grid.
    to_terrain = Transformer.from_crs(3857, terrain_crs, always_xy=True)
    xs, ys = to_terrain.transform(mercator_x, mercator_y)
    inside = (xs >= left) & (xs < right) & (ys > bottom) & (ys <= top)
    column = ((xs[inside] - left) / cell_size).astype(int)
    row = ((top - ys[inside]) / cell_size).astype(int)
    indices, inverse = np.unique(row * columns + column, return_inverse=True)
    positions = {index: position for position, index in enumerate(indices)}
    colors = np.zeros(len(indices), dtype=np.uint8)
    palette = {"#000000": 0}
    mode = str(metadata.get("mode") or "two_way")
    for cell in iter_complete_grid(cells, job):
        position = positions.get(cell["index"])
        if position is not None and cell["state"] != "outside_area":
            color = _color(cell, mode)
            colors[position] = palette.setdefault(color, len(palette))
    pixels = np.zeros((height, width), dtype=np.uint8)
    pixels[inside] = colors[inverse]
    to_wgs84 = Transformer.from_crs(3857, 4326, always_xy=True)
    sw_lon, sw_lat = to_wgs84.transform(west, south)
    ne_lon, ne_lat = to_wgs84.transform(east, north)
    return ReportCoverage(
        _indexed_png(pixels, palette),
        ((sw_lat, sw_lon), (ne_lat, ne_lon)),
        metadata,
        target_report,
    )


def coverage_section(coverage: ReportCoverage) -> str:
    metadata = coverage.metadata
    counts = metadata["counts"]["state_counts"]
    area_cells = metadata["counts"]["requested_cells"] - counts["outside_area"]
    cell_size = metadata["effective_cell_size_m"]
    settings = metadata.get("settings") or {}
    client = settings.get("client") or {}
    mode = {
        "two_way": "Two-way",
        "downlink": "Downlink",
        "uplink": "Uplink",
        "overlap": "Overlap",
        "best_source": "Best source",
    }.get(coverage.mode, coverage.mode)
    summary = (
        ("Coverage mode", mode),
        ("Cell size", f"{cell_size:,.1f} m"),
        ("Analyzed area", f"{area_cells * cell_size**2 / 1e6:,.3f} km²"),
        ("Covered area", f"{counts['covered'] * cell_size**2 / 1e6:,.3f} km²"),
        ("Client profile", client.get("name", "—")),
        ("Client height (m AGL)", client.get("height_agl_m", "—")),
        ("Client TX power (dBm)", client.get("tx_power_dbm", "—")),
        ("Client antenna gain (dBi)", client.get("gain_dbi", "—")),
        ("Client sensitivity (dBm)", client.get("sensitivity_dbm", "—")),
        ("Client feed loss (dB)", client.get("feed_loss_db", "—")),
        ("Client miscellaneous loss (dB)", client.get("miscellaneous_loss_db", "—")),
    )
    metrics = "".join(
        f"<div><dt>{_html(label)}</dt><dd>{_html(value)}</dd></div>" for label, value in summary
    )
    rows = []
    for state, label in (
        ("covered", "Covered"),
        ("uncovered", "Uncovered"),
        ("unknown_terrain", "Unknown terrain"),
        ("unresolved", "Unresolved"),
        ("not_evaluated", "Not evaluated"),
    ):
        count = counts[state]
        percent = f"{count / area_cells:.1%}" if area_cells else "—"
        rows.append(f'<tr><th scope="row">{label}</th><td>{count:,}</td><td>{percent}</td></tr>')
    targets = (
        "<h3>Coverage targets</h3>" + coverage_target_table(coverage.target_report)
        if coverage.target_report
        else ""
    )
    return (
        '<section id="coverage"><h2>Predicted coverage</h2>'
        f'<p class="muted">{_html(metadata["prediction_warning"])}</p><dl>{metrics}</dl>'
        '<table><thead><tr><th scope="col">State</th><th scope="col">Cells</th>'
        f'<th scope="col">Share of area</th></tr></thead><tbody>{"".join(rows)}</tbody></table>'
        f"{targets}</section>"
    )
