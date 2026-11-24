/* Live coverage: one margin raster per radio site, composited on the map.
 * The server returns a Web Mercator-aligned u8 raster per site (POST /api/live-coverage).
 * This script fetches them, previews while a marker is dragged and paints the
 * best margin, overlap and missing terrain into canvas map tiles. Loaded before
 * app.js; everything that touches app.js globals runs after initLiveCoverage(). */
const liveCoverageStorageKey = 'repeaternet.live-coverage';
const liveCoverageWorld = 2 * Math.PI * 6378137;
const liveCoverageColormaps = {
  viridis: {label: 'Viridis', stops: ['#440154', '#482878', '#3e4989', '#31688e', '#26828e', '#1f9e89', '#35b779', '#6ece58', '#b5de2b', '#fde725']},
  plasma: {label: 'Plasma', stops: ['#0d0887', '#46039f', '#7201a8', '#9c179e', '#bd3786', '#d8576b', '#ed7953', '#fb9f3a', '#fdca26', '#f0f921']},
  turbo: {label: 'Turbo (rainbow)', stops: ['#30123b', '#4145ab', '#4675ed', '#39a2fc', '#1bcfd4', '#24eca6', '#61fc6c', '#a4fc3b', '#d1e834', '#f3c63a', '#fe9b2d', '#f36315', '#d93806', '#b11901', '#7a0403']},
  cividis: {label: 'Cividis (colour-blind safe)', stops: ['#00224e', '#123570', '#3b496c', '#575d6d', '#707173', '#8a8779', '#a69d75', '#c4b56c', '#e4cf5b', '#fee838']},
  magma: {label: 'Magma', stops: ['#000004', '#180f3d', '#440f76', '#721f81', '#9e2f7f', '#cd4071', '#f1605d', '#fd9668', '#feca8d', '#fcfdbf']},
  bands: {label: 'Margin bands (orange → green)', dbStops: [[0, '#f1842f'], [5, '#e2cf3f'], [10, '#35ac60'], [15, '#12834e']]},
};
const liveCoverageDefaults = {
  enabled: true, colormap: 'viridis', scale_max_db: 30, overlap: false, overlap_color: '#ff2d95',
  show_unknown: true, opacity: 70, mode: 'two_way', radius_km: 20, detail_m: 30,
  sources: {endpoints: true, proposed: true, route: true, existing: true},
};
const liveCoverageDetailLevels = [[15, 'Fine · 15 m'], [30, 'Standard · 30 m'], [60, 'Coarse · 60 m']];
const liveCoverageModes = {two_way: 'Two-way margin', downlink: 'Router → handheld', uplink: 'Handheld → router'};
const liveCoverage = {
  settings: liveCoverageLoadSettings(), slots: new Map(), cache: new Map(), queue: [], active: 0,
  hidden: new Set(), layer: null, palette: null, timer: 0, interval: 0, drag: null, preview: null,
  previewBusy: false, lastPreview: null, dirty: null, frame: 0, noTerrain: '', hover: null, hoverFrame: 0,
};

