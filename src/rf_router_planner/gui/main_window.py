from __future__ import annotations

import logging
from pathlib import Path
from threading import Event
from typing import Any

import numpy as np
from PySide6.QtCore import QObject, QStandardPaths, Qt, QThread, Signal, Slot
from PySide6.QtGui import QAction
from PySide6.QtWidgets import (
    QApplication,
    QComboBox,
    QFileDialog,
    QHeaderView,
    QInputDialog,
    QLabel,
    QMainWindow,
    QMessageBox,
    QProgressBar,
    QSplitter,
    QStatusBar,
    QTableWidget,
    QTableWidgetItem,
    QTabWidget,
    QToolBar,
    QVBoxLayout,
    QWidget,
)

from rf_router_planner.coordinates import norway_utm_epsg
from rf_router_planner.export import export_route_csv, export_route_geojson
from rf_router_planner.integrations import CoreScopeClient, CoreScopeRepeater
from rf_router_planner.models.link import LinkResult
from rf_router_planner.models.settings import (
    CandidateSettings,
    OptimizationPriority,
    RFSettings,
    TerrainSettings,
)
from rf_router_planner.models.site import Site, SiteKind, SiteOrigin
from rf_router_planner.optimization.optimizer import OptimizationResult, RouteOptimizer
from rf_router_planner.optimization.parallel import evaluate_link_pairs
from rf_router_planner.project import Project, load_project, save_project
from rf_router_planner.rf.propagation import LinkEvaluator
from rf_router_planner.terrain.contours import contour_geojson
from rf_router_planner.terrain.kartverket import (
    DownloadPlan,
    KartverketProvider,
    RouteCorridor,
    load_services,
)
from rf_router_planner.terrain.raster import RasterTerrain

from .map_widget import MapWidget
from .rf_settings import SettingsPanel
from .terrain_profile import TerrainProfileWidget

logger = logging.getLogger(__name__)


class OptimizationWorker(QObject):
    progress = Signal(str, int, int)
    finished = Signal(object)
    failed = Signal(str)

    def __init__(
        self,
        optimizer: RouteOptimizer,
        endpoint_a: Site,
        endpoint_b: Site,
        clients: list[Site] | None = None,
        required_routers: list[Site] | None = None,
    ) -> None:
        super().__init__()
        self.optimizer, self.endpoint_a, self.endpoint_b = optimizer, endpoint_a, endpoint_b
        self.clients = clients
        self.required_routers = required_routers
        self.cancel_event = Event()

    @Slot()
    def run(self) -> None:
        try:
            result = self.optimizer.optimize(
                self.endpoint_a,
                self.endpoint_b,
                progress=lambda stage, done, total: self.progress.emit(stage, done, total),
                cancelled=self.cancel_event.is_set,
                clients=self.clients,
                required_routers=self.required_routers,
            )
            self.finished.emit(result)
        except Exception as exc:
            logger.exception("Optimization failed")
            self.failed.emit(str(exc))

    def cancel(self) -> None:
        self.cancel_event.set()


class TerrainDownloadWorker(QObject):
    progress = Signal(str, int, int)
    finished = Signal(object)
    failed = Signal(str)

    def __init__(
        self,
        provider: KartverketProvider,
        crs: str,
        plan: DownloadPlan,
        include_dom: bool,
    ) -> None:
        super().__init__()
        self.provider, self.crs, self.plan = provider, crs, plan
        self.include_dom = include_dom

    @Slot()
    def run(self) -> None:
        try:
            tile_count = len(self.plan.tiles)
            total_work = tile_count * (2 if self.include_dom else 1)
            paths = [
                str(path)
                for path in self.provider.fetch_plan(
                    "dtm",
                    self.crs,
                    self.plan,
                    lambda index, total, cached: self.progress.emit(
                        f"Kartverket DTM tile {index}/{total}" + (" (cache hit)" if cached else ""),
                        index - 1,
                        total_work,
                    ),
                )
            ]
            self.progress.emit("Kartverket DTM complete", tile_count, total_work)
            dom: list[str] = []
            if self.include_dom:
                dom = [
                    str(path)
                    for path in self.provider.fetch_plan(
                        "dom",
                        self.crs,
                        self.plan,
                        lambda index, total, cached: self.progress.emit(
                            f"Kartverket DOM tile {index}/{total}"
                            + (" (cache hit)" if cached else ""),
                            tile_count + index - 1,
                            total_work,
                        ),
                    )
                ]
                self.progress.emit("Kartverket DOM complete", total_work, total_work)
            self.finished.emit((paths, dom, self.plan))
        except Exception as exc:
            logger.exception("Kartverket download failed")
            self.failed.emit(str(exc))


class CoreScopeWorker(QObject):
    finished = Signal(object)
    failed = Signal(str)

    @Slot()
    def run(self) -> None:
        try:
            self.finished.emit(CoreScopeClient().fetch_repeaters())
        except Exception as exc:
            logger.exception("CoreScope import failed")
            self.failed.emit(str(exc))


class CoverageWorker(QObject):
    progress = Signal(str, int, int)
    finished = Signal(object)
    failed = Signal(str)

    def __init__(
        self,
        terrain: RasterTerrain,
        rf_settings: RFSettings,
        candidate_settings: CandidateSettings,
        sources: list[Site],
        candidates: list[Site],
    ) -> None:
        super().__init__()
        self.terrain = terrain
        self.rf_settings = rf_settings
        self.candidate_settings = candidate_settings
        self.sources = sources
        self.candidates = candidates
        self.cancel_event = Event()

    @Slot()
    def run(self) -> None:
        try:
            evaluator = LinkEvaluator(self.terrain, self.rf_settings)
            selected_ids = {site.id for site in self.sources}
            pairs: list[tuple[Site, Site]] = []
            maximum_distance = self.candidate_settings.maximum_link_distance_m
            for source in self.sources:
                for target in self.candidates:
                    if target.id in selected_ids:
                        continue
                    if (
                        maximum_distance is not None
                        and source.distance_to(target) > maximum_distance
                    ):
                        continue
                    if evaluator.optimistic_margin_db(source, target) >= 0.0:
                        pairs.append((source, target))
            links = evaluate_link_pairs(
                self.terrain,
                self.rf_settings,
                pairs,
                self.candidate_settings.coarse_sample_step_m,
                workers=self.candidate_settings.parallel_workers,
                progress=lambda done, total: self.progress.emit(
                    "Calculating coverage", done, total
                ),
                cancelled=self.cancel_event.is_set,
            )
            self.finished.emit([link for link in links if link.valid])
        except Exception as exc:
            logger.exception("Coverage calculation failed")
            self.failed.emit(str(exc))

    def cancel(self) -> None:
        self.cancel_event.set()


