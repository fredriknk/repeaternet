const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const source = fs.readFileSync(path.join(__dirname, '../../assets/dashboard/live_coverage.js'), 'utf8');

function load(globals = {}) {
  const context = vm.createContext({
    TextDecoder, Math, JSON, Number, String, Object, Array, Float32Array, Uint8Array, Uint8ClampedArray, DataView, Set, Map,
    localStorage: { getItem() { return null; }, setItem() {} },
    ...globals,
  });
  vm.runInContext(source, context);
  context.liveCoverage = vm.runInContext('liveCoverage', context);
  return context;
}

/* Build an RNLC body with a 3857 raster whose west/north edge is at (0, 0). */
function encode(header, codes) {
  const json = Buffer.from(JSON.stringify({ floor_db: -20, step_db: .5, origin_x: 0, origin_y: 0, pixel_size: 10, ...header }));
  const body = Buffer.alloc(8 + json.length + codes.length);
  body.write('RNLC', 0, 'ascii');
  body.writeUInt32LE(json.length, 4);
  json.copy(body, 8);
  Buffer.from(codes).copy(body, 8 + json.length);
  return body.buffer.slice(body.byteOffset, body.byteOffset + body.length);
}
const code = db => 3 + Math.round((db + 20) * 2);

test('live coverage rasters decode margins and keep outside, unknown and below-floor codes apart', () => {
  const { decodeLiveCoverage } = load();
  const raster = decodeLiveCoverage(encode({ width: 2, height: 2 }, [0, 1, 2, code(12.5)]));
  assert.equal(raster.header.width, 2);
  assert.ok(Number.isNaN(raster.lut[0]) && Number.isNaN(raster.lut[1]));
  assert.equal(raster.lut[2], -20);
  assert.equal(raster.lut[code(12.5)], 12.5);
  assert.deepEqual([raster.west, raster.north, raster.east, raster.south], [0, 0, 20, -20]);
  assert.throws(() => decodeLiveCoverage(encode({ width: 4, height: 4 }, [0])), /truncated/);
});

test('sampling interpolates margins, falls back to nearest at unknown terrain and is NaN outside', () => {
  const { decodeLiveCoverage, liveCoverageSample } = load();
  const raster = decodeLiveCoverage(encode({ width: 2, height: 2 }, [code(0), code(10), code(0), code(10)]));
  assert.ok(Math.abs(liveCoverageSample(raster, 10, -4) - 5) < 1e-6, 'midway along the top row');
  const gap = decodeLiveCoverage(encode({ width: 2, height: 2 }, [code(0), code(10), 1, code(10)]));
  assert.equal(liveCoverageSample(gap, 4, -16), null, 'nearest unknown sample');
  assert.equal(liveCoverageSample(gap, 16, -4), 10, 'an unknown neighbour falls back to the nearest margin');
  assert.ok(Number.isNaN(liveCoverageSample(raster, 25, -5)));
  // A below-floor neighbour interpolates from the floor, so the 0 dB contour is smooth.
  const edge = decodeLiveCoverage(encode({ width: 2, height: 1 }, [code(10), 2]));
  const middle = liveCoverageSample(edge, 10, -5);
  assert.ok(Math.abs(middle - -5) < 1e-6);
});

