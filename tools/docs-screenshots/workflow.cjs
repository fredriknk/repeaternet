const path = require('node:path');
const { setTimeout: delay } = require('node:timers/promises');

const desktop = { width: 1440, height: 900 };
const tallDesktop = { width: 1440, height: 1130 };
const projectName = 'Oslo to Harestua';
const dialog = '#action-dialog';
const menu = '.project-menu > summary';
const searchDetails = '#search-settings details > summary';
const radioDetails = '#settings details:first-of-type > summary';
const antennaDetails = '#settings details:nth-of-type(2) > summary';
const sampling = '#coverage-sampling-controls > summary';
const heightControls = '#coverage-comparison-controls';
const targets = '#coverage-targets-controls';
const targetComparison = `${targets} > details:last-child`;
const activeRun = '#coverage-active-run';

async function retry(label, action, signal) {
  for (const backoff of [0, 1500, 4500]) {
    if (backoff) await delay(backoff, undefined, { signal });
    signal.throwIfAborted();
    try {
      return await action();
    } catch (error) {
      console.error(`  ${label}: ${error.message}`);
      if (backoff === 4500 || signal.aborted) throw error;
      console.log(`  Retrying ${label} with backoff`);
    }
  }
}

function createWorkflow(run) {
  const { root, shot, wants, signal } = run;
  const $ = selector => run.page.locator(selector);
  const click = id => $(`#${id}`).click();
  const by = name => run.page.getByRole('button', { name, exact: true });
  const tab = name => by(name).click();
  const coverage = name => $(`[data-coverage-section="${name}"]`).click();
  const visible = (selector, timeout) => $(selector).waitFor({ state: 'visible', timeout });
  const ready = selector => run.page.waitForFunction(selector =>
    !document.querySelector(selector)?.disabled, selector);

  let baseline, scenario;

  async function step(id) {
    const section = $(`[data-plan-section="${id}-setup"]`);
    if (await section.getAttribute('aria-expanded') !== 'true') await section.click();
  }

  async function openDetails(selector) {
    const summary = $(selector);
    if (!await summary.evaluate(element => element.parentElement.open)) await summary.click();
  }

  async function field(selector, value) {
    const input = $(selector);
    await input.fill(String(value));
    await input.blur();
  }

  async function saved() {
    await run.page.waitForFunction(() =>
      document.querySelector('#project-storage').dataset.saveState === 'saved');
  }

  async function waitForText(selector, substring, timeout) {
    await run.page.waitForFunction(([selector, substring]) =>
      document.querySelector(selector)?.textContent.includes(substring), [selector, substring], { timeout });
  }

  async function confirm(name) {
    await visible(dialog, 20_000);
    if (name) await shot(name, dialog);
    await click('action-dialog-confirm');
  }

  async function dismissJob() {
    if (await $('#dismiss-job').isVisible()) await click('dismiss-job');
  }

  async function taller(action) {
    await run.page.setViewportSize(tallDesktop);
    try {
      await action();
    } finally {
      await run.page.setViewportSize(desktop);
    }
  }

  async function mapReady() {
    // Live coverage settles before a map image is taken.
    await run.page.waitForFunction(() => typeof liveCoverage === 'undefined'
      || ![...liveCoverage.slots.values()].some(slot => slot.loading), undefined, { timeout: 120_000 });
    await retry('basemap tiles', async () => {
      try {
        await run.page.waitForFunction(() => {
          // Leaflet retains old tiles during a zoom. Read the active tile level only.
          const layers = Object.values(map._layers).filter(layer => layer instanceof L.TileLayer);
          return !map._animatingZoom && layers.length > 0 && layers.every(layer => {
            const tiles = Object.values(layer._tiles).filter(tile => tile.current);
            return tiles.length > 0 && tiles.every(tile => tile.el.complete && tile.el.naturalWidth);
          });
        }, undefined, { timeout: 8000 });
      } catch (error) {
        await run.page.evaluate(() => map.eachLayer(layer => {
          if (layer instanceof L.TileLayer) layer.redraw();
        }));
        throw error;
      }
    }, signal);
  }

  async function mapPoint(lat, lng) {
    const position = await run.page.evaluate(([lat, lng]) => {
      const point = map.latLngToContainerPoint([lat, lng]);
      return { x: point.x, y: point.y };
    }, [lat, lng]);
    await $('#map').click({ position });
  }

  function crop(name, firstSelector, lastSelector = firstSelector) {
    return shot(name, null, async () => {
      await $(firstSelector).evaluate(element => {
        const sidebar = document.querySelector('#planner-setup');
        sidebar.scrollTop += element.getBoundingClientRect().top - sidebar.getBoundingClientRect().top - 79;
      });
      const first = await $(firstSelector).boundingBox();
      const last = await $(lastSelector).boundingBox();
      const clip = {
        x: Math.round(Math.min(first.x, last.x)),
        y: Math.round(first.y),
        width: Math.round(Math.max(first.x + first.width, last.x + last.width) - Math.min(first.x, last.x)),
        height: Math.round(last.y + last.height - first.y),
      };
      if (clip.y < 0 || clip.y + clip.height > run.page.viewportSize().height) {
        throw new Error('Screenshot crop extends beyond the viewport');
      }
      return clip;
    });
  }

  async function waitForProject(name) {
    await run.page.waitForFunction(name =>
      !switchingProject && document.querySelector('#project-select').selectedOptions[0]?.textContent === name,
    name);
  }

  async function newProject(name) {
    await click('new-project');
    await $('#action-dialog-value').fill(name);
    await confirm();
    await waitForProject(name);
  }

  async function signIn(url) {
    await run.page.goto(url, { waitUntil: 'domcontentloaded' });
    await shot('token-sign-in', 'form');
    await $('[name=token]').fill(run.token);
    await tab('Sign in');
    await run.page.waitForFunction(() => document.querySelector('#project-select')?.value);
  }

  async function mapControls() {
    await shot('map-toolbar', '.map-tools');
    await openDetails('.route-map-legend > summary');
    await shot('map-legend', '.route-map-legend');
    await $('.route-map-legend > summary').click();
    if (wants('map-layer-control')) {
      await $('.leaflet-control-layers').hover();
      await visible('.leaflet-control-layers-expanded');
      await shot('map-layer-control', '.leaflet-control-layers');
      await run.page.mouse.move(750, 150);
    }
  }

  async function routeSetup() {
    await $(menu).click();
    await click('rename-project');
    await $('#action-dialog-value').fill(projectName);
    await confirm();
    await waitForProject(projectName);

    // Match the user-guide project, including the original 3.5 km corridor.
    await step('endpoint');
    await field('#a', '59.942875, 10.737784');
    await field('#b', '60.203769, 10.714008');
    await shot('route-endpoints', '.plan-step[data-step=endpoint-setup]');
    await click('fit');
    if (wants('map-context-menu')) {
      await $('#map').click({ button: 'right', position: { x: 510, y: 405 } });
      await shot('map-context-menu', '.leaflet-popup');
      await click('fit');
      if (await $('.leaflet-popup-close-button').isVisible()) await $('.leaflet-popup-close-button').click();
    }

    await step('search');
    await openDetails(searchDetails);
    await field('[data-key=corridor_width_m]', 3500);
    await field('[data-key=maximum_link_distance_m]', 15500);
    await field('[data-key=maximum_solution_routers]', 5);
    await crop('route-search-advanced', '#search-settings details');
    await $(searchDetails).click();
    await $('#road-access-enabled').check();
    await shot('route-search', '.plan-step[data-step=search-setup]');
    await $('#road-access-enabled').uncheck();

    await step('radio');
    await openDetails(radioDetails);
    await taller(() => shot('route-radio', '.plan-step[data-step=radio-setup]'));
    await $(radioDetails).click();
    await openDetails(antennaDetails);
    await crop('route-radio-antenna', antennaDetails, '#settings details:nth-of-type(2) > .fields:nth-of-type(1)');
    await $(antennaDetails).click();

    await step('router');
    await openDetails('#router-setup .coordinate-entry > summary');
    await field('#router-latitude', 60.059773);
    await field('#router-longitude', 10.69340);
    await field('#router-height', 10);
    await click('add-router-coordinate');
    await taller(() => crop('route-proposed-router', '#manual-list .manual-router'));
    await saved();

    await $(menu).click();
    await shot('project-menu', '.project-menu-actions');
    await run.page.keyboard.press('Escape');
    await shot('project-top-bar', null, async () => ({ x: 0, y: 0, width: 850, height: 56 }));
  }

  async function terrainEstimate() {
    await step('terrain');
    await openDetails('.terrain-tools summary');
    await field('#terrain-resolution', 40);
    await $('#terrain-dom').uncheck();
    await click('estimate-terrain');
    await visible('#prepare-terrain');
    await shot('route-terrain', '.plan-step[data-step=terrain-setup]');
    if (wants('kartverket-estimate')) {
      await click('optimize');
      await visible(dialog);
      await shot('kartverket-estimate', dialog);
      await click('action-dialog-cancel');
      await ready('#optimize');
    }
  }

  async function waitForJob() {
    const state = await run.page.waitForFunction(() =>
      !busy && ['complete', 'failed', 'cancelled', 'stopped', 'disconnected']
        .find(state => state === document.querySelector('#route-job-card').dataset.state),
    undefined, { timeout: 180_000 });
    if (await state.jsonValue() !== 'complete') throw new Error(await $('#job-message').innerText());
  }

  async function downloadTerrain() {
    await retry('Kartverket terrain', async () => {
      await step('terrain');
      await openDetails('.terrain-tools summary');
      await dismissJob();
      if (!await $('#prepare-terrain').isVisible()) await click('estimate-terrain');
      await click('prepare-terrain');
      // The download is confirmed with its size before it starts.
      await visible(dialog);
      await click('action-dialog-confirm');
      await visible('#route-job-card:not([hidden])');
      await waitForJob();
      await waitForText('#coverage-note', 'Ground terrain covers selected sites', 20_000);
    }, signal);
    await dismissJob();
  }

  async function meshcore() {
    await step('router');
    const rows = $('#meshcore-list select');
    await retry('CoreScope routers', async () => {
      // Check the request and the rendered rows before changing any policies.
      const [response] = await Promise.all([
        run.page.waitForResponse(response => new URL(response.url()).pathname === '/api/meshcore', { timeout: 60_000 }),
        click('load-meshcore'),
      ]);
      await ready('#load-meshcore');
      if (!response.ok()) throw new Error(await $('#plan-action-status').innerText());
      if (await rows.count() < 2) throw new Error('Need two nearby routers to illustrate the policy controls');
    }, signal);
    await rows.first().selectOption('optional');
    await rows.nth(1).selectOption('required');
    await crop('route-router-controls', '#router-setup > h3:first-of-type', '#router-setup > .known-controls');
    await crop('route-meshcore', '#meshcore-list');
    if (wants('map-router-policy')) {
      await $('#meshcore-list .router-focus').first().click();
      await $('.leaflet-overlay-pane path[stroke="#3f2c66"]').first().click();
      await shot('map-router-policy', '.leaflet-popup');
      await $('.leaflet-popup-close-button').click();
    }
    await rows.nth(1).selectOption('excluded');
    await saved();
  }

  async function searchRoute() {
    await step('search');
    await retry('route search', async () => {
      await dismissJob();
      await click('optimize');
      await visible('#route-job-card:not([hidden])');
      if (wants('route-search-job')) {
        await shot('route-search-job', '#route-job-card:is([data-state=queued], [data-state=running])');
      }
      await waitForJob();
      await $('#alternative-select option').first().waitFor({ state: 'attached' });
    }, signal);
    await dismissJob();
  }

  async function routeResults() {
    await step('endpoint');
    await shot('dashboard-route-overview', null, mapReady);
    if (wants('map-route-site')) {
      await $('.map-site-proposed,.map-site-existing').first().click();
      if (await $('#route-link-inspector-site').isVisible()) await click('route-link-inspector-site');
      await shot('map-route-site', '.leaflet-popup');
      await $('.leaflet-popup-close-button').click();
    }
    await click('view-analysis-tab');
    await shot('analysis-verdict');
    await openDetails('#alternative-comparison > summary');
    await shot('analysis-alternatives', '#alternative-comparison');
    await $('#alternative-comparison > summary').click();
    await shot('analysis-links', '#hops-disclosure');
    await openDetails('#directional-budget > summary');
    await shot('analysis-terrain-profile', '#profile');
    await shot('route-exports', '#exports');
    await click('view-map-tab');
  }

  async function coverageSetup() {
    await click('view-analysis-tab');
    await $('#alternative-select').selectOption({ index: 1 });
    await ready('#alternative-select');
    await click('route-show-map');
    await tab('2 Coverage');
    await coverage('display');
    await run.page.waitForFunction(() => liveCoverage.slots.size
      && ![...liveCoverage.slots.values()].some(slot => slot.loading), undefined, { timeout: 120_000 });
    const live = await $('#live-coverage-status').textContent();
    if (!live.includes('up to date')) throw new Error(`${live} ${await $('#live-coverage-sites').innerText()}`);
    await shot('live-coverage-map', null, mapReady);
    await $('#live-legend-overlap').check();
    await shot('live-coverage-overlap', null, mapReady);
    await $('#live-legend-overlap').uncheck();
    await coverage('handheld');
    await taller(() => shot('coverage-handheld', '#coverage-section-handheld'));
    await savedRunSettings();
    await openDetails(sampling);
    await field('#coverage-cell-size', 700);
    await field('#coverage-buffer', 1300);
    await field('#coverage-profile-step', 100);
  }

  async function savedRunSettings() {
    await coverage('runs');
    await openDetails('#saved-run-settings > summary');
  }

  async function coverageRun(previous) {
    const status = await run.page.waitForFunction(() => {
      const status = document.querySelector('#coverage-job-status');
      return (status.classList.contains('error') || status.textContent.includes('all requested samples loaded'))
        && !coverageBusy && !coveragePreflightBusy && status.textContent;
    }, undefined, { timeout: 180_000 });
    const message = await status.jsonValue();
    if (!message.includes('all requested samples loaded')) throw new Error(message);
    const id = await run.page.waitForFunction(previous => {
      const value = document.querySelector('#coverage-active-run').value;
      return value && value !== previous && value;
    }, previous);
    return id.jsonValue();
  }

  async function calculateCoverage() {
    await click('coverage-start');
    await confirm('coverage-calculation-estimate');
    baseline = await coverageRun();
    await coverage('runs');
    await shot('coverage-runs', '.coverage-context');
  }

  async function inspectCoverage() {
    await click('coverage-inspect');
    await mapPoint(60.106, 10.726);
    await visible('#coverage-inspection', 30_000);
    await openDetails('#coverage-inspection details > summary');
    await shot('coverage-location-inspection', '#coverage-inspection');
    await click('inspection-show-map');
  }

  async function failureScenario() {
    await tab('3 Scenarios');
    await openDetails('#coverage-failure-scenarios > summary');
    await $('[data-failure-source]').last().check();
    await click('failure-run');
    await waitForText('#failure-summary', 'locally covered cell');
    await shot('scenario-router-failure', '#coverage-failure-scenarios');
    await $('#coverage-failure-scenarios > summary').click();
  }

  async function compareHeights() {
    await taller(async () => {
      await tab('3 Scenarios');
      await openDetails(`${heightControls} > summary`);
      await $('[data-height-preset="10"]').click();
      await crop('scenario-height-controls', `${heightControls} > summary`, '#coverage-height-apply');
      await click('coverage-height-run');
      await confirm('scenario-height-estimate');
      scenario = await coverageRun(baseline);
      await tab('3 Scenarios');
      await openDetails(`${heightControls} > summary`);
      await $('#coverage-baseline-run').selectOption(baseline);
      await $('#coverage-scenario-run').selectOption(scenario);
      await $('#coverage-compare-reference').selectOption({ index: 0 });
      await click('coverage-compare-run');
      await waitForText('#coverage-compare-summary', 'common evaluated samples', 25_000);
      await crop('scenario-heights-comparison', 'label:has(#coverage-baseline-run)', '#coverage-diff-legend');
      await $(`${heightControls} > summary`).click();
    });
  }

  async function assessTargets() {
    await click('view-map-tab');
    await click('fit');
    await tab('3 Scenarios');
    await openDetails(`${targets} > summary`);
    await click('coverage-add-target');
    await mapPoint(60.107, 10.711);
    await $('#action-dialog-value').fill('Nordmarka trail checkpoint');
    await confirm('scenario-target-name');
    await visible('#coverage-target-list .coverage-target-row');
    await click('coverage-assess-targets');
    await visible('#coverage-target-results .known-router');
    await taller(() => crop('scenario-targets', `${targets} > summary`, '#coverage-target-results'));
  }

  async function compareTargets() {
    await tab('2 Coverage');
    await coverage('runs');
    await $(activeRun).selectOption(baseline);
    await tab('3 Scenarios');
    await click('coverage-assess-targets');
    const reports = await run.page.waitForFunction(() =>
      document.querySelector('#coverage-target-baseline').options.length >= 2);
    await reports.dispose();
    await openDetails(`${targetComparison} > summary`);
    await $('#coverage-target-baseline').selectOption({ index: 0 });
    await $('#coverage-target-scenario').selectOption({ index: 1 });
    await click('coverage-target-compare');
    await visible('#coverage-target-comparison-results .known-router');
    await shot('scenario-target-comparison', targetComparison);
  }

  async function coverageExports() {
    await click('view-map-tab');
    await tab('2 Coverage');
    await coverage('runs');
    await $(activeRun).selectOption(baseline);
    await click('coverage-load-run');
    await waitForText('#coverage-load-status', 'All saved cells loaded');
    await click('coverage-export-json');
    await waitForText('#coverage-export-status', 'Export downloaded');
    await taller(() => crop('coverage-export-options', '#coverage-export-controls'));
  }

  async function drawCoverageArea() {
    await click('view-map-tab');
    await click('fit');
    await tab('2 Coverage');
    await savedRunSettings();
    if (await $(sampling).evaluate(element => element.parentElement.open)) await $(sampling).click();
    await $('#coverage-area').selectOption('drawn');
    for (const [lat, lng] of [[60.085, 10.680], [60.113, 10.680], [60.113, 10.735], [60.085, 10.735]]) {
      await mapPoint(lat, lng);
    }
    await click('coverage-finish-area');
    await saved();
    await $('#map-hint').waitFor({ state: 'hidden' });
    await shot('coverage-drawn-area', null, mapReady);
    await $('#coverage-area').selectOption('mesh');
  }

  async function supplementalTerrain() {
    await click('view-map-tab');
    await click('fit');
    await tab('2 Coverage');
    await savedRunSettings();
    // Viewport bounds extend beyond the route terrain tiles even after Fit all.
    await $('#coverage-area').selectOption('view');
    await visible('#coverage-terrain-download', 20_000);
    await click('coverage-terrain-download');
    await visible(dialog);
    await shot('coverage-terrain-download', dialog);
    await click('action-dialog-cancel');
    await $('#coverage-area').selectOption('mesh');
  }

  async function mobile() {
    // Reselect the documented route alternative before framing the mobile map.
    await click('view-analysis-tab');
    await $('#alternative-select').selectOption({ index: 1 });
    await ready('#alternative-select');
    await click('view-map-tab');
    await tab('1 Route');
    await step('endpoint');
    await run.page.setViewportSize({ width: 390, height: 844 });
    try {
      for (const destination of ['map', 'setup', 'analysis']) {
        await $(`button[data-mobile-destination=${destination}]`).click();
        if (destination === 'map') {
          await $('#map-hint').waitFor({ state: 'hidden' });
          await click('fit');
        }
        await shot(`mobile-${destination}`, null, destination === 'map' ? mapReady : undefined);
      }
    } finally {
      await run.page.setViewportSize(desktop);
    }
  }

  async function staleRoute() {
    await click('view-map-tab');
    await tab('1 Route');
    await step('endpoint');
    await field('#b', '60.205000, 10.715000');
    await saved();
    await click('fit');
    await shot('map-stale-route', null, mapReady);
    await click('view-analysis-tab');
    await waitForText('#analysis-badge', 'Stale');
    await shot('route-stale');
  }

  async function archivedProject() {
    await newProject('Kartverket estimate example');
    await $(menu).click();
    await click('archive-project');
    await confirm();
    await waitForProject(projectName);
    await $(menu).click();
    await openDetails('#archived-projects > summary');
    await shot('project-archived', '.project-menu-actions');
    await run.page.keyboard.press('Escape');
  }

  async function localTerrain() {
    await newProject('Synthetic terrain practice');
    await tab('1 Route');
    await step('terrain');
    await openDetails('#terrain-setup > details > summary');
    await $('#dtm').setInputFiles(path.join(root, 'rust/crates/providers/tests/fixtures/tile-west.tif'));
    await waitForText('#dtm-count', '1 DTM');
    const bounds = await run.page.waitForFunction(() => {
      const footprint = coverageLayer.getLayers().find(layer => layer.getBounds);
      return footprint?.getBounds().toBBoxString();
    });
    const [west, south, east, north] = (await bounds.jsonValue()).split(',').map(Number);
    const latitude = ((south + north) / 2).toFixed(6);
    await step('endpoint');
    await field('#a', `${latitude}, ${(west + (east - west) * 0.25).toFixed(6)}`);
    await field('#b', `${latitude}, ${(west + (east - west) * 0.75).toFixed(6)}`);
    await waitForText('#coverage-note', 'Ground terrain covers selected sites');
    await step('terrain');
    await openDetails('#terrain-setup > details > summary');
    await crop('route-terrain-upload', '#terrain-setup > details', '#coverage-note');
  }

  // Listed in execution order. Dependencies supply the required project state for --only.
  return [
    { id: 'sign-in', after: [], images: ['token-sign-in'], run: signIn },
    { id: 'map-controls', after: ['sign-in'], images: ['map-toolbar', 'map-legend', 'map-layer-control'], run: mapControls },
    { id: 'route-setup', after: ['sign-in'], images: [
      'route-endpoints', 'map-context-menu', 'route-search-advanced', 'route-search',
      'route-radio', 'route-radio-antenna', 'route-proposed-router', 'project-menu', 'project-top-bar',
    ], run: routeSetup },
    { id: 'terrain-estimate', after: ['route-setup'], images: ['route-terrain', 'kartverket-estimate'], run: terrainEstimate },
    { id: 'meshcore', after: ['route-setup'], images: [
      'route-router-controls', 'route-meshcore', 'map-router-policy',
    ], run: meshcore },
    { id: 'terrain-download', after: ['terrain-estimate'], images: [], run: downloadTerrain },
    { id: 'route-search', after: ['terrain-download', 'meshcore'], images: ['route-search-job'], run: searchRoute },
    { id: 'route-results', after: ['route-search'], images: [
      'dashboard-route-overview', 'map-route-site', 'analysis-verdict', 'analysis-alternatives',
      'analysis-links', 'analysis-terrain-profile', 'route-exports',
    ], run: routeResults },
    { id: 'coverage-setup', after: ['route-search'], images: [
      'live-coverage-map', 'live-coverage-overlap', 'coverage-handheld',
    ], run: coverageSetup },
    { id: 'coverage-run', after: ['coverage-setup'], images: [
      'coverage-calculation-estimate', 'coverage-runs',
    ], run: calculateCoverage },
    { id: 'coverage-inspection', after: ['coverage-run'], images: ['coverage-location-inspection'], run: inspectCoverage },
    { id: 'failure-scenario', after: ['coverage-run'], images: ['scenario-router-failure'], run: failureScenario },
    { id: 'height-comparison', after: ['coverage-run'], images: [
      'scenario-height-controls', 'scenario-height-estimate', 'scenario-heights-comparison',
    ], run: compareHeights },
    { id: 'target-assessment', after: ['height-comparison'], images: [
      'scenario-target-name', 'scenario-targets',
    ], run: assessTargets },
    { id: 'target-comparison', after: ['target-assessment'], images: ['scenario-target-comparison'], run: compareTargets },
    { id: 'coverage-exports', after: ['coverage-run'], images: ['coverage-export-options'], run: coverageExports },
    { id: 'drawn-area', after: ['coverage-setup'], images: ['coverage-drawn-area'], run: drawCoverageArea },
    { id: 'supplemental-terrain', after: ['coverage-setup'], images: ['coverage-terrain-download'], run: supplementalTerrain },
    { id: 'mobile', after: ['route-search'], images: ['mobile-map', 'mobile-setup', 'mobile-analysis'], run: mobile },
    { id: 'stale-route', after: ['route-search'], images: ['map-stale-route', 'route-stale'], run: staleRoute },
    { id: 'archive', after: ['route-setup'], images: ['project-archived'], run: archivedProject },
    { id: 'local-terrain', after: ['sign-in'], images: ['route-terrain-upload'], run: localTerrain },
  ];
}

module.exports = { createWorkflow };
