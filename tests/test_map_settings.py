import os

os.environ.setdefault("QT_QPA_PLATFORM", "offscreen")
os.environ.setdefault("QTWEBENGINE_CHROMIUM_FLAGS", "--disable-gpu")


def test_local_map_content_can_request_remote_tiles() -> None:
    from PySide6.QtWebEngineCore import QWebEngineSettings
    from PySide6.QtWidgets import QApplication

    from rf_router_planner.gui.map_widget import MAP_HTML, MapWidget

    application = QApplication.instance() or QApplication([])
    widget = MapWidget()
    assert widget.settings().testAttribute(
        QWebEngineSettings.WebAttribute.LocalContentCanAccessRemoteUrls
    )
    assert "Kartverket topo" in MAP_HTML
    assert "Kartverket hiking" in MAP_HTML
    assert "Local DTM contours" in MAP_HTML
    widget.close()
    assert application is not None
