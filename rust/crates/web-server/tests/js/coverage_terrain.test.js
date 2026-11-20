const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

const app = fs.readFileSync(path.join(__dirname, '../../assets/dashboard/app.js'), 'utf8');
const terrainActions = app.slice(
  app.indexOf('function coverageTerrainKey('),
  app.indexOf('function invalidateCoverageEstimate(')
);

test('coverage terrain approval cannot start a download for an outdated map extent', async () => {
  const elements = new Map();
  const requests = [];
  let extent = [60, 10, 61, 11];
  const context = vm.createContext({
    projectInfo: { active_id: 'project' },
    coveragePreflightBusy: false,
    coverageTerrainJobId: null,
    $: id => {
      if (!elements.has(id)) elements.set(id, {});
      return elements.get(id);
    },
    coveragePayload: () => ({ settings: { area_mode: 'view' }, map_bounds: extent }),
    coverageReviewKey: JSON.stringify,
    readCoverageInputs() {},
    syncCoverageLocks() {},
    async autosaveNow() {},
    async api(url, { body }) {
      requests.push([url, JSON.parse(body).map_bounds]);
      return {
        missing_area_km2: 5, tile_count: 1, effective_resolution_m: 25,
        estimated_raw_mib: 2, cache_hits: 0, cache_total: 1,
      };
    },
    async askActionDialog() {
      extent = [61, 11, 62, 12];
      return true;
    },
  });
  vm.runInContext(terrainActions, context);
  await context.requestCoverageTerrain();
  assert.deepEqual(requests, [['/api/coverage/terrain/estimate', [60, 10, 61, 11]]]);
  assert.match(elements.get('coverage-terrain-status').textContent, /coverage area changed/);
  assert.equal(context.coveragePreflightBusy, false);
});
