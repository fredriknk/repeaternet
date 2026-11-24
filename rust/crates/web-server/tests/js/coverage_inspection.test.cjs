const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

const source = fs.readFileSync(path.join(__dirname,
  '../../assets/dashboard/coverage_inspection.js'), 'utf8');
const appSource = fs.readFileSync(path.join(__dirname, '../../assets/dashboard/app.js'), 'utf8');
const appFunction = name => appSource.match(new RegExp(`^function ${name}\\([^]*?^}`, 'm'))[0];
const deferred = () => {
  let resolve, reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
};
const flush = () => new Promise(resolve => setImmediate(resolve));

function harness({ autosaveNow = async () => {}, onStart, onPoll } = {}) {
  const events = [], rendered = [], errors = [], views = [], navigation = [];
  const context = vm.createContext({
    AbortController, DOMException, setTimeout, clearTimeout,
    activeInspectionId: null, interactionMode: 'inspect-coverage', inspectionRequest: 0,
    coverageDisplayInfo: null, coverageJob: null, coverageHeightOverrides: {},
    inspectionController: null, inspectionLayer: { clearLayers() {} },
    coverageSettings: { client: { height_agl_m: 1.5 } },
    $: id => ({
      value: '', selectedOptions: [],
      scrollIntoView() { navigation.push(`scroll:${id}`); },
      focus() { navigation.push(`focus:${id}`); },
    }),
    readCoverageInputs() {}, coveragePayload: () => ({}),
    setMainView(view) { views.push(view); },
    autosaveNow, invalidate() {}, coverageSettingsChanged() {}, coverageBusyState() {},
    async installProject() {},
    setCoverageStatus(message, error) { if (error) errors.push(message); },
    async api(url, options = {}) {
      if (url === '/api/coverage/inspect') {
        const point = JSON.parse(options.body).latitude;
        events.push(`start:${point}`);
        return onStart ? onStart(point) : { job_id: String(point) };
      }
      if (url.endsWith('/cancel')) {
        events.push(`cancel:${url.split('/').at(-2)}`);
        return { ok: true };
      }
      const id = url.split('/').at(-1);
      events.push(`poll:${id}`);
      return onPoll ? onPoll(id) : { state: 'complete', result: { point: id } };
    },
  });
  vm.runInContext(source, context);
  context.renderCoverageInspection = value => rendered.push(value.point);
  return { context, events, rendered, errors, views, navigation,
    click: latitude => context.inspectAt({ lat: latitude, lng: 10 }),
  };
}

test('a delayed autosave cannot submit an older click after a newer result', async () => {
  const oldSave = deferred();
  let saves = 0;
  const h = harness({ autosaveNow: () => ++saves === 1 ? oldSave.promise : Promise.resolve() });
  const old = h.click(1);
  await h.click(2);
  oldSave.resolve();
  await old;
  assert.deepEqual(h.events.filter(value => value.startsWith('start:')), ['start:2']);
  assert.deepEqual(h.rendered, ['2']);
  assert.deepEqual(h.views, ['view-analysis']);
  assert.deepEqual(h.navigation, ['scroll:coverage-inspection', 'focus:inspection-title']);
  assert.deepEqual(h.errors, []);
});

test('a pending start is cancelled before the newer click is dispatched', async () => {
  const oldStart = deferred();
  const h = harness({ onStart: point => point === 1 ? oldStart.promise : { job_id: '2' } });
  const old = h.click(1);
  await flush();
  const newer = h.click(2);
  await flush();
  const startsWhilePending = h.events.filter(value => value.startsWith('start:'));
  oldStart.resolve({ job_id: '1' });
  await Promise.all([old, newer]);
  assert.deepEqual(startsWhilePending, ['start:1']);
  assert.deepEqual(h.events, ['start:1', 'cancel:1', 'start:2', 'poll:2']);
  assert.deepEqual(h.rendered, ['2']);
});

