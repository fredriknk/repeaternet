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
    setAttribute() {},
  });
  const list = element(), errors = [];
  const router = { id: 'router', latitude: 59.93, longitude: 10.82, antenna_height_m: 8 };
  let edits = 0;
  const context = vm.createContext({
    $: () => list, document: { createElement: element },
    manualRouters: [router], manualLayer: { clearLayers() {} }, selectedManualRouterId: null,
    manualMapMarkers: new Map(), ensureManualRouterNames() {}, routedRouterIds: () => new Set(),
    manualRouterName: item => item.name || item.id, shortMapLabel: label => label, routerPolicyLabels: { optional: 'Optional' },
    L: { divIcon() {}, marker: () => ({ bindTooltip() { return this; }, bindPopup() { return this; }, on() {}, addTo() {} }) },
    busy: false, coverageBusy: false, switchingProject: false,
    mapEditsLocked: () => false, mapEditControl: control => control, mapPoint: site => [site.latitude, site.longitude],
    updateManualMarker() {}, mapText: () => element(), coordinateLabel: () => '', mapPopupOptions: () => ({}),
    manualRouterControls: () => () => ({ nameLabel: element(), heightLabel: element(), policyLabel: element(), remove: element() }),
    routerTerminalClutterControl: element,
    focusManualRouter() {}, syncMapEditControls() {}, bindLiveCoverageDrag() {}, liveCoverageSiteHeight: value => value,
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
    readCoverageInputs() {}, validateMeshBuffer: () => true, syncCoverageLocks() {}, updateCoverageActions() {}, setWorkspace() {},
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
    saveRevision: 1, savedRevision: 0, saveQueue: Promise.resolve(), autosaveTimer: null, saveState: 'saved',
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

test('project transitions accept fractional mesh buffers and reveal a hidden invalid field', async () => {
  const html = fs.readFileSync(path.join(__dirname, '../../assets/dashboard/index.html'), 'utf8');
  const meshInput = html.match(/<input id="coverage-buffer"[^>]+>/)[0];
  const min = Number(meshInput.match(/min="([^"]+)"/)[1]);
  const max = Number(meshInput.match(/max="([^"]+)"/)[1]);
  assert.equal(min, Number.MIN_VALUE);
  assert.equal(max, 500000);
  assert.match(meshInput, /step="any"/);
  const elements = new Map(), destinations = [], attributes = new Map();
  const details = { open: false };
  const buffer = {
    value: '1300.5', dataset: {},
    get validity() { return { valid: Number(this.value) >= min && Number(this.value) <= max }; },
    closest: selector => ({
      '.workspace-view': { id: 'workspace-coverage' },
      '[data-coverage-panel]': { dataset: { coveragePanel: 'area' } },
      details,
    })[selector],
    getAttribute: name => attributes.get(name),
    setAttribute: (name, value) => attributes.set(name, value),
    removeAttribute: name => attributes.delete(name),
    addEventListener() {}, removeEventListener() {},
    after(error) { elements.set(error.id, error); },
    scrollIntoView() {},
    focus() { assert.equal(details.open, true); destinations.push('focus'); },
  };
  elements.set('coverage-buffer', buffer);
  elements.set('project-transition-status', {});
  const context = vm.createContext({
    $: id => elements.get(id),
    document: { querySelector: () => null, createElement: () => ({
      setAttribute() {}, remove() { elements.delete(this.id); },
    }) },
    validEndpointCoords: () => true,
    setWorkspace: id => destinations.push(id), setCoverageSection: id => destinations.push(id),
    syncCoverageLocks() {}, setHistoryButtons() {},
    busy: false, routePreflightBusy: false, coverageBusy: false,
    coveragePreflightBusy: false, coverageLoadingRun: false, switchingProject: false,
    savedRevision: 0, saveRevision: 0,
  });
  vm.runInContext(section('function validateMeshBuffer(', 'function readCoverageInputs(') +
    section('let projectTransitionErrorSequence=', 'async function activateProject('), context);
  let operations = 0;
  const switchProject = () => context.transitionProject(() => operations++);
  assert.equal(await switchProject(), true);
  for (const value of ['', '0', '500001']) {
    buffer.value = value;
    assert.equal(await switchProject(), false);
    assert.equal(attributes.get('aria-invalid'), 'true');
    const error = elements.get(attributes.get('aria-describedby'));
    assert.match(error.className, /\bfield-error\b/);
    assert.match(error.textContent, /above zero and at most 500,000 m/);
  }
  assert.deepEqual(destinations.slice(-3), ['workspace-coverage', 'area', 'focus']);
  assert.equal(operations, 1);
});

test('rendering a failure scenario preserves selection and focus for the same run', () => {
  const element = () => ({
    dataset: {}, value: '', children: [],
    append(...children) { this.children.push(...children); },
    replaceChildren() {
      if (this.contains(document.activeElement)) document.activeElement = null;
      this.children = [];
      this.value = '';
    },
    contains(child) { return this.children.some(label => label.children.includes(child)); },
    querySelectorAll() { return this.children.flatMap(label => label.children.filter(child => child.checked)); },
    setAttribute() {},
    checkVisibility: () => true,
    focus() { document.activeElement = this; },
  });
  const document = { createElement: element, activeElement: null };
  const elements = new Map(['failure-sources', 'failure-reference', 'failure-run', 'failure-reset'].map(id => [id, element()]));
  const run = { job_id: 'first', state: 'complete', source_ids: ['K-A', 'K-B'], router_ids: ['K-A', 'K-B'] };
  const context = vm.createContext({
    $: id => elements.get(id), document, activeCoverageRun: () => run,
    routeSiteLabel: id => id, busy: false, coverageBusy: false, coverageFailurePending: false,
  });
  vm.runInContext(section('function renderFailureControls(', 'async function analyzeFailureScenario('), context);
  const sources = elements.get('failure-sources'), reference = elements.get('failure-reference');
  context.renderFailureControls();
  const firstCheckbox = sources.children[0].children[0];
  firstCheckbox.checked = true;
  firstCheckbox.focus();
  reference.value = 'K-B';
  context.renderFailureControls();
  const replacement = sources.children[0].children[0];
  assert.notEqual(replacement, firstCheckbox);
  assert.equal(replacement.checked, true);
  assert.equal(document.activeElement, replacement);
  assert.equal(reference.value, 'K-B');
  run.job_id = 'second';
  context.renderFailureControls();
  assert.equal(sources.children[0].children[0].checked, false);
});

test('invalidating a certified route replaces its verdict until a current result is rendered', () => {
  const verdict = { dataset: {} }, layer = { clearLayers() {}, getLayers: () => [] };
  const context = vm.createContext({
    $: id => id === 'route-verdict' ? verdict : {},
    result: { found: true, search_complete: true }, resultStale: false, resultStaleReason: '',
    coverageCells: [], coverageCursor: 0, coverageStale: false, coverageComparison: null,
    coverageHeightOverrides: {}, inspectionRequest: 0, inspectionController: null,
    predictedCoverageLayer: layer, coverageDiffLayer: layer, inspectionLayer: layer, routeLayer: layer,
    routeLinkInspector: null, closeRouteLinkInspector() {},
    clearTerrainEstimateReview() {}, invalidateCoverageEstimate() {}, setInteractionMode() {}, cancelActiveInspection() {},
    updatePlanReadiness() {}, setCoverageStatus() {}, syncAnalysisBadge() {}, updateCoverageActions() {}, syncRoutedRouterMarkers() {},
  });
  vm.runInContext(section('function invalidate(', 'function updatePlanReadiness(') +
    section('function renderRouteVerdict(', 'function render(){'), context);
  const details = { links: [{ valid: true, worst_margin_db: 12 }], margin: 12, band: { cls: 'strong' } };
  context.renderRouteVerdict(details);
  assert.equal(verdict.dataset.state, 'ok');
  context.invalidate('endpoint coordinates');
  assert.equal(verdict.dataset.state, 'stale');
  assert.match(verdict.textContent, /^Stale route.*endpoint coordinates/);
  context.resultStale = false;
  context.renderRouteVerdict(details);
  assert.equal(verdict.dataset.state, 'ok');
  context.result.found = false;
  context.renderRouteVerdict();
  assert.equal(verdict.hidden, true);
});

test('coverage progress clears after the last cell page for calculations and saved runs', async () => {
  const elements = new Map(), cursors = [];
  const context = vm.createContext({
    $: id => {
      if (!elements.has(id)) elements.set(id, { removeAttribute() {} });
      return elements.get(id);
    },
    coveragePollRequest: 0, coverageLoadRequest: 0, coverageCursor: 0, coverageCells: [],
    coverageStale: false, coverageBusy: false, coverageLoadingRun: false, coverageRunReused: false,
    coveragePreviewActive: false, coveragePreviewCells: [], coverageDisplayInfo: null, coverageJob: null,
    inspectionRequest: 0, inspectionController: null, interactionMode: 'browse',
    predictedCoverageLayer: { clearLayers() {} }, map: { hasLayer: () => true },
    async api(url) {
      if (!url.includes('/cells')) return { state: 'complete', requested_cells: 2 };
      const cursor = Number(new URL(url, 'http://localhost').searchParams.get('cursor'));
      cursors.push(cursor);
      return { cells: [{}], stored_cells: 2, requested_cells: 2, next_cursor: cursor === 0 ? 1 : null };
    },
    appendCoverageCells() {}, cancelActiveInspection() {}, syncCoverageLocks() {}, updateCoverageActions() {},
    renderCoverageLegend() {}, renderFailureControls() {}, renderHeightScenarioControls() {},
    renderCoverageContext() {}, coverageBusyState() {}, refreshCoverageRunSelectors() {},
    setCoverageStatus(message, error) { assert.ok(!error, message); },
  });
  vm.runInContext(section('function setCoverageResultProgress(', 'async function startCoverage(') +
    section('async function loadSavedCoverageRun(', 'function updateHeightScenarioButtons('), context);
  await context.pollCoverage('run');
  assert.equal(elements.get('coverage-result-progress').hidden, true);
  assert.equal(elements.get('coverage-result-progress-copy').hidden, true);
  await context.loadSavedCoverageRun('run');
  assert.deepEqual(cursors, [0, 1, 0, 1]);
  assert.equal(elements.get('coverage-load-progress').hidden, true);
  assert.match(elements.get('coverage-load-status').textContent, /^All saved cells loaded/);
});
