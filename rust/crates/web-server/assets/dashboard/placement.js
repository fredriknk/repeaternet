let placementPreview = null, placementLayer, placementOperation = null;
let placementSelectionRevision = 0, placementApplying = false;

function movablePlacementSite(site) {
  return site?.kind === 'manual' || site?.kind === 'search';
}

function placementAvailable(sites, stale) {
  return sites.length >= 2 && sites.some(movablePlacementSite) &&
    !(stale && sites.some(site => site.kind === 'search'));
}

function placementRadius(raw) {
  const value = raw.trim();
  if (!/^\+?(?:\d+(?:\.\d*)?|\.\d+)$/.test(value)) return null;
  const radius = Number(value);
  return radius > 0 && radius <= 2000 ? radius : null;
}

function installPlacement() {
  placementLayer = L.layerGroup().addTo(map);
  siteSelection.subscribe(() => {
    placementSelectionRevision++;
    clearPlacementPreview();
  });
  siteSelection.registerAction({
    id: 'optimize-placement',
    label: 'Optimize placement',
    isAvailable: sites => placementAvailable(sites, resultStale),
    run: ids => void startPlacement(ids),
  });
}

function clearPlacementPreview() {
  if (!placementPreview) return;
  placementPreview = null;
  placementLayer.clearLayers();
  $('placement-map-legend').hidden = true;
}

function placementFeedback(message) {
  selectionMessage = message;
  renderSiteSelection();
  setMapHint(message, true);
}

async function startPlacement(ids) {
  if (mapEditsLocked() || alternativeSelecting) return;
  const sites = [...ids].map(findMapSite).filter(Boolean);
  if (!placementAvailable(sites, resultStale)) return;
  if (sites.length > 32) {
    placementFeedback('Select up to 32 sites to optimize placement.');
    return;
  }
  const invalid = routeMode === 'mesh' ? null : ['a', 'b'].find(key => !validEndpointCoords(key));
  if (invalid) {
    focusProjectTransitionError($(invalid), `Correct Endpoint ${invalid.toUpperCase()} before optimizing placement.`);
    return;
  }
  const panel = document.activeElement.closest('.site-selection-panel');
  const revision = placementSelectionRevision;
  const defaultRadius = candidates.refine_radius_m > 0 ? candidates.refine_radius_m : 250;
  const movable = sites.filter(movablePlacementSite).length;
  const raw = await askActionDialog({
    title: 'Optimize placement',
    copy: `Search around ${movable} ${movable === 1 ? 'router' : 'routers'} across ${sites.length} selected sites. Endpoints, mesh clients and existing MeshCore routers stay fixed. Review the predicted link margins before applying any moves.`,
    label: 'Move radius · m',
    value: String(defaultRadius),
    validate: value => placementRadius(value) === null ? 'Enter a move radius greater than 0 and at most 2,000 m.' : '',
    help: {
      title: 'How links are chosen',
      copy: 'With a current route or mesh, only its links between selected sites count. Otherwise, all selected pairs within the configured link range count. The search improves the weakest link first, then the total margin.',
    },
    confirm: 'Run optimization',
  });
  if (raw === null || mapEditsLocked() || alternativeSelecting || revision !== placementSelectionRevision) return;
  placementOperation = {panel, revision};
  activeJobKind = 'placement';
  activeJobId = null;
  jobStartedAt = Date.now();
  $('route-job-card').hidden = true;
  setBusy(true);
  try {
    await flushAutosave();
    if (revision !== placementSelectionRevision) throw Error('The selection changed. Run the optimization again.');
    const job = await api('/api/optimize-placement', {
      method: 'POST',
      headers: {'Content-Type': 'application/json'},
      body: JSON.stringify({site_ids: [...ids], radius_m: placementRadius(raw)}),
    });
    activeJobId = job.job_id;
    showPlacementJob(job);
    $('cancel').focus({preventScroll: true});
    void pollPlacement();
  } catch (error) {
    finishPlacementJob('failed', 'Placement optimization did not start', error.message);
  }
}

function showPlacementJob(job) {
  updateJobCard({
    kind: 'placement', state: job.state, phase: job.stage,
    message: 'Optimizing placement for the selected routers.',
    progress: {known: job.total > 0, done: job.done || 0, total: job.total || 0, unit: 'router passes'},
  });
}