class MainWindow(QMainWindow):
    def __init__(self) -> None:
        super().__init__()
        self.setWindowTitle("RF Router Planner")
        self.resize(1500, 900)
        self.terrain: RasterTerrain | None = None
        self.detail_terrain: RasterTerrain | None = None
        self.detail_validation_note: str | None = None
        self.endpoint_a: Site | None = None
        self.endpoint_b: Site | None = None
        self.additional_clients: list[Site] = []
        self.manual_routers: list[Site] = []
        self.known_repeaters: list[CoreScopeRepeater] = []
        self.enabled_known_routers: list[Site] = []
        self.result: OptimizationResult | None = None
        self.project_path: Path | None = None
        self.worker: OptimizationWorker | None = None
        self.worker_thread: QThread | None = None
        self.download_worker: TerrainDownloadWorker | None = None
        self.download_thread: QThread | None = None
        self.corescope_worker: CoreScopeWorker | None = None
        self.corescope_thread: QThread | None = None
        self.coverage_worker: CoverageWorker | None = None
        self.coverage_thread: QThread | None = None
        self._coverage_restart = False
        self.selected_site_id: str | None = None
        self._forward: Any | None = None
        self._reverse: Any | None = None
        self._build_ui()

    def _build_ui(self) -> None:
        self.settings_panel = SettingsPanel()
        self.settings_panel.coordinates_applied.connect(self._coordinates_applied)
        self.settings_panel.show_candidates.toggled.connect(self._show_candidates)
        self.map_widget = MapWidget()
        self.settings_panel.add_client_requested.connect(
            lambda: self.map_widget.set_mode("client")
        )
        self.settings_panel.add_router_requested.connect(
            lambda: self.map_widget.set_mode("router")
        )
        self.settings_panel.remove_site_requested.connect(self.remove_selected_site)
        self.map_widget.bridge.clicked.connect(self._map_clicked)
        self.map_widget.bridge.moved.connect(self._marker_moved)
        self.map_widget.bridge.link_selected.connect(self._link_selected)
        self.map_widget.bridge.site_selected.connect(self._site_selected)
        self.profile = TerrainProfileWidget()
        self.results_table = self._create_results_table()
        self.summary = QLabel("Load DTM terrain, then place endpoints A and B.")
        self.summary.setWordWrap(True)
        self.solution_selector = QComboBox()
        self.solution_selector.setToolTip("Choose the best solution for an exact router count")
        self.solution_selector.currentIndexChanged.connect(self._solution_changed)
        self.solution_selector.hide()
        results_page = QWidget()
        results_layout = QVBoxLayout(results_page)
        results_layout.addWidget(self.summary)
        results_layout.addWidget(self.solution_selector)
        results_layout.addWidget(self.results_table)
        tabs = QTabWidget()
        tabs.addTab(results_page, "Route results")
        tabs.addTab(self.profile, "Terrain / Fresnel profile")
        right = QSplitter(Qt.Orientation.Vertical)
        right.addWidget(self.map_widget)
        right.addWidget(tabs)
        right.setSizes([600, 300])
        root = QSplitter()
        root.addWidget(self.settings_panel)
        root.addWidget(right)
        root.setSizes([330, 1170])
        self.setCentralWidget(root)
        self._build_toolbar()
        status = QStatusBar()
        self.progress_label = QLabel("Ready")
        self.progress_bar = QProgressBar()
        self.progress_bar.setMaximumWidth(250)
        self.progress_bar.hide()
        status.addWidget(self.progress_label, 1)
        status.addPermanentWidget(self.progress_bar)
        self.setStatusBar(status)

    def _create_results_table(self) -> QTableWidget:
        headers = [
            "Hop",
            "From",
            "To",
            "Distance",
            "FSPL",
            "Diffraction",
            "RX power",
            "Raw margin",
            "Usable margin",
            "Reverse",
            "Fresnel",
            "LOS",
            "Status",
        ]
        table = QTableWidget(0, len(headers))
        table.setHorizontalHeaderLabels(headers)
        table.horizontalHeader().setSectionResizeMode(QHeaderView.ResizeMode.ResizeToContents)
        table.setSelectionBehavior(QTableWidget.SelectionBehavior.SelectRows)
        table.cellClicked.connect(lambda row, _column: self._link_selected(row))
        return table

    def _build_toolbar(self) -> None:
        toolbar = QToolBar("Plan")
        toolbar.setMovable(False)
        self.addToolBar(toolbar)
        for label, callback in [
            ("New", self.new_project),
            ("Open", self.open_project),
            ("Save", self.save_project),
        ]:
            action = QAction(label, self)
            action.triggered.connect(callback)
            toolbar.addAction(action)
        toolbar.addSeparator()
        for label, callback in [
            ("+ Client", lambda: self.map_widget.set_mode("client")),
            ("+ Router", lambda: self.map_widget.set_mode("router")),
            ("Remove", self.remove_selected_site),
        ]:
            action = QAction(label, self)
            action.triggered.connect(callback)
            toolbar.addAction(action)
        toolbar.addSeparator()
        self.optimize_action = QAction("Optimize", self)
        self.optimize_action.triggered.connect(self.optimize)
        toolbar.addAction(self.optimize_action)
        cancel = QAction("Cancel", self)
        cancel.triggered.connect(self.cancel_optimization)
        toolbar.addAction(cancel)

        file_menu = self.menuBar().addMenu("File")
        file_menu.addAction("Save As…", self.save_project_as)
        file_menu.addSeparator()
        file_menu.addAction("Export CSV…", self.export_csv)
        file_menu.addAction("Export GeoJSON…", self.export_geojson)
        terrain_menu = self.menuBar().addMenu("Terrain")
        terrain_menu.addAction("Prepare/download automatically", self.download_terrain)
        terrain_menu.addAction("Load local GeoTIFF…", self.load_terrain)
        terrain_menu.addAction("Validate selected network in detail", self.download_route_detail)
        view_menu = self.menuBar().addMenu("View")
        self.contour_action = view_menu.addAction("DTM contours")
        self.contour_action.setCheckable(True)
        self.contour_action.triggered.connect(self.toggle_dtm_contours)
        self.coverage_action = view_menu.addAction("Predicted coverage")
        self.coverage_action.setCheckable(True)
        self.coverage_action.triggered.connect(self.toggle_coverage)
        tools_menu = self.menuBar().addMenu("Tools")
        tools_menu.addAction("Import known CoreScope routers", self.import_corescope_routers)
        tools_menu.addAction("Enable/disable selected known router", self.toggle_known_router)
        tools_menu.addSeparator()
        tools_menu.addAction("Lock/unlock selected", self.toggle_selected_lock)
        tools_menu.addAction("Re-optimize unlocked", self.reoptimize_unlocked)
        tools_menu.addAction("Copy coordinates", self.copy_coordinates)

    def _set_transformers(self) -> None:
        if not self.terrain:
            return
        try:
            from pyproj import Transformer
        except ImportError as exc:
            raise RuntimeError("PyProj is required by the map interface") from exc
        self._forward = Transformer.from_crs("EPSG:4326", self.terrain.crs, always_xy=True)
        self._reverse = Transformer.from_crs(self.terrain.crs, "EPSG:4326", always_xy=True)

    def _metric_site(self, site_id: str, latitude: float, longitude: float, kind: SiteKind) -> Site:
        if not self.terrain or not self._forward:
            return Site(site_id, longitude, latitude, latitude, longitude, kind)
        x, y = self._forward.transform(longitude, latitude)
        ground = float(self.terrain.sample(np.array([x]), np.array([y]))[0])
        if not np.isfinite(ground):
            raise ValueError("The selected point is outside the loaded DTM")
        surface = None
        if self.terrain.has_surface:
            value = float(self.terrain.sample(np.array([x]), np.array([y]), surface=True)[0])
            surface = value if np.isfinite(value) else None
        rf = self.settings_panel.rf_settings()
        heights = {
            SiteKind.ENDPOINT_A: rf.endpoint_a.height_agl_m,
            SiteKind.ENDPOINT_B: rf.endpoint_b.height_agl_m,
            SiteKind.CLIENT: rf.endpoint_a.height_agl_m,
        }
        return Site(
            site_id,
            x,
            y,
            latitude,
            longitude,
            kind,
            ground,
            surface,
            heights.get(kind, rf.router.height_agl_m),
        )

    def _all_clients(self) -> list[Site]:
        return [
            *([self.endpoint_a] if self.endpoint_a else []),
            *([self.endpoint_b] if self.endpoint_b else []),
            *self.additional_clients,
        ]

    def _required_routers(self) -> list[Site]:
        return [*self.manual_routers, *self.enabled_known_routers]

    def _refresh_sites_panel(self) -> None:
        rows: list[tuple[str, str, str]] = []
        for site in self._all_clients():
            rows.append((site.id, "Client", "Ready" if self.terrain else "Terrain pending"))
        for site in self.manual_routers:
            rows.append((site.id, "Required router", "Included in every solution"))
        for site in self.enabled_known_routers:
            rows.append((site.id, "CoreScope router", "Enabled"))
        self.settings_panel.set_sites(rows)

    def _add_client(self, latitude: float, longitude: float) -> None:
        if self.endpoint_a is None:
            self._set_endpoint("A", latitude, longitude)
            return
        if self.endpoint_b is None:
            self._set_endpoint("B", latitude, longitude)
            return
        site_id = f"C{len(self.additional_clients) + 3}"
        site = self._metric_site(site_id, latitude, longitude, SiteKind.CLIENT)
        site.origin = SiteOrigin.MANUAL
        self.additional_clients.append(site)
        self.map_widget.set_point(site_id, latitude, longitude, role="client")
        self._refresh_sites_panel()
        self.progress_label.setText(f"Added client {site_id}")

    @Slot(float, float, float, float)
    def _coordinates_applied(self, a_lat: float, a_lon: float, b_lat: float, b_lon: float) -> None:
        try:
            self._set_endpoint("A", a_lat, a_lon)
            self._set_endpoint("B", b_lat, b_lon)
        except ValueError as exc:
            QMessageBox.warning(self, "Coordinates", str(exc))

    @Slot(str, float, float)
    def _map_clicked(self, mode: str, latitude: float, longitude: float) -> None:
        try:
            if mode in {"A", "B"}:
                self._set_endpoint(mode, latitude, longitude)
            elif mode == "client":
                self._add_client(latitude, longitude)
            elif mode == "router":
                self._add_manual_router(latitude, longitude)
        except ValueError as exc:
            QMessageBox.warning(self, "Map point", str(exc))

    def _set_endpoint(self, endpoint: str, latitude: float, longitude: float) -> None:
        kind = SiteKind.ENDPOINT_A if endpoint == "A" else SiteKind.ENDPOINT_B
        site = self._metric_site(endpoint, latitude, longitude, kind)
        if endpoint == "A":
            self.endpoint_a = site
            self.settings_panel.a_lat.setValue(latitude)
            self.settings_panel.a_lon.setValue(longitude)
        else:
            self.endpoint_b = site
            self.settings_panel.b_lat.setValue(latitude)
            self.settings_panel.b_lon.setValue(longitude)
        self.map_widget.set_point(endpoint, latitude, longitude)
        site.origin = SiteOrigin.MANUAL
        self._refresh_sites_panel()
        self.progress_label.setText(f"Endpoint {endpoint}: {latitude:.6f}, {longitude:.6f}")

    @Slot(str, float, float)
    def _marker_moved(self, site_id: str, latitude: float, longitude: float) -> None:
        try:
            if site_id in {"A", "B"}:
                self._set_endpoint(site_id, latitude, longitude)
            elif site_id in {site.id for site in [*self.additional_clients, *self.manual_routers]}:
                collection = (
                    self.additional_clients
                    if any(site.id == site_id for site in self.additional_clients)
                    else self.manual_routers
                )
                old = next(site for site in collection if site.id == site_id)
                replacement = self._metric_site(site_id, latitude, longitude, old.kind)
                replacement.origin = old.origin
                replacement.required = old.required
                replacement.locked = old.locked
                collection[collection.index(old)] = replacement
                self._refresh_sites_panel()
            elif self.result:
                site = next((item for item in self.result.route if item.id == site_id), None)
                if site and not site.locked:
                    replacement = self._metric_site(site_id, latitude, longitude, SiteKind.ROUTER)
                    self.result.route[self.result.route.index(site)] = replacement
                    self._recalculate_manual_route()
        except ValueError as exc:
            QMessageBox.warning(self, "Move site", str(exc))

    @Slot(str)
    def _site_selected(self, site_id: str) -> None:
        self.selected_site_id = site_id
        for row in range(self.settings_panel.sites_table.rowCount()):
            item = self.settings_panel.sites_table.item(row, 0)
            if item and item.text() == site_id:
                self.settings_panel.sites_table.selectRow(row)
                break
        self.progress_label.setText(f"Selected {site_id}")

    def import_corescope_routers(self) -> None:
        if self.corescope_thread and self.corescope_thread.isRunning():
            return
        self.progress_label.setText("Loading known CoreScope repeaters…")
        self.progress_bar.setRange(0, 0)
        self.progress_bar.show()
        self.corescope_worker = CoreScopeWorker()
        self.corescope_thread = QThread(self)
        self.corescope_worker.moveToThread(self.corescope_thread)
        self.corescope_thread.started.connect(self.corescope_worker.run)
        self.corescope_worker.finished.connect(self._corescope_finished)
        self.corescope_worker.failed.connect(self._corescope_failed)
        self.corescope_worker.finished.connect(self.corescope_thread.quit)
        self.corescope_worker.failed.connect(self.corescope_thread.quit)
        self.corescope_thread.finished.connect(self.corescope_worker.deleteLater)
        self.corescope_thread.finished.connect(self.corescope_thread.deleteLater)
        self.corescope_thread.finished.connect(self._corescope_cleanup)
        self.corescope_thread.start()

    @Slot(object)
    def _corescope_finished(self, repeaters: list[CoreScopeRepeater]) -> None:
        self.progress_bar.hide()
        clients = self._all_clients()
        if clients:
            latitudes = [site.latitude for site in clients if site.latitude is not None]
            longitudes = [site.longitude for site in clients if site.longitude is not None]
            if latitudes and longitudes:
                repeaters = [
                    item
                    for item in repeaters
                    if min(latitudes) - 3 <= item.latitude <= max(latitudes) + 3
                    and min(longitudes) - 6 <= item.longitude <= max(longitudes) + 6
                ]
        self.known_repeaters = repeaters
        self._render_known_routers()
        self.progress_label.setText(
            f"Loaded {len(repeaters)} nearby CoreScope repeaters — gray sites are disabled"
        )

    @Slot(str)
    def _corescope_failed(self, message: str) -> None:
        self.progress_bar.hide()
        QMessageBox.warning(
            self,
            "CoreScope import",
            "The optional CoreScope feed could not be loaded. The RF planner remains usable.\n\n"
            + message,
        )

    @Slot()
    def _corescope_cleanup(self) -> None:
        self.corescope_worker = None
        self.corescope_thread = None

    def _render_known_routers(self) -> None:
        enabled = {site.id for site in self.enabled_known_routers}
        self.map_widget.set_known_routers(
            [
                {
                    "id": item.id,
                    "name": item.name,
                    "lat": item.latitude,
                    "lon": item.longitude,
                    "status": item.freshness,
                    "enabled": item.id in enabled,
                }
                for item in self.known_repeaters
            ]
        )

    def toggle_known_router(self) -> None:
        if not self.selected_site_id:
            return
        repeater = next(
            (item for item in self.known_repeaters if item.id == self.selected_site_id), None
        )
        if repeater is None:
            return
        existing = next(
            (site for site in self.enabled_known_routers if site.id == repeater.id), None
        )
        if existing:
            self.enabled_known_routers.remove(existing)
            state = "disabled"
        else:
            site = self._metric_site(
                repeater.id,
                repeater.latitude,
                repeater.longitude,
                SiteKind.ROUTER,
            )
            site.origin = SiteOrigin.KNOWN
            site.locked = True
            self.enabled_known_routers.append(site)
            state = "enabled"
        self._render_known_routers()
        self._refresh_sites_panel()
        self.progress_label.setText(f"{repeater.name}: {state}")

    def load_terrain(self) -> None:
        dtm_paths, _ = QFileDialog.getOpenFileNames(
            self,
            "Load DTM GeoTIFF tile(s)",
            str(self._project_directory()),
            "GeoTIFF (*.tif *.tiff)",
        )
        if not dtm_paths:
            return
        answer = QMessageBox.question(
            self,
            "Surface model",
            "Load matching DOM/surface tiles?",
            QMessageBox.StandardButton.Yes | QMessageBox.StandardButton.No,
        )
        dom_paths: list[str] = []
        if answer == QMessageBox.StandardButton.Yes:
            dom_paths, _ = QFileDialog.getOpenFileNames(
                self, "Load DOM GeoTIFF tile(s)", "", "GeoTIFF (*.tif *.tiff)"
            )
        try:
            self._clear_detail_terrain()
            self._clear_dtm_contours()
            if self.terrain:
                self.terrain.close()
            self.terrain = RasterTerrain(dtm_paths, dom_paths)
            self._set_transformers()
            if self.endpoint_a:
                self._set_endpoint(
                    "A", self.endpoint_a.latitude or 0, self.endpoint_a.longitude or 0
                )
            if self.endpoint_b:
                self._set_endpoint(
                    "B", self.endpoint_b.latitude or 0, self.endpoint_b.longitude or 0
                )
            self._refresh_metric_planning_sites()
            message = f"Loaded {len(dtm_paths)} DTM tile(s) in {self.terrain.crs}"
            if not self.terrain.has_surface:
                message += " — Surface obstruction data unavailable — terrain only"
            self.progress_label.setText(message)
        except Exception as exc:
            QMessageBox.critical(self, "Terrain load failed", str(exc))

    def download_terrain(self) -> None:
        if len(self._all_clients()) < 2:
            QMessageBox.information(self, "Kartverket terrain", "Add at least two clients first.")
            return
        try:
            provider, crs, plan = self._terrain_download_plan()
            include_dom = (
                QMessageBox.question(
                    self,
                    "Kartverket terrain",
                    "Download DOM surface data too?",
                    QMessageBox.StandardButton.Yes | QMessageBox.StandardButton.No,
                )
                == QMessageBox.StandardButton.Yes
            )
            product_count = 2 if include_dom else 1
            cached = provider.cached_tile_count("dtm", crs, plan)
            if include_dom:
                cached += provider.cached_tile_count("dom", crs, plan)
            total_product_tiles = len(plan.tiles) * product_count
            summary = plan.summary(product_count)
            estimate = f"Download plan: {summary}. Cache hits: {cached}/{total_product_tiles}."
            self.settings_panel.download_estimate.setText(estimate)
            confirmation = QMessageBox.question(
                self,
                "Kartverket download plan",
                estimate + "\n\nContinue?",
                QMessageBox.StandardButton.Yes | QMessageBox.StandardButton.No,
            )
            if confirmation != QMessageBox.StandardButton.Yes:
                return
            self._start_terrain_download(
                TerrainDownloadWorker(provider, crs, plan, include_dom),
                self._download_finished,
            )
        except Exception as exc:
            QMessageBox.critical(self, "Kartverket terrain", str(exc))

    def _terrain_download_plan(
        self,
    ) -> tuple[KartverketProvider, str, DownloadPlan]:
        from pyproj import Transformer

        sites = [*self._all_clients(), *self._required_routers()]
        if len(self._all_clients()) < 2:
            raise ValueError("Add at least two clients first")
        longitude = sum(site.longitude or 0 for site in sites) / len(sites)
        crs = f"EPSG:{norway_utm_epsg(longitude)}"
        transform = Transformer.from_crs("EPSG:4326", crs, always_xy=True)
        points = tuple(
            transform.transform(site.longitude, site.latitude)
            for site in sites
            if site.longitude is not None and site.latitude is not None
        )
        if len(points) < 2:
            raise ValueError("Every client needs valid latitude and longitude")
        padding = self.settings_panel.corridor.value() * 1000
        xs, ys = zip(*points, strict=True)
        bounds = (
            min(xs) - padding,
            min(ys) - padding,
            max(xs) + padding,
            max(ys) + padding,
        )
        services_path = Path(__file__).parents[1] / "data" / "kartverket_wcs.json"
        provider = KartverketProvider(
            load_services(services_path),
            self.settings_panel.cache_directory.text(),
            self.settings_panel.maximum_area.value(),
            int(self.settings_panel.maximum_tile_pixels.value() * 1_000_000),
            self.settings_panel.maximum_tiles.value(),
        )
        corridor_points = points
        if (
            self.settings_panel.priority.currentData()
            == OptimizationPriority.MAXIMUM_RELIABILITY
            and len(points) > 2
        ):
            corridor_points = (*points, points[0])
        plan = provider.plan_download(
            bounds,
            self.settings_panel.download_resolution.value(),
            corridor=RouteCorridor(corridor_points, padding),
            auto_resolution=self.settings_panel.auto_resolution.isChecked(),
            maximum_total_pixels=int(
                self.settings_panel.maximum_total_pixels.value() * 1_000_000
            ),
        )
        return provider, crs, plan

    def download_route_detail(self) -> None:
        if not self.result or not self.result.found or not self.terrain:
            QMessageBox.information(
                self,
                "Detailed route validation",
                "Optimize a route on the broad terrain first.",
            )
            return
        if self.download_thread and self.download_thread.isRunning():
            QMessageBox.information(self, "Detailed route validation", "A download is running.")
            return
        try:
            detail_sites = (
                self.result.active_solution.sites
                if self.result.active_solution
                else self.result.route
            )
            route_points = tuple((site.x, site.y) for site in detail_sites)
            padding = self.settings_panel.detail_corridor.value() * 1000
            xs, ys = zip(*route_points, strict=True)
            bounds = (
                min(xs) - padding,
                min(ys) - padding,
                max(xs) + padding,
                max(ys) + padding,
            )
            services_path = Path(__file__).parents[1] / "data" / "kartverket_wcs.json"
            provider = KartverketProvider(
                load_services(services_path),
                self.settings_panel.cache_directory.text(),
                self.settings_panel.maximum_area.value(),
                int(self.settings_panel.maximum_tile_pixels.value() * 1_000_000),
                self.settings_panel.maximum_tiles.value(),
            )
            crs = self.terrain.crs
            plan = provider.plan_download(
                bounds,
                self.settings_panel.detail_resolution.value(),
                corridor=RouteCorridor(route_points, padding),
                auto_resolution=False,
                maximum_total_pixels=int(
                    self.settings_panel.maximum_total_pixels.value() * 1_000_000
                ),
            )
            include_dom = (
                QMessageBox.question(
                    self,
                    "Detailed route validation",
                    "Download DOM surface obstructions too?",
                    QMessageBox.StandardButton.Yes | QMessageBox.StandardButton.No,
                )
                == QMessageBox.StandardButton.Yes
            )
            products = 2 if include_dom else 1
            cached = provider.cached_tile_count("dtm", crs, plan)
            if include_dom:
                cached += provider.cached_tile_count("dom", crs, plan)
            estimate = (
                f"Final-route strip: {plan.summary(products)}. "
                f"Cache hits: {cached}/{len(plan.tiles) * products}."
            )
            self.settings_panel.download_estimate.setText(estimate)
            if (
                QMessageBox.question(
                    self,
                    "Detailed route download plan",
                    estimate + "\n\nDownload and revalidate every selected hop?",
                    QMessageBox.StandardButton.Yes | QMessageBox.StandardButton.No,
                )
                != QMessageBox.StandardButton.Yes
            ):
                return
            self._start_terrain_download(
                TerrainDownloadWorker(provider, crs, plan, include_dom),
                self._detail_download_finished,
            )
        except Exception as exc:
            QMessageBox.critical(self, "Detailed route validation", str(exc))

    def _start_terrain_download(self, worker: TerrainDownloadWorker, finished_slot: Any) -> None:
        if self.download_thread and self.download_thread.isRunning():
            raise RuntimeError("A terrain download is already running")
        self.download_worker = worker
        self.download_thread = QThread(self)
        worker.moveToThread(self.download_thread)
        self.download_thread.started.connect(worker.run)
        worker.progress.connect(self._optimization_progress)
        worker.finished.connect(finished_slot)
        worker.failed.connect(self._download_failed)
        worker.finished.connect(self.download_thread.quit)
        worker.failed.connect(self.download_thread.quit)
        self.download_thread.finished.connect(worker.deleteLater)
        self.download_thread.finished.connect(self.download_thread.deleteLater)
        self.download_thread.finished.connect(self._download_cleanup)
        self.progress_bar.show()
        self.download_thread.start()

    @Slot(object)
    def _download_finished(self, payload: tuple[list[str], list[str], DownloadPlan]) -> None:
        self.progress_bar.hide()
        try:
            dtm_paths, dom_paths, plan = payload
            self._clear_detail_terrain()
            self._clear_dtm_contours()
            if self.terrain:
                self.terrain.close()
            self.terrain = RasterTerrain(dtm_paths, dom_paths)
            self._set_transformers()
            if self.endpoint_a:
                self._set_endpoint(
                    "A", self.endpoint_a.latitude or 0, self.endpoint_a.longitude or 0
                )
            if self.endpoint_b:
                self._set_endpoint(
                    "B", self.endpoint_b.latitude or 0, self.endpoint_b.longitude or 0
                )
            self._refresh_metric_planning_sites()
            self.progress_label.setText(
                f"Kartverket terrain loaded: {len(plan.tiles)} tile(s) at "
                f"{plan.effective_resolution_m:g} m"
            )
        except Exception as exc:
            QMessageBox.critical(self, "Kartverket terrain", str(exc))

    def _refresh_metric_planning_sites(self) -> None:
        if not self.terrain:
            return
        for collection in (
            self.additional_clients,
            self.manual_routers,
            self.enabled_known_routers,
        ):
            for index, old in enumerate(list(collection)):
                if old.latitude is None or old.longitude is None:
                    continue
                replacement = self._metric_site(
                    old.id, old.latitude, old.longitude, old.kind
                )
                replacement.origin = old.origin
                replacement.required = old.required
                replacement.enabled = old.enabled
                replacement.locked = old.locked
                collection[index] = replacement
        self._refresh_sites_panel()

    @Slot(object)
    def _detail_download_finished(
        self, payload: tuple[list[str], list[str], DownloadPlan]
    ) -> None:
        self.progress_bar.hide()
        if not self.result or not self.result.found:
            return
        try:
            dtm_paths, dom_paths, plan = payload
            self._clear_detail_terrain()
            self.detail_terrain = RasterTerrain(dtm_paths, dom_paths)
            solution = self.result.active_solution
            route = solution.sites if solution else self.result.route
            x = np.array([site.x for site in route])
            y = np.array([site.y for site in route])
            ground = self.detail_terrain.sample(x, y)
            if not np.all(np.isfinite(ground)):
                raise ValueError("The detailed DTM does not cover every selected route site")
            surface = (
                self.detail_terrain.sample(x, y, surface=True)
                if self.detail_terrain.has_surface
                else np.full(len(route), np.nan)
            )
            for index, site in enumerate(route):
                site.ground_elevation_m = float(ground[index])
                site.surface_elevation_m = (
                    float(surface[index]) if np.isfinite(surface[index]) else None
                )
            evaluator = LinkEvaluator(self.detail_terrain, self.settings_panel.rf_settings())
            sample_step = max(
                self.detail_terrain.resolution_m,
                self.settings_panel.candidate_settings().final_sample_step_m,
            )
            if solution:
                by_id = {site.id: site for site in route}
                self.result.links = [
                    evaluator.evaluate(
                        by_id[link.source_id], by_id[link.target_id], sample_step
                    )
                    for link in solution.links
                ]
                solution.links = list(self.result.links)
            else:
                self.result.links = [
                    evaluator.evaluate(a, b, sample_step)
                    for a, b in zip(route, route[1:], strict=False)
                ]
            invalid = [link for link in self.result.links if not link.valid]
            self.detail_validation_note = (
                f"Detailed validation: {plan.effective_resolution_m:g} m DTM"
                + (" + DOM" if self.detail_terrain.has_surface else " (terrain only)")
                + f", {len(invalid)} invalid hop(s)"
            )
            self._display_result()
            self.progress_label.setText(self.detail_validation_note)
            if invalid:
                weakest = min(invalid, key=lambda link: link.worst_margin_db)
                QMessageBox.warning(
                    self,
                    "Detailed route validation",
                    f"The high-resolution strip invalidated {len(invalid)} hop(s). "
                    f"Weakest: {weakest.source_id} to {weakest.target_id} "
                    f"({weakest.worst_margin_db:.1f} dB margin).",
                )
        except Exception as exc:
            self._clear_detail_terrain()
            QMessageBox.critical(self, "Detailed route validation", str(exc))

    def _clear_detail_terrain(self) -> None:
        if self.detail_terrain:
            self.detail_terrain.close()
        self.detail_terrain = None
        self.detail_validation_note = None

    def _clear_dtm_contours(self) -> None:
        self.map_widget.clear_contours()
        if hasattr(self, "contour_action"):
            self.contour_action.setChecked(False)

    @Slot(bool)
    def toggle_dtm_contours(self, checked: bool) -> None:
        if not checked:
            self.map_widget.clear_contours()
            self.progress_label.setText("DTM contours hidden")
            return
        if not self.terrain:
            self.contour_action.setChecked(False)
            QMessageBox.information(self, "DTM contours", "Load DTM terrain first.")
            return
        interval_m, accepted = QInputDialog.getDouble(
            self,
            "DTM contours",
            "Contour interval (metres):",
            20.0,
            1.0,
            500.0,
            1,
        )
        if not accepted:
            self.contour_action.setChecked(False)
            return
        try:
            self.progress_label.setText("Generating DTM contours…")
            QApplication.processEvents()
            geojson = contour_geojson(self.terrain, interval_m)
            self.map_widget.set_contours(geojson, True)
            features = geojson.get("features", [])
            self.progress_label.setText(
                f"Showing {len(features)} contour levels at {interval_m:g} m intervals"
            )
        except Exception as exc:
            self.contour_action.setChecked(False)
            self.map_widget.clear_contours()
            QMessageBox.critical(self, "DTM contours", str(exc))

    @Slot(bool)
    def toggle_coverage(self, checked: bool) -> None:
        if not checked:
            self._coverage_restart = False
            if self.coverage_worker:
                self.coverage_worker.cancel()
            self.map_widget.clear_coverage()
            self.progress_label.setText("Predicted coverage hidden")
            return
        if not self.result or not self.result.active_solution or not self.terrain:
            self.coverage_action.setChecked(False)
            QMessageBox.information(
                self,
                "Predicted coverage",
                "Optimize a network first. Coverage is calculated over its terrain candidates.",
            )
            return
        if self.coverage_thread and self.coverage_thread.isRunning():
            self._coverage_restart = True
            if self.coverage_worker:
                self.coverage_worker.cancel()
            return
        self._coverage_restart = False
        self.map_widget.clear_coverage()
        self.progress_label.setText("Preparing coverage samples…")
        self.progress_bar.setRange(0, 0)
        self.progress_bar.show()
        self.coverage_worker = CoverageWorker(
            self.terrain,
            self.settings_panel.rf_settings(),
            self.settings_panel.candidate_settings(),
            list(self.result.active_solution.sites),
            list(self.result.candidates),
        )
        self.coverage_thread = QThread(self)
        self.coverage_worker.moveToThread(self.coverage_thread)
        self.coverage_thread.started.connect(self.coverage_worker.run)
        self.coverage_worker.progress.connect(self._optimization_progress)
        self.coverage_worker.finished.connect(self._coverage_finished)
        self.coverage_worker.failed.connect(self._coverage_failed)
        self.coverage_worker.finished.connect(self.coverage_thread.quit)
        self.coverage_worker.failed.connect(self.coverage_thread.quit)
        self.coverage_thread.finished.connect(self.coverage_worker.deleteLater)
        self.coverage_thread.finished.connect(self.coverage_thread.deleteLater)
        self.coverage_thread.finished.connect(self._coverage_cleanup)
        self.coverage_thread.start()

    @Slot(object)
    def _coverage_finished(self, links: list[LinkResult]) -> None:
        if not self.coverage_action.isChecked() or self._coverage_restart:
            return
        self.progress_bar.hide()
        self._display_coverage_links(links)

    def _display_coverage_links(self, links: list[LinkResult]) -> None:
        if not self.result or not self.result.active_solution:
            return
        solution = self.result.active_solution
        selected_ids = {site.id for site in solution.sites}
        by_id = {site.id: site for site in self.result.candidates}
        samples: dict[str, dict[str, object]] = {}
        colors = ["#1976d2", "#f28c28", "#2a9d8f", "#e76f51", "#8e44ad", "#6a994e"]
        source_colors = {
            site.id: colors[index % len(colors)]
            for index, site in enumerate(solution.sites)
        }
        for link in links:
            if not link.valid:
                continue
            for source_id, target_id in (
                (link.source_id, link.target_id),
                (link.target_id, link.source_id),
            ):
                if source_id not in selected_ids or target_id in selected_ids:
                    continue
                target = by_id.get(target_id)
                if target is None:
                    continue
                sample = samples.setdefault(
                    target_id,
                    {
                        "sources": [],
                        "margins": [],
                        "color": source_colors[source_id],
                        "site": target,
                    },
                )
                sources = sample["sources"]
                margins = sample["margins"]
                if isinstance(sources, list) and source_id not in sources:
                    sources.append(source_id)
                if isinstance(margins, list):
                    margins.append(link.worst_margin_db)
        points: list[dict[str, object]] = []
        for sample in samples.values():
            site = sample["site"]
            if not isinstance(site, Site):
                continue
            latitude, longitude = self._lat_lon(site)
            margins = sample["margins"]
            points.append(
                {
                    "lat": latitude,
                    "lon": longitude,
                    "sources": sample["sources"],
                    "margin": min(margins) if isinstance(margins, list) and margins else 0,
                    "color": sample["color"],
                }
            )
        self.map_widget.set_coverage(points)
        overlap_count = sum(len(point["sources"]) > 1 for point in points)  # type: ignore[arg-type]
        self.progress_label.setText(
            f"Showing {len(points)} evaluated coverage samples; "
            f"{overlap_count} overlap candidates highlighted"
        )

    @Slot(str)
    def _coverage_failed(self, message: str) -> None:
        self.progress_bar.hide()
        self.coverage_action.setChecked(False)
        self.map_widget.clear_coverage()
        QMessageBox.critical(self, "Coverage calculation failed", message)

    @Slot()
    def _coverage_cleanup(self) -> None:
        restart = self._coverage_restart and self.coverage_action.isChecked()
        self.coverage_worker = None
        self.coverage_thread = None
        self._coverage_restart = False
        if restart:
            self.toggle_coverage(True)

    @Slot(str)
    def _download_failed(self, message: str) -> None:
        self.progress_bar.hide()
        QMessageBox.critical(self, "Kartverket download failed", message)

    @Slot()
    def _download_cleanup(self) -> None:
        self.download_worker = None
        self.download_thread = None

    def optimize(self) -> None:
        clients = self._all_clients()
        if len(clients) < 2:
            QMessageBox.information(self, "Optimize", "Add at least two clients first.")
            return
        if self.worker_thread and self.worker_thread.isRunning():
            return
        if not self.terrain or not self._terrain_covers_planning_sites():
            if self.settings_panel.auto_terrain.isChecked():
                self._ensure_terrain_then_optimize()
            else:
                QMessageBox.information(
                    self,
                    "Optimize",
                    "Terrain does not cover every planning site. Use Terrain → Prepare/download.",
                )
            return
        self._start_optimization()

    def _terrain_covers_planning_sites(self) -> bool:
        if not self.terrain:
            return False
        try:
            from pyproj import Transformer

            transform = Transformer.from_crs("EPSG:4326", self.terrain.crs, always_xy=True)
            points = [
                transform.transform(site.longitude, site.latitude)
                for site in [*self._all_clients(), *self._required_routers()]
                if site.longitude is not None and site.latitude is not None
            ]
            if not points:
                return False
            x, y = zip(*points, strict=True)
            return bool(np.all(np.isfinite(self.terrain.sample(np.asarray(x), np.asarray(y)))))
        except Exception:
            return False

    def _ensure_terrain_then_optimize(self) -> None:
        try:
            provider, crs, plan = self._terrain_download_plan()
            dtm_cached = provider.cached_tile_count("dtm", crs, plan)
            dom_cached = provider.cached_tile_count("dom", crs, plan)
            tile_count = len(plan.tiles)
            if dtm_cached == tile_count and dom_cached == tile_count:
                payload = (
                    [
                        str(provider.cache_path("dtm", crs, tile.bounds, plan.effective_resolution_m))
                        for tile in plan.tiles
                    ],
                    [
                        str(provider.cache_path("dom", crs, tile.bounds, plan.effective_resolution_m))
                        for tile in plan.tiles
                    ],
                    plan,
                )
                self._download_finished(payload)
                self.progress_label.setText(
                    f"Loaded {tile_count} cached terrain tile(s); starting optimization…"
                )
                self._start_optimization()
                return
            missing = tile_count * 2 - dtm_cached - dom_cached
            message = (
                f"Terrain is missing for part of this plan. Download {missing} tile product(s) "
                f"and then optimize?\n\n{plan.summary(2)}"
            )
            if (
                QMessageBox.question(
                    self,
                    "Prepare terrain",
                    message,
                    QMessageBox.StandardButton.Yes | QMessageBox.StandardButton.No,
                )
                != QMessageBox.StandardButton.Yes
            ):
                return
            self._start_terrain_download(
                TerrainDownloadWorker(provider, crs, plan, True),
                self._download_finished_then_optimize,
            )
        except Exception as exc:
            QMessageBox.critical(self, "Prepare terrain", str(exc))

    @Slot(object)
    def _download_finished_then_optimize(
        self, payload: tuple[list[str], list[str], DownloadPlan]
    ) -> None:
        self._download_finished(payload)
        if self.terrain:
            self._start_optimization()

    def _start_optimization(self) -> None:
        if not self.terrain:
            return
        clients = self._all_clients()
        if len(clients) < 2:
            return
        self._clear_detail_terrain()
        self.progress_label.setText("Starting optimization…")
        self.progress_bar.setRange(0, 0)
        self.progress_bar.show()
        self.optimize_action.setEnabled(False)
        rf = self.settings_panel.rf_settings()
        for index, client in enumerate(clients):
            client.antenna_height_m = (
                rf.endpoint_a.height_agl_m if index != 1 else rf.endpoint_b.height_agl_m
            )
        optimizer = RouteOptimizer(self.terrain, rf, self.settings_panel.candidate_settings())
        self.worker = OptimizationWorker(
            optimizer,
            clients[0],
            clients[1],
            clients,
            self._required_routers(),
        )
        self.worker_thread = QThread(self)
        self.worker.moveToThread(self.worker_thread)
        self.worker_thread.started.connect(self.worker.run)
        self.worker.progress.connect(self._optimization_progress)
        self.worker.finished.connect(self._optimization_finished)
        self.worker.failed.connect(self._optimization_failed)
        self.worker.finished.connect(self.worker_thread.quit)
        self.worker.failed.connect(self.worker_thread.quit)
        self.worker_thread.finished.connect(self.worker.deleteLater)
        self.worker_thread.finished.connect(self.worker_thread.deleteLater)
        self.worker_thread.finished.connect(self._optimization_cleanup)
        self.worker_thread.start()

    @Slot(str, int, int)
    def _optimization_progress(self, stage: str, done: int, total: int) -> None:
        self.progress_label.setText(stage)
        self.progress_bar.setRange(0, max(total, 1))
        self.progress_bar.setValue(done)

    @Slot(object)
    def _optimization_finished(self, result: OptimizationResult) -> None:
        self.progress_bar.hide()
        self.result = result
        self.solution_selector.blockSignals(True)
        self.solution_selector.clear()
        for solution in result.alternatives:
            resilience = (
                f", {solution.achieved_path_count} independent path(s)"
                if solution.requested_path_count > 1
                else ""
            )
            self.solution_selector.addItem(solution.name + resilience)
        if result.alternatives:
            self.solution_selector.setCurrentIndex(result.active_solution_index)
            self.solution_selector.show()
        else:
            self.solution_selector.hide()
        self.solution_selector.blockSignals(False)
        self._display_result()
        if self.coverage_action.isChecked():
            self.toggle_coverage(True)

    @Slot(int)
    def _solution_changed(self, index: int) -> None:
        if not self.result or not 0 <= index < len(self.result.alternatives):
            return
        self.result.select_solution(index)
        self._display_result()
        if self.coverage_action.isChecked():
            self.toggle_coverage(True)

    @Slot(str)
    def _optimization_failed(self, message: str) -> None:
        self.progress_bar.hide()
        self.progress_label.setText("Optimization failed")
        QMessageBox.critical(self, "Optimization failed", message)

    @Slot()
    def _optimization_cleanup(self) -> None:
        self.worker = None
        self.worker_thread = None
        self.optimize_action.setEnabled(True)

    def cancel_optimization(self) -> None:
        if self.worker:
            self.worker.cancel()
            self.progress_label.setText("Cancelling…")

    def _display_result(self) -> None:
        if not self.result:
            return
        if not self.result.found:
            self.summary.setText("\n".join(self.result.diagnostics))
            self.progress_label.setText("No route found")
            return
        solution = self.result.active_solution
        margins = [link.worst_margin_db for link in self.result.links]
        lengths = [link.distance_m for link in self.result.links]
        fresnel = [link.minimum_fresnel_clearance_ratio for link in self.result.links]
        if solution:
            self.summary.setText(
                "MESH SOLUTION\n"
                f"Clients: {len(solution.client_ids)}  |  Routers: {solution.router_count}  |  "
                f"Independent paths: {solution.achieved_path_count}/{solution.requested_path_count}  |  "
                f"Viable selected-node links: {len(solution.links)}\n"
                f"Worst margin: {min(margins):.1f} dB  |  Longest link: {max(lengths) / 1000:.2f} km  |  "
                f"Minimum Fresnel: {100 * min(fresnel):.0f}%"
                + (f"\n{self.detail_validation_note}" if self.detail_validation_note else "")
                + (f"\n{' '.join(solution.diagnostics)}" if solution.diagnostics else "")
            )
        else:
            self.summary.setText(
                "ROUTE FOUND\n"
                f"Routers required: {self.result.router_count}  |  "
                f"{' → '.join(site.id for site in self.result.route)}\n"
                f"Worst margin: {min(margins):.1f} dB  |  Longest hop: {max(lengths) / 1000:.2f} km  |  "
                f"Minimum Fresnel: {100 * min(fresnel):.0f}%  |  Total route: {sum(lengths) / 1000:.2f} km"
            )
        self.results_table.setRowCount(len(self.result.links))
        for row, link in enumerate(self.result.links):
            values = [
                str(row + 1),
                link.source_id,
                link.target_id,
                f"{link.distance_m / 1000:.2f} km",
                f"{link.forward.fspl_db:.1f} dB",
                f"{link.diffraction_loss_db:.1f} dB",
                f"{link.forward.received_power_dbm:.1f} dBm",
                f"{link.forward.raw_margin_db:.1f} dB",
                f"{link.forward.usable_margin_db:.1f} dB",
                f"{link.reverse.usable_margin_db:.1f} dB",
                f"{100 * link.minimum_fresnel_clearance_ratio:.0f}%",
                "Clear" if link.los_clear else "Obstructed",
                "Valid" if link.valid else "Invalid",
            ]
            for column, value in enumerate(values):
                self.results_table.setItem(row, column, QTableWidgetItem(value))
        self._update_map_result()
        self._show_candidates(self.settings_panel.show_candidates.isChecked())
        self.progress_label.setText(f"Route found in {self.result.elapsed_seconds:.2f} seconds")
        if self.result.links:
            self._link_selected(0)

    def _lat_lon(self, site: Site) -> tuple[float, float]:
        if site.latitude is not None and site.longitude is not None:
            return site.latitude, site.longitude
        if not self._reverse:
            return site.y, site.x
        lon, lat = self._reverse.transform(site.x, site.y)
        site.latitude, site.longitude = lat, lon
        return lat, lon

    def _update_map_result(self) -> None:
        if not self.result or not self.result.found:
            return
        solution = self.result.active_solution
        displayed_sites = solution.sites if solution else self.result.route
        point_data, by_id = [], {}
        for site in displayed_sites:
            lat, lon = self._lat_lon(site)
            by_id[site.id] = (lat, lon)
            role = (
                "client"
                if solution and site.id in solution.client_ids
                else "manual"
                if site.origin == SiteOrigin.MANUAL
                else "known"
                if site.origin == SiteOrigin.KNOWN
                else "router"
            )
            label = site.id
            if solution and site.id in solution.router_ids:
                label = f"R{solution.router_ids.index(site.id) + 1}"
            point_data.append(
                {
                    "id": site.id,
                    "label": label,
                    "lat": lat,
                    "lon": lon,
                    "role": role,
                    "draggable": site.origin != SiteOrigin.KNOWN,
                }
            )
        link_data: list[dict[str, object]] = []
        for index, link in enumerate(self.result.links):
            a_lat, a_lon = by_id[link.source_id]
            b_lat, b_lon = by_id[link.target_id]
            status = (
                "strong"
                if link.worst_margin_db >= 10
                else "acceptable"
                if link.worst_margin_db >= 0
                else "invalid"
            )
            link_data.append(
                {
                    "index": index,
                    "from": link.source_id,
                    "to": link.target_id,
                    "a_lat": a_lat,
                    "a_lon": a_lon,
                    "b_lat": b_lat,
                    "b_lon": b_lon,
                    "margin": link.worst_margin_db,
                    "valid": link.valid,
                    "status": status,
                }
            )
        backbone_keys: set[frozenset[str]] = set()
        if solution:
            for paths in solution.client_paths.values():
                for path in paths:
                    backbone_keys.update(
                        frozenset((left, right))
                        for left, right in zip(path, path[1:], strict=False)
                    )
        backbone = [
            item
            for item, link in zip(link_data, self.result.links, strict=True)
            if not solution
            or frozenset((link.source_id, link.target_id)) in backbone_keys
        ]
        mesh = [item for item in link_data if item not in backbone]
        self.map_widget.set_network(point_data, backbone, mesh)

    @Slot(bool)
    def _show_candidates(self, visible: bool) -> None:
        if not self.result:
            return
        selected_sites = (
            self.result.active_solution.sites
            if self.result.active_solution
            else self.result.route
        )
        selected = {id(site) for site in selected_sites}
        points = [
            {"id": site.id, "lat": self._lat_lon(site)[0], "lon": self._lat_lon(site)[1]}
            for site in self.result.candidates
            if id(site) not in selected and site.kind == SiteKind.CANDIDATE
        ]
        self.map_widget.set_candidates(points, visible)

    @Slot(int)
    def _link_selected(self, index: int) -> None:
        if self.result and 0 <= index < len(self.result.links):
            self.profile.show_link(
                self.result.links[index],
                self.settings_panel.rf_settings().required_fresnel_clearance,
            )

    def _add_manual_router(self, latitude: float, longitude: float) -> None:
        site_id = f"M{len(self.manual_routers) + 1}"
        site = self._metric_site(site_id, latitude, longitude, SiteKind.ROUTER)
        site.origin = SiteOrigin.MANUAL
        site.required = True
        site.locked = True
        self.manual_routers.append(site)
        self.map_widget.set_point(site_id, latitude, longitude, role="manual")
        self._refresh_sites_panel()
        self.progress_label.setText(f"Added required router {site_id}; optimize to include it")

    def remove_selected_site(self) -> None:
        site_id = self.selected_site_id or self.settings_panel.selected_site_id()
        if not site_id:
            return
        if site_id == "A":
            self.endpoint_a = None
        elif site_id == "B":
            self.endpoint_b = None
        else:
            for collection in (
                self.additional_clients,
                self.manual_routers,
                self.enabled_known_routers,
            ):
                collection[:] = [site for site in collection if site.id != site_id]
        self.selected_site_id = None
        self.result = None
        self._redraw_planning_sites()
        self._refresh_sites_panel()

    def _redraw_planning_sites(self) -> None:
        self.map_widget.clear()
        for site in self._all_clients():
            if site.latitude is not None and site.longitude is not None:
                self.map_widget.set_point(
                    site.id, site.latitude, site.longitude, role="client"
                )
        for site in self.manual_routers:
            if site.latitude is not None and site.longitude is not None:
                self.map_widget.set_point(
                    site.id, site.latitude, site.longitude, role="manual"
                )
        for site in self.enabled_known_routers:
            if site.latitude is not None and site.longitude is not None:
                self.map_widget.set_point(
                    site.id,
                    site.latitude,
                    site.longitude,
                    False,
                    "known",
                )
        self._render_known_routers()

    def delete_selected_router(self) -> None:
        if any(site.id == self.selected_site_id for site in self.manual_routers):
            self.remove_selected_site()
            return
        if not self.result or not self.selected_site_id:
            return
        site = next((item for item in self.result.route if item.id == self.selected_site_id), None)
        if site and site.kind == SiteKind.ROUTER:
            self.result.route.remove(site)
            self._rename_routers()
            self.selected_site_id = None
            self._recalculate_manual_route()

    def toggle_selected_lock(self) -> None:
        if not self.result or not self.selected_site_id:
            return
        site = next((item for item in self.result.route if item.id == self.selected_site_id), None)
        if site and site.kind == SiteKind.ROUTER:
            site.locked = not site.locked
            self.progress_label.setText(f"{site.id} {'locked' if site.locked else 'unlocked'}")

    def reoptimize_unlocked(self) -> None:
        if not self.result or not self.result.found or not self.terrain:
            return
        optimizer = RouteOptimizer(
            self.terrain,
            self.settings_panel.rf_settings(),
            self.settings_panel.candidate_settings(),
        )
        for index in range(1, len(self.result.route) - 1):
            if not self.result.route[index].locked:
                self.result.route[index] = optimizer._refine_site(
                    self.result.route[index - 1],
                    self.result.route[index],
                    self.result.route[index + 1],
                )
        self._rename_routers()
        self._recalculate_manual_route()

    def copy_coordinates(self) -> None:
        if not self.result or not self.selected_site_id:
            return
        site = next((item for item in self.result.route if item.id == self.selected_site_id), None)
        if site:
            lat, lon = self._lat_lon(site)
            from PySide6.QtWidgets import QApplication

            QApplication.clipboard().setText(f"{lat:.6f}, {lon:.6f}")
            self.progress_label.setText(f"Copied {site.id}: {lat:.6f}, {lon:.6f}")

    def _rename_routers(self) -> None:
        if self.result:
            for index, site in enumerate(self.result.route[1:-1], 1):
                site.id = f"R{index}"

    def _recalculate_manual_route(self) -> None:
        if not self.result or not self.terrain:
            return
        self._clear_detail_terrain()
        evaluator = RouteOptimizer(
            self.terrain,
            self.settings_panel.rf_settings(),
            self.settings_panel.candidate_settings(),
        ).evaluator
        try:
            self.result.links = [
                evaluator.evaluate(
                    a, b, self.settings_panel.candidate_settings().final_sample_step_m
                )
                for a, b in zip(self.result.route, self.result.route[1:], strict=False)
            ]
            self._display_result()
        except ValueError as exc:
            QMessageBox.warning(self, "Route recalculation", str(exc))

    def new_project(self) -> None:
        self._clear_detail_terrain()
        self._clear_dtm_contours()
        self.endpoint_a = self.endpoint_b = None
        self.additional_clients.clear()
        self.manual_routers.clear()
        self.enabled_known_routers.clear()
        self.known_repeaters.clear()
        self.result, self.project_path = None, None
        self.map_widget.clear()
        self.results_table.setRowCount(0)
        self.solution_selector.hide()
        self._refresh_sites_panel()
        self.summary.setText("Add two or more clients, then click Optimize.")

    def _current_project(self) -> Project:
        terrain_settings = TerrainSettings(
            cache_directory=self.settings_panel.cache_directory.text(),
            requested_resolution_m=self.settings_panel.download_resolution.value(),
            auto_resolution=self.settings_panel.auto_resolution.isChecked(),
            maximum_total_pixels=int(self.settings_panel.maximum_total_pixels.value() * 1_000_000),
            maximum_download_area_km2=self.settings_panel.maximum_area.value(),
            maximum_pixels_per_tile=int(
                self.settings_panel.maximum_tile_pixels.value() * 1_000_000
            ),
            maximum_download_tiles=self.settings_panel.maximum_tiles.value(),
            detail_resolution_m=self.settings_panel.detail_resolution.value(),
            detail_corridor_width_m=self.settings_panel.detail_corridor.value() * 1000,
        )
        if self.terrain:
            terrain_settings.dtm_paths = [dataset.name for dataset in self.terrain._dtm]
            terrain_settings.dom_paths = [dataset.name for dataset in self.terrain._dom]
        selected_optimized: list[Site] = []
        if self.result and self.result.active_solution:
            selected_optimized = [
                site
                for site in self.result.active_solution.sites
                if site.id in self.result.active_solution.router_ids
                and site.origin == SiteOrigin.OPTIMIZED
            ]
        elif self.result and self.result.found:
            selected_optimized = self.result.route[1:-1]
        return Project(
            self.endpoint_a,
            self.endpoint_b,
            selected_optimized,
            self.settings_panel.rf_settings(),
            terrain_settings,
            self.settings_panel.candidate_settings(),
            [],
            list(self.additional_clients),
            list(self.manual_routers),
            list(self.enabled_known_routers),
        )

    def save_project(self) -> None:
        if not self.project_path:
            self.save_project_as()
            return
        save_project(self._current_project(), self.project_path)
        self.progress_label.setText(f"Saved {self.project_path.name}")

    def save_project_as(self) -> None:
        path, _ = QFileDialog.getSaveFileName(
            self,
            "Save project",
            str(self._project_directory() / "Untitled.rfplan.json"),
            "RF plan (*.rfplan.json)",
        )
        if path:
            self.project_path = Path(path)
            self.save_project()

    def open_project(self) -> None:
        path, _ = QFileDialog.getOpenFileName(
            self,
            "Open project",
            str(self._project_directory()),
            "RF plan (*.rfplan.json *.json)",
        )
        if not path:
            return
        try:
            project = load_project(path)
            self.project_path = Path(path)
            self.settings_panel.set_rf_settings(project.rf_settings)
            self.settings_panel.set_candidate_settings(project.candidate_settings)
            self.settings_panel.cache_directory.setText(project.terrain_settings.cache_directory)
            self.settings_panel.download_resolution.setValue(
                project.terrain_settings.requested_resolution_m
            )
            self.settings_panel.auto_resolution.setChecked(project.terrain_settings.auto_resolution)
            self.settings_panel.maximum_total_pixels.setValue(
                project.terrain_settings.maximum_total_pixels / 1_000_000
            )
            self.settings_panel.maximum_area.setValue(
                project.terrain_settings.maximum_download_area_km2
            )
            self.settings_panel.maximum_tile_pixels.setValue(
                project.terrain_settings.maximum_pixels_per_tile / 1_000_000
            )
            self.settings_panel.maximum_tiles.setValue(
                project.terrain_settings.maximum_download_tiles
            )
            self.settings_panel.detail_resolution.setValue(
                project.terrain_settings.detail_resolution_m
            )
            self.settings_panel.detail_corridor.setValue(
                project.terrain_settings.detail_corridor_width_m / 1000
            )
            self._clear_detail_terrain()
            self._clear_dtm_contours()
            self.map_widget.clear()
            self.result = None
            self.results_table.setRowCount(0)
            self.solution_selector.hide()
            self.additional_clients = list(project.additional_clients)
            self.manual_routers = list(project.manual_routers)
            self.enabled_known_routers = list(project.known_routers)
            if self.terrain:
                self.terrain.close()
                self.terrain = None
            if project.terrain_settings.dtm_paths:
                self.terrain = RasterTerrain(
                    project.terrain_settings.dtm_paths, project.terrain_settings.dom_paths
                )
                self._set_transformers()
            self.endpoint_a, self.endpoint_b = project.endpoint_a, project.endpoint_b
            for site in self._all_clients():
                if site and site.latitude is not None and site.longitude is not None:
                    self.map_widget.set_point(
                        site.id, site.latitude, site.longitude, role="client"
                    )
            for site in self.manual_routers:
                if site.latitude is not None and site.longitude is not None:
                    self.map_widget.set_point(
                        site.id, site.latitude, site.longitude, role="manual"
                    )
            for site in self.enabled_known_routers:
                if site.latitude is not None and site.longitude is not None:
                    self.map_widget.set_point(
                        site.id,
                        site.latitude,
                        site.longitude,
                        False,
                        "known",
                    )
            if self.terrain and self.endpoint_a and self.endpoint_b and project.selected_routers:
                route = [self.endpoint_a, *project.selected_routers, self.endpoint_b]
                self.result = OptimizationResult(route, [], route, [], elapsed_seconds=0)
                self._rename_routers()
                self._recalculate_manual_route()
            self._refresh_sites_panel()
            self.progress_label.setText(f"Opened {self.project_path.name}")
        except Exception as exc:
            QMessageBox.critical(self, "Open project", str(exc))

    @staticmethod
    def _project_directory() -> Path:
        documents = QStandardPaths.writableLocation(
            QStandardPaths.StandardLocation.DocumentsLocation
        )
        directory = Path(documents or Path.home()) / "RF Router Planner"
        directory.mkdir(parents=True, exist_ok=True)
        return directory

    def export_csv(self) -> None:
        if self.result and self.result.found:
            path, _ = QFileDialog.getSaveFileName(self, "Export route CSV", "", "CSV (*.csv)")
            if path:
                export_route_csv(self.result, path)

    def export_geojson(self) -> None:
        if self.result and self.result.found:
            path, _ = QFileDialog.getSaveFileName(
                self, "Export route GeoJSON", "", "GeoJSON (*.geojson)"
            )
            if path:
                reverse = self._reverse
                converter = (lambda x, y: reverse.transform(x, y)) if reverse else None
                export_route_geojson(self.result, path, converter)

    def closeEvent(self, event) -> None:  # type: ignore[no-untyped-def]
        self.cancel_optimization()
        if self.coverage_worker:
            self.coverage_worker.cancel()
        if self.worker_thread and self.worker_thread.isRunning():
            self.worker_thread.quit()
            self.worker_thread.wait(2000)
        if self.download_thread and self.download_thread.isRunning():
            self.download_thread.quit()
            self.download_thread.wait(2000)
        if self.corescope_thread and self.corescope_thread.isRunning():
            self.corescope_thread.quit()
            self.corescope_thread.wait(2000)
        if self.coverage_thread and self.coverage_thread.isRunning():
            self.coverage_thread.quit()
            self.coverage_thread.wait(2000)
        if self.terrain:
            self.terrain.close()
        self._clear_detail_terrain()
        event.accept()