test('tiles paint the best margin, highlight overlap and hatch unknown terrain', () => {
  let painted;
  const canvas = { getContext: () => ({ clearRect() {}, createImageData: (w, h) => ({ data: new Uint8ClampedArray(w * h * 4) }), putImageData(image) { painted = image.data; } }) };
  const context = load();
  const { decodeLiveCoverage, liveCoveragePalette, drawLiveCoverageTile, liveCoverage } = context;
  // At zoom 0 a tile pixel spans about 156 km, so build rasters covering one pixel each.
  const pixel = 2 * Math.PI * 6378137 / 256, west = -pixel * 128, north = pixel * 128;
  const covered = (x, y, db) => decodeLiveCoverage(encode({ width: 1, height: 1, origin_x: west + x * pixel, origin_y: north - y * pixel, pixel_size: pixel }, [code(db)]));
  const unknown = decodeLiveCoverage(encode({ width: 1, height: 1, origin_x: west + 3 * pixel, origin_y: north, pixel_size: pixel }, [1]));
  liveCoverage.palette = liveCoveragePalette('viridis', 30);
  const rasters = [covered(0, 0, 30), covered(1, 0, 5), covered(1, 0, 20), unknown];
  liveCoverage.slots = new Map(rasters.map((raster, i) => [String(i), { slot: String(i), raster, site: { label: `S${i}` } }]));
  const draw = () => { drawLiveCoverageTile(canvas, { x: 0, y: 0, z: 0 }); return painted; };
  let data = draw();
  assert.deepEqual([...data.slice(0, 4)], [0xfd, 0xe7, 0x25, 255], 'top of the viridis scale');
  assert.ok(data[7] > 0 && data[7] <= 255, 'second pixel covered');
  assert.equal(data[11], 0, 'third pixel clear');
  assert.ok(data[15] > 0, 'unknown terrain is hatched');
  liveCoverage.settings.overlap = true;
  data = draw();
  assert.deepEqual([...data.slice(4, 7)], [0xff, 0x2d, 0x95], 'two sites overlap');
  liveCoverage.settings.show_unknown = false;
  liveCoverage.hidden.add('2');
  data = draw();
  assert.notDeepEqual([...data.slice(4, 7)], [0xff, 0x2d, 0x95], 'a hidden site no longer overlaps');
  assert.equal(data[15], 0);
});

test('margin bands keep the shared 0, 5 and 10 dB colours', () => {
  const { liveCoveragePalette } = load();
  const palette = liveCoveragePalette('bands', 15);
  const at = db => [...palette.slice(Math.round(db / 15 * 255) * 3, Math.round(db / 15 * 255) * 3 + 3)];
  assert.deepEqual(at(0), [0xf1, 0x84, 0x2f]);
  assert.deepEqual(at(15), [0x12, 0x83, 0x4e]);
  const ten = at(10);
  assert.ok(Math.abs(ten[0] - 0x35) <= 2 && Math.abs(ten[1] - 0xac) <= 2);
});

test('sites cover endpoints, proposed, route and included existing routers once each', () => {
  const context = load({
    rf: { router: { height_agl_m: 6 }, endpoint_a: { height_agl_m: 4 }, endpoint_b: { height_agl_m: 5 } },
    validEndpointCoords: name => (name === 'a' ? [60, 10] : [60.5, 10.5]),
    manualRouters: [{ id: 'M-1', latitude: 60.2, longitude: 10.2, antenna_height_m: 12 }],
    result: { found: true, active_alternative_id: 'x', alternatives: [{ id: 'x', route: [
      { id: 'A', kind: 'endpoint_a', latitude: 60, longitude: 10 },
      { id: 'cand-1', kind: 'candidate', latitude: 60.3, longitude: 10.3, antenna_height_m: 9 },
      { id: 'known-1', kind: 'router', origin: 'known', latitude: 60.4, longitude: 10.4, antenna_height_m: 7 },
    ] }] },
    resultStale: false,
    routeSiteLabel: id => ({ 'cand-1': 'R1', 'known-1': 'Hill' }[id] || id),
    selectedKnownRouters: () => [
      { id: 'known-1', name: 'Hill', policy: 'optional', latitude: 60.4, longitude: 10.4 },
      { id: 'known-2', name: 'Off', policy: 'excluded', latitude: 60.6, longitude: 10.6 },
      { id: 'known-3', policy: 'required', latitude: 60.7, longitude: 10.7 },
    ],
  });
  const sites = context.liveCoverageSites();
  assert.deepEqual(Array.from(sites, site => site.slot), ['endpoint:a', 'endpoint:b', 'manual:M-1', 'route:cand-1', 'known:known-1', 'known:known-3']);
  assert.deepEqual(Array.from(sites, site => site.antenna_height_m), [4, 5, 12, 9, 7, 6]);
  context.resultStale = true;
  context.liveCoverage.settings.sources.endpoints = false;
  assert.deepEqual(Array.from(context.liveCoverageSites(), site => site.label), ['M1', 'Hill', 'known-3']);
});
