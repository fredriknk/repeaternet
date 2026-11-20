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

test('applying blank router coordinates preserves the saved site, while zero is valid', () => {
  const element = () => ({
    dataset: {},
    children: [],
    append(...children) { this.children.push(...children); },
    replaceChildren() { this.children.length = 0; },
  });
  const list = element(), errors = [];
  const router = { id: 'router', latitude: 59.93, longitude: 10.82, antenna_height_m: 8 };
  let edits = 0;
  const context = vm.createContext({
    $: () => list, document: { createElement: element },
    manualRouters: [router], manualLayer: { clearLayers() {} }, selectedManualRouterId: null,
    L: { divIcon() {}, marker: () => ({ bindTooltip() { return this; }, bindPopup() { return this; }, on() {}, addTo() {} }) },
    busy: false, coverageBusy: false, switchingProject: false,
    mapEditsLocked: () => false, mapEditControl: control => control, mapPoint: site => [site.latitude, site.longitude],
    updateManualMarker() {}, mapText: () => element(), coordinateLabel: () => '', mapPopupOptions: () => ({}),
    manualRouterControls: () => () => ({ heightLabel: element(), policyLabel: element(), remove: element() }),
    focusManualRouter() {}, syncMapEditControls() {},
    recordEdit() { edits++; }, invalidate() {}, scheduleCoverageCheck() {},
    status(message, isError) { if (isError) errors.push(message); },
  });
  vm.runInContext(section('function parseCoordinates(', 'function validEndpointCoords(') +
    section('function moveManualRouter(', 'function renderManualRouters(') +
    section('function renderManualRouters(', 'function focusManualRouter('), context);
  context.renderManualRouters();
  const [, coordinates, apply] = list.children[0].children;
  const [latitude, longitude] = coordinates.children.map(label => label.children[0]);
  latitude.value = '';
  apply.onclick();
  latitude.value = '0';
  longitude.value = ' ';
  apply.onclick();
  assert.equal(edits, 0);
  assert.equal(router.latitude, 59.93);
  assert.equal(router.longitude, 10.82);
  assert.equal(errors.length, 2);
  longitude.value = '0';
  apply.onclick();
  assert.equal(edits, 1);
  assert.equal(router.latitude, 0);
  assert.equal(router.longitude, 0);
});

test('ordinary estimates, calculations and previews use plan heights after a height scenario', async () => {
  const sent = [], actions = [], errors = [];
  const context = vm.createContext({
    coverageSettings: { area_mode: 'mesh', source_ids: [] },
    coverageHeightOverrides: {}, coverageEstimate: null, coverageRevision: 0, coverageEstimateRequest: 0,
    result: { found: true, active_alternative_id: 'alt', snapshot_version: 1, route: [{ kind: 'router' }] },
    resultStale: false, busy: false, routePreflightBusy: false, coverageBusy: false,
    coveragePreflightBusy: false, coverageLoadingRun: false, switchingProject: false,
    map: { getBounds: () => ({ getSouth: () => 59, getWest: () => 10, getNorth: () => 60, getEast: () => 11 }) },
    document: { querySelectorAll: () => [{ value: '9', dataset: { originalHeight: '5', heightSource: 'router' } }] },
    $: () => ({}), clone: value => JSON.parse(JSON.stringify(value)),
    coverageRouteReady: () => true,
    readCoverageInputs() {}, syncCoverageLocks() {}, updateCoverageActions() {}, setWorkspace() {},
    invalidateCoverageEstimate() {}, coverageEstimateSummary: () => '',
    setCoverageStatus(message, error) { if (error) errors.push(message); },
    async autosaveNow() {}, async askActionDialog() { return true; },
    async api(url, { body }) {
      assert.equal(url, '/api/coverage/estimate');
      sent.push(JSON.parse(body).source_height_overrides);
      return {};
    },
    async startCoverage(approved) { actions.push(['calculate', { ...approved.payload.source_height_overrides }]); },
    async previewCoverage(approved) { actions.push(['preview', { ...approved.payload.source_height_overrides }]); },
  });
  vm.runInContext(
    section('async function startCoverageHeightScenario(', 'const routeRender=') +
    section('function coveragePayload(', 'function setCoverageStatus(') +
    section('function coverageReviewKey(', 'function invalidateChangedCoverageExtent(') +
    section('async function estimateCoverage(', 'async function previewCoverage('),
    context,
  );
  await context.startCoverageHeightScenario();
  await context.requestCoverageAction();
  await context.requestCoverageAction({ preview: true });
  assert.deepEqual(errors, []);
  assert.deepEqual(sent, [{ router: 9 }, {}, {}]);
  assert.deepEqual(actions, [['calculate', { router: 9 }], ['calculate', {}], ['preview', {}]]);
});

test('a job-locked autosave retries once the job is idle', async () => {
  const timer = new Map(), states = [], posts = [];
  let jobState = 'preparing', nextTimer = 0;
  const context = vm.createContext({
    saveRevision: 1, savedRevision: 0, saveQueue: Promise.resolve(), autosaveTimer: null,
    projectInfo: { active_id: 'project-a' }, $: () => ({}),
    setTimeout(fn) { timer.set(++nextTimer, fn); return nextTimer; },
    clearTimeout(id) { timer.delete(id); },
    setProjectSaveState(message, state) { states.push([message, state]); },
    plan: () => ({ terrain: { requested_resolution_m: 25 } }), clone: value => value,
    async api(url, options) {
      if (url === '/api/state') return { job: { state: jobState } };
      posts.push(JSON.parse(options.body));
      if (jobState === 'preparing') {
        throw Object.assign(new Error('Plan inputs are locked while a job is active'), { status: 409 });
      }
      return { has_previous_revision: true };
    },
  });
  vm.runInContext(
    section('function showProjectSaved(', 'function renderProjects(') +
    section('function formatBytes(', 'function scheduleAutosave(') +
    section('function retryAutosaveAfterJob(', "$('retry-save').onclick"),
    context,
  );
  await assert.rejects(context.autosaveNow(), /locked/);
  assert.deepEqual(states.at(-1), ['Changes will save when the job finishes', 'saving']);
  await timer.get(nextTimer)();
  assert.equal(posts.length, 1);
  jobState = 'terrain_ready';
  await timer.get(nextTimer)();
  assert.equal(posts.length, 2);
  assert.deepEqual(posts[1], posts[0]);
  assert.equal(context.savedRevision, 1);
  assert.equal(states.at(-1)[1], 'saved');
});

test('clearing route terrain confirms the supplemental tiles affected by the same operation', async () => {
  const button = { dataset: { clear: 'dom' } }, dialogs = [], requests = [];
  const context = vm.createContext({
    document: { querySelectorAll: () => [button] },
    $: () => ({ dataset: { tileCount: '2' } }),
    coverageTerrainLoaded: true, coverageTerrainTileCounts: { dtm: 3, dom: 1 },
    async askActionDialog(options) { dialogs.push(options); return dialogs.length === 2; },
    async api(url, { method }) { requests.push([url, method]); },
    invalidate() {}, async refresh() {}, async updateTerrainCoverage() {},
  });
  vm.runInContext(section("document.querySelectorAll('[data-clear]')", "for(const id of ['terrain-resolution'"), context);
  await button.onclick();
  assert.deepEqual(requests, []);
  assert.match(dialogs[0].title, /2 surface \(DOM\) tiles/);
  assert.match(dialogs[0].copy, /supplemental coverage terrain set \(3 DTM, 1 DOM\)/);
  await button.onclick();
  assert.deepEqual(requests, [['/api/terrain/dom', 'DELETE']]);
});
