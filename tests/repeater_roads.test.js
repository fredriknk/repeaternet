const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

const app = fs.readFileSync(path.join(__dirname, '../src/rf_router_planner/web_assets/app.js'), 'utf8');

test('the map draws roads only for the selected alternative and honours the toggle', () => {
  const lineA = [[60, 15], [60, 15.01]];
  const lineB = [[61, 15], [61, 15.01]];
  const control = { hidden: true };
  const checkbox = { checked: true };
  const rendered = [];
  const visible = new Set();
  const map = {
    hasLayer: layer => visible.has(layer),
    removeLayer: layer => visible.delete(layer),
  };
  const repeaterRoadLayer = {
    clearLayers: () => { rendered.length = 0; },
    addTo: () => visible.add(repeaterRoadLayer),
  };
  const context = vm.createContext({
    result: {
      active_alternative_id: 'A',
      alternatives: [
        { id: 'A', road_polyline_indices: [0] },
        { id: 'B', road_polyline_indices: [1] },
      ],
      road_polylines: [lineA, lineB],
    },
    L: { polyline: (lines, style) => ({ addTo: () => rendered.push({ line: lines[0], style }) }) },
    map, repeaterRoadLayer,
    $: id => id === 'show-repeater-roads-control' ? control : checkbox,
  });
  vm.runInContext(app.slice(app.indexOf('function showRepeaterRoads()'), app.indexOf('function render(){')), context);
  context.showRepeaterRoads();
  assert.deepEqual(rendered.map(item => item.line), [lineA, lineA]);
  assert.ok(rendered.every(item => item.style.pane === 'repeaterRoadsPane' && !item.style.interactive));
  assert.ok(visible.has(repeaterRoadLayer));
  assert.equal(control.hidden, false);

  context.result.active_alternative_id = 'B';
  context.showRepeaterRoads();
  assert.deepEqual(rendered.map(item => item.line), [lineB, lineB]);

  checkbox.checked = false;
  checkbox.onchange();
  assert.equal(rendered.length, 0);
  assert.equal(visible.size, 0);
  assert.equal(control.hidden, false);

  context.result = { active_alternative_id: 'old', alternatives: [{ id: 'old' }] };
  context.showRepeaterRoads();
  assert.equal(control.hidden, true);
  assert.equal(visible.size, 0);
});
