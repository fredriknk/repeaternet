let routeMode = 'endpoints';
let meshClients = [];
const clientMarkers = new Map();

function routeLocationsReady() {
  return routeMode === 'mesh' ? meshClients.length >= 2
    : !!(validEndpointCoords('a') && validEndpointCoords('b'));
}

function validateMeshClient(values, clients, id) {
  const name = values.name.trim();
  if (!name || Array.from(name).length > 80 || /\p{Cc}/u.test(name)) {
    return {field: 'name', error: 'Enter a name of up to 80 characters.'};
  }
  const latitude = parseCoordinates(values.latitude, '0')?.[0];
  if (latitude === undefined) return {field: 'latitude', error: 'Enter a latitude from −90 to 90.'};
  const longitude = parseCoordinates('0', values.longitude)?.[1];
  if (longitude === undefined) return {field: 'longitude', error: 'Enter a longitude from −180 to 180.'};
  const lat = Number(latitude.toFixed(6)), lon = Number(longitude.toFixed(6));
  if (clients.some(client => client.id !== id && client.latitude === lat && client.longitude === lon)) {
    return {field: 'latitude', error: 'Another client is already at these coordinates.'};
  }
  const rawHeight = values.antenna_height_m.trim(), height = Number(rawHeight);
  if (!rawHeight || !Number.isFinite(height) || height < .1 || height > 1000) {
    return {field: 'antenna_height_m', error: 'Enter a height from 0.1 to 1,000 m.'};
  }
  return {client: {name, latitude: lat, longitude: lon, antenna_height_m: height}};
}

function loadPlanningMode(data) {
  const mode = data.mode ?? 'endpoints';
  if (!['mesh', 'endpoints'].includes(mode)) throw Error('Choose a plan with a valid planning mode.');
  const items = data.clients ?? [];
  if (!Array.isArray(items)) throw Error('The plan must contain a list of clients.');
  const clients = [];
  for (const item of items) {
    if (!/^CL-[a-zA-Z0-9_-]{1,61}$/.test(item?.id ?? '') || clients.some(client => client.id === item.id)) {
      throw Error('Each client needs a unique client ID.');
    }
    const values = Object.fromEntries(['name', 'latitude', 'longitude', 'antenna_height_m']
      .map(key => [key, String(item[key] ?? '')]));
    const validated = validateMeshClient(values, clients, item.id);
    if (validated.error) throw Error(validated.error);
    clients.push({...validated.client, id: item.id});
  }
  routeMode = mode;
  meshClients = clients;
}

function syncPlanningMode() {
  const mesh = routeMode === 'mesh';
  $('planning-mode').value = routeMode;
  $('route-site-step-label').textContent = mesh ? 'Clients' : 'Endpoints';
  $('endpoint-fields').hidden = mesh;
  $('mesh-client-fields').hidden = !mesh;
  $('mesh-client-legend').hidden = !mesh;
  $('live-terminal-source-label').textContent = mesh ? 'Named clients' : 'Endpoints A and B';
  $('optimize').firstChild.textContent = mesh ? 'Find mesh ' : 'Find repeater route ';
  $('route-show-map').textContent = mesh ? 'Show mesh on map' : 'Show route on map';
  for (const button of document.querySelectorAll('.map-tools [data-place]')) {
    const place = button.dataset.place;
    if (place === 'client') button.hidden = !mesh;
    if (place === 'a' || place === 'b') button.hidden = mesh;
  }
  for (const marker of Object.values(markers)) {
    if (mesh) marker.remove();
    else if (!map.hasLayer(marker)) marker.addTo(map);
  }
  renderMeshClients();
  refreshSiteSelection();
}

function initMeshControls() {
  $('planning-mode').onchange = event => {
    if (mapEditsLocked()) { event.target.value = routeMode; return; }
    routeMode = event.target.value;
    buildSettings();
    syncPlanningMode();
    commitMeshEdit('planning mode');
  };
  $('add-client-coordinate').onclick = event => openSidebarClientEditor(null, event.currentTarget);
}

function commitMeshEdit(reason) {
  recordEdit();
  invalidate(reason);
  scheduleCoverageCheck();
}

function planEndpoint(key) {
  if (routeMode !== 'mesh') return coords(key);
  const point = markers[key].getLatLng();
  return validEndpointCoords(key) || [point.lat, point.lng];
}

