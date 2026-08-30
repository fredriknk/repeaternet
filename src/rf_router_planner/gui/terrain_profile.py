from __future__ import annotations

from PySide6.QtWidgets import QLabel, QPlainTextEdit, QVBoxLayout, QWidget

from rf_router_planner.models.link import LinkResult

try:
    from matplotlib.backends.backend_qtagg import FigureCanvasQTAgg
    from matplotlib.figure import Figure
except ImportError:
    FigureCanvasQTAgg = None  # type: ignore[assignment,misc]


class TerrainProfileWidget(QWidget):
    def __init__(self, parent=None) -> None:  # type: ignore[no-untyped-def]
        super().__init__(parent)
        layout = QVBoxLayout(self)
        self.message = QLabel("Select a link to inspect its terrain and Fresnel profile.")
        self.message.setWordWrap(True)
        layout.addWidget(self.message)
        self.figure = Figure(figsize=(8, 3)) if FigureCanvasQTAgg is not None else None
        self.canvas = FigureCanvasQTAgg(self.figure) if self.figure else None
        self.hover = QLabel("")
        self.details = QPlainTextEdit()
        self.details.setReadOnly(True)
        self.details.setMaximumHeight(150)
        if self.canvas:
            layout.addWidget(self.canvas)
            self.canvas.mpl_connect("motion_notify_event", self._mouse_moved)
        layout.addWidget(self.hover)
        layout.addWidget(self.details)
        self._current_link: LinkResult | None = None

    def show_link(self, link: LinkResult, required_ratio: float) -> None:
        profile = link.profile
        self._current_link = link
        if profile is None or self.figure is None or self.canvas is None:
            self.message.setText("Profile plotting requires Matplotlib.")
            return
        suffix = (
            ""
            if profile.surface_available
            else " — Surface obstruction data unavailable — terrain only"
        )
        self.message.setText(
            f"{link.source_id} → {link.target_id}: {link.distance_m / 1000:.2f} km, "
            f"worst margin {link.worst_margin_db:.1f} dB{suffix}"
        )
        forward, reverse = link.forward, link.reverse
        self.details.setPlainText(
            "CALCULATION DETAILS\n"
            f"Forward {forward.source_id} → {forward.target_id}: TX power {forward.tx_power_dbm:.2f} dBm; "
            f"TX pattern gain {forward.tx_gain_dbi:.2f} dBi; TX feed loss {forward.tx_feed_loss_db:.2f} dB; "
            f"FSPL {forward.fspl_db:.2f} dB; "
            f"diffraction {forward.diffraction_loss_db:.2f} dB; clutter {forward.clutter_loss_db:.2f} dB; "
            f"total path loss {forward.total_path_loss_db:.2f} dB; RX pattern gain {forward.rx_gain_dbi:.2f} dBi; "
            f"RX feed loss {forward.rx_feed_loss_db:.2f} dB; miscellaneous loss {forward.miscellaneous_loss_db:.2f} dB; "
            f"RX power {forward.received_power_dbm:.2f} dBm; sensitivity {forward.sensitivity_dbm:.2f} dBm; "
            f"raw margin {forward.raw_margin_db:.2f} dB; fade-adjusted margin {forward.usable_margin_db:.2f} dB.\n"
            f"Reverse {reverse.source_id} → {reverse.target_id}: TX gain {reverse.tx_gain_dbi:.2f} dBi; "
            f"RX gain {reverse.rx_gain_dbi:.2f} dBi; RX power {reverse.received_power_dbm:.2f} dBm; "
            f"raw margin {reverse.raw_margin_db:.2f} dB; fade-adjusted margin {reverse.usable_margin_db:.2f} dB.\n"
            f"Departure/arrival: {forward.departure_angle_deg:.2f}° / {forward.arrival_angle_deg:.2f}°. "
            f"Minimum Fresnel clearance {100 * link.minimum_fresnel_clearance_ratio:.1f}% at "
            f"{link.minimum_clearance_distance_m / 1000:.3f} km; maximum radius {link.maximum_fresnel_radius_m:.2f} m."
        )
        self.figure.clear()
        axes = self.figure.add_subplot(111)
        distance = profile.distances_m / 1000.0
        axes.plot(
            distance,
            profile.dtm_elevation_m + profile.earth_bulge_m,
            label="DTM + curvature",
            color="#795548",
        )
        if profile.surface_available:
            axes.plot(
                distance,
                profile.surface_elevation_m + profile.earth_bulge_m,
                label="DOM + curvature",
                color="#2e7d32",
            )
        axes.plot(distance, profile.los_elevation_m, label="Direct RF LOS", color="#1565c0")
        axes.plot(
            distance,
            profile.los_elevation_m - profile.fresnel_radius_m,
            label="100% Fresnel boundary",
            color="#7e57c2",
            linestyle="--",
        )
        axes.plot(
            distance,
            profile.los_elevation_m - required_ratio * profile.fresnel_radius_m,
            label=f"{required_ratio:.0%} Fresnel boundary",
            color="#ef6c00",
            linestyle=":",
        )
        if link.dominant_obstacles:
            obstacle = link.dominant_obstacles[0]
            axes.axvline(
                obstacle["distance_m"] / 1000.0,
                color="#c62828",
                alpha=0.6,
                label="Dominant obstruction",
            )
        axes.set_xlabel("Distance from transmitter (km)")
        axes.set_ylabel("Elevation (m)")
        axes.grid(True, alpha=0.2)
        axes.legend(fontsize="small", ncols=2)
        self.figure.tight_layout()
        self.canvas.draw_idle()

    def _mouse_moved(self, event) -> None:  # type: ignore[no-untyped-def]
        if not self._current_link or event.xdata is None or self._current_link.profile is None:
            return
        profile = self._current_link.profile
        import numpy as np

        index = int(np.argmin(np.abs(profile.distances_m / 1000.0 - event.xdata)))
        self.hover.setText(
            f"Distance {profile.distances_m[index] / 1000:.3f} km | "
            f"DTM {profile.dtm_elevation_m[index]:.1f} m | DOM {profile.surface_elevation_m[index]:.1f} m | "
            f"LOS {profile.los_elevation_m[index]:.1f} m | Fresnel radius {profile.fresnel_radius_m[index]:.1f} m | "
            f"clearance {profile.clearance_m[index]:.1f} m"
        )