function reconnectPlacement(job) {
  activeJobKind = 'placement';
  activeJobId = job.job_id;
  jobStartedAt = (job.started_at || Date.now() / 1000) * 1000;
  placementOperation = {panel: null, revision: placementSelectionRevision, reconnect: true};
  setBusy(true);
  showPlacementJob(job);
  void pollPlacement();
}

async function pollPlacement() {
  const id = activeJobId;
  const current = () => activeJobKind === 'placement' && activeJobId === id;
  try {
    const job = await api('/api/optimize-placement/' + encodeURIComponent(id));
    if (!current()) return;
    if (job.state === 'queued' || job.state === 'running') {
      showPlacementJob(job);
      setTimeout(() => { if (current()) void pollPlacement(); }, 400);
      return;
    }
    if (job.state === 'complete' && placementOperation.revision === placementSelectionRevision) {
      const {panel, reconnect} = placementOperation;
      if (reconnect) siteSelection.replace(job.preview.sites.map(site => site.id));
      finishPlacementJob('complete', job.stage, '', false);
      placementPreview = job;
      setMainView('view-map');
      $('route-job-card').hidden = true;
      renderPlacementMap(job.preview);
      renderSiteSelection();
      requestAnimationFrame(() => {
        if (placementPreview === job) fitPlacementPreview(job.preview);
      });
      const heading = visiblePlacementPanel(panel)?.querySelector('.placement-preview-heading');
      heading?.focus({preventScroll: true});
      heading?.scrollIntoView({block: 'nearest'});
    } else if (job.state === 'complete') {
      finishPlacementJob('complete', 'Selection changed. Run placement again.', '');
    } else {
      finishPlacementJob(job.state, job.state === 'cancelled' ? job.stage : 'Placement optimization failed',
        job.state === 'failed' ? job.stage : '');
    }
  } catch (error) {
    if (!current()) return;
    if (error.status === 404 || error.status === 409) {
      finishPlacementJob('failed', 'Placement preview unavailable', error.message);
    } else {
      showJobConnectionError(error);
      $('retry-job').focus({preventScroll: true});
    }
  }
}

function visiblePlacementPanel(preferred) {
  return [preferred, $('map-site-selection'), $('sidebar-site-selection')]
    .find(panel => panel?.checkVisibility());
}

function focusPlacementAction(preferred) {
  const panel = visiblePlacementPanel(preferred);
  const action = panel?.querySelector('[data-selection-focus="action-optimize-placement"]');
  const clear = panel?.querySelector('[data-selection-focus="clear"]');
  (action && !action.disabled ? action : clear || $('select-map-sites')).focus({preventScroll: true});
}

function finishPlacementJob(state, phase, message, restoreFocus = true) {
  const panel = placementOperation?.panel;
  placementOperation = null;
  activeJobId = null;
  activeJobKind = null;
  setBusy(false);
  updateJobCard({kind: 'placement', state, phase, message});
  if (restoreFocus) focusPlacementAction(panel);
}

function placementMapLabel(id) {
  return findMapSite(id)?.label || routeSiteLabel(id);
}

function fitPlacementPreview(preview) {
  const mobile = mobileViewport.matches;
  fitMapSites(false, {
    positions: preview.sites.flatMap(site => [site.before, site.after]),
    rightPadding: mobile ? 36 : $('map-site-selection').offsetWidth + 46,
    maxZoom: 16,
  });
}

function renderPlacementMap(preview) {
  const positions = new Map(preview.sites.map(site => [site.id, site.after]));
  for (const link of preview.links) {
    const style = routeLinkMapStyle({worst_margin_db: link.after?.db, valid: link.after?.valid}, false);
    L.polyline([positions.get(link.source_id), positions.get(link.target_id)], {
      ...style, dashArray: link.after?.valid ? '2 4' : style.dashArray, weight: 3, interactive: false,
    }).addTo(placementLayer);
  }
  for (const site of preview.sites.filter(site => site.moved)) {
    L.polyline([site.before, site.after], {color: '#263f3a', dashArray: '7 5', weight: 2, interactive: false})
      .addTo(placementLayer);
    L.circleMarker(site.after, {radius: 7, color: '#263f3a', fillColor: '#fff', fillOpacity: 1,
      weight: 2, interactive: false}).addTo(placementLayer);
  }
  $('placement-map-legend').hidden = false;
}

