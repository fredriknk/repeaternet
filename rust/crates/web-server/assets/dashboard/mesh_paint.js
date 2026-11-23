const paintCellMetres = 250;
const paintCellLimit = 1024;
const paintSpanLimit = 120;
const paintMaxIndex = 80149;
const paintHalfWorld = Math.PI * 6378137;

function paintDefaults() {
  return {cells: [], target_percent: 90, minimum_margin_db: 0};
}

function paintCellKey(cell) {
  return cell.join(',');
}

function paintCellsFit(cells) {
  if (cells.length > paintCellLimit) return false;
  return [0, 1].every(axis => {
    const indices = cells.map(cell => cell[axis]);
    return Math.max(...indices) - Math.min(...indices) < paintSpanLimit;
  });
}

function readMeshPaint(value) {
  if (value === undefined) return paintDefaults();
  const {cells, target_percent, minimum_margin_db} = value || {};
  if (!Array.isArray(cells) || !cells.every(cell => Array.isArray(cell) && cell.length === 2 &&
      cell.every(i => Number.isInteger(i) && i >= -paintMaxIndex - 1 && i <= paintMaxIndex)) ||
      !paintCellsFit(cells) || new Set(cells.map(paintCellKey)).size !== cells.length ||
      !Number.isInteger(target_percent) || target_percent < 1 || target_percent > 100 ||
      !Number.isFinite(minimum_margin_db) || minimum_margin_db < 0 || minimum_margin_db > 60) {
    throw Error('The painted area or its coverage settings are invalid.');
  }
  return {cells: cells.map(cell => [...cell]), target_percent, minimum_margin_db};
}

// Points are in fractional grid coordinates. Interpolating along the segment
// keeps fast pointer movement from leaving gaps between brush stamps.
function paintBrush(cells, from, to, diameter, erase) {
  const radius = diameter / 2;
  const distance = Math.hypot(to[0] - from[0], to[1] - from[1]);
  if (!erase && distance >= paintSpanLimit) return null;
  const next = new Map(cells.map(cell => [paintCellKey(cell), cell]));
  const steps = Math.max(1, Math.ceil(distance * 2));
  // For long erase drags, test the already-painted cells against the segment.
  if (erase) {
    const dx = to[0] - from[0], dy = to[1] - from[1];
    for (const [key, [x, y]] of next) {
      const px = x + .5 - from[0], py = y + .5 - from[1];
      const t = distance ? Math.max(0, Math.min(1, (px * dx + py * dy) / (distance * distance))) : 0;
      if (Math.hypot(px - t * dx, py - t * dy) <= radius) next.delete(key);
    }
    // A one-cell brush always edits the cell under the pointer.
    next.delete(paintCellKey(to.map(Math.floor)));
  } else {
    for (let i = 0; i <= steps; i++) {
      const [x, y] = from.map((v, axis) => v + (to[axis] - v) * i / steps);
      const column = Math.floor(x), row = Math.floor(y);
      for (let cy = Math.floor(y - radius); cy <= y + radius; cy++) {
        for (let cx = Math.floor(x - radius); cx <= x + radius; cx++) {
          if (cx < -paintMaxIndex - 1 || cx > paintMaxIndex || cy < -paintMaxIndex - 1 || cy > paintMaxIndex) continue;
          if ((cx === column && cy === row) || Math.hypot(cx + .5 - x, cy + .5 - y) <= radius) {
            next.set(paintCellKey([cx, cy]), [cx, cy]);
          }
        }
      }
    }
  }
  const result = [...next.values()].sort((a, b) => a[1] - b[1] || a[0] - b[0]);
  return paintCellsFit(result) ? result : null;
}

function paintRowRuns(cells) {
  const runs = [];
  for (const [x, y] of [...cells].sort((a, b) => a[1] - b[1] || a[0] - b[0])) {
    const last = runs.at(-1);
    if (last?.[1] === y && last[2] === x) last[2]++;
    else runs.push([x, y, x + 1]);
  }
  return runs;
}

let meshPaint = paintDefaults();
let paintLayer, paintStroke, paintOpener;
let restorePaintDragging = false, restorePaintDoubleClick = false, restorePaintLegend = false;

function activeMeshPaint() {
  return routeMode === 'mesh' && meshPaint.cells.length > 0;
}

function paintModeActive(mode = interactionMode) {
  return mode === 'paint-mesh' || mode === 'erase-mesh';
}

function paintLatLng(x, y) {
  return L.CRS.EPSG3857.unproject(L.point(x * paintCellMetres, y * paintCellMetres));
}

