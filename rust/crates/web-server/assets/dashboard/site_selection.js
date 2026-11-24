function createSiteSelection(onActionsChanged = () => {}) {
  let ids = new Set();
  const listeners = new Set();
  const actions = new Map();
  const notify = () => {
    for (const listener of listeners) listener(new Set(ids));
  };
  function replace(siteIds) {
    const next = new Set(siteIds);
    if (next.size === ids.size && [...next].every(id => ids.has(id))) return;
    ids = next;
    notify();
  }
  return {
    get ids() { return new Set(ids); },
    has: id => ids.has(id),
    replace,
    toggle(id) {
      const next = new Set(ids);
      if (!next.delete(id)) next.add(id);
      replace(next);
    },
    clear: () => replace([]),
    subscribe(listener) {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    registerAction(action) {
      actions.set(action.id, action);
      onActionsChanged();
      return () => {
        if (actions.get(action.id) !== action) return;
        actions.delete(action.id);
        onActionsChanged();
      };
    },
    get actions() { return [...actions.values()]; }
  };
}

// Use siteSelection.ids for a snapshot and subscribe(ids => ...) for updates.
// registerAction({id, label, isAvailable(sites), run(ids)}) adds a button in
// both views. The UI checks the edit lock before calling run. Site IDs are
// project-local. Use findMapSite(id) to resolve them against the current plan.
const siteSelection = createSiteSelection(() => {
  if (siteSelection.ids.size) renderSiteSelection();
});
const selectableSiteMarkers = new Map();
let selectionTapMode = false, selectionOpener, selectionMessage = '';

function clearSiteSelectionFeedback() {
  selectionMessage = '';
}

function syncSelectedMarker(marker, id) {
  const element = marker.getElement();
  if (!element) return;
  const selected = siteSelection.has(id);
  element.classList.toggle('is-multi-selected', selected);
  element.setAttribute('aria-pressed', String(selected));
}

function trackSelectableMarker(marker, describe) {
  marker.on('add', () => {
    const id = describe().id;
    selectableSiteMarkers.set(marker, id);
    syncSelectedMarker(marker, id);
  });
  marker.on('remove', () => selectableSiteMarkers.delete(marker));
}

function selectMapSite(id, event, opener) {
  if (interactionMode !== 'browse' ||
      !(selectionTapMode || event?.shiftKey || event?.ctrlKey || event?.metaKey)) return false;
  closeMapSelection();
  closeRouteLinkInspector();
  map.closePopup();
  selectionOpener = opener;
  selectionMessage = '';
  siteSelection.toggle(id);
  const target = opener();
  if (target?.checkVisibility()) target.focus({preventScroll:true});
  return true;
}

function setSiteSelectionTapMode(enabled) {
  selectionTapMode = enabled;
  $('select-map-sites').setAttribute('aria-pressed', String(enabled));
  if (enabled) setMapHint('Select sites on the map. Press Esc to clear the selection.');
  else if (interactionMode === 'browse') setMapHint();
}

function clearSiteSelection() {
  if (!siteSelection.ids.size && !selectionTapMode) return false;
  selectionMessage = '';
  setSiteSelectionTapMode(false);
  siteSelection.clear();
  selectionOpener = null;
  return true;
}

function selectableMarkers() {
  return [...selectableSiteMarkers].filter(([marker]) => !marker.getElement()?.hidden);
}

// Call after replacing map layers so an ID can survive a marker re-render.
function refreshSiteSelection() {
  const ids = siteSelection.ids;
  if (!ids.size) return;
  const available = new Set(selectableMarkers().map(([, id]) => id));
  siteSelection.replace([...ids].filter(id => available.has(id)));
  if (ids.size === siteSelection.ids.size) renderSiteSelection();
}

function installSiteSelection() {
  $('select-map-sites').onclick = () => {
    if (interactionMode !== 'browse') setInteractionMode('browse');
    setSiteSelectionTapMode(!selectionTapMode);
  };
  siteSelection.subscribe(() => {
    for (const [marker, id] of selectableSiteMarkers) syncSelectedMarker(marker, id);
    const count = siteSelection.ids.size;
    $('site-selection-status').textContent = count ? `${count} ${count === 1 ? 'site' : 'sites'} selected` : 'Selection cleared';
    renderSiteSelection();
  });
  installSiteSelectionBox();
}

function installSiteSelectionBox() {
  const container = map.getContainer(), box = $('site-selection-box');
  let start = null, moved = false, suppressClick = false;
  const stop = () => {
    start = null;
    box.hidden = true;
  };
  container.addEventListener('mousedown', event => {
    suppressClick = false;
    if (interactionMode !== 'browse' || event.button !== 0 || !event.shiftKey ||
        event.target.closest('.leaflet-control, .leaflet-popup')) return;
    event.preventDefault();
    event.stopPropagation();
    start = map.mouseEventToContainerPoint(event);
    moved = false;
  }, true);
  window.addEventListener('mousemove', event => {
    if (!start) return;
    const end = map.mouseEventToContainerPoint(event);
    if (!moved && start.distanceTo(end) < 5) return;
    moved = true;
    const {min, max} = L.bounds(start, end);
    Object.assign(box.style, {left:`${min.x}px`, top:`${min.y}px`,
      width:`${max.x - min.x}px`, height:`${max.y - min.y}px`});
    box.hidden = false;
  });
  window.addEventListener('mouseup', event => {
    if (!start || event.button !== 0) return;
    if (moved) {
      suppressClick = true;
      if (interactionMode === 'browse') {
        const bounds = L.bounds(start, map.mouseEventToContainerPoint(event));
        const viewport = L.bounds(L.point(0, 0), map.getSize());
        const ids = selectableMarkers().filter(([marker]) => {
          const point = map.latLngToContainerPoint(marker.getLatLng());
          return viewport.contains(point) && bounds.contains(point);
        }).map(([, id]) => id);
        closeMapSelection();
        closeRouteLinkInspector();
        map.closePopup();
        selectionOpener = () => container;
        if (document.activeElement === document.body) container.focus({preventScroll:true});
        selectionMessage = '';
        siteSelection.replace(ids);
      }
    }
    stop();
  }, true);
  container.addEventListener('click', event => {
    if (!suppressClick) return;
    suppressClick = false;
    event.stopPropagation();
  }, true);
  window.addEventListener('blur', stop);
  window.addEventListener('keydown', event => {
    if (start && event.key === 'Escape') {
      suppressClick = true;
      stop();
    }
  }, true);
}

function bulkSharedValue(sites, key) {
  const value = sites[0]?.[key];
  return sites.every(site => site[key] === value) ? value : undefined;
}

function validateBulkSiteFields(sites, rawFields) {
  const changes = {};
  const heightMax = sites.some(site => site.kind === 'known') ? 500 : 1000;
  const bounds = {antenna_height_m: [0.1, heightMax], gain_dbi: [-100, 100]};
  for (const [key, raw] of Object.entries(rawFields)) {
    if (key === 'policy') {
      if (!['required', 'optional', 'excluded'].includes(raw)) return {key, error:'Choose a policy.'};
      changes[key] = raw;
      continue;
    }
    const value = raw.trim();
    if (!/^[+-]?(?:\d+(?:\.\d*)?|\.\d+)(?:e[+-]?\d+)?$/i.test(value)) {
      return {key, error:'Enter a number.'};
    }
    const [min, max] = bounds[key], number = Number(value);
    if (!Number.isFinite(number) || number < min || number > max) {
      return {key, error:`Enter a number from ${min} to ${max}.`};
    }
    changes[key] = number;
  }
  return {changes};
}

function replaceSelectedSite(oldId, newId) {
  siteSelection.replace([...siteSelection.ids].map(id => id === oldId ? newId : id));
}

function pinSearchRouter(site) {
  if (dismissedSearchRouterIds().has(site.id)) return null;
  const router = {id:'M-' + randomId(), name:nextManualRouterName(), latitude:site.latitude,
    longitude:site.longitude, antenna_height_m:site.antenna_height_m, policy:'optional',
    terminal_clutter:site.terminal_clutter};
  const radio = rf.site_overrides?.[site.id];
  if (radio) saveSiteRadio(router.id, {...radio});
  manualRouters.push(router);
  dismissSearchRouterIds([site.id]);
  return router;
}

function updateSelectedSites(sites, changes) {
  const selectedIds = [];
  let changed = false;
  for (const site of sites) {
    const height = changes.antenna_height_m, gain = changes.gain_dbi, policy = changes.policy;
    const setHeight = height !== undefined && height !== site.antenna_height_m;
    if (site.kind === 'client') {
      const client = meshClients.find(client => client.id === site.id);
      if (setHeight && client) {
        client.antenna_height_m = height;
        changed = true;
      }
      selectedIds.push(site.id);
      continue;
    }
    const setGain = gain !== undefined && gain !== site.gain_dbi;
    const setPolicy = site.kind === 'known' && policy !== undefined && policy !== site.policy;
    if (!setHeight && !setGain && !setPolicy) {
      selectedIds.push(site.id);
      continue;
    }
    if (site.kind === 'endpoint') {
      const radio = rf['endpoint_' + site.key];
      if (setHeight) radio.height_agl_m = height;
      if (setGain) radio.gain_dbi = gain;
      changed = true;
      selectedIds.push(site.id);
      continue;
    }
    let router;
    if (site.kind === 'search') {
      router = pinSearchRouter(site);
    } else {
      router = (site.kind === 'known' ? meshcoreRouters : manualRouters).find(router => router.id === site.id);
    }
    if (!router) continue;
    changed = true;
    if (setHeight) {
      router.antenna_height_m = height;
      if (site.kind === 'known') router.height_override = true;
    }
    if (setGain) saveSiteRadio(router.id, {...rf.site_overrides?.[router.id], gain_dbi:gain});
    if (setPolicy) routerPolicies.set(router.id, policy);
    selectedIds.push(router.id);
  }
  return changed ? selectedIds : null;
}

function removableSelectedRouter(site) {
  return site.kind === 'manual' || site.kind === 'known';
}

function removeSelectedSites(sites) {
  const remove = new Set(sites.filter(removableSelectedRouter).map(site => site.id));
  if (!remove.size) return null;
  manualRouters = manualRouters.filter(router => !remove.has(router.id));
  meshcoreRouters = meshcoreRouters.filter(router => !remove.has(router.id));
  if (remove.has(selectedManualRouterId)) selectedManualRouterId = null;
  if (remove.has(selectedKnownRouterId)) selectedKnownRouterId = null;
  for (const id of remove) {
    routerPolicies.delete(id);
    saveSiteRadio(id, null);
  }
  return sites.filter(site => !remove.has(site.id)).map(site => site.id);
}

function commitBulkSiteEdit(sites, changes, {remove = false} = {}) {
  if (!rf || mapEditsLocked()) return;
  const invalid = routeMode === 'mesh' ? null : ['a', 'b'].find(key => !validEndpointCoords(key));
  if (invalid) {
    focusProjectTransitionError($(invalid), `Correct Endpoint ${invalid.toUpperCase()} before editing the selected sites.`);
    return;
  }
  const ids = remove ? removeSelectedSites(sites) : updateSelectedSites(sites, changes);
  if (!ids) return;
  const count = sites.length - ids.length;
  const reason = remove ? 'selected routers removed' : 'selected site settings';
  selectionMessage = remove ? `Removed ${count} ${count === 1 ? 'router' : 'routers'}. Use Undo to restore.` : 'Changes applied. Use Undo to restore.';
  recordEdit();
  invalidate(reason);
  if (!remove && sites.some(site => site.kind === 'endpoint')) buildSettings();
  if (!remove && changes.antenna_height_m !== undefined && sites.some(site => site.kind === 'client')) renderMeshClients();
  renderManualRouters();
  renderMeshcoreRouters();
  siteSelection.replace(ids);
  scheduleCoverageCheck();
  if (remove) setMapHint(selectionMessage, true);
}

function renderSiteSelection() {
  const ids = siteSelection.ids;
  const sites = [...ids].map(findMapSite).filter(Boolean);
  const panels = [$('map-site-selection'), $('sidebar-site-selection')];
  const active = document.activeElement;
  const focusedPanel = panels.find(panel => panel.contains(active));
  const focusKey = active?.dataset.selectionFocus;
  const forms = [];
  const rawFields = {};

  for (const panel of panels) {
    panel.replaceChildren();
    panel.hidden = !sites.length;
    if (!sites.length) continue;
    const heading = mapText('h2', `${sites.length} ${sites.length === 1 ? 'site' : 'sites'} selected`);
    heading.id = panel.id + '-heading';
    const summary = mapText('p', sites.map(site => site.label).join(', '));
    summary.className = 'site-selection-labels';
    const header = document.createElement('div');
    header.className = 'site-selection-header';
    const clear = mapPopupButton('Clear', clearSiteSelection);
    clear.dataset.selectionFocus = 'clear';
    clear.setAttribute('aria-label', 'Clear site selection');
    header.append(heading, clear);
    panel.append(header, summary);
    if (selectionMessage) {
      const feedback = mapText('p', selectionMessage);
      feedback.className = 'hint';
      feedback.role = 'status';
      panel.append(feedback);
    }
    if (renderPlacementPreview(panel)) continue;
    const actions = siteSelection.actions.filter(action => action.isAvailable?.(sites) ?? true);
    for (const action of actions) {
      const button = mapEditControl(mapPopupButton(action.label, () => {
        if (!mapEditsLocked()) action.run(siteSelection.ids);
      }));
      button.dataset.selectionFocus = 'action-' + action.id;
      panel.append(button);
    }
    if (sites.length > 1) {
      forms.push(renderBulkSiteForm(panel, sites, rawFields, forms));
    }
  }

  if (focusedPanel) {
    const controls = [...focusedPanel.querySelectorAll('[data-selection-focus]')];
    const destination = controls.find(control => control.dataset.selectionFocus === focusKey && !control.disabled) ||
      controls.find(control => control.dataset.selectionFocus === 'clear');
    const fallback = focusedPanel === panels[0] ? $('select-map-sites') :
      document.querySelector('[data-workspace-target="workspace-plan"]');
    const targets = [destination, selectionOpener?.(), fallback, map.getContainer()];
    targets.find(target => target?.checkVisibility())?.focus({preventScroll:true});
  }
}

function renderBulkSiteForm(panel, sites, rawFields, forms) {
  const form = document.createElement('form');
  form.className = 'site-selection-form';
  const fields = {};
  const existing = sites.filter(site => site.kind === 'known');
  const removable = sites.filter(removableSelectedRouter);
  const fieldGrid = document.createElement('div');
  fieldGrid.className = 'site-selection-fields';
  form.append(fieldGrid);

  function field(key, label, applicable, choices) {
    const value = bulkSharedValue(applicable, key);
    const parts = mapEditorField(fieldGrid, key, label, value, {type:'text', choices});
    const {control, error} = parts;
    fields[key] = parts;
    control.required = false;
    control.dataset.selectionFocus = key;
    error.id = `${panel.id}-${key}-error`;
    control.setAttribute('aria-describedby', error.id);
    if (choices) {
      if (value === undefined) {
        const mixed = new Option('Mixed', '', true, true);
        mixed.disabled = true;
        control.add(mixed, 0);
      }
    } else {
      control.removeAttribute('maxlength');
      control.inputMode = 'decimal';
      if (value === undefined) control.placeholder = 'Mixed';
    }
    control.oninput = () => {
      rawFields[key] = control.value;
      for (const other of forms) {
        other[key].control.value = control.value;
        other[key].control.removeAttribute('aria-invalid');
        other[key].error.hidden = true;
      }
    };
  }

  const gainSites = sites.filter(site => site.kind !== 'client');
  field('antenna_height_m', 'Antenna height · m', sites);
  if (gainSites.length) {
    field('gain_dbi', 'Antenna gain · dBi', gainSites);
  }
  if (existing.length) field('policy', 'Existing router policy', existing, routerPolicyLabels);
  const notes = [];
  if (existing.length && existing.length < sites.length) notes.push('Policy applies to existing routers only.');
  if (gainSites.length && gainSites.length < sites.length) notes.push('Antenna gain uses the shared client radio settings for mesh clients.');
  if (sites.some(site => site.kind === 'endpoint')) notes.push('Remove skips endpoints.');
  if (sites.some(site => site.kind === 'search')) notes.push('Remove skips search results.');
  if (sites.some(site => site.kind === 'search')) notes.push('Changing height or gain pins search sites as proposed routers.');

  const actions = document.createElement('div');
  actions.className = 'site-selection-actions';
  const apply = mapEditControl(mapText('button', 'Apply changes'));
  apply.type = 'submit';
  apply.dataset.selectionFocus = 'apply';
  actions.append(apply);
  if (removable.length) {
    const button = mapEditControl(mapPopupButton(`Remove ${removable.length} ${removable.length === 1 ? 'router' : 'routers'}`,
      () => commitBulkSiteEdit(sites, {}, {remove:true})));
    button.dataset.selectionFocus = 'remove';
    actions.append(button);
  }
  form.append(actions);
  if (notes.length) {
    const note = mapText('p', notes.join(' '));
    note.className = 'hint';
    form.append(note);
  }
  form.onsubmit = event => {
    event.preventDefault();
    const {key, error, changes} = validateBulkSiteFields(sites, rawFields);
    if (error) {
      fields[key].error.textContent = error;
      fields[key].error.hidden = false;
      fields[key].control.setAttribute('aria-invalid', 'true');
      fields[key].control.focus();
      return;
    }
    commitBulkSiteEdit(sites, changes);
  };
  panel.append(form);
  return fields;
}
