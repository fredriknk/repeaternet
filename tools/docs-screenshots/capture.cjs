const { spawn } = require('node:child_process');
const { randomUUID, randomBytes } = require('node:crypto');
const fs = require('node:fs/promises');
const os = require('node:os');
const path = require('node:path');
const { setTimeout: delay } = require('node:timers/promises');
const { parseArgs } = require('node:util');
const { createWorkflow } = require('./workflow.cjs');

const root = path.resolve(__dirname, '../..');
const guideImages = path.join(root, 'docs/images');
const defaultExe = path.join(root, 'rust/target/release',
  process.platform === 'win32' ? 'repeaternet.exe' : 'repeaternet');
const pngLimit = 400_000;

async function startServer(exe, data, token, signal) {
  // Port 0 lets the server bind a free port directly without a reservation race.
  const server = spawn(exe, ['--host', '127.0.0.1', '--port', '0', '--data', data, '--no-browser'], {
    cwd: data,
    env: {
      ...process.env,
      RF_PLANNER_TOKEN: token,
      RF_PLANNER_CACHE_DIR: process.env.RF_PLANNER_CACHE_DIR || path.join(data, 'shared-cache'),
    },
    stdio: ['ignore', 'pipe', 'inherit'],
    windowsHide: true,
  });
  const closed = new Promise(resolve => server.once('close', resolve));
  function stop() {
    server.kill('SIGKILL');
    return closed;
  }
  try {
    const startup = AbortSignal.any([signal, AbortSignal.timeout(30_000)]);
    const url = await new Promise((resolve, reject) => {
      let output = '';
      const abort = () => reject(startup.reason);
      startup.addEventListener('abort', abort, { once: true });
      server.on('error', reject);
      server.once('close', code => reject(new Error(`Server exited during startup (code ${code})`)));
      server.stdout.on('data', chunk => {
        output += chunk;
        const match = output.match(/RepeaterNet is listening at (http:\/\/127\.0\.0\.1:\d+\/)/);
        if (match) {
          startup.removeEventListener('abort', abort);
          resolve(match[1]);
          output = '';
        }
      });
    });
    while (!startup.aborted) {
      try {
        const health = await fetch(new URL('health', url), {
          signal: AbortSignal.any([startup, AbortSignal.timeout(2000)]),
        });
        if (health.ok && (await health.json()).status === 'ok') return { url, stop };
      } catch {
        // The listener can announce its address just before it handles requests.
      }
      await delay(200, undefined, { signal: startup });
    }
    startup.throwIfAborted();
  } catch (error) {
    await stop();
    throw error;
  }
}

async function fitPng(png) {
  if (png.length < pngLimit) return png;
  const sharp = require('sharp');
  const { width } = await sharp(png).metadata();
  // An indexed palette usually preserves the original resolution and keeps UI text sharp.
  for (const scale of [1, 0.9, 0.8, 0.7, 0.6, 0.5]) {
    const image = sharp(png);
    if (scale < 1) image.resize({ width: Math.round(width * scale) });
    const compact = await image.png({
      palette: true, colours: 192, dither: 0, compressionLevel: 9,
    }).toBuffer();
    if (compact.length < pngLimit) return compact;
  }
  throw new Error('Could not fit PNG under 400 KB');
}

async function publishPng(out, name, png) {
  // Keep the previous image intact if rendering or writing fails.
  const target = path.join(out, `${name}.png`);
  const temporary = path.join(out, `.${name}-${randomUUID()}.tmp`);
  try {
    await fs.writeFile(temporary, png);
    await fs.rename(temporary, target);
  } finally {
    await fs.rm(temporary, { force: true });
  }
}

async function validateImages(stages) {
  const planned = stages.flatMap(stage => stage.images);
  const guide = await fs.readFile(path.join(root, 'docs/USER_GUIDE.md'), 'utf8');
  const references = [...new Set([...guide.matchAll(/\(images\/([\w-]+)\.png\)/g)]
    .map(match => match[1]))];
  const files = (await fs.readdir(guideImages))
    .filter(name => name.endsWith('.png')).map(name => path.basename(name, '.png'));
  for (const [source, names] of [['guide references', references], ['docs/images', files]]) {
    const missing = names.filter(name => !planned.includes(name));
    const extra = planned.filter(name => !names.includes(name));
    if (missing.length || extra.length) {
      throw new Error(`Capture stages differ from ${source}. Missing: ${missing.join(', ') || 'none'}. Extra: ${extra.join(', ') || 'none'}.`);
    }
  }
  if (new Set(planned).size !== planned.length) throw new Error('Capture stage lists an image twice');
  return planned;
}

