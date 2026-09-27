const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

const source = fs.readFileSync(path.join(__dirname,
  '../../src/rf_router_planner/web_assets/coverage_inspection.js'), 'utf8');
const deferred = () => {
  let resolve, reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
};
const flush = () => new Promise(resolve => setImmediate(resolve));

function harness({ autosaveNow = async () => {}, onStart, onPoll } = {}) {
  const events = [], rendered = [], errors = [];
  const context = vm.createContext({
    AbortController, DOMException, setTimeout, clearTimeout,
    activeInspectionId: null, inspectCoverage: true, inspectionRequest: 0,
    inspectionController: null, inspectionLayer: { clearLayers() {} },
    coverageSettings: { client: { height_agl_m: 1.5 } },
    $: () => ({}), readCoverageInputs() {}, coveragePayload: () => ({}),
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
  return { context, events, rendered, errors,
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
  h.context.inspectCoverage = false;
  h.context.inspectionController.abort();
  started.resolve({ job_id: '1' });
  await click;
  assert.deepEqual(h.events, ['start:1', 'cancel:1']);
  assert.deepEqual(h.rendered, []);
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
