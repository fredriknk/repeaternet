
const data = JSON.parse(document.getElementById('route-map-data').textContent);
const map = L.map('route-map', {attributionControl: false, maxZoom: 19, scrollWheelZoom: false});
L.control.attribution({prefix: false}).addTo(map);
L.control.scale({imperial: false}).addTo(map);

const [, , live] = JSON.parse(document.getElementById('report-live-coverage').textContent);
const coverageLayers = new Map();
map.createPane('coverage').style.zIndex = 250;
if (live) {
  const legend = document.getElementById('live-coverage-legend');
  const summary = document.createElement('p');
  summary.textContent = `Live coverage prediction · ${live.mode} · ${live.ready} of ${live.total} sources included`;
  const scale = document.createElement('div');
  scale.className = 'coverage-scale';
  const ramp = document.createElement('div');
  ramp.className = 'coverage-ramp';
  ramp.style.background = live.gradient;
  for (let index = 0; index <= 4; index++) {
    const tick = document.createElement('span');
    tick.textContent = `${live.max * index / 4} dB`;
    scale.append(tick);
  }
  const symbols = document.createElement('div');
  symbols.className = 'legend';
  for (const [label, background] of live.symbols) {
    const entry = document.createElement('span');
    const swatch = document.createElement('i');
    swatch.className = 'swatch';
    swatch.style.background = background;
    entry.append(swatch, label);
    symbols.append(entry);
  }
  legend.append(summary, ramp, scale, symbols);
  const omission = document.getElementById('saved-coverage-omission');
  if (omission) omission.hidden = true;
  coverageLayers.set('Live coverage', {
    layer: L.imageOverlay(live.image, live.bounds, {
      pane: 'coverage', opacity: live.opacity, alt: 'Predicted live coverage'
    }), legend
  });
}
if (data.coverage) {
  coverageLayers.set('Saved coverage grid', {
    layer: L.imageOverlay(data.coverage.image, data.coverage.bounds, {
      pane: 'coverage', opacity: 0.54, alt: 'Predicted saved coverage grid'
    }), legend: document.getElementById('saved-coverage-legend')
  });
}
if (coverageLayers.size) {
  const label = document.createElement('label');
  label.className = 'coverage-control';
  const select = document.createElement('select');
  for (const name of coverageLayers.keys()) select.add(new Option(name, name));
  select.add(new Option('Hide coverage', ''));
  label.append('Map layer', select);
  document.getElementById('route-map').before(label);
  const updateCoverage = () => {
    for (const [name, {layer, legend}] of coverageLayers) {
      const visible = select.value === name;
      if (visible) layer.addTo(map);
      else layer.remove();
      legend.hidden = !visible;
    }
  };
  select.addEventListener('change', updateCoverage);
  updateCoverage();
}

L.polyline(data.roads, {
  color: '#525c62', weight: 2, opacity: 0.85,
  attribution: data.roads.length
    ? 'Roads © <a href="https://www.openstreetmap.org/copyright">OpenStreetMap contributors</a>'
    : null
}).addTo(map);
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
const fitRoute = () => {
  const bounds = L.latLngBounds(data.sites.map(site => site.coordinates));
  for (const {layer} of coverageLayers.values()) {
    if (map.hasLayer(layer)) bounds.extend(layer.getBounds());
  }
  map.invalidateSize({pan: false});
  map.fitBounds(bounds, {padding: [28, 28], maxZoom: 16, animate: false});
};
fitRoute();
window.addEventListener('beforeprint', fitRoute);
window.addEventListener('afterprint', fitRoute);