function paintGridPoint(latlng) {
  const {x, y} = L.CRS.EPSG3857.project(latlng.wrap());
  return [Math.max(-paintHalfWorld, Math.min(paintHalfWorld, x)) / paintCellMetres,
    Math.max(-paintHalfWorld, Math.min(paintHalfWorld, y)) / paintCellMetres];
}

function paintBounds() {
  return activeMeshPaint() ? paintRowRuns(meshPaint.cells).flatMap(([x, y, right]) =>
    [paintLatLng(x, y), paintLatLng(right, y + 1)]) : [];
}

function renderMeshPaint() {
  if (!paintLayer) return;
  paintLayer.clearLayers();
  const painted = routeMode === 'mesh' ? paintStroke?.cells ?? meshPaint.cells : [];
  const coverage = result?.paint_coverage;
  const uncovered = routeMode === 'mesh' && coverage && !resultStale && !paintStroke ? coverage.uncovered_cells : [];
  function draw(cells, style) {
    for (const [x, y, right] of paintRowRuns(cells)) {
      L.rectangle([paintLatLng(x, y), paintLatLng(right, y + 1)],
        {pane: 'paintMeshPane', interactive: false, weight: 1.5, ...style}).addTo(paintLayer);
    }
  }
  draw(painted, {color: '#185f70', fillColor: '#61b9c5', fillOpacity: .18});
  draw(uncovered, {color: 'var(--map-invalid)', fillColor: '#e39479', fillOpacity: .36, dashArray: '4 3'});
  $('mesh-paint-legend').hidden = !painted.length;
  $('mesh-paint-uncovered-legend').hidden = !uncovered.length;
  const summary = $('mesh-paint-analysis');
  summary.hidden = !coverage;
  if (coverage) {
    summary.classList.toggle('stale', resultStale);
    summary.textContent = `${coverage.covered_cells} of ${coverage.cell_count} painted cells at or above ${coverage.minimum_margin_db.toFixed(1)} dB${resultStale ? ' · stale' : ''}`;
  }
}

function renderMeshPaintCount() {
  const count = meshPaint.cells.length;
  $('mesh-paint-count').textContent = count ? `${count.toLocaleString()} painted ${count === 1 ? 'cell' : 'cells'}` : 'Paint an area to plan a mesh with or without clients.';
  $('mesh-paint-clear').hidden = !count;
}

function syncMeshPaintPanel() {
  $('paint-mesh-tool').hidden = routeMode !== 'mesh';
  renderMeshPaintCount();
  for (const key of ['target-percent', 'minimum-margin']) {
    const field = $('mesh-paint-' + key);
    field.value = key === 'target-percent' ? meshPaint.target_percent : meshPaint.minimum_margin_db;
    field.removeAttribute('aria-invalid');
    $('mesh-paint-' + key + '-error').hidden = true;
  }
  renderMeshPaint();
}

function editMeshPaint(next, keepMapMode = false) {
  if (mapEditsLocked() || routeMode !== 'mesh' || JSON.stringify(next) === JSON.stringify(meshPaint)) return;
  meshPaint = next;
  renderMeshPaintCount();
  recordEdit();
  invalidate('painted coverage', {keepMapMode});
  scheduleCoverageCheck();
}

function validateMeshPaintSettings(field, focus = false) {
  const raw = field.value.trim();
  const valid = raw && !field.validity.badInput && !field.validity.rangeUnderflow &&
    !field.validity.rangeOverflow && !field.validity.stepMismatch;
  const error = $(field.id + '-error');
  field.setAttribute('aria-invalid', String(!valid));
  error.hidden = !!valid;
  if (!valid && focus) field.focus();
  return !!valid;
}

function syncPaintInteraction(previous, next) {
  const wasActive = paintModeActive(previous), active = paintModeActive(next);
  if (!paintLayer) return;
  const container = map.getContainer();
  const legend = document.querySelector('.route-map-legend');
  if (!wasActive && active) {
    restorePaintLegend = legend.open;
    legend.open = false;
    restorePaintDragging = map.dragging.enabled();
    restorePaintDoubleClick = map.doubleClickZoom.enabled();
    map.dragging.disable();
    map.doubleClickZoom.disable();
    container.focus({preventScroll: true});
  } else if (wasActive && !active) {
    paintStroke = null;
    if (restorePaintLegend) legend.open = true;
    if (restorePaintDragging) map.dragging.enable();
    if (restorePaintDoubleClick) map.doubleClickZoom.enable();
    renderMeshPaint();
  }
  container.classList.toggle('mesh-paint-active', active);
  $('mesh-paint-map-controls').hidden = !active;
  for (const id of ['paint-mesh-tool', 'mesh-paint-start']) {
    $(id).setAttribute('aria-pressed', String(next === 'paint-mesh'));
  }
  $('erase-mesh-tool').setAttribute('aria-pressed', String(next === 'erase-mesh'));
}

