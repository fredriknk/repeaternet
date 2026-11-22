const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

const app = fs.readFileSync(path.join(__dirname, '../../assets/dashboard/app.js'), 'utf8');
function section(start, end) {
  const first = app.indexOf(start), last = app.indexOf(end, first);
  assert.ok(first >= 0 && last > first);
  return app.slice(first, last);
}

test('terrain estimation sends the route plan over HTTP without crypto.randomUUID', async () => {
  const elements = new Map();
  const $ = id => {
    if (!elements.has(id)) elements.set(id, {});
    return elements.get(id);
  };
  const plan = { a: [59.91, 10.75], b: [59.95, 10.85] };
  const errors = [], requests = [];
  const context = vm.createContext({
    $,
    crypto: { getRandomValues: bytes => bytes.fill(0xff) },
    clone: structuredClone, plan: () => plan,
    projectInfo: { active_id: 'project' }, saveRevision: 1, dataSourcesRevision: 1,
    mapEditsLocked: () => false, validTerrainResolution: () => true,
    validEndpointCoords: name => plan[name],
    clearTerrainEstimateReview() {}, previewTerrainSources() {}, renderTerrainEstimateDetail() {}, setTerrainDomNote() {},
    terrainEstimateReview: null, terrainEstimateCost: () => '1 tile',
    sourceJsonRequest: (method, snapshot) => ({ method, body: JSON.stringify(snapshot) }),
    async api(url, request) {
      requests.push([url, request.method, JSON.parse(request.body)]);
      return { summary: 'Terrain available' };
    },
    status(message, error) { if (error) errors.push(message); },
  });
  vm.runInContext(
    section('function randomId(', 'function setHistoryButtons(') +
    section('function createRouteOperation(', 'async function startRouteSearch(') +
    section('async function estimateTerrain(', 'async function startTerrainPreparation(') +
    section('function routeOperationIsCurrent(', 'async function pollTerrainJob('),
    context,
  );
  await $('estimate-terrain').onclick();
  assert.deepEqual(errors, []);
  assert.deepEqual(requests, [['/api/terrain/estimate', 'POST', plan]]);
  assert.equal(context.terrainEstimateReview.operation.id, 'ffffffff-ffff-4fff-bfff-ffffffffffff');
  assert.equal($('prepare-terrain').hidden, false);
});

test('registry definitions populate the layer control and a failed GET leaves the fallback', async () => {
  const visible = new Set(), controls = [], plannerLayer = {};
  const tileLayer = (url, options) => ({
    kind: 'xyz', url, options,
    addTo() { visible.add(this); return this; },
  });
  tileLayer.wms = (url, options) => ({ ...tileLayer(url, options), kind: 'wms' });
  const map = {
    getZoom: () => 12,
    getBounds: () => ({ getSouth: () => -24, getNorth: () => -22, getWest: () => -44, getEast: () => -42 }),
    hasLayer: layer => visible.has(layer),
    removeLayer: layer => visible.delete(layer),
    off() {},
  };
  let registry, remembered;
  const context = vm.createContext({
    map, clearTimeout, localStorage: { getItem: () => remembered },
    plannerMapOverlays: { 'Terrain coverage': plannerLayer },
    sourceHtmlText: value => value,
    L: {
      Util: { wrapNum: value => value }, tileLayer,
      control: {
        layers: (_, overlays) => ({
          base: [], overlays, addedOverlays: [],
          addBaseLayer(layer, label) { this.base.push({ layer, label }); },
          addOverlay(layer, label) { this.addedOverlays.push({ layer, label }); },
          addTo() { controls.push(this); },
          remove() { this.removed = true; },
          getContainer() { return { querySelector() {}, addEventListener() {} }; },
        }),
      },
    },
    async api(url) {
      assert.equal(url, '/api/data-sources');
      if (!registry) throw Error('Not found');
      return registry;
    },
    dataSources: null, dataSourcesError: '', dataSourcesRevision: 0, terrainPreviewKey: '',
    clearTerrainEstimateReview() {}, updateTerrainSourceSelect() {}, scheduleTerrainSourcePreview() {},
  });
  vm.runInContext(
    section('const fallbackBasemaps =', "map.createPane('coveragePane')") +
    section('function makeSourceMapLayer(', "map.on('layeradd layerremove'") +
    section('function applySourceRegistry(', 'const sourceKindLabels=') +
    'rebuildSourceLayers(fallbackBasemaps);', context,
  );
  const fallback = controls[0];
  await context.loadDataSources();
  assert.match(context.dataSourcesError, /Not found/);
  assert.equal(controls.length, 1);
  assert.match(fallback.base[0].label, /OpenStreetMap/);
  assert.match(fallback.base[1].label, /Kartverket topo.*outside view/);
  assert.ok(visible.has(fallback.base[0].layer));

  const url = 'https://example.org';
  registry = { basemaps: [
    { id: 'regional', name: 'Regional', kind: 'xyz', url, default: true, coverage: { type: 'bbox', bbox: [4, 57.9, 31.3, 71.3] } },
    { id: 'world', name: 'World', kind: 'xyz', url, max_zoom: 19 },
    { id: 'alternate', name: 'Alternate', kind: 'xyz', url },
    { id: 'overlay', name: 'WMS overlay', kind: 'wms', url, overlay: true, opacity: 0.4, wms: { layers: 'contours' } },
    { id: 'disabled', name: 'Disabled', kind: 'xyz', url, enabled: false },
  ] };
  remembered = 'alternate';
  await context.loadDataSources();
  const control = controls[1];
  assert.equal(fallback.removed, true);
  assert.equal(control.base.length, 3);
  assert.ok(visible.has(control.base[2].layer));
  assert.equal(control.base[1].layer.options.maxZoom, 19);
  assert.equal(control.overlays['Terrain coverage'], plannerLayer);
  const { layer: overlay, label } = control.addedOverlays[0];
  assert.match(label, /WMS overlay/);
  assert.equal(overlay.kind, 'wms');
  assert.equal(overlay.options.layers, 'contours');
  assert.equal(overlay.options.opacity, 0.4);

  overlay.addTo();
  remembered = 'world';
  context.applySourceRegistry(registry);
  assert.ok(visible.has(controls[2].base[2].layer), 'refresh keeps the selected base');
  assert.ok(visible.has(controls[2].addedOverlays[0].layer), 'refresh keeps active overlays');
});