function applyMeshClientEdit(id, changes) {
  if (mapEditsLocked() || routeMode !== 'mesh') return;
  if (!id) {
    id = 'CL-' + randomId();
    meshClients.push({id, ...changes});
  } else if (changes) {
    meshClients = meshClients.map(client => client.id === id ? {...client, ...changes} : client);
  } else {
    meshClients = meshClients.filter(client => client.id !== id);
  }
  renderMeshClients();
  commitMeshEdit('mesh clients');
  return id;
}

function clientForm(client, finish, cancel) {
  const form = document.createElement('form');
  form.className = 'map-site-editor';
  form.noValidate = true;
  const name = mapEditorField(form, 'name', 'Name', client?.name, {type: 'text'});
  name.control.maxLength = 80;
  const grid = document.createElement('div');
  grid.className = 'coordinate-grid';
  form.append(grid);
  const fields = {
    name,
    latitude: mapEditorField(grid, 'latitude', 'Latitude', client?.latitude?.toFixed(6), {min: -90, max: 90}),
    longitude: mapEditorField(grid, 'longitude', 'Longitude', client?.longitude?.toFixed(6), {min: -180, max: 180}),
    antenna_height_m: mapEditorField(form, 'antenna_height_m', 'Antenna height · m',
      client?.antenna_height_m ?? rf?.endpoint_a?.height_agl_m ?? 2, {min: .1, max: 1000})
  };
  const actions = document.createElement('div');
  actions.className = 'map-editor-actions';
  const save = mapEditControl(mapText('button', client?.id ? 'Save changes' : 'Add client'));
  save.type = 'submit';
  actions.append(save, mapPopupButton('Cancel', cancel));
  form.append(actions);
  form.onsubmit = event => {
    event.preventDefault();
    if (mapEditsLocked()) return;
    const values = Object.fromEntries(Object.entries(fields).map(([key, field]) => [key, field.control.value]));
    const validated = validateMeshClient(values, meshClients, client?.id);
    if (validated.error) {
      const {control, error} = fields[validated.field];
      control.setAttribute('aria-invalid', 'true');
      error.textContent = validated.error;
      error.hidden = false;
      control.focus();
      return;
    }
    finish(applyMeshClientEdit(client?.id, validated.client));
  };
  return form;
}

function openSidebarClientEditor(client, opener) {
  if (mapEditsLocked()) return;
  const editor = $('mesh-client-editor');
  editor.hidden = false;
  const close = () => { editor.hidden = true; editor.replaceChildren(); opener.focus(); };
  editor.replaceChildren(clientForm(client, id => focusMeshClient(id), close));
  editor.querySelector('input').focus();
}

function focusMeshClient(id) {
  const row = [...$('mesh-client-list').children].find(row => row.dataset.clientId === id);
  (row?.querySelector('button') || $('add-client-coordinate')).focus({preventScroll: true});
}

function meshClientDescription(client) {
  return {...client, kind: 'client', label: client.name, gain_dbi: rf?.endpoint_a.gain_dbi,
    terminal_clutter: rf?.endpoint_a.terminal_clutter};
}

function renderMeshClients() {
  const list = $('mesh-client-list'), editor = $('mesh-client-editor');
  const focused = document.activeElement?.closest('[data-client-id]')?.dataset.clientId;
  editor.hidden = true;
  editor.replaceChildren();
  list.replaceChildren();
  for (const marker of clientMarkers.values()) marker.remove();
  clientMarkers.clear();
  if (routeMode !== 'mesh') return;
  for (const client of meshClients) {
    const row = document.createElement('li');
    row.dataset.clientId = client.id;
    const description = document.createElement('div');
    description.append(mapText('strong', client.name),
      mapText('small', `${coordinateLabel(mapPoint(client))} · ${formatMetric(client.antenna_height_m)} m`));
    const edit = mapEditControl(mapPopupButton('Edit', event => openSidebarClientEditor(client, event.currentTarget)));
    edit.setAttribute('aria-label', 'Edit ' + client.name);
    const remove = mapEditControl(mapPopupButton('Remove', () => {
      applyMeshClientEdit(client.id, null);
      focusMeshClient(client.id);
    }));
    remove.setAttribute('aria-label', 'Remove ' + client.name);
    row.append(description, edit, remove);
    list.append(row);
    const icon = L.divIcon({className: 'client-map-icon', iconSize: [44, 44], iconAnchor: [22, 22],
      html: mapText('span', client.name)});
    const marker = L.marker(mapPoint(client), {icon, draggable: !mapEditsLocked(), title: client.name, zIndexOffset: 150});
    clientMarkers.set(client.id, marker);
    bindMapSite(marker, () => meshClientDescription(client));
    bindLiveCoverageDrag(marker, () => meshClientCoverageSite(client));
    marker.on('dragend', () => {
      const point = marker.getLatLng().wrap();
      const validated = validateMeshClient({
        name: client.name, latitude: String(point.lat), longitude: String(point.lng),
        antenna_height_m: String(client.antenna_height_m)
      }, meshClients, client.id);
      if (validated.error || mapEditsLocked()) {
        marker.setLatLng(mapPoint(client));
        if (validated.error) setMapHint(validated.error, true);
        return;
      }
      applyMeshClientEdit(client.id, validated.client);
    });
    marker.addTo(map);
  }
  if (focused) focusMeshClient(focused);
}