function cancelMapInteraction() {
  const painted = paintModeActive();
  setInteractionMode('browse');
  if (painted) {
    (paintOpener?.checkVisibility() ? paintOpener : $('paint-mesh-tool')).focus({preventScroll: true});
  }
}

function initMeshPaintControls() {
  map.createPane('paintMeshPane').style.zIndex = 380;
  paintLayer = L.layerGroup().addTo(map);
  const container = map.getContainer();
  for (const id of ['paint-mesh-tool', 'mesh-paint-start', 'erase-mesh-tool']) {
    mapEditControl($(id)).onclick = event => {
      if (mapEditsLocked() || routeMode !== 'mesh') return;
      const mode = id === 'erase-mesh-tool' && interactionMode !== 'erase-mesh' ? 'erase-mesh' : 'paint-mesh';
      if (id !== 'erase-mesh-tool' && interactionMode === mode) {
        cancelMapInteraction();
        return;
      }
      if (!paintModeActive()) paintOpener = event.currentTarget;
      setInteractionMode(mode);
      if (id === 'erase-mesh-tool') container.focus({preventScroll: true});
    };
  }
  mapEditControl($('mesh-paint-clear')).onclick = () => {
    editMeshPaint({...meshPaint, cells: []});
    $('mesh-paint-start').focus();
  };
  $('mesh-paint-handheld').onclick = () => {
    setWorkspace('workspace-coverage');
    document.querySelector('[data-coverage-section="handheld"]').click();
    $('client-height').focus();
  };
  for (const [id, key] of [['mesh-paint-target-percent', 'target_percent'], ['mesh-paint-minimum-margin', 'minimum_margin_db']]) {
    mapEditControl($(id)).onchange = event => {
      if (!validateMeshPaintSettings(event.target, true)) return;
      editMeshPaint({...meshPaint, [key]: Number(event.target.value)});
    };
  }
  mapEditControl($('mesh-paint-brush'));
  const apply = (cells, from, to) => paintBrush(cells, from, to, Number($('mesh-paint-brush').value), interactionMode === 'erase-mesh');
  function updateStroke(point) {
    const cells = apply(paintStroke.cells, paintStroke.last, point);
    paintStroke.last = point;
    if (cells) {
      paintStroke.cells = cells;
      renderMeshPaint();
    } else {
      setMapHint('Paint up to 1,024 cells within 120 cells across.');
    }
  }
  container.addEventListener('pointerdown', event => {
    if (!paintModeActive() || mapEditsLocked() || event.button !== 0 || !event.isPrimary ||
        event.target.closest('.leaflet-control, .leaflet-popup')) return;
    event.preventDefault();
    event.stopImmediatePropagation();
    container.focus({preventScroll: true});
    const point = paintGridPoint(map.mouseEventToLatLng(event));
    paintStroke = {id: event.pointerId, last: point, cells: meshPaint.cells};
    container.setPointerCapture(event.pointerId);
    updateStroke(point);
  }, true);
  container.addEventListener('pointermove', event => {
    if (paintStroke?.id !== event.pointerId) return;
    updateStroke(paintGridPoint(map.mouseEventToLatLng(event)));
  });
  container.addEventListener('pointerup', event => {
    if (paintStroke?.id !== event.pointerId) return;
    updateStroke(paintGridPoint(map.mouseEventToLatLng(event)));
    const {cells} = paintStroke;
    paintStroke = null;
    editMeshPaint({...meshPaint, cells}, true);
    renderMeshPaint();
  });
  container.addEventListener('lostpointercapture', event => {
    if (paintStroke?.id !== event.pointerId) return;
    paintStroke = null;
    renderMeshPaint();
  });
  container.addEventListener('click', event => {
    if (paintModeActive() && !event.target.closest('.leaflet-control')) event.stopImmediatePropagation();
  }, true);
  container.addEventListener('keydown', event => {
    if (!paintModeActive() || mapEditsLocked() || event.target !== container || !['Enter', ' '].includes(event.key)) return;
    event.preventDefault();
    const point = paintGridPoint(map.getCenter());
    const cells = apply(meshPaint.cells, point, point);
    if (cells) editMeshPaint({...meshPaint, cells}, true);
    else setMapHint('Paint up to 1,024 cells within 120 cells across.');
  });
}

if (typeof module !== 'undefined') module.exports = {paintBrush, paintRowRuns, readMeshPaint};