test('terrain source and fallback round-trip through plan and load and record an edit', () => {
  const elements = new Map(), actions = [];
  const $ = id => {
    if (!elements.has(id)) elements.set(id, {
      set value(value) { this.textValue = String(value); },
      get value() { return this.textValue; },
      validity: { valid: true }, replaceChildren(...options) { this.options = options; },
      removeAttribute() {}, click() {},
    });
    return elements.get(id);
  };
  const context = vm.createContext({
    $, Option: function (text, value) { Object.assign(this, { text, value }); },
    coords: name => name === 'a' ? [59, 10] : [60, 11],
    dataSources: { terrain: [
      { id: 'local', name: 'Local elevation', resolution_m: 2, coverage: { type: 'global' } },
      { id: 'regional', name: 'Regional', resolution_m: 1, region: 'Norway', coverage: { type: 'bbox', bbox: [4, 57.9, 31.3, 71.3] } },
      { id: 'off', name: 'Disabled', resolution_m: 4, enabled: false },
    ] },
    mapEditsLocked: () => false,
    recordEdit: () => actions.push('recordEdit'),
    invalidate: () => actions.push('invalidate'),
    scheduleCoverageCheck: () => actions.push('scheduleCoverageCheck'),
    buildSettings() {}, syncCoverageInputs() {}, mark() {}, renderMeshcoreRouters() {},
    renderManualRouters() {}, ensureManualRouterNames() {}, renderCoverageTargets() {}, selectedKnownRouters: () => [],
    baseCandidates: {}, defaultCoverageSettings: { client: {} },
    selectedKnownRouterId: null, clone: structuredClone,
  });
  vm.runInContext(
    section('const defaultTerrainOptions =', 'let dataSources =') +
    section('function sourceCoverageBoxes(', 'function sourceRingIntersectsView(') +
    section('function sourceCoverageBounds(', 'function setTerrainDomNote(') +
    section('function plan(){', 'async function applyHistory('), context,
  );
  const basePlan = { a: [55, 10], b: [56, 11], rf: {}, candidates: {} };
  context.load(basePlan);
  assert.deepEqual($('terrain-source').options.map(option => option.value), ['auto', 'local', 'regional']);
  assert.deepEqual($('terrain-source').options.slice(1).map(option => option.text),
    ['Local elevation · 2 m · Global', 'Regional · 1 m · Norway'], 'regions label coverage instead of bounds');
  actions.length = 0;
  $('terrain-source').value = 'local';
  $('terrain-fallback').checked = false;
  context.editTerrainControls({ target: { id: 'terrain-source' } });
  assert.deepEqual(actions, ['recordEdit', 'invalidate', 'scheduleCoverageCheck']);
  assert.equal(context.plan().terrain.source, 'local');
  assert.equal(context.plan().terrain.fallback, false);

  context.load(structuredClone(context.plan()));
  assert.equal($('terrain-source').value, 'local');
  assert.equal($('terrain-fallback').checked, false);
  assert.equal($('terrain-fallback-label').hidden, false);
  context.load(basePlan);
  assert.equal(context.plan().terrain.source, 'auto', 'a plan without a source resets a previous pin');
  assert.equal(context.plan().terrain.fallback, true);
  assert.equal($('terrain-fallback-label').hidden, true);
});

