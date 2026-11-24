const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

const script = fs.readFileSync(path.join(__dirname, '../../assets/dashboard/site_selection.js'), 'utf8');
const plain = value => JSON.parse(JSON.stringify(value));

test('selection keeps distinct IDs and notifies only when membership changes', () => {
  const {createSiteSelection} = vm.runInNewContext(script + '\n({createSiteSelection})');
  const selection = createSiteSelection();
  const seen = [];
  const unsubscribe = selection.subscribe(ids => {
    seen.push([...ids]);
    ids.clear();
  });
  selection.toggle('A');
  selection.replace(['A', 'P1', 'A']);
  selection.replace(['P1', 'A']);
  selection.ids.clear();
  assert.deepEqual([...selection.ids], ['A', 'P1']);
  selection.toggle('A');
  selection.clear();
  unsubscribe();
  selection.toggle('B');
  assert.deepEqual(seen, [['A'], ['A', 'P1'], ['P1'], []]);
});

test('selection actions can be registered and removed independently of selected IDs', () => {
  const {createSiteSelection} = vm.runInNewContext(script + '\n({createSiteSelection})');
  const selection = createSiteSelection();
  selection.replace(['A', 'P1']);
  const action = {id:'optimize', label:'Optimize placement', run() {}};
  const seen = [];
  selection.subscribe(ids => seen.push([...ids]));
  const unregister = selection.registerAction(action);
  assert.equal(selection.actions[0], action);
  unregister();
  assert.equal(selection.actions.length, 0);
  assert.deepEqual(seen, []);
});

test('bulk validation preserves untouched values and rejects invalid raw values', () => {
  const {bulkSharedValue, validateBulkSiteFields} = vm.runInNewContext(script + '\n({bulkSharedValue, validateBulkSiteFields})');
  const sites = [{kind:'endpoint', gain_dbi:2, antenna_height_m:3}, {kind:'known', gain_dbi:2, antenna_height_m:8}];
  assert.equal(bulkSharedValue(sites, 'gain_dbi'), 2);
  assert.equal(bulkSharedValue(sites, 'antenna_height_m'), undefined);
  assert.deepEqual(plain(validateBulkSiteFields(sites, {gain_dbi:' 0 '})), {changes:{gain_dbi:0}});
  for (const input of ['', ' ', '50 metres', '0x10', '1e999', '501', '-0.1']) {
    assert.equal(validateBulkSiteFields(sites, {antenna_height_m:input}).key, 'antenna_height_m', input);
  }
  assert.equal(validateBulkSiteFields(sites, {policy:'Mixed'}).key, 'policy');
  assert.deepEqual(plain(validateBulkSiteFields(sites.slice(0, 1), {antenna_height_m:'5e2', policy:'required'})),
    {changes:{antenna_height_m:500, policy:'required'}});
});

function planFixture() {
  const calls = [];
  const dismissed = new Set();
  const context = vm.createContext({
    rf: {endpoint_a:{height_agl_m:3, gain_dbi:6}, router:{gain_dbi:2}, site_overrides:{P1:{gain_dbi:8}}},
    meshcoreRouters: [{id:'K1'}], routerPolicies: new Map([['K1', 'optional']]),
    manualRouters: [{id:'P1', antenna_height_m:12, policy:'optional'}],
    meshClients: [{id:'C1', antenna_height_m:3}, {id:'C2', antenna_height_m:3}],
    selectedManualRouterId: 'P1', selectedKnownRouterId: 'K1',
    randomId: () => 'new', nextManualRouterName: () => 'P2',
    routeMode: 'endpoints', mapEditsLocked: () => false, validEndpointCoords: () => [60, 10],
    recordEdit: () => calls.push('recordEdit'),
    invalidate: () => calls.push('invalidate'),
    scheduleCoverageCheck: () => calls.push('scheduleCoverageCheck'),
    buildSettings() {}, renderManualRouters() {}, renderMeshcoreRouters() {}, renderMeshClients() {}, status() {}, setMapHint() {},
    dismissedSearchRouterIds: () => dismissed,
    dismissSearchRouterIds: ids => ids.forEach(id => dismissed.add(id)),
    saveSiteRadio(id, profile) {
      if (profile) context.rf.site_overrides[id] = profile;
      else delete context.rf.site_overrides[id];
    }
  });
  vm.runInContext(script, context);
  const sites = [
    {id:'A', key:'a', kind:'endpoint', antenna_height_m:3, gain_dbi:6},
    {id:'K1', kind:'known', antenna_height_m:3, gain_dbi:2, policy:'optional'},
    {id:'P1', kind:'manual', antenna_height_m:12, gain_dbi:8, policy:'optional'}
  ];
  return {context, calls, sites, dismissed};
}

