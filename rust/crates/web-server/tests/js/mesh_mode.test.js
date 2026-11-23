const {test} = require('node:test');
const assert = require('node:assert/strict');
const {readFileSync} = require('node:fs');
const {join} = require('node:path');
const vm = require('node:vm');

const assets = join(__dirname, '../../assets/dashboard');
const meshScript = readFileSync(join(assets, 'mesh_mode.js'), 'utf8');
const app = readFileSync(join(assets, 'app.js'), 'utf8');
const parseCoordinates = app.slice(app.indexOf('function parseCoordinates('), app.indexOf('function validEndpointCoords('));

function load(globals = {}) {
  const context = vm.createContext({validEndpointCoords: () => [59, 10], ...globals});
  vm.runInContext(parseCoordinates + meshScript, context);
  return context;
}

const frank = {id: 'CL-one', name: 'Frank', latitude: 59.97, longitude: 10.68, antenna_height_m: 2};
const raw = {name: '  Alice  ', latitude: '59.9700001', longitude: '10.68', antenna_height_m: '2.5'};

test('client validation normalizes names and coordinates and rejects blanks and duplicate positions', () => {
  const context = load();
  assert.deepEqual(JSON.parse(JSON.stringify(context.validateMeshClient(raw, [], null))), {
    client: {name: 'Alice', latitude: 59.97, longitude: 10.68, antenna_height_m: 2.5}
  });
  for (const field of Object.keys(raw)) {
    assert.equal(context.validateMeshClient({...raw, [field]: ''}, [], null).field, field);
  }
  assert.equal(context.validateMeshClient(raw, [frank], null).field, 'latitude');
  assert.equal(context.validateMeshClient(raw, [frank], frank.id).client.name, 'Alice');
});

test('plans can save a draft mesh, but search requires two distinct named clients', () => {
  const context = load();
  context.loadPlanningMode({});
  assert.equal(context.routeLocationsReady(), true);
  context.loadPlanningMode({mode: 'mesh', clients: [frank]});
  assert.equal(context.routeLocationsReady(), false);
  const bob = {...frank, id: 'CL-two', name: 'Bob', longitude: 10.7};
  context.loadPlanningMode({mode: 'mesh', clients: [frank, bob]});
  assert.equal(context.routeLocationsReady(), true);
  assert.throws(() => context.loadPlanningMode({mode: 'mesh', clients: [frank, frank]}), /unique client ID/);
});

test('adding, renaming and removing clients uses the shared edit sequence', () => {
  const effects = [];
  const context = load({
    mapEditsLocked: () => false, randomId: () => 'two',
    recordEdit: () => effects.push('recordEdit'),
    invalidate: () => effects.push('invalidate'),
    scheduleCoverageCheck: () => effects.push('scheduleCoverageCheck')
  });
  vm.runInContext('renderMeshClients = () => {};', context);
  context.loadPlanningMode({mode: 'mesh', clients: [frank]});
  const id = context.applyMeshClientEdit(null, {name: 'Bob', latitude: 59.98, longitude: 10.7, antenna_height_m: 2});
  assert.equal(id, 'CL-two');
  context.applyMeshClientEdit(id, {name: 'Alice'});
  assert.equal(vm.runInContext('meshClients[1].name', context), 'Alice');
  context.applyMeshClientEdit(id, null);
  assert.equal(vm.runInContext('meshClients.length', context), 1);
  const chain = ['recordEdit', 'invalidate', 'scheduleCoverageCheck'];
  assert.deepEqual(effects, [...chain, ...chain, ...chain]);
});

test('mesh metrics use the largest hop count across all client pairs', () => {
  const context = load({formatMetric: value => value.toFixed(1)});
  const snapshot = {found: true, search_complete: true, client_count: 3, clients_connected: 3,
    new_router_count: 1, client_pairs: [{hops: 1}, {hops: 2}, {hops: 1}]};
  const metrics = context.meshMetricTiles(snapshot, 9, {label: 'Good', cls: 'good'});
  assert.deepEqual(Array.from(metrics, row => row[0]), ['3/3', '1', '9.0 dB', '2']);
  assert.match(context.meshVerdict(snapshot, 9), /All 3 clients are connected/);
});