test('leaving inspect mode cancels a job whose ID arrives after the mode change', async () => {
  const started = deferred();
  const h = harness({ onStart: () => started.promise });
  const click = h.click(1);
  await flush();
  h.context.interactionMode = 'browse';
  h.context.inspectionController.abort();
  started.resolve({ job_id: '1' });
  await click;
  assert.deepEqual(h.events, ['start:1', 'cancel:1']);
  assert.deepEqual(h.rendered, []);
  assert.deepEqual(h.views, []);
});

test('a failed obsolete start does not poison the queue or report an obsolete error', async () => {
  const oldStart = deferred();
  const h = harness({ onStart: point => point === 1 ? oldStart.promise : { job_id: '2' } });
  const old = h.click(1);
  await flush();
  const newer = h.click(2);
  oldStart.reject(new Error('Old request failed'));
  await Promise.all([old, newer]);
  assert.deepEqual(h.rendered, ['2']);
  assert.deepEqual(h.errors, []);
});

test('an aborted older poll cannot replace the newer point even if its response arrives late', async () => {
  const oldPoll = deferred();
  const h = harness({ onPoll: id => id === '1' ? oldPoll.promise :
    { state: 'complete', result: { point: id } } });
  const old = h.click(1);
  await flush();
  await h.click(2);
  oldPoll.resolve({ state: 'complete', result: { point: '1' } });
  await old;
  assert.deepEqual(h.rendered, ['2']);
  assert.ok(h.events.includes('cancel:1'));
});

test('mesh clients stay selected when router sources are narrowed', () => {
  const select = vm.runInNewContext(`(${appFunction('selectedCoverageSources')})`);
  const sites = [
    { id: 'CL-school', kind: 'client' }, { id: 'N506', kind: 'candidate' },
    { id: 'R-other', kind: 'router' },
  ];
  assert.equal(select(sites, { include_endpoints: false, source_ids: ['N506'] })
    .map(site => site.id).join(','), 'CL-school,N506');
});

test('a direct route offers one source repair before entering inspection', () => {
  const h = harness();
  const elements = new Map();
  const focused = [];
  Object.assign(h.context, {
    $: id => {
      if (!elements.has(id)) elements.set(id, { value: '', focus: () => focused.push(id) });
      return elements.get(id);
    },
    result: {
      found: true, active_alternative_id: 'direct',
      route: [{ id: 'A', kind: 'endpoint_a' }, { id: 'B', kind: 'endpoint_b' }],
    },
    resultStale: false, interactionMode: 'browse',
    busy: false, routePreflightBusy: false, coverageBusy: false,
    coveragePreflightBusy: false, coverageLoadingRun: false, switchingProject: false,
    coverageRouteReady: () => true,
    updateCoverageTerrainAction() {},
    setInteractionMode(mode) { h.context.interactionMode = mode; },
  });
  h.context.coverageSettings.source_ids = [];
  h.context.coverageSettings.include_endpoints = false;
  vm.runInContext(appFunction('selectedCoverageSources') + '\n' +
    appFunction('updateCoverageActions'), h.context);
  vm.runInContext(source.slice(source.indexOf("$('coverage-inspect').onclick"),
    source.indexOf('let coverageProbe')), h.context);
  const inspect = h.context.$('coverage-inspect');
  const repair = h.context.$('inspection-include-endpoints');
  const emptyState = h.context.$('inspection-no-sources');
  const endpoints = h.context.$('coverage-endpoints');
  endpoints.click = () => {
    endpoints.checked = h.context.coverageSettings.include_endpoints = true;
    h.context.updateCoverageActions();
  };
  h.context.updateCoverageActions();
  assert.equal(inspect.hidden, true);
  assert.equal(emptyState.hidden, false);
  inspect.onclick();
  assert.equal(h.context.interactionMode, 'browse');
  repair.onclick();
  assert.equal(emptyState.hidden, true);
  assert.equal(inspect.hidden, false);
  assert.equal(focused.at(-1), 'coverage-inspect');
  inspect.onclick();
  assert.equal(h.context.interactionMode, 'inspect-coverage');
  assert.deepEqual(h.events, []);
});