test('bulk edits update eligible sites with one undo and invalidation chain', () => {
  const {context, calls, sites} = planFixture();
  context.commitBulkSiteEdit(sites, {antenna_height_m:20, policy:'required'});
  assert.equal(context.rf.endpoint_a.height_agl_m, 20);
  assert.equal(context.rf.endpoint_a.gain_dbi, 6);
  assert.deepEqual(plain(context.meshcoreRouters[0]), {id:'K1', antenna_height_m:20, height_override:true});
  assert.equal(context.routerPolicies.get('K1'), 'required');
  assert.deepEqual(context.manualRouters[0], {id:'P1', antenna_height_m:20, policy:'optional'});
  assert.deepEqual(context.rf.site_overrides.P1, {gain_dbi:8});
  assert.deepEqual(calls, ['recordEdit', 'invalidate', 'scheduleCoverageCheck']);
});

test('editing the gain of a search site pins it at its original coordinates and height', () => {
  const {context, dismissed} = planFixture();
  const site = {id:'R1', kind:'search', latitude:60, longitude:10, antenna_height_m:12, gain_dbi:2};
  assert.deepEqual([...context.updateSelectedSites([site], {gain_dbi:0})], ['M-new']);
  assert.deepEqual(plain(context.manualRouters[1]), {id:'M-new', name:'P2', latitude:60, longitude:10,
    antenna_height_m:12, policy:'optional'});
  assert.equal(context.rf.site_overrides['M-new'].gain_dbi, 0);
  assert.ok(dismissed.has('R1'));
  assert.equal(context.pinSearchRouter(site), null);
  assert.equal(context.manualRouters.length, 2);
});

test('replacing a selected search ID preserves the other selected sites', () => {
  const {context} = planFixture();
  const selection = vm.runInContext('siteSelection', context);
  selection.replace(['R1', 'P1', 'K1']);
  context.replaceSelectedSite('R1', 'P2');
  context.replaceSelectedSite('R2', 'P3');
  assert.deepEqual([...selection.ids], ['P2', 'P1', 'K1']);
});

test('bulk height changes the selected client with the routers in one edit', () => {
  const {context, calls, sites} = planFixture();
  context.routeMode = 'mesh';
  context.commitBulkSiteEdit([...sites.slice(1), {id:'C1', kind:'client', antenna_height_m:3}],
    {antenna_height_m:16, gain_dbi:4});
  assert.deepEqual(context.meshClients, [{id:'C1', antenna_height_m:16}, {id:'C2', antenna_height_m:3}]);
  assert.deepEqual([...vm.runInContext('siteSelection.ids', context)], ['K1', 'P1', 'C1']);
  assert.deepEqual(calls, ['recordEdit', 'invalidate', 'scheduleCoverageCheck']);
});

test('bulk removal cleans up planned router settings while leaving endpoints and search sites', () => {
  const {context, calls, sites} = planFixture();
  sites.push({id:'R1', kind:'search'});
  context.commitBulkSiteEdit(sites, {}, {remove:true});
  assert.equal(context.manualRouters.length, 0);
  assert.equal(context.meshcoreRouters.length, 0);
  assert.equal(context.selectedManualRouterId, null);
  assert.equal(context.selectedKnownRouterId, null);
  assert.equal(context.routerPolicies.size, 0);
  assert.deepEqual(context.rf.site_overrides, {});
  assert.deepEqual([...vm.runInContext('siteSelection.ids', context)], ['A', 'R1']);
  assert.deepEqual(calls, ['recordEdit', 'invalidate', 'scheduleCoverageCheck']);
});

test('a no-op or an edit during the lock records no history', () => {
  const {context, calls, sites} = planFixture();
  context.commitBulkSiteEdit(sites, {});
  context.mapEditsLocked = () => true;
  context.commitBulkSiteEdit(sites, {gain_dbi:0});
  context.commitBulkSiteEdit(sites, {}, {remove:true});
  assert.equal(context.manualRouters.length, 1);
  assert.equal(context.rf.endpoint_a.gain_dbi, 6);
  assert.deepEqual(calls, []);
});
