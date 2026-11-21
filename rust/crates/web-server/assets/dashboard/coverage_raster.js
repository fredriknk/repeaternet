/* Warp the terrain grid to a single Web Mercator canvas for Leaflet. */
const coverageMarginStops = [
  [0, [241, 132, 47], .55],
  [5, [226, 207, 63], .90],
  [10, [53, 172, 96], 1],
  [15, [18, 131, 78], 1],
];
const coveragePatterns = {
  unknown: ['#a7afb0', '#626f72'],
  unresolved: ['#c4b6d5', '#765497'],
};
const coverageOverlapColors = ['#f0ca62', '#61a88c', '#287b64'];
const coverageChangeColors = {lost:'#c95158', retained:'#628bb8', gained:'#24946d'};

function coverageRgb(hex) {
  const n = Number.parseInt(hex.slice(1), 16);
  return [n >> 16, n >> 8 & 255, n & 255];
}

function coverageMarginPaint(margin) {
  const db = Math.max(0, Math.min(15, margin));
  const index = Math.min(2, Math.floor(db / 5));
  const [start, from, a] = coverageMarginStops[index];
  const [end, to, b] = coverageMarginStops[index + 1];
  const t = (db - start) / (end - start);
  return [...from.map((value, i) => value + t * (to[i] - value)), a + t * (b - a)];
}

function coverageRasterTriangle(a, b, c) {
  const dx1 = b.x - a.x, dy1 = b.y - a.y, dx2 = c.x - a.x, dy2 = c.y - a.y;
  const det = dx1 * dy2 - dx2 * dy1;
  if (!det) return null;
  const ux = ((b.u - a.u) * dy2 - (c.u - a.u) * dy1) / det;
  const uy = ((c.u - a.u) * dx1 - (b.u - a.u) * dx2) / det;
  const vx = ((b.v - a.v) * dy2 - (c.v - a.v) * dy1) / det;
  const vy = ((c.v - a.v) * dx1 - (b.v - a.v) * dx2) / det;
  return {
    left: Math.min(a.x, b.x, c.x), right: Math.max(a.x, b.x, c.x),
    top: Math.min(a.y, b.y, c.y), bottom: Math.max(a.y, b.y, c.y),
    ax: a.x, ay: a.y, dx1: dx1 / det, dy1: dy1 / det, dx2: dx2 / det, dy2: dy2 / det,
    u: a.u, v: a.v, ux, uy, vx, vy,
  };
}

function coverageTriangleContains(triangle, dx, dy) {
  const s = dx * triangle.dy2 - dy * triangle.dx2;
  const t = dy * triangle.dx1 - dx * triangle.dy1;
  return s >= -1e-9 && t >= -1e-9 && s + t <= 1 + 1e-9;
}

function coverageRasterGeometry(mesh, project) {
  const {across, down, width, height, vertices} = mesh;
  const points = vertices.map(project);
  let left = Infinity, right = -Infinity, south = Infinity, north = -Infinity;
  for (const {x, y} of points) {
    left = Math.min(left, x);
    right = Math.max(right, x);
    south = Math.min(south, y);
    north = Math.max(north, y);
  }
  if (!(right > left && north > south)) return null;
  // Give sparse previews enough pixels for hatching, and cap large canvas allocations.
  const extent = Math.max(right - left, north - south);
  const longest = Math.max(160, Math.min(1536, 3 * Math.max(width, height)));
  const pixelWidth = Math.max(1, Math.round(longest * (right - left) / extent));
  const pixelHeight = Math.max(1, Math.round(longest * (north - south) / extent));
  const toRaster = (x, y) => ({x:(x - left) / (right - left) * pixelWidth,
    y:(north - y) / (north - south) * pixelHeight});
  const grid = points.map(({x, y}, i) => ({...toRaster(x, y),
    u:(i % (across + 1)) / across * width, v:Math.floor(i / (across + 1)) / down * height}));
  const triangles = [];
  for (let row = 0; row < down; row++) {
    for (let column = 0; column < across; column++) {
      const i = row * (across + 1) + column;
      const [a, b, c, d] = [grid[i], grid[i + 1], grid[i + across + 1], grid[i + across + 2]];
      triangles.push(coverageRasterTriangle(a, b, d), coverageRasterTriangle(a, d, c));
    }
  }
  return {left, right, south, north, pixelWidth, pixelHeight, toRaster, triangles:triangles.filter(Boolean)};
}

function coverageRasterSamples({pixelWidth, pixelHeight, triangles}) {
  // Triangles share vertices and are sampled into one buffer. Avoid the antialiased
  // seams that canvas clipping and independently composited triangles can leave.
  const samples = new Float32Array(pixelWidth * pixelHeight * 2).fill(-1);
  for (const tri of triangles) {
    const start = Math.max(0, Math.ceil(tri.left - .5));
    const end = Math.min(pixelWidth - 1, Math.floor(tri.right - .5));
    for (let y = Math.max(0, Math.ceil(tri.top - .5)); y < pixelHeight && y + .5 <= tri.bottom; y++) {
      const dy = y + .5 - tri.ay;
      for (let x = start; x <= end; x++) {
        const dx = x + .5 - tri.ax;
        if (!coverageTriangleContains(tri, dx, dy)) continue;
        const i = (y * pixelWidth + x) * 2;
        samples[i] = Math.max(0, tri.u + dx * tri.ux + dy * tri.uy);
        samples[i + 1] = Math.max(0, tri.v + dx * tri.vx + dy * tri.vy);
      }
    }
  }
  return samples;
}

