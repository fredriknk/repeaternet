from __future__ import annotations

import json
from pathlib import Path

from PySide6.QtCore import QObject, QUrl, Signal, Slot
from PySide6.QtWebChannel import QWebChannel
from PySide6.QtWebEngineCore import QWebEngineSettings
from PySide6.QtWebEngineWidgets import QWebEngineView

MAP_HTML = r"""<!doctype html>
<html><head><meta charset="utf-8"><style>html,body,#map{height:100%;margin:0}</style>
<link rel="stylesheet" href="leaflet/leaflet.css">
<script src="qrc:///qtwebchannel/qwebchannel.js"></script>
<script src="leaflet/leaflet.js"></script></head>
<body><div id="map"></div><script>
const map=L.map('map').setView([64.5,11.0],5);
L.tileLayer('https://tile.openstreetmap.org/{z}/{x}/{y}.png', {
 maxZoom:19,
 attribution:'© OpenStreetMap contributors',
 crossOrigin:true
}).addTo(map);
let bridge=null, mode=null, markers={}, candidateLayer=L.layerGroup(), routeLayer=L.layerGroup();
new QWebChannel(qt.webChannelTransport, channel=>{bridge=channel.objects.bridge;});
map.on('click', e=>{if(bridge&&mode){bridge.mapClicked(mode,e.latlng.lat,e.latlng.lng);mode=null;}});
function setMode(value){mode=value;}
function clearAll(){Object.values(markers).forEach(m=>map.removeLayer(m));markers={};candidateLayer.clearLayers();routeLayer.clearLayers();}
function setPoint(id,lat,lon,label,draggable){
 if(markers[id]) map.removeLayer(markers[id]);
 const m=L.marker([lat,lon],{draggable:draggable}).addTo(map).bindTooltip(label,{permanent:true,direction:'top'});
 m.on('dragend',()=>{const p=m.getLatLng();if(bridge)bridge.markerMoved(id,p.lat,p.lng);});m.on('click',()=>{if(bridge)bridge.siteClicked(id);});markers[id]=m;
}
function setCandidates(items,visible){candidateLayer.clearLayers();if(!visible)return;items.forEach(p=>L.circleMarker([p.lat,p.lon],{radius:3,color:'#777',fillOpacity:.5}).addTo(candidateLayer).bindTooltip(p.id));candidateLayer.addTo(map);}
function setRoute(points,links){
 routeLayer.clearLayers();Object.keys(markers).filter(k=>k.startsWith('R')).forEach(k=>{map.removeLayer(markers[k]);delete markers[k];});
 points.forEach(p=>setPoint(p.id,p.lat,p.lon,p.id,true));
 links.forEach(l=>{const color=l.valid?(l.margin>=10?'#18864b':l.margin>=0?'#d88b00':'#c33'):'#c33';
  const line=L.polyline([[l.a_lat,l.a_lon],[l.b_lat,l.b_lon]],{color:color,weight:5,dashArray:l.margin<0?'8,6':null}).addTo(routeLayer);
  line.bindTooltip(`${l.from} → ${l.to}: ${l.margin.toFixed(1)} dB (${l.status})`);line.on('click',()=>{if(bridge)bridge.linkClicked(l.index);});});
 routeLayer.addTo(map);if(points.length)map.fitBounds(L.latLngBounds(points.map(p=>[p.lat,p.lon])).pad(.2));
}
</script></body></html>"""


class MapBridge(QObject):
    clicked = Signal(str, float, float)
    moved = Signal(str, float, float)
    link_selected = Signal(int)
    site_selected = Signal(str)

    @Slot(str, float, float)
    def mapClicked(self, mode: str, latitude: float, longitude: float) -> None:
        self.clicked.emit(mode, latitude, longitude)

    @Slot(str, float, float)
    def markerMoved(self, site_id: str, latitude: float, longitude: float) -> None:
        self.moved.emit(site_id, latitude, longitude)

    @Slot(int)
    def linkClicked(self, index: int) -> None:
        self.link_selected.emit(index)

    @Slot(str)
    def siteClicked(self, site_id: str) -> None:
        self.site_selected.emit(site_id)


class MapWidget(QWebEngineView):
    def __init__(self, parent=None) -> None:  # type: ignore[no-untyped-def]
        super().__init__(parent)
        # The HTML and Leaflet runtime are bundled locally for offline use, but
        # the optional OpenStreetMap background is remote. QtWebEngine blocks
        # local pages from making remote requests unless explicitly permitted.
        self.settings().setAttribute(
            QWebEngineSettings.WebAttribute.LocalContentCanAccessRemoteUrls, True
        )
        self.bridge = MapBridge()
        channel = QWebChannel(self.page())
        channel.registerObject("bridge", self.bridge)
        self.page().setWebChannel(channel)
        data_directory = Path(__file__).parents[1] / "data"
        self.setHtml(MAP_HTML, QUrl.fromLocalFile(str(data_directory.resolve()) + "/"))

    def _js(self, script: str) -> None:
        self.page().runJavaScript(script)

    def set_mode(self, mode: str) -> None:
        self._js(f"setMode({json.dumps(mode)})")

    def set_point(
        self, site_id: str, latitude: float, longitude: float, draggable: bool = True
    ) -> None:
        args = ",".join(map(json.dumps, [site_id, latitude, longitude, site_id, draggable]))
        self._js(f"setPoint({args})")

    def clear(self) -> None:
        self._js("clearAll()")

    def set_candidates(self, points: list[dict[str, object]], visible: bool) -> None:
        self._js(f"setCandidates({json.dumps(points)},{str(visible).lower()})")

    def set_route(self, points: list[dict[str, object]], links: list[dict[str, object]]) -> None:
        self._js(f"setRoute({json.dumps(points)},{json.dumps(links)})")