test('profile-limit inspection renders unresolved paths without formatting missing margins', () => {
  const elements = new Map();
  const renderedMarkers = [];
  const statuses = [];
  const context = vm.createContext({
    AbortController, DOMException, setTimeout, clearTimeout,
    inspectionLayer: {},
    coverageSettings: { client: { height_agl_m: 1.5 } },
    $: id => {
      if (!elements.has(id)) elements.set(id, { value: '', selectedOptions: [] });
      return elements.get(id);
    },
    readCoverageInputs() {}, coveragePayload: () => ({}), autosaveNow: async () => {},
    invalidate() {}, coverageSettingsChanged() {}, coverageBusyState() {},
    routeSiteLabel: () => 'R1',
    async installProject() {},
    setCoverageStatus(message) { statuses.push(message); },
    async api() { throw new Error('Unexpected API request'); },
    L: { circleMarker(point, options) { return { addTo() { renderedMarkers.push({ point, options }); } }; } },
  });
  vm.runInContext(source, context);

  context.renderCoverageInspection({
    state: 'unresolved',
    client: { height_agl_m: 1.5 },
    message: 'No candidate profile fits the sample limit.',
    sources: [{
      source_id: 'R-1',
      rejection: 'profile_sample_limit',
      rejection_detail: 'Requires 100 samples; the configured maximum is 3.',
      two_way_margin_db: null,
    }],
  }, 60, 10);

  assert.match(elements.get('inspection-summary').textContent, /No candidate profile fits/);
  assert.match(elements.get('inspection-detail').textContent, /Requires 100 samples/);
  assert.equal(elements.get('inspection-chart').hidden, true);
  assert.match(statuses.at(-1), /unresolved/i);
  assert.equal(renderedMarkers.length, 1);
  assert.equal(renderedMarkers[0].options.color, '#765497');
});

test('inspection summary, detail and map popup use site labels with IDs in tooltips', () => {
  const h = harness();
  const elements = new Map();
  const popup = {
    setLatLng() { return this; },
    setContent(content) { this.content = content; return this; },
    openOn() {},
  };
  Object.assign(h.context, {
    $: id => {
      if (!elements.has(id)) elements.set(id, { value: '' });
      return elements.get(id);
    },
    document: { createElement: () => ({ append(...children) { this.children = children; } }) },
    L: { circleMarker: () => ({ addTo() {} }), popup: () => popup },
    map: {},
    routeSiteLabel: id => ({ 'CL-school': 'Maridalen school', N506: 'R1' }[id] || id),
  });
  vm.runInContext(source.slice(source.indexOf('function renderCoverageInspection'),
    source.indexOf('function drawCoverageInspectionProfile')), h.context);
  const link = source_id => ({
    source_id, distance_m: 700, downlink_margin_db: 35.5, uplink_margin_db: 33.5,
    two_way_margin_db: 33.5, valid_two_way: true,
  });
  h.context.renderCoverageInspection({
    state: 'evaluated', client: { height_agl_m: 1.5 }, ground_elevation_m: 255,
    selected_source_id: 'CL-school', selected_source_component: 'CL-kirkeby',
    selected_source_component_size: 2,
    source_sites: [{ id: 'CL-kirkeby', kind: 'client', name: 'Kirkeby' }],
    sources: [link('CL-school'), link('CL-kirkeby'), link('N506'), link('B')],
  }, 60, 10);
  const summary = elements.get('inspection-summary');
  const detail = elements.get('inspection-detail');
  assert.match(summary.textContent, /^Maridalen school .* component Kirkeby/);
  assert.match(detail.textContent, /R1 · down.*\nB · down/);
  assert.doesNotMatch(summary.textContent + detail.textContent, /CL-|N506/);
  assert.match(summary.title, /CL-school\nCL-kirkeby/);
  assert.match(detail.title, /N506/);
  assert.equal(popup.content.children[0].textContent, 'Maridalen school');
  assert.equal(popup.content.children[0].title, 'CL-school');
});