function paintCoverageRaster(image, samples, colors, patterns, columns, rows, categorical) {
  const {data, width} = image;
  for (let i = 0, p = 0; i < samples.length; i += 2, p += 4) {
    const u = samples[i], v = samples[i + 1];
    if (u < 0) continue;
    const nearest = Math.min(columns - 1, Math.floor(u)) + Math.min(rows - 1, Math.floor(v)) * columns;
    const pattern = patterns[nearest];
    if (pattern) {
      const pixel = i / 2;
      const stripe = (pixel % width + Math.floor(pixel / width)) % 8 < 3;
      const [r, g, b] = coveragePatternRgb[pattern - 1][Number(stripe)];
      data[p] = r; data[p + 1] = g; data[p + 2] = b; data[p + 3] = stripe ? 196 : 128;
      continue;
    }

    // Bilinear interpolation is centred on the RF sample positions. The grid
    // boundary clamps to the nearest sample; missing and unusable samples are clear.
    const x = Math.max(0, Math.min(columns - 1, u - .5));
    const y = Math.max(0, Math.min(rows - 1, v - .5));
    const x0 = Math.floor(x), y0 = Math.floor(y), fx = x - x0, fy = y - y0;
    const a = (y0 * columns + x0) * 4;
    const b = x0 + 1 < columns ? a + 4 : a;
    const c = y0 + 1 < rows ? a + columns * 4 : a;
    const d = c + (b - a);
    const wa = (1 - fx) * (1 - fy), wb = fx * (1 - fy), wc = (1 - fx) * fy, wd = fx * fy;
    const alpha = colors[a + 3] * wa + colors[b + 3] * wb + colors[c + 3] * wc + colors[d + 3] * wd;
    // A short fade follows the silhouette but leaves terrain shadows clear.
    if (alpha < 24 || (categorical && !colors[nearest * 4 + 3])) {
      data[p + 3] = 0;
      continue;
    }
    data[p + 3] = Math.min(alpha, (alpha - 24) * 2);
    const n = nearest * 4;
    for (let k = 0; k < 3; k++) {
      data[p + k] = categorical ? colors[n + k] * 255 / colors[n + 3]
        : (colors[a + k] * wa + colors[b + k] * wb + colors[c + k] * wc + colors[d + k] * wd) * 255 / alpha;
    }
  }
}

const coveragePatternRgb = Object.values(coveragePatterns).map(pair => pair.map(coverageRgb));

function createCoverageRaster(info, opacity, paint, categorical = false) {
  const {raster_mesh:mesh, columns, rows} = info;
  if (!mesh || !columns || !rows) return null;
  const crs = L.CRS.EPSG3857;
  const geometry = coverageRasterGeometry(mesh, latlng => crs.project(L.latLng(latlng)));
  if (!geometry) return null;
  const {left, right, south, north, pixelWidth, pixelHeight, triangles, toRaster} = geometry;
  const samples = coverageRasterSamples(geometry);
  const canvas = document.createElement('canvas');
  canvas.width = pixelWidth;
  canvas.height = pixelHeight;
  const ctx = canvas.getContext('2d');
  const image = ctx.createImageData(pixelWidth, pixelHeight);
  const colors = new Uint8ClampedArray(columns * rows * 4);
  const patterns = new Uint8Array(columns * rows);
  const stored = new Array(columns * rows);
  const bounds = L.latLngBounds(crs.unproject(L.point(left, south)), crs.unproject(L.point(right, north)));
  // SVGOverlay positions an arbitrary DOM element with ImageOverlay's bounds and zoom handling.
  const layer = L.svgOverlay(canvas, bounds, {pane:'coveragePane', opacity, interactive:false});
  canvas.classList.add('coverage-raster');
  let frame = 0;

  function draw() {
    frame = 0;
    paintCoverageRaster(image, samples, colors, patterns, columns, rows, categorical);
    ctx.putImageData(image, 0, 0);
  }

  layer.addCells = cells => {
    for (const cell of cells) {
      const index = cell.index;
      const style = paint(cell);
      stored[index] = cell;
      if (typeof style === 'number') patterns[index] = style;
      else if (style) {
        const [r, g, b, alpha = 1] = style;
        colors.set([r * alpha, g * alpha, b * alpha, 255 * alpha], index * 4);
      }
    }
    if (!frame) frame = requestAnimationFrame(draw);
  };
  layer.cellAt = latlng => {
    const point = crs.project(latlng);
    if (point.x < left || point.x > right || point.y < south || point.y > north) return null;
    const {x, y} = toRaster(point.x, point.y);
    for (const tri of triangles) {
      const dx = x - tri.ax, dy = y - tri.ay;
      if (!coverageTriangleContains(tri, dx, dy)) continue;
      const column = Math.max(0, Math.min(columns - 1, Math.floor(tri.u + dx * tri.ux + dy * tri.uy)));
      const row = Math.max(0, Math.min(rows - 1, Math.floor(tri.v + dx * tri.vx + dy * tri.vy)));
      return stored[row * columns + column];
    }
    return null;
  };
  return layer;
}
