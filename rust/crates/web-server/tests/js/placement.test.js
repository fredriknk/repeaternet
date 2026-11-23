const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

const read = name => fs.readFileSync(path.join(__dirname, '../../assets/dashboard', name), 'utf8');
const scripts = read('site_selection.js') + '\n' + read('placement.js');

test('placement needs a movable router and a second site', () => {
  const {placementAvailable} = vm.runInNewContext(scripts + '\n({placementAvailable})');
  const proposed = {kind: 'manual'}, found = {kind: 'search'};
  const anchors = [{kind: 'endpoint'}, {kind: 'known'}, {kind: 'client'}];
  assert.equal(placementAvailable(anchors), false);
  assert.equal(placementAvailable([proposed]), false);
  assert.equal(placementAvailable([proposed, anchors[0]]), true);
  assert.equal(placementAvailable([found, anchors[2]]), true);
  assert.equal(placementAvailable([found, anchors[2]], true), false);
});

test('move radius checks the raw string and the search bounds', () => {
  const {placementRadius} = vm.runInNewContext(scripts + '\n({placementRadius})');
  for (const raw of ['', ' ', '0', '-20', '2000.01', '0x10', 'NaN', '25 m']) {
    assert.equal(placementRadius(raw), null, raw);
  }
  assert.equal(placementRadius(' 250.5 '), 250.5);
  assert.equal(placementRadius('2000'), 2000);
});

function placementFixture() {
  const calls = [];
  const dismissed = [];
  const proposed = {id: 'P', name: 'Hill', kind: 'manual', latitude: 60, longitude: 10, policy: 'required'};
  const found = {id: 'R', kind: 'search', antenna_height_m: 4, terminal_clutter: 'none'};
  const anchor = {id: 'A', kind: 'endpoint', latitude: 60, longitude: 10.2};
  const context = vm.createContext({
    findMapSite: id => [proposed, found, anchor].find(site => site.id === id),
    manualRouters: [proposed], mapEditsLocked: () => false,
    randomId: () => 'new', nextManualRouterName: () => 'P2',
    rf: {site_overrides: {R: {gain_dbi: 5}}},
    saveSiteRadio: (id, value) => { context.rf.site_overrides[id] = value; },
    dismissSearchRouterIds: ids => dismissed.push(...ids),
    commitManualRouterLocations: () => calls.push('commit'),
  });
  vm.runInContext(scripts + "\nsiteSelection.replace(['A', 'P', 'R']);", context);
  return {context, calls, proposed, anchor, dismissed};
}

test('apply updates proposed routers, pins moved search routers and commits once', () => {
  const {context, calls, proposed, anchor, dismissed} = placementFixture();
  assert.equal(context.commitPlacement({sites: [
    {id: 'A', moved: false},
    {id: 'P', moved: true, after: [60.01, 10.02]},
    {id: 'R', moved: true, after: [60.03, 10.04]},
  ]}), true);
  assert.deepEqual(calls, ['commit']);
  assert.deepEqual(dismissed, ['R']);
  assert.equal(proposed.latitude, 60.01);
  assert.equal(proposed.longitude, 10.02);
  assert.equal(proposed.policy, 'required');
  assert.equal(anchor.latitude, 60);
  const pinned = context.manualRouters[1];
  assert.equal(pinned.id, 'M-new');
  assert.equal(pinned.latitude, 60.03);
  assert.equal(pinned.longitude, 10.04);
  assert.equal(pinned.antenna_height_m, 4);
  assert.equal(pinned.terminal_clutter, 'none');
  assert.equal(context.rf.site_overrides['M-new'].gain_dbi, 5);
  assert.deepEqual([...vm.runInContext('siteSelection.ids', context)], ['A', 'P', 'M-new']);
});

test('invalid or empty moves leave the plan and edit history intact', () => {
  const {context, calls, proposed} = placementFixture();
  for (const id of ['A', 'missing']) {
    assert.equal(context.commitPlacement({sites: [
      {id: 'P', moved: true, after: [60.01, 10.02]},
      {id, moved: true, after: [60.03, 10.04]},
    ]}), false);
  }
  assert.equal(context.commitPlacement({sites: [{id: 'P', moved: false}]}), false);
  assert.equal(proposed.latitude, 60);
  assert.deepEqual(calls, []);
});