async function main() {
  const { values } = parseArgs({
    options: {
      exe: { type: 'string' },
      only: { type: 'string' },
      out: { type: 'string' },
      'keep-data': { type: 'boolean' },
      help: { type: 'boolean', short: 'h' },
    },
  });
  if (values.help) {
    console.log('Usage: npm run capture -- [--exe <path>] [--only <name[,name]>] [--out <dir>] [--keep-data]');
    return;
  }

  const exe = values.exe ? path.resolve(values.exe) : defaultExe;
  if (!(await fs.stat(exe).catch(() => null))?.isFile()) {
    throw new Error(`RepeaterNet executable missing at ${exe}. From rust/, run cargo build --release -p repeaternet.`);
  }
  const out = values.out ? path.resolve(values.out) : guideImages;
  const produced = new Set();
  const controller = new AbortController();
  const { signal } = controller;
  const token = randomBytes(24).toString('hex');
  const started = Date.now();
  let browser, page, data, server, stage;
  let interruptCode;
  let requested;
  const stages = createWorkflow({
    get page() { return page; },
    root,
    token,
    signal,
    wants: name => requested.has(name),
    async shot(name, selector, prepare) {
      if (!stage.images.includes(name)) throw new Error(`${name} is missing from stage ${stage.id}`);
      if (!requested.has(name)) return;
      signal.throwIfAborted();
      try {
        const clip = await prepare?.();
        const options = { animations: 'disabled', timeout: 5000, clip };
        const png = selector
          ? await page.locator(selector).screenshot(options)
          : await page.screenshot(options);
        const fitted = await fitPng(png);
        signal.throwIfAborted();
        await publishPng(out, name, fitted);
        produced.add(name);
        console.log(`  Saved ${name}.png (${fitted.length} bytes)`);
      } catch (error) {
        console.error(`  Skipped ${name}.png: ${error.message}`);
      }
    },
  });
  const planned = await validateImages(stages);
  requested = new Set(values.only === undefined ? planned
    : values.only.split(',').map(name => name.trim().replace(/\.png$/, '')));
  const unknown = [...requested].filter(name => !planned.includes(name));
  if (unknown.length) throw new Error(`Unknown screenshot: ${unknown.map(JSON.stringify).join(', ')}`);

  const needed = new Set();
  function requireStage(item) {
    if (needed.has(item.id)) return;
    needed.add(item.id);
    for (const id of item.after) requireStage(stages.find(candidate => candidate.id === id));
  }
  stages.filter(item => item.images.some(name => requested.has(name))).forEach(requireStage);

  function interrupt(code) {
    interruptCode = code;
    controller.abort(new Error('Capture interrupted'));
    // Closing Chrome releases any in-flight Playwright wait so finally can run.
    void browser?.close().catch(() => {});
  }
  const onInterrupt = () => interrupt(130);
  const onTerminate = () => interrupt(143);
  process.on('SIGINT', onInterrupt);
  process.on('SIGTERM', onTerminate);
  try {
    const { chromium } = require('playwright-core');
    data = await fs.mkdtemp(path.join(os.tmpdir(), 'repeaternet-docs-'));
    signal.throwIfAborted();
    server = await startServer(exe, data, token, signal);
    browser = await chromium.launch({
      channel: 'chrome', headless: true, handleSIGINT: false, handleSIGTERM: false,
    });
    signal.throwIfAborted();
    page = await browser.newPage({ viewport: { width: 1440, height: 900 }, locale: 'en-US' });
    page.setDefaultTimeout(8000);
    page.on('pageerror', error => console.error(`Page error: ${error.message}`));
    page.on('response', async response => {
      const { pathname } = new URL(response.url());
      if (pathname.startsWith('/api/') && response.status() >= 400) {
        const body = await response.text().catch(() => '');
        console.error(`HTTP ${response.status()} ${response.request().method()} ${pathname}: ${body.slice(0, 500)}`);
      }
    });
    page.on('requestfailed', request => {
      const { pathname } = new URL(request.url());
      if (pathname.startsWith('/api/')) {
        console.error(`Request failed: ${request.method()} ${pathname}: ${request.failure()?.errorText}`);
      }
    });
    await fs.mkdir(out, { recursive: true });
    const failed = new Set();
    for (stage of stages) {
      if (!needed.has(stage.id)) continue;
      signal.throwIfAborted();
      const dependency = stage.after.find(id => failed.has(id));
      try {
        if (dependency) throw new Error(`Prerequisite stage ${dependency} failed`);
        console.log(`Stage: ${stage.id}`);
        await stage.run(server.url);
      } catch (error) {
        failed.add(stage.id);
        const skipped = stage.images.filter(name => requested.has(name) && !produced.has(name));
        console.error(`Stage ${stage.id} failed: ${error.message}`);
        if (skipped.length) console.error(`  Skipped: ${skipped.join(', ')}`);
      }
    }
  } catch (error) {
    console.error(error.message);
  } finally {
    const cleanups = await Promise.allSettled([browser?.close(), server?.stop()]);
    if (data) {
      if (values['keep-data']) console.log(`Temporary data kept at ${data}`);
      else cleanups.push(...await Promise.allSettled([
        fs.rm(data, { recursive: true, force: true, maxRetries: 5 }),
      ]));
    }
    process.off('SIGINT', onInterrupt);
    process.off('SIGTERM', onTerminate);
    const cleanupErrors = cleanups.filter(result => result.status === 'rejected');
    for (const error of cleanupErrors) console.error(`Cleanup failed: ${error.reason.message}`);
    const skipped = [...requested].filter(name => !produced.has(name));
    console.log(`Captured ${produced.size}/${requested.size} images in ${((Date.now() - started) / 1000).toFixed(1)} s.`);
    if (skipped.length) console.error(`Skipped (existing PNGs kept): ${skipped.join(', ')}`);
    process.exitCode = interruptCode ?? (skipped.length || cleanupErrors.length ? 1 : 0);
  }
}

main().catch(error => {
  console.error(error.message);
  process.exitCode = 1;
});
