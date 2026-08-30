from __future__ import annotations

import logging
from pathlib import Path
from threading import Event
from typing import Any

import numpy as np
from PySide6.QtCore import QObject, Qt, QThread, Signal, Slot
from PySide6.QtGui import QAction
from PySide6.QtWidgets import (
    QApplication,
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
from rf_router_planner.models.settings import TerrainSettings
from rf_router_planner.models.site import Site, SiteKind
from rf_router_planner.optimization.optimizer import OptimizationResult, RouteOptimizer
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

    def __init__(self, optimizer: RouteOptimizer, endpoint_a: Site, endpoint_b: Site) -> None:
        super().__init__()
        self.optimizer, self.endpoint_a, self.endpoint_b = optimizer, endpoint_a, endpoint_b
        self.cancel_event = Event()

    @Slot()
    def run(self) -> None:
        try:
            result = self.optimizer.optimize(
                self.endpoint_a,
                self.endpoint_b,
                progress=lambda stage, done, total: self.progress.emit(stage, done, total),
                cancelled=self.cancel_event.is_set,
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
        self.result: OptimizationResult | None = None
        self.project_path: Path | None = None
        self.worker: OptimizationWorker | None = None
        self.worker_thread: QThread | None = None
        self.download_worker: TerrainDownloadWorker | None = None
        self.download_thread: QThread | None = None
        self.selected_site_id: str | None = None
        self._forward: Any | None = None
        self._reverse: Any | None = None
        self._build_ui()

    def _build_ui(self) -> None:
        self.settings_panel = SettingsPanel()
        self.settings_panel.coordinates_applied.connect(self._coordinates_applied)
        self.settings_panel.show_candidates.toggled.connect(self._show_candidates)
        self.map_widget = MapWidget()
        self.map_widget.bridge.clicked.connect(self._map_clicked)
        self.map_widget.bridge.moved.connect(self._marker_moved)
        self.map_widget.bridge.link_selected.connect(self._link_selected)
        self.map_widget.bridge.site_selected.connect(self._site_selected)
        self.profile = TerrainProfileWidget()
        self.results_table = self._create_results_table()
        self.summary = QLabel("Load DTM terrain, then place endpoints A and B.")
        self.summary.setWordWrap(True)
        results_page = QWidget()
        results_layout = QVBoxLayout(results_page)
        results_layout.addWidget(self.summary)
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
        toolbar = QToolBar("Project")
        toolbar.setMovable(False)
        self.addToolBar(toolbar)
        actions = [
            ("New", self.new_project),
            ("Open", self.open_project),
            ("Save", self.save_project),
            ("Save As", self.save_project_as),
            ("Load terrain", self.load_terrain),
            ("Download terrain", self.download_terrain),
            ("Validate route detail", self.download_route_detail),
            ("Set A", lambda: self.map_widget.set_mode("A")),
            ("Set B", lambda: self.map_widget.set_mode("B")),
            ("Add router", lambda: self.map_widget.set_mode("router")),
            ("Delete router", self.delete_selected_router),
            ("Lock/unlock", self.toggle_selected_lock),
            ("Re-optimize unlocked", self.reoptimize_unlocked),
            ("Copy coordinates", self.copy_coordinates),
            ("DTM contours", self.toggle_dtm_contours),
            ("Optimize", self.optimize),
            ("Cancel", self.cancel_optimization),
            ("Export CSV", self.export_csv),
            ("Export GeoJSON", self.export_geojson),
        ]
        for label, callback in actions:
            action = QAction(label, self)
            if label == "DTM contours":
                action.setCheckable(True)
                self.contour_action = action
            action.triggered.connect(callback)
            toolbar.addAction(action)
            if label == "Optimize":
                self.optimize_action = action

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
        self.progress_label.setText(f"Endpoint {endpoint}: {latitude:.6f}, {longitude:.6f}")

    @Slot(str, float, float)
    def _marker_moved(self, site_id: str, latitude: float, longitude: float) -> None:
        try:
            if site_id in {"A", "B"}:
                self._set_endpoint(site_id, latitude, longitude)
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
        self.progress_label.setText(f"Selected {site_id}")

    def load_terrain(self) -> None:
        dtm_paths, _ = QFileDialog.getOpenFileNames(
            self, "Load DTM GeoTIFF tile(s)", "", "GeoTIFF (*.tif *.tiff)"
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
            message = f"Loaded {len(dtm_paths)} DTM tile(s) in {self.terrain.crs}"
            if not self.terrain.has_surface:
                message += " — Surface obstruction data unavailable — terrain only"
            self.progress_label.setText(message)
        except Exception as exc:
            QMessageBox.critical(self, "Terrain load failed", str(exc))

    def download_terrain(self) -> None:
        if not self.endpoint_a or not self.endpoint_b:
            QMessageBox.information(self, "Kartverket terrain", "Set endpoints A and B first.")
            return
        try:
            from pyproj import Transformer

            longitude = ((self.endpoint_a.longitude or 0) + (self.endpoint_b.longitude or 0)) / 2
            crs = f"EPSG:{norway_utm_epsg(longitude)}"
            transform = Transformer.from_crs("EPSG:4326", crs, always_xy=True)
            ax, ay = transform.transform(self.endpoint_a.longitude, self.endpoint_a.latitude)
            bx, by = transform.transform(self.endpoint_b.longitude, self.endpoint_b.latitude)
            padding = self.settings_panel.corridor.value() * 1000
            bounds = (
                min(ax, bx) - padding,
                min(ay, by) - padding,
                max(ax, bx) + padding,
                max(ay, by) + padding,
            )
            services_path = Path(__file__).parents[1] / "data" / "kartverket_wcs.json"
            provider = KartverketProvider(
                load_services(services_path),
                self.settings_panel.cache_directory.text(),
                self.settings_panel.maximum_area.value(),
                int(self.settings_panel.maximum_tile_pixels.value() * 1_000_000),
                self.settings_panel.maximum_tiles.value(),
            )
            include_dom = (
                QMessageBox.question(
                    self,
                    "Kartverket terrain",
                    "Download DOM surface data too?",
                    QMessageBox.StandardButton.Yes | QMessageBox.StandardButton.No,
                )
                == QMessageBox.StandardButton.Yes
            )
            plan = provider.plan_download(
                bounds,
                self.settings_panel.download_resolution.value(),
                corridor=(ax, ay, bx, by, padding),
                auto_resolution=self.settings_panel.auto_resolution.isChecked(),
                maximum_total_pixels=int(
                    self.settings_panel.maximum_total_pixels.value() * 1_000_000
                ),
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
            route_points = tuple((site.x, site.y) for site in self.result.route)
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
            self.progress_label.setText(
                f"Kartverket terrain loaded: {len(plan.tiles)} tile(s) at "
                f"{plan.effective_resolution_m:g} m"
            )
        except Exception as exc:
            QMessageBox.critical(self, "Kartverket terrain", str(exc))

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
            route = self.result.route
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

    @Slot(str)
    def _download_failed(self, message: str) -> None:
        self.progress_bar.hide()
        QMessageBox.critical(self, "Kartverket download failed", message)

    @Slot()
    def _download_cleanup(self) -> None:
        self.download_worker = None
        self.download_thread = None

    def optimize(self) -> None:
        if not self.terrain or not self.endpoint_a or not self.endpoint_b:
            QMessageBox.information(
                self, "Optimize", "Load terrain and set endpoints A and B first."
            )
            return
        if self.worker_thread and self.worker_thread.isRunning():
            return
        self._clear_detail_terrain()
        self.progress_label.setText("Starting optimization…")
        self.progress_bar.setRange(0, 0)
        self.progress_bar.show()
        self.optimize_action.setEnabled(False)
        rf = self.settings_panel.rf_settings()
        self.endpoint_a.antenna_height_m = rf.endpoint_a.height_agl_m
        self.endpoint_b.antenna_height_m = rf.endpoint_b.height_agl_m
        optimizer = RouteOptimizer(self.terrain, rf, self.settings_panel.candidate_settings())
        self.worker = OptimizationWorker(optimizer, self.endpoint_a, self.endpoint_b)
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
        self._display_result()

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
        margins = [link.worst_margin_db for link in self.result.links]
        lengths = [link.distance_m for link in self.result.links]
        fresnel = [link.minimum_fresnel_clearance_ratio for link in self.result.links]
        self.summary.setText(
            "ROUTE FOUND\n"
            f"Routers required: {self.result.router_count}  |  "
            f"{' → '.join(site.id for site in self.result.route)}\n"
            f"Worst margin: {min(margins):.1f} dB  |  Longest hop: {max(lengths) / 1000:.2f} km  |  "
            f"Minimum Fresnel: {100 * min(fresnel):.0f}%  |  Total route: {sum(lengths) / 1000:.2f} km"
            + (f"\n{self.detail_validation_note}" if self.detail_validation_note else "")
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
        point_data, by_id = [], {}
        for site in self.result.route:
            lat, lon = self._lat_lon(site)
            by_id[site.id] = (lat, lon)
            if site.kind == SiteKind.ROUTER:
                point_data.append({"id": site.id, "lat": lat, "lon": lon})
        link_data = []
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
        self.map_widget.set_route(point_data, link_data)
        if self.endpoint_a:
            self.map_widget.set_point("A", *self._lat_lon(self.endpoint_a))
        if self.endpoint_b:
            self.map_widget.set_point("B", *self._lat_lon(self.endpoint_b))

    @Slot(bool)
    def _show_candidates(self, visible: bool) -> None:
        if not self.result:
            return
        selected = {id(site) for site in self.result.route}
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
        if not self.result or not self.result.found or not self.terrain:
            QMessageBox.information(
                self, "Manual router", "Run an optimization before adding a router."
            )
            return
        result = self.result
        site = self._metric_site("manual", latitude, longitude, SiteKind.ROUTER)

        def segment_distance(index: int) -> float:
            a, b = result.route[index], result.route[index + 1]
            dx, dy = b.x - a.x, b.y - a.y
            t = max(
                0.0,
                min(
                    1.0, ((site.x - a.x) * dx + (site.y - a.y) * dy) / max(dx * dx + dy * dy, 1e-9)
                ),
            )
            return ((site.x - (a.x + t * dx)) ** 2 + (site.y - (a.y + t * dy)) ** 2) ** 0.5

        position = min(range(len(result.route) - 1), key=segment_distance) + 1
        result.route.insert(position, site)
        self._rename_routers()
        self._recalculate_manual_route()

    def delete_selected_router(self) -> None:
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
        self.result, self.project_path = None, None
        self.map_widget.clear()
        self.results_table.setRowCount(0)
        self.summary.setText("Load DTM terrain, then place endpoints A and B.")

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
        return Project(
            self.endpoint_a,
            self.endpoint_b,
            self.result.route[1:-1] if self.result and self.result.found else [],
            self.settings_panel.rf_settings(),
            terrain_settings,
            self.settings_panel.candidate_settings(),
        )

    def save_project(self) -> None:
        if not self.project_path:
            self.save_project_as()
            return
        save_project(self._current_project(), self.project_path)
        self.progress_label.setText(f"Saved {self.project_path.name}")

    def save_project_as(self) -> None:
        path, _ = QFileDialog.getSaveFileName(self, "Save project", "", "RF plan (*.rfplan.json)")
        if path:
            self.project_path = Path(path)
            self.save_project()

    def open_project(self) -> None:
        path, _ = QFileDialog.getOpenFileName(
            self, "Open project", "", "RF plan (*.rfplan.json *.json)"
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
            if project.terrain_settings.dtm_paths:
                if self.terrain:
                    self.terrain.close()
                self.terrain = RasterTerrain(
                    project.terrain_settings.dtm_paths, project.terrain_settings.dom_paths
                )
                self._set_transformers()
            self.endpoint_a, self.endpoint_b = project.endpoint_a, project.endpoint_b
            for site in [self.endpoint_a, self.endpoint_b]:
                if site and site.latitude is not None and site.longitude is not None:
                    self.map_widget.set_point(site.id, site.latitude, site.longitude)
            if self.terrain and self.endpoint_a and self.endpoint_b and project.selected_routers:
                route = [self.endpoint_a, *project.selected_routers, self.endpoint_b]
                self.result = OptimizationResult(route, [], route, [], elapsed_seconds=0)
                self._rename_routers()
                self._recalculate_manual_route()
            self.progress_label.setText(f"Opened {self.project_path.name}")
        except Exception as exc:
            QMessageBox.critical(self, "Open project", str(exc))

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
        if self.worker_thread and self.worker_thread.isRunning():
            self.worker_thread.quit()
            self.worker_thread.wait(2000)
        if self.terrain:
            self.terrain.close()
        self._clear_detail_terrain()
        event.accept()
