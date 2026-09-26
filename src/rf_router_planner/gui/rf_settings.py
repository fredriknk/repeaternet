from __future__ import annotations

from PySide6.QtCore import Signal
from PySide6.QtWidgets import (
    QAbstractItemView,
    QCheckBox,
    QComboBox,
    QDoubleSpinBox,
    QFileDialog,
    QFormLayout,
    QGroupBox,
    QHBoxLayout,
    QLabel,
    QLineEdit,
    QPushButton,
    QScrollArea,
    QSpinBox,
    QTableWidget,
    QTableWidgetItem,
    QToolBox,
    QVBoxLayout,
    QWidget,
)

from rf_router_planner.models.settings import (
    CandidateSettings,
    OptimizationPriority,
    RFSettings,
    ValidationMode,
    default_cache_directory,
)


def _spin(
    minimum: float, maximum: float, value: float, suffix: str = "", decimals: int = 2
) -> QDoubleSpinBox:
    widget = QDoubleSpinBox()
    widget.setRange(minimum, maximum)
    widget.setDecimals(decimals)
    widget.setValue(value)
    widget.setSuffix(suffix)
    widget.setKeyboardTracking(False)
    return widget


class SettingsPanel(QScrollArea):
    coordinates_applied = Signal(float, float, float, float)
    add_client_requested = Signal()
    add_router_requested = Signal()
    remove_site_requested = Signal()

    def __init__(self, parent=None) -> None:  # type: ignore[no-untyped-def]
        super().__init__(parent)
        self.setWidgetResizable(True)
        body = QWidget()
        layout = QVBoxLayout(body)
        layout.addWidget(self._workflow_group())
        layout.addWidget(self._planning_group())
        details = QToolBox()
        details.addItem(self._radio_group(), "Radio and antennas")
        details.addItem(self._antenna_group(), "Antenna details")
        details.addItem(self._optimization_group(), "Search details")
        details.addItem(self._propagation_group(), "Advanced physics")
        details.addItem(self._terrain_group(), "Terrain and cache (advanced)")
        details.addItem(self._coordinates_group(), "Legacy A-B coordinate entry")
        layout.addWidget(details)
        layout.addStretch()
        self.setWidget(body)

    def _workflow_group(self) -> QGroupBox:
        group = QGroupBox("Plan")
        layout = QVBoxLayout(group)
        help_text = QLabel(
            "1. Add two or more clients\n"
            "2. Add any required router locations\n"
            "3. Click Optimize — cached terrain is reused automatically"
        )
        help_text.setWordWrap(True)
        layout.addWidget(help_text)
        buttons = QHBoxLayout()
        add_client = QPushButton("+ Client")
        add_client.setToolTip("Click, then place a client on the map")
        add_client.clicked.connect(self.add_client_requested.emit)
        add_router = QPushButton("+ Required router")
        add_router.setToolTip("Click, then place a router every solution must include")
        add_router.clicked.connect(self.add_router_requested.emit)
        remove = QPushButton("Remove")
        remove.clicked.connect(self.remove_site_requested.emit)
        buttons.addWidget(add_client)
        buttons.addWidget(add_router)
        buttons.addWidget(remove)
        layout.addLayout(buttons)
        self.sites_table = QTableWidget(0, 3)
        self.sites_table.setHorizontalHeaderLabels(["Site", "Role", "Status"])
        self.sites_table.setSelectionBehavior(QAbstractItemView.SelectionBehavior.SelectRows)
        self.sites_table.setEditTriggers(QAbstractItemView.EditTrigger.NoEditTriggers)
        self.sites_table.setMaximumHeight(150)
        layout.addWidget(self.sites_table)
        return group

    def set_sites(self, sites: list[tuple[str, str, str]]) -> None:
        self.sites_table.setRowCount(len(sites))
        for row, values in enumerate(sites):
            for column, value in enumerate(values):
                self.sites_table.setItem(row, column, QTableWidgetItem(value))

    def selected_site_id(self) -> str | None:
        rows = self.sites_table.selectionModel().selectedRows()
        if not rows:
            return None
        item = self.sites_table.item(rows[0].row(), 0)
        return item.text() if item else None

    def _planning_group(self) -> QGroupBox:
        group = QGroupBox("Everyday planning")
        form = QFormLayout(group)
        self.priority = QComboBox()
        for label, value in [
            ("Fewest routers", OptimizationPriority.MINIMUM_ROUTERS),
            ("Balanced infrastructure", OptimizationPriority.MINIMUM_INFRASTRUCTURE),
            ("Resilient mesh (independent paths)", OptimizationPriority.MAXIMUM_RELIABILITY),
        ]:
            self.priority.addItem(label, value)
        self.corridor = _spin(0.1, 100, 10, " km")
        self.max_solution_routers = QSpinBox()
        self.max_solution_routers.setRange(0, 20)
        self.max_solution_routers.setValue(4)
        self.reliability_paths = QSpinBox()
        self.reliability_paths.setRange(1, 4)
        self.reliability_paths.setValue(2)
        self.auto_terrain = QCheckBox("Prepare terrain automatically when optimizing")
        self.auto_terrain.setChecked(True)
        form.addRow("Goal", self.priority)
        form.addRow("Search corridor each side", self.corridor)
        form.addRow("Show solutions through", self.max_solution_routers)
        form.addRow("Independent paths", self.reliability_paths)
        form.addRow(self.auto_terrain)
        return group

    def _coordinates_group(self) -> QGroupBox:
        group = QGroupBox("Coordinates")
        form = QFormLayout(group)
        self.a_lat = _spin(-90, 90, 59.9139, "°", 6)
        self.a_lon = _spin(-180, 180, 10.7522, "°", 6)
        self.b_lat = _spin(-90, 90, 60.39299, "°", 6)
        self.b_lon = _spin(-180, 180, 5.32415, "°", 6)
        form.addRow("A latitude", self.a_lat)
        form.addRow("A longitude", self.a_lon)
        form.addRow("B latitude", self.b_lat)
        form.addRow("B longitude", self.b_lon)
        button = QPushButton("Apply coordinates")
        button.clicked.connect(
            lambda: self.coordinates_applied.emit(
                self.a_lat.value(), self.a_lon.value(), self.b_lat.value(), self.b_lon.value()
            )
        )
        form.addRow(button)
        return group

    def _terrain_group(self) -> QGroupBox:
        group = QGroupBox("Terrain data")
        form = QFormLayout(group)
        self.cache_directory = QLineEdit(default_cache_directory())
        self.cache_directory.setReadOnly(True)
        self.cache_directory.setToolTip("Managed terrain cache; change only when moving storage")
        browse = QPushButton("Change…")
        browse.clicked.connect(self._browse_cache)
        cache_row = QWidget()
        row = QHBoxLayout(cache_row)
        row.setContentsMargins(0, 0, 0, 0)
        row.addWidget(self.cache_directory)
        row.addWidget(browse)
        self.download_resolution = _spin(1, 1000, 10, " m", 0)
        self.auto_resolution = QCheckBox("Automatically coarsen large downloads")
        self.auto_resolution.setChecked(True)
        self.maximum_total_pixels = _spin(1, 1000, 50, " million", 0)
        self.maximum_area = _spin(1, 10_000, 400, " km²", 0)
        self.maximum_tile_pixels = _spin(0.1, 100, 4, " million", 1)
        self.maximum_tiles = QSpinBox()
        self.maximum_tiles.setRange(1, 10_000)
        self.maximum_tiles.setValue(256)
        self.detail_resolution = _spin(1, 100, 10, " m", 0)
        self.detail_corridor = _spin(0.05, 5, 0.5, " km", 2)
        self.download_estimate = QLabel("A corridor tile estimate is shown before download.")
        self.download_estimate.setWordWrap(True)
        form.addRow("Kartverket cache", cache_row)
        form.addRow("Minimum/requested resolution", self.download_resolution)
        form.addRow(self.auto_resolution)
        form.addRow("Total pixel budget", self.maximum_total_pixels)
        form.addRow("Maximum WCS tile area", self.maximum_area)
        form.addRow("Maximum pixels per tile", self.maximum_tile_pixels)
        form.addRow("Maximum tile count", self.maximum_tiles)
        form.addRow("Final-route detail resolution", self.detail_resolution)
        form.addRow("Final-route detail buffer", self.detail_corridor)
        form.addRow(self.download_estimate)
        return group

    def _browse_cache(self) -> None:
        path = QFileDialog.getExistingDirectory(
            self, "Kartverket cache directory", self.cache_directory.text()
        )
        if path:
            self.cache_directory.setText(path)

    def _radio_group(self) -> QGroupBox:
        group = QGroupBox("Radio")
        form = QFormLayout(group)
        self.frequency = _spin(0.001, 100_000, 869.5, " MHz", 3)
        self.tx_power = _spin(-100, 100, 22, " dBm")
        self.sensitivity = _spin(-200, 0, -130, " dBm")
        self.fade_margin = _spin(0, 100, 10, " dB")
        self.misc_loss = _spin(0, 100, 0, " dB")
        form.addRow("Frequency", self.frequency)
        form.addRow("TX power", self.tx_power)
        form.addRow("Manual sensitivity", self.sensitivity)
        form.addRow("Required fade margin", self.fade_margin)
        form.addRow("Misc. loss", self.misc_loss)
        self.lora = QCheckBox("Calculate LoRa sensitivity")
        form.addRow(self.lora)
        self.bandwidth = _spin(1_000, 2_000_000, 125_000, " Hz", 0)
        self.sf = QComboBox()
        self.sf.addItems([f"SF{x}" for x in range(7, 13)])
        self.sf.setCurrentText("SF12")
        self.noise_figure = _spin(0, 30, 6, " dB")
        form.addRow("Bandwidth", self.bandwidth)
        form.addRow("Spreading factor", self.sf)
        form.addRow("Receiver noise figure", self.noise_figure)
        return group

    def _antenna_group(self) -> QGroupBox:
        group = QGroupBox("Antennas")
        form = QFormLayout(group)
        self.a_gain = _spin(-100, 100, 2.15, " dBi")
        self.b_gain = _spin(-100, 100, 2.15, " dBi")
        self.router_gain = _spin(-100, 100, 2.15, " dBi")
        self.feed_loss = _spin(0, 100, 0, " dB")
        self.a_height = _spin(0.1, 200, 3, " m")
        self.b_height = _spin(0.1, 200, 3, " m")
        self.router_height = _spin(0.1, 200, 3, " m")
        self.pattern_path = QLineEdit()
        browse = QPushButton("…")
        browse.clicked.connect(self._browse_pattern)
        pattern_row = QWidget()
        row = QHBoxLayout(pattern_row)
        row.setContentsMargins(0, 0, 0, 0)
        row.addWidget(self.pattern_path)
        row.addWidget(browse)
        form.addRow("A gain", self.a_gain)
        form.addRow("B gain", self.b_gain)
        form.addRow("Router gain", self.router_gain)
        form.addRow("Feed/cable loss", self.feed_loss)
        form.addRow("A height AGL", self.a_height)
        form.addRow("B height AGL", self.b_height)
        form.addRow("Router height AGL", self.router_height)
        form.addRow("Elevation pattern CSV", pattern_row)
        return group

    def _propagation_group(self) -> QGroupBox:
        group = QGroupBox("Propagation model — advanced")
        form = QFormLayout(group)
        self.validation = QComboBox()
        self.validation.addItem("RF propagation (diffraction)", ValidationMode.PROPAGATION)
        self.validation.addItem("Strict LOS + Fresnel", ValidationMode.STRICT_LOS)
        self.k_factor = _spin(0.1, 10, 4 / 3, "", 3)
        self.fresnel = _spin(0, 100, 60, " %", 0)
        form.addRow("Validation", self.validation)
        form.addRow("Effective Earth k-factor", self.k_factor)
        form.addRow("Required Fresnel clearance", self.fresnel)
        return group

    def _optimization_group(self) -> QGroupBox:
        group = QGroupBox("Optimization")
        form = QFormLayout(group)
        self.grid_spacing = _spin(0.01, 20, 1, " km")
        self.max_candidates = QSpinBox()
        self.max_candidates.setRange(10, 10_000)
        self.max_candidates.setValue(800)
        self.max_neighbors = QSpinBox()
        self.max_neighbors.setRange(2, 200)
        self.max_neighbors.setValue(16)
        self.parallel_workers = QSpinBox()
        self.parallel_workers.setRange(0, 128)
        self.parallel_workers.setSpecialValueText("Auto (all cores)")
        self.max_link_distance = _spin(0, 1_000, 0, " km")
        self.max_link_distance.setSpecialValueText("Automatic")
        self.refine_radius = _spin(0, 2_000, 250, " m", 0)
        self.refine_step = _spin(1, 500, 50, " m", 0)
        self.show_candidates = QCheckBox("Show candidate sites")
        self.optimize_heights = QCheckBox("Optimize router heights")
        self.min_height = _spin(0.1, 100, 2, " m")
        self.max_height = _spin(0.1, 200, 10, " m")
        self.height_step = _spin(0.1, 20, 1, " m")
        form.addRow("Candidate grid spacing", self.grid_spacing)
        form.addRow("Maximum candidates", self.max_candidates)
        form.addRow("Neighbors per candidate", self.max_neighbors)
        form.addRow("RF worker processes", self.parallel_workers)
        form.addRow("Maximum link distance", self.max_link_distance)
        form.addRow("Local refinement radius", self.refine_radius)
        form.addRow("Local refinement step", self.refine_step)
        form.addRow(self.show_candidates)
        form.addRow(self.optimize_heights)
        form.addRow("Minimum router height", self.min_height)
        form.addRow("Maximum router height", self.max_height)
        form.addRow("Height step", self.height_step)
        return group

    def _browse_pattern(self) -> None:
        path, _ = QFileDialog.getOpenFileName(self, "Load elevation pattern", "", "CSV (*.csv)")
        if path:
            self.pattern_path.setText(path)

    def rf_settings(self) -> RFSettings:
        settings = RFSettings.eu868_meshcore()
        settings.frequency_mhz = self.frequency.value()
        settings.tx_power_dbm = self.tx_power.value()
        settings.receiver_sensitivity_dbm = self.sensitivity.value()
        settings.fade_margin_db = self.fade_margin.value()
        settings.miscellaneous_loss_db = self.misc_loss.value()
        settings.k_factor = self.k_factor.value()
        settings.required_fresnel_clearance = self.fresnel.value() / 100.0
        settings.validation_mode = self.validation.currentData()
        settings.endpoint_a.gain_dbi = self.a_gain.value()
        settings.endpoint_b.gain_dbi = self.b_gain.value()
        settings.router.gain_dbi = self.router_gain.value()
        settings.endpoint_a.height_agl_m = self.a_height.value()
        settings.endpoint_b.height_agl_m = self.b_height.value()
        settings.router.height_agl_m = self.router_height.value()
        for antenna in [settings.endpoint_a, settings.endpoint_b, settings.router]:
            antenna.feed_loss_db = self.feed_loss.value()
            antenna.pattern_csv = self.pattern_path.text() or None
        settings.lora.enabled = self.lora.isChecked()
        settings.lora.manual_sensitivity_override = not self.lora.isChecked()
        settings.lora.bandwidth_hz = self.bandwidth.value()
        settings.lora.spreading_factor = int(self.sf.currentText()[2:])
        settings.lora.noise_figure_db = self.noise_figure.value()
        return settings

    def candidate_settings(self) -> CandidateSettings:
        return CandidateSettings(
            corridor_width_m=self.corridor.value() * 1000,
            grid_spacing_m=self.grid_spacing.value() * 1000,
            maximum_candidates=self.max_candidates.value(),
            maximum_neighbors_per_site=self.max_neighbors.value(),
            maximum_solution_routers=self.max_solution_routers.value(),
            reliability_paths=self.reliability_paths.value(),
            parallel_workers=self.parallel_workers.value(),
            maximum_link_distance_m=(self.max_link_distance.value() * 1000 or None),
            refine_radius_m=self.refine_radius.value(),
            refine_step_m=self.refine_step.value(),
            optimize_heights=self.optimize_heights.isChecked(),
            minimum_router_height_m=self.min_height.value(),
            maximum_router_height_m=self.max_height.value(),
            router_height_step_m=self.height_step.value(),
            priority=self.priority.currentData(),
        )

    def set_rf_settings(self, settings: RFSettings) -> None:
        self.frequency.setValue(settings.frequency_mhz)
        self.tx_power.setValue(settings.tx_power_dbm)
        self.sensitivity.setValue(settings.receiver_sensitivity_dbm)
        self.fade_margin.setValue(settings.fade_margin_db)
        self.misc_loss.setValue(settings.miscellaneous_loss_db)
        self.k_factor.setValue(settings.k_factor)
        self.fresnel.setValue(settings.required_fresnel_clearance * 100)
        self.validation.setCurrentIndex(self.validation.findData(settings.validation_mode))
        self.a_gain.setValue(settings.endpoint_a.gain_dbi)
        self.b_gain.setValue(settings.endpoint_b.gain_dbi)
        self.router_gain.setValue(settings.router.gain_dbi)
        self.feed_loss.setValue(settings.router.feed_loss_db)
        self.a_height.setValue(settings.endpoint_a.height_agl_m)
        self.b_height.setValue(settings.endpoint_b.height_agl_m)
        self.router_height.setValue(settings.router.height_agl_m)
        self.pattern_path.setText(settings.router.pattern_csv or "")
        self.lora.setChecked(
            settings.lora.enabled and not settings.lora.manual_sensitivity_override
        )
        self.bandwidth.setValue(settings.lora.bandwidth_hz)
        self.sf.setCurrentText(f"SF{settings.lora.spreading_factor}")
        self.noise_figure.setValue(settings.lora.noise_figure_db)

    def set_candidate_settings(self, settings: CandidateSettings) -> None:
        self.corridor.setValue(settings.corridor_width_m / 1000)
        self.grid_spacing.setValue(settings.grid_spacing_m / 1000)
        self.max_candidates.setValue(settings.maximum_candidates)
        self.max_neighbors.setValue(settings.maximum_neighbors_per_site)
        self.max_solution_routers.setValue(settings.maximum_solution_routers)
        self.reliability_paths.setValue(settings.reliability_paths)
        self.parallel_workers.setValue(settings.parallel_workers)
        self.max_link_distance.setValue((settings.maximum_link_distance_m or 0) / 1000)
        self.refine_radius.setValue(settings.refine_radius_m)
        self.refine_step.setValue(settings.refine_step_m)
        self.optimize_heights.setChecked(settings.optimize_heights)
        self.min_height.setValue(settings.minimum_router_height_m)
        self.max_height.setValue(settings.maximum_router_height_m)
        self.height_step.setValue(settings.router_height_step_m)
        self.priority.setCurrentIndex(self.priority.findData(settings.priority))