function liveCoverageLoadSettings() {
  try {
    const saved = JSON.parse(localStorage.getItem(liveCoverageStorageKey) || '{}');
    const settings = {...liveCoverageDefaults, ...saved, sources: {...liveCoverageDefaults.sources, ...(saved.sources || {})}};
    if (!liveCoverageColormaps[settings.colormap]) settings.colormap = liveCoverageDefaults.colormap;
    if (!liveCoverageModes[settings.mode]) settings.mode = liveCoverageDefaults.mode;
    if (!liveCoverageDetailLevels.some(([value]) => value === settings.detail_m)) settings.detail_m = liveCoverageDefaults.detail_m;
    if (!(settings.radius_km >= 1 && settings.radius_km <= 80)) settings.radius_km = liveCoverageDefaults.radius_km;
    if (![20, 30, 40, 60].includes(settings.scale_max_db)) settings.scale_max_db = liveCoverageDefaults.scale_max_db;
    if (!/^#[0-9a-f]{6}$/i.test(settings.overlap_color)) settings.overlap_color = liveCoverageDefaults.overlap_color;
    if (!(settings.opacity >= 0 && settings.opacity <= 100)) settings.opacity = liveCoverageDefaults.opacity;
    return settings;
  } catch {
    return JSON.parse(JSON.stringify(liveCoverageDefaults));
  }
}

function liveCoverageSaveSettings() {
  try { localStorage.setItem(liveCoverageStorageKey, JSON.stringify(liveCoverage.settings)); } catch {}
}

function liveCoverageHex(hex) {
  const n = Number.parseInt(hex.slice(1), 16);
  return [n >> 16 & 255, n >> 8 & 255, n & 255];
}

/* 256 RGB entries spanning 0 dB to the scale maximum. */
function liveCoveragePalette(name, scaleMax) {
  const map = liveCoverageColormaps[name] || liveCoverageColormaps.viridis;
  const stops = map.dbStops
    ? map.dbStops.map(([db, hex]) => [Math.min(1, db / scaleMax), liveCoverageHex(hex)])
    : map.stops.map((hex, i, all) => [i / (all.length - 1), liveCoverageHex(hex)]);
  if (stops[stops.length - 1][0] < 1) stops.push([1, stops[stops.length - 1][1]]);
  const palette = new Uint8ClampedArray(256 * 3);
  for (let i = 0; i < 256; i++) {
    const t = i / 255;
    let k = 0;
    while (k < stops.length - 2 && t > stops[k + 1][0]) k++;
    const [t0, from] = stops[k], [t1, to] = stops[k + 1];
    const f = t1 > t0 ? Math.max(0, Math.min(1, (t - t0) / (t1 - t0))) : 0;
    for (let c = 0; c < 3; c++) palette[i * 3 + c] = from[c] + f * (to[c] - from[c]);
  }
  return palette;
}

function liveCoverageGradientCss(name, scaleMax) {
  const palette = liveCoveragePalette(name, scaleMax), parts = [];
  for (let i = 0; i <= 16; i++) {
    const p = Math.round(i / 16 * 255) * 3;
    parts.push(`rgb(${palette[p]},${palette[p + 1]},${palette[p + 2]}) ${(i / 16 * 100).toFixed(1)}%`);
  }
  return `linear-gradient(90deg,${parts.join(',')})`;
}

/* Decode the RNLC body: magic, u32 header length, JSON header, u8 raster. */
function decodeLiveCoverage(buffer) {
  const bytes = new Uint8Array(buffer);
  if (bytes.length < 8 || String.fromCharCode(...bytes.subarray(0, 4)) !== 'RNLC') throw Error('Unexpected live coverage response');
  const length = new DataView(buffer).getUint32(4, true);
  const header = JSON.parse(new TextDecoder().decode(bytes.subarray(8, 8 + length)));
  const size = header.width * header.height;
  if (!(size > 0) || bytes.length < 8 + length + size) throw Error('Live coverage raster is truncated');
  const data = bytes.slice(8 + length, 8 + length + size);
  // Code 0 is outside the radius, 1 unknown terrain, 2 below the floor and 3+ a margin.
  const lut = new Float32Array(256);
  lut[0] = NaN; lut[1] = NaN; lut[2] = header.floor_db;
  for (let v = 3; v < 256; v++) lut[v] = header.floor_db + (v - 3) * header.step_db;
  const {origin_x: west, origin_y: north, pixel_size: size3857} = header;
  return {header, data, lut, west, north, east: west + header.width * size3857, south: north - header.height * size3857};
}

/* Margin at a Web Mercator point: NaN outside, null for unknown terrain. */
function liveCoverageSample(raster, x, y) {
  const {header: {width, height, pixel_size: ps}, data, lut} = raster;
  const colf = (x - raster.west) / ps - .5, rowf = (raster.north - y) / ps - .5;
  if (colf < -.5 || rowf < -.5 || colf > width - .5 || rowf > height - .5) return NaN;
  const nearest = data[Math.min(height - 1, Math.max(0, Math.round(rowf))) * width + Math.min(width - 1, Math.max(0, Math.round(colf)))];
  if (nearest === 0) return NaN;
  if (nearest === 1) return null;
  const c0 = Math.max(0, Math.min(width - 1, Math.floor(colf))), r0 = Math.max(0, Math.min(height - 1, Math.floor(rowf)));
  const c1 = Math.min(width - 1, c0 + 1), r1 = Math.min(height - 1, r0 + 1);
  const fx = Math.max(0, Math.min(1, colf - c0)), fy = Math.max(0, Math.min(1, rowf - r0));
  const a = data[r0 * width + c0], b = data[r0 * width + c1], c = data[r1 * width + c0], d = data[r1 * width + c1];
  if (a < 2 || b < 2 || c < 2 || d < 2) return lut[nearest];
  return (lut[a] * (1 - fx) + lut[b] * fx) * (1 - fy) + (lut[c] * (1 - fx) + lut[d] * fx) * fy;
}

function liveCoverageVisibleRasters() {
  const rasters = [];
  for (const slot of liveCoverage.slots.values()) {
    if (slot.raster && !liveCoverage.hidden.has(slot.slot)) rasters.push({raster: slot.raster, slot});
  }
  return rasters;
}

function drawLiveCoverageTile(canvas, coords) {
  const {west, north} = liveCoverageTileBounds(coords);
  const step = liveCoverageWorld / (256 * 2 ** coords.z);
  const rasters = liveCoverageVisibleRasters().map(item => item.raster)
    .filter(r => r.west < west + 256 * step && r.east > west && r.south < north && r.north > north - 256 * step);
  drawLiveCoverageRaster(canvas, rasters, west, north, step, coords.x * 256 + coords.y * 256);
}

function drawLiveCoverageRaster(canvas, rasters, west, north, step, stripeOffset = 0) {
  const ctx = canvas.getContext('2d');
  const {width, height} = canvas;
  ctx.clearRect(0, 0, width, height);
  if (!rasters.length) return;
  const size = width * height;
  const best = new Float32Array(size).fill(-Infinity), count = new Uint8Array(size), unknown = new Uint8Array(size);
  for (const raster of rasters) {
    const px0 = Math.max(0, Math.ceil((raster.west - west) / step - .5)), px1 = Math.min(width - 1, Math.floor((raster.east - west) / step - .5));
    const py0 = Math.max(0, Math.ceil((north - raster.north) / step - .5)), py1 = Math.min(height - 1, Math.floor((north - raster.south) / step - .5));
    if (px0 > px1 || py0 > py1 || !(raster.header.pixel_size > 0)) continue;
    for (let py = py0; py <= py1; py++) {
      const y = north - (py + .5) * step;
      for (let px = px0, i = py * width + px0; px <= px1; px++, i++) {
        const margin = liveCoverageSample(raster, west + (px + .5) * step, y);
        if (margin === null) { unknown[i] = 1; continue; }
        if (!(margin >= 0)) continue;
        if (count[i] < 3) count[i]++;
        if (margin > best[i]) best[i] = margin;
      }
    }
  }
  const settings = liveCoverage.settings, palette = liveCoverage.palette, scaleMax = settings.scale_max_db;
  const overlap = liveCoverageHex(settings.overlap_color), overlapDark = overlap.map(value => value * .62);
  const image = ctx.createImageData(width, height), out = image.data;
  for (let i = 0, p = 0; i < size; i++, p += 4) {
    if (count[i]) {
      let rgb;
      if (settings.overlap && count[i] >= 2) rgb = count[i] >= 3 ? overlapDark : overlap;
      else {
        const k = Math.round(Math.min(1, best[i] / scaleMax) * 255) * 3;
        rgb = [palette[k], palette[k + 1], palette[k + 2]];
      }
      out[p] = rgb[0]; out[p + 1] = rgb[1]; out[p + 2] = rgb[2];
      // Fade the weakest few dB so the edge of coverage and the basemap stay readable.
      out[p + 3] = 255 * (.5 + .5 * Math.min(1, best[i] / 4));
    } else if (unknown[i] && settings.show_unknown) {
      const stripe = (stripeOffset + i % width + Math.floor(i / width)) % 10 < 3;
      out[p] = out[p + 1] = out[p + 2] = stripe ? 88 : 150;
      out[p + 3] = stripe ? 150 : 60;
    }
  }
  ctx.putImageData(image, 0, 0);
}

function liveCoverageReportSnapshot() {
  const settings = liveCoverage.settings;
  if (!map.hasLayer(liveCoverage.layer) || !settings.opacity || resultStale) return null;
  const sites = liveCoverageSites().filter(site => !liveCoverage.hidden.has(site.slot));
  const rasters = [];
  for (const site of sites) {
    const slot = liveCoverage.slots.get(site.slot);
    if (slot?.raster && !slot.raster.preview && !slot.loading && !slot.error
      && slot.key === liveCoverageKey(site)) rasters.push(slot.raster);
  }
  if (!rasters.length) return null;
  let west = Infinity, south = Infinity, east = -Infinity, north = -Infinity, finest = Infinity;
  for (const raster of rasters) {
    west = Math.min(west, raster.west);
    south = Math.min(south, raster.south);
    east = Math.max(east, raster.east);
    north = Math.max(north, raster.north);
    finest = Math.min(finest, raster.header.pixel_size);
  }
  const step = Math.max(finest, Math.max(east - west, north - south) / 768);
  const canvas = document.createElement('canvas');
  canvas.width = Math.min(768, Math.ceil((east - west) / step));
  canvas.height = Math.min(768, Math.ceil((north - south) / step));
  drawLiveCoverageRaster(canvas, rasters, west, north, step);
  const corner = (x, y) => {
    const {lat, lng} = L.CRS.EPSG3857.unproject(L.point(x, y));
    return [lat, lng];
  };
  const symbols = [];
  if (settings.overlap) symbols.push(
    ['Two sources', settings.overlap_color],
    ['Three or more sources', `rgb(${liveCoverageHex(settings.overlap_color).map(c => Math.round(c * .62)).join(',')})`]
  );
  if (settings.show_unknown) symbols.push(['Unknown terrain', 'repeating-linear-gradient(135deg,#666 0 2px,#ddd 2px 5px)']);
  return {
    image: canvas.toDataURL('image/png'),
    bounds: [corner(west, north - canvas.height * step), corner(west + canvas.width * step, north)],
    opacity: settings.opacity / 100,
    gradient: liveCoverageGradientCss(settings.colormap, settings.scale_max_db),
    max: settings.scale_max_db, mode: liveCoverageModes[settings.mode], symbols,
    ready: rasters.length, total: sites.length
  };
}

async function downloadLiveCoverageReport(event) {
  if (event.ctrlKey || event.metaKey || event.shiftKey || event.altKey) return;
  const link = event.currentTarget, status = $('route-export-status');
  if (link.getAttribute('aria-busy') === 'true') { event.preventDefault(); return; }
  status.textContent = '';
  let snapshot;
  try { snapshot = liveCoverageReportSnapshot(); }
  catch (error) {
    event.preventDefault();
    status.textContent = `Could not prepare live coverage. ${error.message}`;
    return;
  }
  if (!snapshot) return;
  event.preventDefault();
  const identity = [result.job_id, result.snapshot_version];
  link.setAttribute('aria-busy', 'true');
  status.textContent = 'Preparing HTML report…';
  try {
    const response = await fetch(link.href);
    if (!response.ok) throw Error(`Could not download the report (HTTP ${response.status}).`);
    const html = await response.text();
    // Fill the report's inert JSON slot only when it describes the captured route.
    const slot = /<script id="report-live-coverage" type="application\/json">([^<]*)<\/script>/.exec(html);
    if (!slot || JSON.stringify(JSON.parse(slot[1])) !== JSON.stringify(identity)) {
      throw Error('The route changed during export. Download the report again.');
    }
    const data = JSON.stringify([...identity, snapshot]).replaceAll('<', '\\u003c');
    const start = slot.index + slot[0].indexOf('>') + 1;
    const blob = new Blob([html.slice(0, start), data, html.slice(start + slot[1].length)], {type: 'text/html;charset=utf-8'});
    const url = URL.createObjectURL(blob);
    const download = document.createElement('a');
    download.href = url;
    download.download = 'route-report.html';
    download.click();
    setTimeout(() => URL.revokeObjectURL(url), 1000);
    status.textContent = '';
  } catch (error) {
    status.textContent = error.message;
  } finally {
    link.removeAttribute('aria-busy');
  }
}

function liveCoverageTileBounds(coords) {
  const m = liveCoverageWorld / (256 * 2 ** coords.z);
  const west = coords.x * 256 * m - liveCoverageWorld / 2, north = liveCoverageWorld / 2 - coords.y * 256 * m;
  return {west, north, east: west + 256 * m, south: north - 256 * m};
}

/* Repaint tiles that intersect the changed rasters (or every tile) on the next frame. */
function redrawLiveCoverage(...rasters) {
  if (liveCoverage.dirty !== 'all') {
    const boxes = rasters.filter(Boolean);
    if (!boxes.length) liveCoverage.dirty = 'all';
    else (liveCoverage.dirty ||= []).push(...boxes.map(({west, east, south, north}) => ({west, east, south, north})));
  }
  if (liveCoverage.frame) return;
  liveCoverage.frame = requestAnimationFrame(() => {
    liveCoverage.frame = 0;
    const dirty = liveCoverage.dirty;
    liveCoverage.dirty = null;
    const layer = liveCoverage.layer;
    if (layer?._map) {
      for (const tile of Object.values(layer._tiles || {})) {
        const bounds = liveCoverageTileBounds(tile.coords);
        if (dirty !== 'all' && !dirty.some(box => box.west < bounds.east && box.east > bounds.west && box.south < bounds.north && box.north > bounds.south)) continue;
        drawLiveCoverageTile(tile.el, tile.coords);
      }
    }
    renderLiveCoverageLegend();
  });
}

function liveCoverageSiteHeight(value, fallback) {
  const height = Number(value);
  return height > 0 && height <= 1000 ? height : fallback;
}

/* Every endpoint and router on the map that the source toggles include. */
function liveCoverageSites() {
  const sources = liveCoverage.settings.sources, sites = [], positions = new Set();
  const add = site => {
    if (![site.latitude, site.longitude, site.antenna_height_m].every(Number.isFinite)) return;
    const position = `${site.latitude.toFixed(5)},${site.longitude.toFixed(5)}`;
    if (positions.has(position) || sites.some(item => item.slot === site.slot)) return;
    positions.add(position);
    sites.push(site);
  };
  const routerHeight = rf?.router?.height_agl_m ?? 3;
  if (sources.endpoints && routeMode === 'mesh') {
    meshClients.forEach(client => add(meshClientCoverageSite(client)));
  } else if (sources.endpoints) {
    for (const name of ['a', 'b']) {
      const point = validEndpointCoords(name);
      if (point) add({slot: `endpoint:${name}`, id: name.toUpperCase(), label: name.toUpperCase(), group: 'Endpoint',
        kind: `endpoint_${name}`, latitude: point[0], longitude: point[1], antenna_height_m: liveCoverageSiteHeight(rf?.[`endpoint_${name}`]?.height_agl_m, 2)});
    }
  }
  if (sources.proposed) {
    manualRouters.filter(router=>router.policy!=='excluded').forEach((router, index) => add({slot: `manual:${router.id}`, id: router.id, label: router.name || `M${index + 1}`,
      group: 'Proposed router', kind: 'router', latitude: Number(router.latitude), longitude: Number(router.longitude),
      antenna_height_m: liveCoverageSiteHeight(router.antenna_height_m, routerHeight), terminal_clutter: router.terminal_clutter}));
  }
  if (sources.route && result?.found && !resultStale) {
    const active = result.mode === 'mesh' ? result.route || []
      : result.alternatives?.find(item => item.id === result.active_alternative_id)?.route || result.route || [];
    for (const site of active) {
      if (['endpoint_a', 'endpoint_b', 'client'].includes(site.kind)) continue;
      const known = site.origin === 'known';
      add({slot: known ? `known:${site.id}` : `route:${site.id}`, id: site.id, label: routeSiteLabel(site.id),
        group: known ? 'Existing router on route' : 'Route repeater', kind: 'router', latitude: Number(site.latitude),
        longitude: Number(site.longitude), antenna_height_m: liveCoverageSiteHeight(site.antenna_height_m, routerHeight), terminal_clutter: site.terminal_clutter});
    }
  }
  if (sources.existing) {
    for (const router of selectedKnownRouters()) {
      if (router.policy === 'excluded') continue;
      add({slot: `known:${router.id}`, id: router.id, label: router.name || router.id, group: 'Existing router', kind: 'router',
        latitude: Number(router.latitude), longitude: Number(router.longitude),
        antenna_height_m: router.height_override ? liveCoverageSiteHeight(router.antenna_height_m, routerHeight) : routerHeight, terminal_clutter: router.terminal_clutter});
    }
  }
  return sites;
}

function liveCoverageTerrainKey() {
  return JSON.stringify([terrainLoaded, $('dtm-count')?.dataset.tileCount, $('dom-count')?.dataset.tileCount, coverageTerrainTileCounts]);
}

function liveCoverageRequest(site, resolution) {
  const {mode, radius_km} = liveCoverage.settings;
  return {project_id: projectInfo?.active_id, mode, radius_m: radius_km * 1000, resolution_m: resolution,
    client: coverageSettings.client, site: {id: String(site.id).slice(0, 64), kind: site.kind, latitude: site.latitude,
      longitude: site.longitude, antenna_height_m: site.antenna_height_m,
      terminal_clutter: site.terminal_clutter ?? rf?.[site.kind]?.terminal_clutter}};
}

function liveCoverageKey(site, resolution = liveCoverage.settings.detail_m) {
  const body = liveCoverageRequest(site, resolution);
  body.site.latitude = site.latitude.toFixed(6);
  body.site.longitude = site.longitude.toFixed(6);
  // Radio overrides are keyed by site ID.
  if(!rf?.site_overrides?.[site.id])delete body.site.id;
  return JSON.stringify([body, liveCoverageTerrainKey(), JSON.stringify(rf)]);
}

async function fetchLiveCoverage(body, signal) {
  const response = await fetch('/api/live-coverage', {method: 'POST', headers: {'Content-Type': 'application/json'}, body: JSON.stringify(body), signal});
  if (!response.ok) {
    const problem = await response.json().catch(() => ({detail: response.statusText}));
    const detail = problem.detail ?? problem.error ?? response.statusText;
    const error = Error(typeof detail === 'string' ? detail : JSON.stringify(detail));
    error.status = response.status;
    throw error;
  }
  return decodeLiveCoverage(await response.arrayBuffer());
}

function liveCoverageCachePut(key, raster) {
  liveCoverage.cache.delete(key);
  liveCoverage.cache.set(key, raster);
  while (liveCoverage.cache.size > 32) liveCoverage.cache.delete(liveCoverage.cache.keys().next().value);
}

function scheduleLiveCoverage(delay = 80) {
  clearTimeout(liveCoverage.timer);
  liveCoverage.timer = setTimeout(syncLiveCoverage, delay);
}

/* Match slots to the current sites and queue detailed rasters for changed sites. */
function syncLiveCoverage() {
  if (!liveCoverage.layer || switchingProject || !projectInfo || !rf) return;
  // The server reads radio settings from the saved plan. Do not cache a
  // response under new settings while autosave is still committing them.
  if (savedRevision < saveRevision) return;
  syncProposedMeshLinks();
  restoreDisplacedLiveCoverage();
  const projectId = projectInfo?.active_id ?? null;
  const sites = liveCoverage.settings.enabled ? liveCoverageSites() : [];
  const present = new Set(sites.map(site => site.slot));
  const changed = [];
  // Without ground terrain every request would fail: say so once instead.
  const waiting = !terrainLoaded, waitingCopy = 'No terrain yet. Download terrain to see coverage.';
  if (waiting) liveCoverage.noTerrain = waitingCopy;
  else if (liveCoverage.noTerrain === waitingCopy) liveCoverage.noTerrain = '';
  for (const [id, slot] of liveCoverage.slots) {
    if (present.has(id) && slot.projectId === projectId) continue;
    slot.controller?.abort();
    liveCoverage.slots.delete(id);
    if (slot.raster) changed.push(slot.raster);
  }
  for (const site of sites) {
    let slot = liveCoverage.slots.get(site.slot);
    if (!slot) {
      slot = {slot: site.slot, projectId, raster: null, key: null};
      const preview = liveCoverage.lastPreview;
      // A dragged route repeater is pinned as a new proposed router: keep its preview on screen.
      if (preview && Math.abs(preview.latitude - site.latitude) < 1e-4 && Math.abs(preview.longitude - site.longitude) < 1e-4) slot.raster = preview.raster;
      liveCoverage.slots.set(site.slot, slot);
    }
    slot.site = site;
    if (liveCoverage.drag?.slot === site.slot) continue;
    if (waiting) {
      slot.controller?.abort();
      slot.key = null; slot.loading = false; slot.error = '';
      if (slot.raster) { changed.push(slot.raster); slot.raster = null; }
      continue;
    }
    const key = liveCoverageKey(site);
    if (slot.key === key && (slot.loading || slot.error || (slot.raster && !slot.raster.preview))) continue;
    slot.controller?.abort();
    slot.key = key; slot.error = ''; slot.loading = false;
    const cached = liveCoverage.cache.get(key);
    if (cached) {
      liveCoverageCachePut(key, cached);
      changed.push(slot.raster, cached);
      slot.raster = cached;
    } else {
      slot.loading = true;
      liveCoverage.queue.push(slot);
    }
  }
  if (changed.length) redrawLiveCoverage(...changed);
  renderLiveCoverageSites();
  pumpLiveCoverage();
}

async function pumpLiveCoverage() {
  while (liveCoverage.active < 2 && liveCoverage.queue.length) {
    const slot = liveCoverage.queue.shift();
    if (liveCoverage.slots.get(slot.slot) !== slot || !slot.loading) continue;
    liveCoverage.active++;
    void loadLiveCoverageSlot(slot).finally(() => {
      liveCoverage.active--;
      pumpLiveCoverage();
      renderLiveCoverageSites();
    });
  }
  renderLiveCoverageSites();
}

async function loadLiveCoverageSlot(slot) {
  const key = slot.key, controller = new AbortController();
  slot.controller = controller;
  try {
    // The server reads radio settings from the saved plan, so save pending edits first.
    await flushAutosave().catch(() => {});
    if (slot.key !== key || controller.signal.aborted) return;
    const raster = await fetchLiveCoverage(liveCoverageRequest(slot.site, liveCoverage.settings.detail_m), controller.signal);
    liveCoverageCachePut(key, raster);
    liveCoverage.noTerrain = '';
    slot.retries = 0;
    if (slot.key !== key || liveCoverage.slots.get(slot.slot) !== slot) return;
    const previous = slot.raster;
    slot.raster = raster;
    slot.loading = false;
    redrawLiveCoverage(previous, raster);
  } catch (error) {
    if (error.name === 'AbortError' || slot.key !== key) return;
    // Radio settings or terrain changed while this site was calculated: try again.
    if (error.status === 409 && /changed during the request/i.test(error.message) && (slot.retries = (slot.retries || 0) + 1) <= 3) {
      setTimeout(() => {
        if (liveCoverage.slots.get(slot.slot) !== slot || slot.key !== key) return;
        liveCoverage.queue.push(slot);
        pumpLiveCoverage();
      }, 300 * slot.retries);
      return;
    }
    slot.loading = false;
    slot.error = error.message;
    if (error.status === 409 && /terrain/i.test(error.message) && /download/i.test(error.message)) liveCoverage.noTerrain = error.message;
    if (slot.raster?.preview) { const previous = slot.raster; slot.raster = null; redrawLiveCoverage(previous); }
  } finally {
    if (slot.controller === controller) slot.controller = null;
  }
}


const proposedMeshLinks={pairs:new Map(),active:0,projectId:null,layer:null,flush:null,dispatch:0};

function proposedMeshSites(drag){
  const sites=new Map();
  const add=(site,id,kind,height,isProposed=false)=>{
    if(![site.latitude,site.longitude,height].every(Number.isFinite)||sites.has(id))return;
    const moving=drag?.site.id===id;
    sites.set(id,{site:{id,kind,latitude:moving?drag.latlng.lat:site.latitude,
      longitude:moving?drag.latlng.lng:site.longitude,antenna_height_m:height,
      terminal_clutter:site.terminal_clutter},isProposed});
  };
  if(routeMode==='mesh')for(const client of meshClients){
    add(meshClientCoverageSite(client),client.id,'client',client.antenna_height_m);
  }
  for(const key of routeMode==='mesh'?[]:['a','b']){
    const point=validEndpointCoords(key);
    if(point)add({latitude:point[0],longitude:point[1]},key.toUpperCase(),'endpoint_'+key,rf['endpoint_'+key].height_agl_m);
  }
  const routerHeight=rf.router.height_agl_m;
  for(const router of manualRouters){
    if(router.policy!=='excluded')add(router,router.id,'router',liveCoverageSiteHeight(router.antenna_height_m,routerHeight),true);
  }
  for(const router of selectedKnownRouters()){
    if(router.policy!=='excluded')add(router,router.id,'router',router.height_override?router.antenna_height_m:routerHeight);
  }
  for(const site of searchRouterSites()){
    add(site,site.id,site.kind,site.antenna_height_m);
  }
  return sites;
}
function mapLinkPairId(a,b){
  return a<b?a+'|'+b:b+'|'+a;
}
function removeProposedMeshLink(id,pair){
  pair.controller?.abort();
  pair.line?.removeFrom(proposedMeshLinks.layer);
  proposedMeshLinks.pairs.delete(id);
}
function pruneProposedMeshLinks(){
  if(!proposedMeshLinks.pairs.size)return;
  const sites=proposedMeshSites();
  for(const [id,pair] of proposedMeshLinks.pairs){
    const {source,target}=pair.wanted;
    if(!sites.has(source.id)||!sites.has(target.id))removeProposedMeshLink(id,pair);
  }
}
function syncProposedMeshLinks(){
  if(!proposedMeshLinks.layer)return;
  const projectId=projectInfo?.active_id;
  if(proposedMeshLinks.projectId!==projectId||!terrainLoaded){
    for(const pair of proposedMeshLinks.pairs.values())pair.controller?.abort();
    proposedMeshLinks.pairs.clear();
    proposedMeshLinks.layer.clearLayers();
    proposedMeshLinks.projectId=projectId;
  }
  if(!projectId||!terrainLoaded||switchingProject||!rf)return;
  const drag=liveCoverage.drag, sites=proposedMeshSites(drag), wanted=new Set();
  const routePairs=new Set(resultStale?[]:(result?.links||[]).map(link=>mapLinkPairId(link.source_id,link.target_id)));
  const radioKey=JSON.stringify([rf,liveCoverageTerrainKey()]);
  for(const {site:source,isProposed} of sites.values()){
    if(!isProposed)continue;
    for(const {site:target} of sites.values()){
      if(source.id===target.id||Math.hypot(source.latitude-target.latitude,source.longitude-target.longitude)<.000001)continue;
      const id=mapLinkPairId(source.id,target.id);
      if(routePairs.has(id)||wanted.has(id))continue;
      wanted.add(id);
      const key=JSON.stringify([radioKey,source,target]);
      let pair=proposedMeshLinks.pairs.get(id);
      if(!pair){
        pair={id,projectId};
        proposedMeshLinks.pairs.set(id,pair);
      }
      if(pair.wanted?.key!==key){
        pair.wanted={key,source,target};
        pair.line?.setLatLngs([mapPoint(source),mapPoint(target)]);
      }
    }
  }
  for(const [id,pair] of proposedMeshLinks.pairs){
    if(!wanted.has(id))removeProposedMeshLink(id,pair);
  }
  pumpProposedMeshLinks();
}
function pumpProposedMeshLinks(){
  if(mapEditsLocked()||proposedMeshLinks.active>=2)return;
  const pending=[...proposedMeshLinks.pairs.values()].filter(pair=>!pair.controller&&pair.checked!==pair.wanted.key);
  pending.sort((a,b)=>(a.dispatched??0)-(b.dispatched??0));
  for(const pair of pending){
    pair.dispatched=++proposedMeshLinks.dispatch;
    proposedMeshLinks.active++;
    void refreshProposedMeshLink(pair).finally(()=>{
      proposedMeshLinks.active--;
      pumpProposedMeshLinks();
    });
    if(proposedMeshLinks.active>=2)break;
  }
}
async function refreshProposedMeshLink(pair){
  const {key,source,target}=pair.wanted, controller=new AbortController();
  pair.controller=controller;
  try{
    if(!proposedMeshLinks.flush){
      proposedMeshLinks.flush=flushAutosave().finally(()=>{proposedMeshLinks.flush=null;});
    }
    await proposedMeshLinks.flush;
    if(controller.signal.aborted||pair.wanted.key!==key)return;
    let link;
    try{
      link=await requestMapLink(source,target,{projectId:pair.projectId,signal:controller.signal});
    }catch(error){
      if(error.name==='AbortError')return;
    }
    if(!controller.signal.aborted&&(pair.wanted.key===key||liveCoverage.drag)){
      renderProposedMeshLink(pair,link);
    }
    pair.checked=key;
  }catch{
    // Saving failed. The next saved-plan change can retry this pair.
    pair.checked=key;
  }finally{
    pair.controller=null;
  }
}
function renderProposedMeshLink(pair,link){
  if(!link||!(link.worst_margin_db>=0)){
    pair.line?.removeFrom(proposedMeshLinks.layer);
    pair.line=null;
    return;
  }
  pair.link=link;
  const style={...routeLinkMapStyle({...link,valid:true},false),weight:2,dashArray:'2 5',className:'map-proposed-mesh-link'};
  const points=[mapPoint(pair.wanted.source),mapPoint(pair.wanted.target)];
  if(pair.line){
    pair.line.setLatLngs(points).setStyle(style).setTooltipContent(proposedMeshTooltip(link));
  }else{
    pair.line=new MapLinkLine(points,style).bindTooltip(proposedMeshTooltip(link));
    bindMapLink(pair.line,()=>pair.link,()=>void showProposedLinkInAnalysis(pair),signal=>requestProposedLinkProfile(pair,signal));
    pair.line.addTo(proposedMeshLinks.layer);
  }
}
function proposedMeshTooltip(link){
  return mapText('span',mapLinkLabel(link)+' · '+formatMetric(link.worst_margin_db)+' dB · Proposed mesh link');
}
function proposedLinksAtSite(id){
  return [...proposedMeshLinks.pairs.values()]
    .filter(({line,link})=>line&&(link.source_id===id||link.target_id===id))
    .map(pair=>({
      link:pair.link,
      showAnalysis:()=>void showProposedLinkInAnalysis(pair),
      loadProfile:signal=>requestProposedLinkProfile(pair,signal)
    }));
}
function requestProposedLinkProfile(pair,signal){
  const {source,target}=pair.wanted;
  return requestMapLink(source,target,{projectId:pair.projectId,profile:true,signal});
}
async function showProposedLinkInAnalysis(pair){
  const section=openMapAnalysisSelection(mapLinkLabel(pair.link));
  const body=mapText('div','Loading terrain profile…');
  body.setAttribute('role','status');
  section.append(body);
  try{
    const link=await requestProposedLinkProfile(pair);
    if(body.isConnected)renderRouteInspectorLink({body},link);
  }catch(error){
    if(body.isConnected)body.textContent=error.message;
  }
}


/* Follow a dragged marker with quick coarse previews, one request at a time. */
function bindLiveCoverageDrag(marker, describe) {
  marker.on('dragstart', () => {
    const site = describe();
    if (!site || mapEditsLocked()) return;
    liveCoverage.drag = {marker, describe, site, slot: site.slot, latlng: marker.getLatLng().wrap()};
    syncProposedMeshLinks();
    requestLiveCoveragePreview();
  });
  marker.on('drag', () => {
    if (liveCoverage.drag?.marker !== marker) return;
    liveCoverage.drag.latlng = marker.getLatLng().wrap();
    syncProposedMeshLinks();
    requestLiveCoveragePreview();
  });
  marker.on('dragend', () => {
    if (liveCoverage.drag?.marker !== marker) return;
    liveCoverage.drag = null;
    scheduleLiveCoverage(0);
  });
}

async function requestLiveCoveragePreview() {
  const drag = liveCoverage.drag;
  if (!drag || !liveCoverage.slots.has(drag.slot) || liveCoverage.previewBusy) return;
  const site = drag.describe(), latlng = drag.latlng;
  if (!site) return;
  liveCoverage.previewBusy = true;
  const preview = {...site, latitude: latlng.lat, longitude: latlng.lng};
  try {
    const resolution = Math.min(500, Math.max(liveCoverage.settings.detail_m * 4, 120));
    const raster = await fetchLiveCoverage(liveCoverageRequest(preview, resolution));
    raster.preview = true;
    const slot = liveCoverage.slots.get(site.slot);
    if (slot && (liveCoverage.drag?.slot === site.slot || slot.loading)) {
      const previous = slot.raster;
      slot.raster = raster;
      liveCoverage.lastPreview = {latitude: latlng.lat, longitude: latlng.lng, raster};
      redrawLiveCoverage(previous, raster);
    }
  } catch {
    // The detailed request after release reports errors.
  } finally {
    liveCoverage.previewBusy = false;
  }
  const next = liveCoverage.drag;
  if (next && next.slot === site.slot && !next.latlng.equals(latlng)) requestLiveCoveragePreview();
}

function liveCoverageProgress() {
  const slots = [...liveCoverage.slots.values()];
  return {total: slots.length, loading: slots.filter(slot => slot.loading).length, errors: slots.filter(slot => slot.error).length};
}

function liveCoverageStatusText() {
  const {total, loading, errors} = liveCoverageProgress();
  if (!liveCoverage.settings.enabled) return 'Live coverage is off.';
  if (liveCoverage.displaced) return 'Live coverage is hidden while a saved run is shown on the map.';
  if (liveCoverage.noTerrain) return liveCoverage.noTerrain;
  if (!total) return 'Add endpoints or routers to see their coverage.';
  if (loading) return `Updating coverage · ${total - loading} of ${total} sites ready…`;
  if (errors) return `Coverage ready for ${total - errors} of ${total} sites · ${errors} failed.`;
  return `Live coverage · ${total} site${total === 1 ? '' : 's'} up to date.`;
}

function renderLiveCoverageSites() {
  const list = $('live-coverage-sites');
  const status = liveCoverageStatusText();
  const action = $('live-coverage-status');
  if (action && action.textContent !== status) { action.textContent = status; action.title = status; }
  const fallback = $('live-coverage-model-note');
  const header = [...liveCoverage.slots.values()]
    .filter(slot => !slot.loading && !slot.error)
    .map(slot => slot.raster?.header)
    .find(header => header?.diffraction_model_configured && header.diffraction_model_configured !== header.diffraction_model);
  if (header && liveCoverage.settings.enabled && !liveCoverage.displaced) {
    const note = `Live coverage uses ${diffractionModelName(header.diffraction_model)}. ${diffractionModelName(header.diffraction_model_configured)} applies to route search.`;
    if (fallback.textContent !== note) fallback.textContent = note;
    fallback.hidden = false;
  } else fallback.hidden = true;
  if (!list) return;
  const noTerrain = $('live-coverage-no-terrain');
  if (noTerrain) noTerrain.hidden = !liveCoverage.noTerrain;
  const focused = document.activeElement?.dataset?.liveSlot;
  const rows = [];
  let unknown = 0;
  for (const slot of liveCoverage.slots.values()) {
    const site = slot.site, row = document.createElement('label'), box = document.createElement('input');
    row.className = 'live-site';
    box.type = 'checkbox';
    box.checked = !liveCoverage.hidden.has(slot.slot);
    box.dataset.liveSlot = slot.slot;
    box.setAttribute('data-display-only', '');
    box.onchange = () => {
      if (box.checked) liveCoverage.hidden.delete(slot.slot); else liveCoverage.hidden.add(slot.slot);
      redrawLiveCoverage(slot.raster);
    };
    const name = document.createElement('strong'), detail = document.createElement('small');
    name.textContent = site.label;
    name.title = `${site.group} · ${site.id}`;
    const header = slot.raster?.header;
    if (header && !slot.raster.preview) unknown = Math.max(unknown, header.unknown_fraction || 0);
    detail.textContent = liveCoverage.noTerrain && !slot.raster && !slot.loading ? 'Waiting for terrain'
      : slot.error ? slot.error
      : slot.loading ? (slot.raster?.preview ? 'Preview · refining…' : 'Calculating…')
      : header ? `${(header.covered_fraction * 100).toFixed(0)}% of ${(header.radius_m / 1000).toFixed(0)} km radius${header.unknown_fraction > .005 ? ` · ${(header.unknown_fraction * 100).toFixed(0)}% no terrain` : ''}`
      : 'Waiting…';
    if (slot.error && !liveCoverage.noTerrain) detail.className = 'error';
    row.append(box, name, detail);
    rows.push(row);
  }
  if (!rows.length) {
    const empty = document.createElement('p');
    empty.className = 'hint';
    empty.textContent = liveCoverage.settings.enabled ? 'Add endpoints or routers to see their coverage.' : 'Live coverage is off.';
    rows.push(empty);
  }
  list.replaceChildren(...rows);
  if (focused) list.querySelector(`[data-live-slot="${CSS.escape(focused)}"]`)?.focus({preventScroll: true});
  const gap = $('live-terrain-gap');
  if (gap) {
    gap.hidden = !(unknown > .01) || !!coverageTerrainJobId;
    $('live-terrain-copy').textContent = unknown > .01 ? `Up to ${(unknown * 100).toFixed(0)}% of a site's radius has no terrain data (hatched on the map).` : '';
  }
  renderLiveCoverageLegend();
}

function renderLiveCoverageLegend() {
  const legend = $('live-coverage-legend');
  if (!legend) return;
  const settings = liveCoverage.settings, visible = liveCoverageVisibleRasters();
  legend.hidden = !settings.enabled || !liveCoverage.layer?._map || !visible.length;
  if (legend.hidden) return;
  $('live-legend-caption').textContent = `${liveCoverageModes[settings.mode]} · handheld ${coverageSettings.client.height_agl_m} m`;
  const ticks = $('live-legend-ticks'), max = settings.scale_max_db;
  const values = [0, max / 3, 2 * max / 3, max].map(value => `${Math.round(value)}${value === max ? '+' : ''}`);
  if (ticks.dataset.key !== values.join()) {
    ticks.dataset.key = values.join();
    ticks.replaceChildren(...values.map(value => Object.assign(document.createElement('span'), {textContent: value})));
  }
  $('live-legend-ramp').style.background = liveCoverageGradientCss(settings.colormap, max);
  $('live-legend-overlap-items').hidden = !settings.overlap;
  $('live-legend-overlap-2').style.background = settings.overlap_color;
  $('live-legend-overlap-3').style.background = `rgb(${liveCoverageHex(settings.overlap_color).map(value => Math.round(value * .62)).join(',')})`;
  const unknown = visible.some(({raster}) => raster.header.unknown_fraction > .005);
  $('live-legend-unknown').hidden = !unknown || !settings.show_unknown;
  const {total, loading} = liveCoverageProgress();
  $('live-legend-status').textContent = loading ? `Updating ${loading} of ${total}…` : `${visible.length} of ${total} site${total === 1 ? '' : 's'} shown`;
}

function liveCoverageHoverText(latlng) {
  const point = L.CRS.EPSG3857.project(latlng), covered = [];
  let unknown = false;
  for (const {raster, slot} of liveCoverageVisibleRasters()) {
    const margin = liveCoverageSample(raster, point.x, point.y);
    if (margin === null) unknown = true;
    else if (margin >= 0) covered.push([slot.site.label, margin]);
  }
  if (!covered.length) return unknown ? 'No terrain data here' : null;
  covered.sort((a, b) => b[1] - a[1]);
  const heading = covered.length > 1 ? `${covered.length} sites overlap` : `Covered by ${covered[0][0]}`;
  const lines = covered.slice(0, 6).map(([label, margin]) => `${label} · ${margin.toFixed(1)} dB`);
  if (covered.length > 6) lines.push(`+${covered.length - 6} more`);
  return [heading, ...lines];
}

function showLiveCoverageHover(event) {
  if (liveCoverage.hoverFrame) return;
  liveCoverage.hoverFrame = requestAnimationFrame(() => {
    liveCoverage.hoverFrame = 0;
    const tooltip = liveCoverage.hover;
    const blocked = interactionMode !== 'browse' || !liveCoverage.layer?._map || !liveCoverage.settings.opacity
      || event.originalEvent?.target?.closest?.('.leaflet-interactive, .leaflet-marker-icon');
    const text = blocked ? null : liveCoverageHoverText(event.latlng);
    if (!text) { tooltip.remove(); return; }
    const content = document.createElement('div');
    for (const [index, line] of [].concat(text).entries()) content.append(index ? mapText('span', line) : mapText('strong', line));
    tooltip.setLatLng(event.latlng).setContent(content);
    if (!map.hasLayer(tooltip)) tooltip.addTo(map);
  });
}

function setLiveCoverageEnabled(enabled) {
  liveCoverage.settings.enabled = enabled;
  liveCoverageSaveSettings();
  liveCoverage.displaced = false;
  if (enabled && !map.hasLayer(liveCoverage.layer)) {
    if (map.hasLayer(predictedCoverageLayer)) map.removeLayer(predictedCoverageLayer);
    liveCoverage.layer.addTo(map);
  } else if (!enabled && map.hasLayer(liveCoverage.layer)) map.removeLayer(liveCoverage.layer);
  syncLiveCoverageControls();
  syncLiveCoverage();
}

function syncLiveCoverageControls() {
  const s = liveCoverage.settings;
  const set = (id, value, prop = 'value') => { const element = $(id); if (element) element[prop] = value; };
  set('live-enabled', s.enabled, 'checked');
  for (const id of ['live-colormap', 'live-legend-colormap']) set(id, s.colormap);
  set('live-scale', String(s.scale_max_db));
  for (const id of ['live-overlap', 'live-legend-overlap']) set(id, s.overlap, 'checked');
  set('live-overlap-color', s.overlap_color);
  set('live-show-unknown', s.show_unknown, 'checked');
  set('live-mode', s.mode);
  set('live-radius', String(s.radius_km));
  set('live-detail', String(s.detail_m));
  for (const [group, value] of Object.entries(s.sources)) set(`live-src-${group}`, value, 'checked');
  set('live-opacity', String(s.opacity));
  const output = $('live-opacity-value');
  if (output) output.value = `${s.opacity}%`;
  $('live-opacity')?.setAttribute('aria-valuetext', `${s.opacity}%`);
}

/* Display-only changes repaint; model changes request new rasters. */
function changeLiveCoverageSetting(key, value, recompute = false) {
  liveCoverage.settings[key] = value;
  liveCoverageSaveSettings();
  if (key === 'colormap' || key === 'scale_max_db') liveCoverage.palette = liveCoveragePalette(liveCoverage.settings.colormap, liveCoverage.settings.scale_max_db);
  if (key === 'opacity') liveCoverage.layer.setOpacity(value / 100);
  syncLiveCoverageControls();
  if (recompute) syncLiveCoverage(); else redrawLiveCoverage();
}

function bindLiveCoverageControls() {
  for (const id of ['live-colormap', 'live-legend-colormap']) {
    const select = $(id);
    if (!select) continue;
    for (const [value, {label}] of Object.entries(liveCoverageColormaps)) select.add(new Option(label, value));
    select.onchange = () => changeLiveCoverageSetting('colormap', select.value);
  }
  for (const [value, label] of liveCoverageDetailLevels) $('live-detail')?.add(new Option(label, String(value)));
  for (const [value, label] of Object.entries(liveCoverageModes)) $('live-mode')?.add(new Option(label, value));
  for (const id of ['live-overlap', 'live-legend-overlap']) {
    const box = $(id);
    if (box) box.onchange = () => changeLiveCoverageSetting('overlap', box.checked);
  }
  $('live-enabled').onchange = event => setLiveCoverageEnabled(event.target.checked);
  $('live-scale').onchange = event => changeLiveCoverageSetting('scale_max_db', Number(event.target.value));
  $('live-overlap-color').oninput = event => changeLiveCoverageSetting('overlap_color', event.target.value);
  $('live-show-unknown').onchange = event => changeLiveCoverageSetting('show_unknown', event.target.checked);
  $('live-mode').onchange = event => changeLiveCoverageSetting('mode', event.target.value, true);
  $('live-detail').onchange = event => changeLiveCoverageSetting('detail_m', Number(event.target.value), true);
  $('live-opacity').oninput = event => changeLiveCoverageSetting('opacity', Number(event.target.value));
  $('live-radius').onchange = event => {
    const field = event.target, error = $('live-radius-error'), raw = field.value.trim(), value = Number(raw);
    const valid = raw !== '' && Number.isFinite(value) && value >= 1 && value <= 80;
    field.setAttribute('aria-invalid', String(!valid));
    error.hidden = valid;
    if (!valid) { error.textContent = 'Enter a radius from 1 to 80 km.'; field.focus(); return; }
    changeLiveCoverageSetting('radius_km', value, true);
  };
  for (const group of Object.keys(liveCoverageDefaults.sources)) {
    const box = $(`live-src-${group}`);
    if (box) box.onchange = () => { liveCoverage.settings.sources[group] = box.checked; liveCoverageSaveSettings(); syncLiveCoverage(); };
  }
  $('live-coverage-refresh').onclick = () => {
    liveCoverage.cache.clear();
    liveCoverage.noTerrain = '';
    for (const slot of liveCoverage.slots.values()) { slot.key = null; slot.error = ''; }
    syncLiveCoverage();
  };
  $('live-coverage-open-terrain').onclick = () => {
    setWorkspace('workspace-plan');
    setPlanSection('terrain-setup');
    $('terrain-setup').scrollIntoView({behavior: 'smooth', block: 'start'});
  };
  $('live-terrain-download').onclick = () => void requestLiveCoverageTerrain();
  const collapse = $('live-legend-collapse');
  collapse.onclick = () => {
    const expanded = collapse.getAttribute('aria-expanded') !== 'true';
    collapse.setAttribute('aria-expanded', String(expanded));
    collapse.textContent = expanded ? 'Hide' : 'Show';
    $('live-legend-body').hidden = !expanded;
  };
  // Keep the map usable on phones: the legend collapses at phone widths.
  const phone = window.matchMedia('(max-width: 650px)');
  let autoCollapsed = false;
  const fitLegend = () => {
    const expanded = collapse.getAttribute('aria-expanded') === 'true';
    if (phone.matches && expanded) { collapse.click(); autoCollapsed = true; }
    else if (!phone.matches && !expanded && autoCollapsed) { collapse.click(); autoCollapsed = false; }
  };
  collapse.addEventListener('click', event => { if (event.isTrusted) autoCollapsed = false; });
  phone.addEventListener('change', fitLegend);
  fitLegend();
}

/* Prepare supplemental terrain for the union of every site's coverage radius. */
async function requestLiveCoverageTerrain() {
  const sites = [...liveCoverage.slots.values()].map(slot => ({latitude: slot.site.latitude, longitude: slot.site.longitude}));
  if (!sites.length || coverageTerrainJobId) return;
  const button = $('live-terrain-download');
  button.disabled = true;
  coveragePreflightBusy = true; syncCoverageLocks();
  setCoverageTerrainStatus('Estimating missing terrain around the coverage sites…');
  try {
    await autosaveNow();
    const request = {method: 'POST', headers: {'Content-Type': 'application/json'},
      body: JSON.stringify({live: {sites: sites.slice(0, 256), radius_m: liveCoverage.settings.radius_km * 1000}})};
    const estimate = await api('/api/coverage/terrain/estimate', request);
    if (!estimate.tile_count) {
      setCoverageTerrainStatus('Installed terrain tiles already span every coverage radius. Remaining gaps are missing elevation values in the source data.');
      return;
    }
    const copy = coverageTerrainEstimateCopy(estimate);
    const action = terrainFromCache(estimate) ? cachedTerrainAction(estimate) : 'Download terrain';
    const confirmed = await askActionDialog({title: action+' for live coverage?', copy, confirm: action, terrainEstimate: estimate});
    if (!confirmed) { setCoverageTerrainStatus(''); return; }
    const job = await api('/api/coverage/terrain/prepare', request);
    if (!job.job_id) { setCoverageTerrainStatus(job.stage); return; }
    coverageTerrainJobId = job.job_id;
    renderCoverageTerrainProgress(job);
    void pollCoverageTerrain();
  } catch (error) {
    setCoverageTerrainStatus(error.message, true);
  } finally {
    button.disabled = false;
    if (!coverageTerrainJobId) { coveragePreflightBusy = false; syncCoverageLocks(); }
    renderLiveCoverageSites();
  }
}

function restoreDisplacedLiveCoverage() {
  // A saved run that is still calculating or loading keeps its place on the map.
  const savedRunShown = map.hasLayer(predictedCoverageLayer)
    && (predictedCoverageLayer.getLayers().length || coverageBusy || coverageLoadingRun);
  if (!liveCoverage.displaced || savedRunShown) return;
  liveCoverage.displaced = false;
  if (liveCoverage.settings.enabled) liveCoverage.layer.addTo(map);
  renderLiveCoverageSites();
}

/* Called once from app.js after the map, plan and controls exist. */
function initLiveCoverage() {
  proposedMeshLinks.layer=L.layerGroup().addTo(map);
  $('route-export-html').addEventListener('click', downloadLiveCoverageReport);
  const LiveCoverageLayer = L.GridLayer.extend({
    createTile(coords) {
      const canvas = L.DomUtil.create('canvas', 'live-coverage-tile');
      canvas.width = canvas.height = 256;
      drawLiveCoverageTile(canvas, coords);
      return canvas;
    },
  });
  liveCoverage.palette = liveCoveragePalette(liveCoverage.settings.colormap, liveCoverage.settings.scale_max_db);
  liveCoverage.layer = new LiveCoverageLayer({pane: 'coveragePane', tileSize: 256, noWrap: true, keepBuffer: 2,
    updateWhenZooming: false, opacity: liveCoverage.settings.opacity / 100, className: 'live-coverage-layer'});
  liveCoverage.hover = L.tooltip({direction: 'top', className: 'coverage-sample-tooltip live-coverage-tooltip', offset: [0, -8]});
  plannerMapOverlays['Live coverage'] = liveCoverage.layer;
  mapLayerControl?.addOverlay(liveCoverage.layer, 'Live coverage');
  liveCoverage.layer.on('add remove', event => {
    const enabled = event.type === 'add';
    if (enabled) liveCoverage.displaced = false;
    if (!liveCoverage.displaced && liveCoverage.settings.enabled !== enabled) { liveCoverage.settings.enabled = enabled; liveCoverageSaveSettings(); syncLiveCoverageControls(); syncLiveCoverage(); }
    if (!enabled) liveCoverage.hover.remove();
    renderLiveCoverageLegend();
  });
  // A saved fixed-grid run and the live map would paint the same pane: show one at a time.
  // The live layer steps aside without turning off and returns when the saved run goes.
  predictedCoverageLayer.on('add', () => {
    if (!map.hasLayer(liveCoverage.layer)) return;
    liveCoverage.displaced = true;
    map.removeLayer(liveCoverage.layer);
    renderLiveCoverageSites();
  });
  predictedCoverageLayer.on('remove', restoreDisplacedLiveCoverage);
  map.on('mousemove', showLiveCoverageHover).on('mouseout movestart', () => liveCoverage.hover.remove());
  bindLiveCoverageControls();
  syncLiveCoverageControls();
  if (liveCoverage.settings.enabled) liveCoverage.layer.addTo(map);
  // Plan edits, results and terrain changes all pass through these functions.
  for (const name of ['scheduleCoverageCheck', 'render', 'resetRouteResultView', 'renderManualRouters', 'renderMeshcoreRouters', 'syncCoverageInputs', 'readCoverageInputs']) {
    const original = window[name];
    if (typeof original !== 'function') continue;
    window[name] = function (...args) {
      const value = original.apply(this, args);
      scheduleLiveCoverage();
      return value;
    };
  }
  const refreshState = window.refresh;
  window.refresh = async (...args) => {
    const value = await refreshState(...args);
    scheduleLiveCoverage();
    return value;
  };
  // A cheap safety net for edits that bypass the hooks (undo, project switch).
  liveCoverage.interval = setInterval(() => { if (!document.hidden) syncLiveCoverage(); }, 1500);
  scheduleLiveCoverage(0);
}