test('a preview response is stale as soon as route changes schedule the next preview', async () => {
  const copy = {}, domNote = {}, requests = [], timers = new Map();
  let a = [59, 10];
  const context = vm.createContext({
    $: id => ({ 'terrain-source-preview': copy, 'terrain-dom-note': domNote }[id] || { hidden: false }),
    dataSources: {}, terrainOptions: { source: 'auto' }, manualRouters: [],
    terrainPreviewRequest: 0, terrainPreviewTimer: null, terrainPreviewKey: null,
    terrainPreviewDomNote: '', terrainEstimateReview: null,
    selectedKnownRouters: () => [], validEndpointCoords: name => name === 'a' ? a : [60, 11],
    setTimeout(callback) { const id = Symbol(); timers.set(id, callback); return id; },
    clearTimeout(id) { timers.delete(id); },
    api: (url, options) => new Promise(resolve => requests.push({ url, body: JSON.parse(options.body), resolve })),
  });
  vm.runInContext(section('function setTerrainDomNote(', 'function applySourceRegistry('), context);
  context.scheduleTerrainSourcePreview();
  timers.values().next().value();
  assert.deepEqual(requests[0].body.a, [59, 10]);
  a = [-23, -43];
  context.scheduleTerrainSourcePreview();
  requests[0].resolve({ summary: 'Old route', dom_available: true });
  await new Promise(setImmediate);
  assert.match(copy.textContent, /Checking sources/);

  timers.values().next().value();
  requests[1].resolve({ summary: 'New route', dom_available: false, sources: [
    { name: 'Surface DEM', surface_model: true, role: 'primary' },
    { name: 'Backup DSM', surface_model: true, role: 'fallback' },
  ] });
  await new Promise(setImmediate);
  assert.match(copy.textContent, /For this route: New route/);
  assert.match(copy.textContent, /Surface models such as Surface DEM can include trees and buildings/);
  assert.doesNotMatch(copy.textContent, /Backup DSM/, 'fallback-only sources carry no caveat');
  assert.match(domNote.textContent, /No surface terrain/);
});

test('validation issues map to the most specific field and focus the first one', () => {
  const nodes = new Map();
  let focused;
  const makeField = (id, sourcePath) => {
    const attributes = new Map();
    const field = {
      id, attributes, dataset: { sourcePath },
      matches: () => false, checkVisibility: () => true,
      setAttribute: (key, value) => attributes.set(key, value),
      removeAttribute: key => attributes.delete(key),
      closest: () => ({ after: error => nodes.set(error.id, error) }),
      focus() { focused = field; },
    };
    nodes.set(id, field);
    return field;
  };
  const product = makeField('dtm', 'products.dtm');
  const crs = makeField('native-crs', 'products.dtm.native_crs');
  const url = makeField('url', 'products.dtm.endpoint');
  makeField('data-sources-status');
  nodes.set('data-source-json-panel', { open: false });
  const fields = [product, crs, url];
  nodes.set('data-source-editor', { querySelectorAll: selector => selector === '[data-source-path]' ? fields : [] });
  const context = vm.createContext({ $: id => nodes.get(id), document: { createElement: () => ({}) } });
  vm.runInContext(
    section('function sourceDialogStatus(', 'function sourceRowButton(') +
    section('function clearSourceEditorIssues(', 'function openSourceEditor('), context,
  );
  context.showSourceEditorIssues([
    { path: 'terrain[0].products.dtm.native_crs[1]', message: 'Unsupported CRS' },
    { path: 'terrain[0].products.dtm.endpoint', message: 'Use HTTPS' },
  ], 'terrain[0]');
  assert.equal(focused, crs);
  assert.equal(crs.attributes.get('aria-invalid'), 'true');
  assert.equal(crs.attributes.get('aria-describedby'), 'native-crs-error');
  assert.equal(nodes.get('native-crs-error').textContent, 'Unsupported CRS');
  assert.equal(nodes.get('url-error').textContent, 'Use HTTPS');
  assert.equal(product.attributes.has('aria-invalid'), false);
});