function placeMeshClient(point) {
  setInteractionMode('browse');
  const content = document.createElement('div');
  const opener = () => document.querySelector('.map-tools [data-place="client"]');
  const finish = id => {
    closeMapSelection();
    (clientMarkers.get(id)?.getElement() || opener()).focus({preventScroll: true});
  };
  content.append(mapText('strong', 'Add client'),
    clientForm({latitude: point.lat, longitude: point.lng}, finish, () => closeMapSelection(true)));
  showMapObjectPopup(point, content, opener, undefined, {editor: true});
}

function meshClientCoverageSite(client) {
  return {...client, slot: 'client:' + client.id, kind: 'client', label: client.name,
    group: 'Client', terminal_clutter: rf?.endpoint_a?.terminal_clutter};
}

function openMeshClientContext(client, point, opener) {
  const content = document.createElement('div');
  const slot = 'client:' + client.id;
  const shown = liveCoverage.settings.enabled && liveCoverage.settings.sources.endpoints && !liveCoverage.hidden.has(slot);
  content.append(mapText('strong', client.name),
    mapEditControl(mapPopupButton('Edit client', () => {
      const editor = document.createElement('div');
      editor.append(mapText('strong', 'Edit ' + client.name), clientForm(client, () => closeMapSelection(true),
        () => openMeshClientContext(client, point, opener)));
      showMapObjectPopup(point, editor, opener, undefined, {editor: true});
    })),
    mapPopupButton(shown ? 'Hide coverage' : 'Show coverage', () => {
      if (shown) liveCoverage.hidden.add(slot);
      else {
        if (!liveCoverage.settings.enabled || !liveCoverage.settings.sources.endpoints) {
          for (const peer of meshClients) liveCoverage.hidden.add('client:' + peer.id);
        }
        liveCoverage.hidden.delete(slot);
        liveCoverage.settings.sources.endpoints = true;
        setLiveCoverageEnabled(true);
      }
      redrawLiveCoverage(liveCoverage.slots.get(slot)?.raster);
      renderLiveCoverageSites();
      scheduleCoverageCheck();
      closeMapSelection(true);
    }),
    mapEditControl(mapPopupButton('Remove client', () => {
      applyMeshClientEdit(client.id, null);
      closeMapSelection(true);
    })));
  showMapObjectPopup(point, content, opener);
}

function renderMeshPairs(pairs) {
  const body = $('mesh-pair-rows');
  body.replaceChildren();
  for (const pair of pairs) {
    const row = document.createElement('tr');
    for (const value of [pair.path.map(analysisSiteLabel).join(' → '), String(pair.hops), marginPill(pair.worst_margin_db)]) {
      const cell = document.createElement('td');
      cell.append(value);
      row.append(cell);
    }
    body.append(row);
  }
  $('mesh-pairs').hidden = !pairs.length;
}

function meshVerdict(snapshot, margin) {
  if (!snapshot.found) return `No solution connected all ${snapshot.client_count} clients within the search limits.`;
  const verdict = `All ${snapshot.clients_connected} clients are connected. The weakest link has ${formatMetric(margin)} dB of margin.`;
  return snapshot.search_complete ? verdict : verdict + ' Search incomplete.';
}

function meshMetricTiles(snapshot, margin, band) {
  const hops = snapshot.client_pairs.reduce((largest, pair) => Math.max(largest, pair.hops), 0);
  return [
    [`${snapshot.clients_connected}/${snapshot.client_count}`, 'Clients connected', ''],
    [String(snapshot.new_router_count), 'New routers needed', ''],
    [formatMetric(margin) + ' dB', 'Weakest link · ' + band.label, 'margin-' + band.cls],
    [String(hops), 'Largest client-to-client hop count', '']
  ];
}

function meshTopology(snapshot) {
  return {sites: snapshot.route,
    links: snapshot.links.map(({source_id, target_id, distance_m, worst_margin_db}) =>
      ({source_id, target_id, distance_m, worst_margin_db})),
    client_pairs: snapshot.client_pairs};
}