function placementMargin(margin) {
  const value = marginPill(margin?.db, '');
  if (!margin) value.textContent = 'Unknown';
  else if (!margin.valid) {
    value.className = 'margin-pill margin-fail';
    value.textContent += ' · fail';
    value.title = 'Fails the selected RF validation';
  }
  return value;
}

function renderPlacementPreview(panel) {
  if (!placementPreview) return false;
  const {preview} = placementPreview;
  const heading = mapText('h3', 'Placement preview');
  heading.className = 'placement-preview-heading';
  heading.tabIndex = -1;
  const moved = preview.sites.filter(site => site.moved);
  const summary = mapText('p', moved.length
    ? `${moved.length} ${moved.length === 1 ? 'router' : 'routers'} would move · ${moved.map(site => placementMapLabel(site.id)).join(', ')}`
    : 'No better position was found within this radius.');
  const actions = document.createElement('div');
  actions.className = 'site-selection-actions';
  if (moved.length) {
    const apply = mapEditControl(mapPopupButton('Apply', () => void applyPlacement(panel)));
    apply.dataset.selectionFocus = 'placement-apply';
    actions.append(apply);
  }
  const discard = mapPopupButton(moved.length ? 'Discard' : 'Close', () => {
    clearPlacementPreview();
    renderSiteSelection();
    focusPlacementAction(panel);
  });
  discard.dataset.selectionFocus = 'placement-discard';
  actions.append(discard);
  const table = document.createElement('table');
  table.className = 'placement-link-table';
  const linkScope = preview.link_scope === 'topology'
    ? (routeMode === 'mesh' ? 'Selected mesh links' : 'Selected route links')
    : 'Selected links within range';
  table.append(mapText('caption', linkScope + ' · dB'));
  const header = table.createTHead().insertRow();
  for (const label of ['Link', 'Before', 'After']) {
    const cell = mapText('th', label);
    cell.scope = 'col';
    header.append(cell);
  }
  const rows = table.createTBody();
  for (const link of preview.links) {
    const row = rows.insertRow();
    const label = mapText('th', `${placementMapLabel(link.source_id)} ↔ ${placementMapLabel(link.target_id)}`);
    label.scope = 'row';
    row.append(label);
    row.insertCell().append(placementMargin(link.before));
    row.insertCell().append(placementMargin(link.after));
  }
  const legend = mapText('p', 'Hollow circles mark proposed positions. Long dashes show each move. Coloured lines show predicted links.');
  legend.className = 'hint';
  panel.append(heading, summary, actions, table, legend);
  return true;
}

function placementMoves(preview, resolve) {
  const moves = [];
  const movedIds = new Set();
  for (const {id, moved, after} of preview.sites) {
    if (!moved) continue;
    const site = resolve(id);
    if (movedIds.has(id) || !movablePlacementSite(site) || after?.length !== 2 || !after.every(Number.isFinite)) return null;
    moves.push({site, after});
    movedIds.add(id);
  }
  return moves;
}

function commitPlacement(preview) {
  if (mapEditsLocked()) return false;
  const moves = placementMoves(preview, findMapSite);
  if (!moves?.length) return false;
  const ids = new Map();
  for (const {site, after: [latitude, longitude]} of moves) {
    const router = site.kind === 'manual' ? manualRouters.find(item => item.id === site.id)
      : pinSearchRouter(site);
    Object.assign(router, {latitude, longitude});
    ids.set(site.id, router.id);
  }
  commitManualRouterLocations('optimized router positions');
  selectionMessage = 'Router positions applied. Use Undo to restore.';
  siteSelection.replace([...siteSelection.ids].map(id => ids.get(id) || id));
  return true;
}

async function applyPlacement(panel) {
  const job = placementPreview;
  if (!job || mapEditsLocked() || alternativeSelecting || placementApplying) return;
  placementApplying = true;
  try {
    const current = await api('/api/optimize-placement/' + encodeURIComponent(job.job_id));
    if (job !== placementPreview || mapEditsLocked() || alternativeSelecting) return;
    if (!commitPlacement(current.preview)) throw Error('The selected sites changed. Run the optimization again.');
    renderSiteSelection();
    focusPlacementAction(panel);
  } catch (error) {
    if (job !== placementPreview) return;
    if (error.status === 404 || error.status === 409) clearPlacementPreview();
    placementFeedback(error.message);
    if (!placementPreview) focusPlacementAction(panel);
  } finally {
    placementApplying = false;
  }
}
