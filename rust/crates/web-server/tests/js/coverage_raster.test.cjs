const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const code = fs.readFileSync(path.join(__dirname, '../../assets/dashboard/coverage_raster.js'), 'utf8');
const sandbox = vm.createContext({});
const {coverageRasterGeometry, coverageRasterSamples, coverageMarginPaint, paintCoverageRaster} =
  vm.runInContext(`${code}\n;({coverageRasterGeometry, coverageRasterSamples, coverageMarginPaint, paintCoverageRaster});`, sandbox);

test('coverage warp follows a sheared grid without cracks on shared triangle edges', () => {
  const mesh = {
    across: 2, down: 1, width: 8, height: 4,
    vertices: [[0, 40], [40, 40], [80, 40], [40, 0], [80, 0], [120, 0]],
  };
  const geometry = coverageRasterGeometry(mesh, ([x, y]) => ({x, y}));
  const {pixelWidth, pixelHeight} = geometry;
  const pixels = coverageRasterSamples(geometry);
  for (let y = 0; y < pixelHeight; y++) {
    for (let x = 0; x < pixelWidth; x++) {
      const v = 4 * (y + .5) / pixelHeight;
      const u = 12 * (x + .5) / pixelWidth - v;
      const i = (y * pixelWidth + x) * 2;
      if (u < -1e-8 || u > 8 + 1e-8) assert.equal(pixels[i], -1);
      else {
        assert.ok(Math.abs(pixels[i] - u) < 1e-6, `column at pixel ${x},${y}`);
        assert.ok(Math.abs(pixels[i + 1] - v) < 1e-6, `row at pixel ${x},${y}`);
      }
    }
  }
});

test('coverage samples fade into clear pixels and patterns replace signal in unknown cells', () => {
  const image = {width: 6, data: new Uint8ClampedArray(24)};
  const samples = [.5,.5, 1,.5, 1.5,.5, 2.5,.5, 2.6,.5, 3.5,.5];
  const colors = [20, 120, 40, 255, ...new Array(8).fill(0), 100, 20, 10, 128];
  paintCoverageRaster(image, samples, colors, [0, 0, 1, 0], 4, 1, false);
  const pixel = i => Array.from(image.data.slice(i * 4, i * 4 + 4));
  assert.deepEqual(pixel(0), [20, 120, 40, 255]);
  assert.deepEqual(pixel(1), [20, 120, 40, 128]);
  assert.equal(pixel(2)[3], 0);
  assert.deepEqual(pixel(3), [167, 175, 176, 128]);
  assert.equal(pixel(5)[3], 128);

  paintCoverageRaster(image, samples, colors, [0, 0, 0, 0], 4, 1, true);
  assert.equal(pixel(1)[3], 0, 'categorical data does not invent an adjacent class');
  assert.equal(pixel(3)[3], 0, 'a repainted transparent pixel clears the previous pattern');
});

test('the margin ramp uses the display band boundaries and continuous weak signal opacity', () => {
  const at = margin => Array.from(coverageMarginPaint(margin));
  assert.deepEqual(at(0), [241, 132, 47, .55]);
  assert.deepEqual(at(5), [226, 207, 63, .90]);
  assert.deepEqual(at(10), [53, 172, 96, 1]);
  assert.ok(at(2.5)[3] > at(0)[3] && at(2.5)[3] < at(5)[3]);
  assert.deepEqual(at(50), at(15));
});
