const {test} = require('node:test');
const assert = require('node:assert/strict');
const {paintBrush, paintRowRuns, readMeshPaint} = require('../../assets/dashboard/mesh_paint.js');

test('brush interpolates fast drags and does not duplicate paint', () => {
  const from = [2.5, 1.5], to = [8.5, 1.5];
  const cells = paintBrush([], from, to, 1, false);
  assert.deepEqual(cells, Array.from({length: 7}, (_, index) => [index + 2, 1]));
  assert.deepEqual(paintBrush(cells, to, from, 1, false), cells);
  assert.deepEqual(paintRowRuns(cells), [[2, 1, 9]]);
});

test('brush size controls the stamp and erasing can split a painted patch', () => {
  assert.equal(paintBrush([], [0.5, 0.5], [0.5, 0.5], 5, false).length, 21);
  const cells = paintBrush([], [0.5, 0.5], [4.5, 0.5], 1, false);
  const erasing = paintBrush(cells, [2.5, -3.5], [2.5, 3.5], 1, true);
  assert.deepEqual(paintRowRuns(erasing), [[0, 0, 2], [3, 0, 5]]);
  assert.equal(cells.length, 5);
});

test('painting rejects over-limit strokes without mutating the existing area', () => {
  const cells = [[0, 0]];
  assert.equal(paintBrush(cells, [119.5, 0.5], [120.5, 0.5], 1, false), null);
  assert.deepEqual(cells, [[0, 0]]);
  assert.equal(paintBrush([], [0.5, 0.5], [1000.5, 0.5], 1, false), null);
  const full = Array.from({length: 1024}, (_, i) => [i % 32, Math.floor(i / 32)]);
  assert.equal(paintBrush(full, [40.5, 0.5], [40.5, 0.5], 1, false), null);
});

test('saved painted cells round-trip without aliasing and invalid goals are rejected', () => {
  const paint = {cells: [[100, -100]], target_percent: 90, minimum_margin_db: 2.5};
  const saved = readMeshPaint(JSON.parse(JSON.stringify(paint)));
  assert.deepEqual(saved, paint);
  saved.cells[0][0] = 999;
  assert.equal(paint.cells[0][0], 100);
  assert.throws(() => readMeshPaint({...paint, cells: [[0, 0], [0, 0]]}), /invalid/);
  assert.throws(() => readMeshPaint({...paint, minimum_margin_db: ''}), /invalid/);
  assert.throws(() => readMeshPaint({...paint, target_percent: 101}), /invalid/);
});
