
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
