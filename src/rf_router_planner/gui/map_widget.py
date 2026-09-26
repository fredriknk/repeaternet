from __future__ import annotations

import json
from pathlib import Path

from PySide6.QtCore import QObject, QUrl, Signal, Slot
from PySide6.QtWebChannel import QWebChannel
from PySide6.QtWebEngineCore import QWebEngineSettings
from PySide6.QtWebEngineWidgets import QWebEngineView

MAP_HTML = r"""<!doctype html>
<html><head><meta charset="utf-8"><style>
html,body,#map{height:100%;margin:0}
.rf-site{border:2px solid white;border-radius:50%;box-shadow:0 1px 5px #222;width:18px;height:18px}
.rf-client{background:#1976d2}.rf-router{background:#f28c28}.rf-manual{background:#8e44ad}
.rf-known{background:#8b929a;opacity:.75}.rf-legend{background:rgba(255,255,255,.94);padding:8px 10px;
 border-radius:5px;box-shadow:0 1px 5px #777;font:12px/18px sans-serif}
.rf-legend i{display:inline-block;width:12px;height:12px;border-radius:50%;margin-right:6px;vertical-align:-1px}
</style>
<link rel="stylesheet" href="leaflet/leaflet.css">
<script src="qrc:///qtwebchannel/qwebchannel.js"></script>
<script src="leaflet/leaflet.js"></script></head>
<body><div id="map"></div><script>
const map=L.map('map').setView([64.5,11.0],5);
const osm=L.tileLayer('https://tile.openstreetmap.org/{z}/{x}/{y}.png',{maxZoom:19,attribution:'© OpenStreetMap contributors',crossOrigin:true});
const kartverketTopo=L.tileLayer('https://cache.kartverket.no/v1/wmts/1.0.0/topo/default/webmercator/{z}/{y}/{x}.png',{maxZoom:18,attribution:'© Kartverket',crossOrigin:true});
const kartverketHiking=L.tileLayer('https://cache.kartverket.no/v1/wmts/1.0.0/toporaster/default/webmercator/{z}/{y}/{x}.png',{maxZoom:18,attribution:'© Kartverket',crossOrigin:true});
osm.addTo(map);
let bridge=null,mode=null,markers={};
const candidateLayer=L.layerGroup();
const backboneLayer=L.layerGroup().addTo(map),meshLinkLayer=L.layerGroup().addTo(map);
const coverageLayer=L.layerGroup(),overlapLayer=L.layerGroup();
const knownRouterLayer=L.layerGroup().addTo(map);
const contourLayer=L.geoJSON(null,{
 style:feature=>({color:feature.properties.index?'#5b3a29':'#785643',weight:feature.properties.index?1.7:.8,opacity:feature.properties.index?.85:.58,interactive:true}),
 onEachFeature:(feature,layer)=>layer.bindTooltip(`${feature.properties.elevation_m} m`,{sticky:true})
});
L.control.layers(
 {'OpenStreetMap':osm,'Kartverket topo':kartverketTopo,'Kartverket hiking':kartverketHiking},
 {'Planned network':backboneLayer,'Other viable RF links':meshLinkLayer,
  'Predicted coverage':coverageLayer,'Coverage overlaps':overlapLayer,
  'Known CoreScope routers':knownRouterLayer,'Candidate sites':candidateLayer,
  'Local DTM contours':contourLayer}
).addTo(map);
const legend=L.control({position:'bottomright'});
legend.onAdd=()=>{const d=L.DomUtil.create('div','rf-legend');d.innerHTML=
 '<b>RF plan</b><br><i style="background:#1976d2"></i>Client<br>'+
 '<i style="background:#f28c28"></i>Selected router<br>'+
 '<i style="background:#8e44ad"></i>Required manual router<br>'+
 '<i style="background:#8b929a"></i>Known external router<br>'+
 '<span style="color:#777">━</span> viable link &nbsp; <b style="color:#18864b">━</b> planned link<br>'+
 '<i style="background:#e600ff"></i>Coverage overlap';return d;};legend.addTo(map);
new QWebChannel(qt.webChannelTransport,channel=>{bridge=channel.objects.bridge;});
map.on('click',e=>{if(bridge&&mode){bridge.mapClicked(mode,e.latlng.lat,e.latlng.lng);mode=null;}});
function setMode(value){mode=value;}
function clearAll(){Object.values(markers).forEach(m=>map.removeLayer(m));markers={};candidateLayer.clearLayers();backboneLayer.clearLayers();meshLinkLayer.clearLayers();coverageLayer.clearLayers();overlapLayer.clearLayers();knownRouterLayer.clearLayers();clearContours();}
function setPoint(id,lat,lon,label,draggable,role='client'){
 if(markers[id])map.removeLayer(markers[id]);
 const css=role==='manual'?'rf-manual':role==='router'?'rf-router':role==='known'?'rf-known':'rf-client';
 const icon=L.divIcon({className:'',html:`<div class="rf-site ${css}"></div>`,iconSize:[18,18],iconAnchor:[9,9]});
 const marker=L.marker([lat,lon],{draggable:draggable,icon:icon}).addTo(map).bindTooltip(label,{permanent:true,direction:'top'});
 marker.on('dragend',()=>{const p=marker.getLatLng();if(bridge)bridge.markerMoved(id,p.lat,p.lng);});
 marker.on('click',()=>{if(bridge)bridge.siteClicked(id);});markers[id]=marker;
}
function setCandidates(items,visible){candidateLayer.clearLayers();if(!visible){map.removeLayer(candidateLayer);return;}items.forEach(p=>L.circleMarker([p.lat,p.lon],{radius:3,color:'#777',fillOpacity:.5}).addTo(candidateLayer).bindTooltip(p.id));candidateLayer.addTo(map);}
function setContours(data,visible){contourLayer.clearLayers();if(data)contourLayer.addData(data);if(visible)contourLayer.addTo(map);else map.removeLayer(contourLayer);}
function clearContours(){contourLayer.clearLayers();map.removeLayer(contourLayer);}
function drawLink(layer,l,weight,opacity){const color=l.valid?(l.margin>=10?'#18864b':l.margin>=0?'#d88b00':'#c33'):'#c33';const line=L.polyline([[l.a_lat,l.a_lon],[l.b_lat,l.b_lon]],{color:color,weight:weight,opacity:opacity,dashArray:l.valid?null:'8,6'}).addTo(layer);line.bindTooltip(`${l.from} ↔ ${l.to}: ${l.margin.toFixed(1)} dB (${l.status})`);if(l.index>=0)line.on('click',()=>{if(bridge)bridge.linkClicked(l.index);});}
function setNetwork(points,backboneLinks,meshLinks){backboneLayer.clearLayers();meshLinkLayer.clearLayers();Object.keys(markers).filter(k=>!points.some(p=>p.id===k)).forEach(k=>{map.removeLayer(markers[k]);delete markers[k];});points.forEach(p=>setPoint(p.id,p.lat,p.lon,p.label||p.id,p.draggable!==false,p.role||'router'));meshLinks.forEach(l=>drawLink(meshLinkLayer,l,2,.55));backboneLinks.forEach(l=>drawLink(backboneLayer,l,5,.95));if(points.length)map.fitBounds(L.latLngBounds(points.map(p=>[p.lat,p.lon])).pad(.2));}
function setRoute(points,links){setNetwork(points,links,[]);}
function setKnownRouters(items){knownRouterLayer.clearLayers();items.forEach(p=>{const marker=L.circleMarker([p.lat,p.lon],{radius:6,color:'#666',fillColor:p.enabled?'#8e44ad':'#aeb4ba',fillOpacity:.8,weight:2}).addTo(knownRouterLayer);marker.bindTooltip(`${p.name}<br>${p.status||'CoreScope repeater'}`);marker.on('click',()=>{if(bridge)bridge.siteClicked(p.id);});});}
function setCoverage(items){coverageLayer.clearLayers();overlapLayer.clearLayers();items.forEach(p=>{const overlap=p.sources.length>1;const layer=overlap?overlapLayer:coverageLayer;const color=overlap?'#e600ff':(p.color||'#2b83ba');L.circleMarker([p.lat,p.lon],{radius:overlap?6:5,stroke:false,fillColor:color,fillOpacity:(overlap?0.72:0.42)}).addTo(layer).bindTooltip(`${p.sources.join(' + ')}<br>${p.margin.toFixed(1)} dB worst margin`);});coverageLayer.addTo(map);overlapLayer.addTo(map);}
function clearCoverage(){coverageLayer.clearLayers();overlapLayer.clearLayers();}
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
        self._page_ready = False
        self._pending_scripts: list[str] = []
        self.settings().setAttribute(
            QWebEngineSettings.WebAttribute.LocalContentCanAccessRemoteUrls, True
        )
        self.bridge = MapBridge()
        channel = QWebChannel(self.page())
        channel.registerObject("bridge", self.bridge)
        self.page().setWebChannel(channel)
        data_directory = Path(__file__).parents[1] / "data"
        self.loadFinished.connect(self._map_loaded)
        self.setHtml(MAP_HTML, QUrl.fromLocalFile(str(data_directory.resolve()) + "/"))

    def _js(self, script: str) -> None:
        if self._page_ready:
            self.page().runJavaScript(script)
        else:
            self._pending_scripts.append(script)

    @Slot(bool)
    def _map_loaded(self, successful: bool) -> None:
        if not successful:
            return
        self._page_ready = True
        pending, self._pending_scripts = self._pending_scripts, []
        for script in pending:
            self.page().runJavaScript(script)

    def set_mode(self, mode: str) -> None:
        self._js(f"setMode({json.dumps(mode)})")

    def set_point(
        self,
        site_id: str,
        latitude: float,
        longitude: float,
        draggable: bool = True,
        role: str = "client",
        label: str | None = None,
    ) -> None:
        args = ",".join(
            map(json.dumps, [site_id, latitude, longitude, label or site_id, draggable, role])
        )
        self._js(f"setPoint({args})")

    def clear(self) -> None:
        self._js("clearAll()")

    def set_candidates(self, points: list[dict[str, object]], visible: bool) -> None:
        self._js(f"setCandidates({json.dumps(points)},{str(visible).lower()})")

    def set_contours(self, geojson: dict[str, object], visible: bool = True) -> None:
        self._js(f"setContours({json.dumps(geojson)},{str(visible).lower()})")

    def clear_contours(self) -> None:
        self._js("clearContours()")

    def set_route(self, points: list[dict[str, object]], links: list[dict[str, object]]) -> None:
        self._js(f"setRoute({json.dumps(points)},{json.dumps(links)})")

    def set_network(
        self,
        points: list[dict[str, object]],
        backbone_links: list[dict[str, object]],
        mesh_links: list[dict[str, object]],
    ) -> None:
        self._js(
            f"setNetwork({json.dumps(points)},{json.dumps(backbone_links)},"
            f"{json.dumps(mesh_links)})"
        )

    def set_known_routers(self, points: list[dict[str, object]]) -> None:
        self._js(f"setKnownRouters({json.dumps(points)})")

    def set_coverage(self, points: list[dict[str, object]]) -> None:
        self._js(f"setCoverage({json.dumps(points)})")

    def clear_coverage(self) -> None:
        self._js("clearCoverage()")
