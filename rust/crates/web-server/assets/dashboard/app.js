/* Local assets only: the basemap is the sole browser network dependency. */
const $ = id => document.getElementById(id);
let rf, candidates, result, interactionMode = 'browse', busy = false, switchingProject = false, selected = 0, searchEffort = 'balanced';
let activeJobId = null, activeJobKind = null, lastSnapshotVersion = 0, jobStartedAt = null;
let pendingRouteContinuation=null,routePreflightBusy=false;
let alternativeSelecting=false;
function setRoutePreflightBusy(value){routePreflightBusy=value;syncCoverageLocks();$('optimize').disabled=busy||value;$('clear-cache').disabled=busy||value;}
let meshcoreRouters = [], routerPolicies = new Map(), selectedKnownRouterId = null;
let manualRouters = [], selectedManualRouterId = null, undoStack = [], redoStack = [], lastHistoryPlan = null, resultStale = false, resultStaleReason = '';
let loadedSharedProject = null;
let projectInfo = null, baseRf = null, baseCandidates = null, autosaveTimer = null;
let calibration;
const defaultTerrainOptions = {requested_resolution_m:10,auto_resolution:true,include_dom:true,source:'auto',fallback:true};
let terrainOptions = {...defaultTerrainOptions}, preparingTerrain = false, coverageTimer = null, terrainLoaded = false;
let dataSources = null, dataSourcesRevision = 0, dataSourcesError = '';
let terrainPreviewTimer, terrainPreviewRequest = 0, terrainPreviewKey, terrainPreviewDomNote = '', terrainEstimateReview = null;
let terrainReadiness=null,coverageTerrainLoaded=false,coverageTerrainTileCounts={dtm:0,dom:0};
let coverageSettings = {mode:'two_way',area_mode:'mesh',area_polygon_wgs84:null,cell_size_m:1000,area_buffer_m:5000,profile_step_m:200,maximum_profile_samples:4096,maximum_cells:4096,maximum_sources:64,maximum_evaluations:250000,include_endpoints:false,source_ids:[],client:{name:'Handheld · 1.5 m',height_agl_m:1.5,tx_power_dbm:20,gain_dbi:2.15,feed_loss_db:0,sensitivity_dbm:-130,miscellaneous_loss_db:0}};
const defaultCoverageSettings=JSON.parse(JSON.stringify(coverageSettings));
let coverageBusy = false, coverageJob = null, coverageDisplayInfo = null, coveragePreviewActive = false, coveragePreviewCells = [], coverageCells = [], coverageCursor = 0, coverageStale = false, inspectionRequest = 0, inspectionController = null, activeInspectionId = null, coverageHeightOverrides = {}, coverageComparison = null, comparisonRuns = [], coverageTargets = [], coverageTargetStates = new Map();
let coverageRunReused=false,coverageEstimate=null,coverageEstimateRequest=0,coverageRevision=0,coveragePreflightBusy=false,coverageLoadingRun=false,coverageLoadRequest=0,coveragePollRequest=0,coverageRunMetadata=[];
let coverageTerrainCheck=null,coverageTerrainUnknownKey=null,coverageTerrainJobId=null;
let coverageDrawingPoints=[];
const map = L.map('map',{zoomSnap:0,wheelPxPerZoomLevel:120});
let pendingMapFit = null, mapHintTimer = null, mapContextPopup = null;
let mapSelection = null, mapObjectPopup = null, endpointNames = {};
let labelledRoute = null, routeSiteLabels = new Map();
const mapPane = $('view-map'), mapWrap = map.getContainer().parentElement;
const mapResizeObserver = new ResizeObserver(resizeMapViewport);
mapResizeObserver.observe(mapPane);
const fallbackBasemaps = [
  {id:'osm',name:'OpenStreetMap',kind:'xyz',url:'https://tile.openstreetmap.org/{z}/{x}/{y}.png',max_zoom:19,attribution:'© OpenStreetMap contributors',default:true},
  {id:'kartverket-topo',name:'Kartverket topo',kind:'xyz',url:'https://cache.kartverket.no/v1/wmts/1.0.0/topo/default/webmercator/{z}/{y}/{x}.png',max_zoom:18,attribution:'© Kartverket',coverage:{type:'bbox',bbox:[4,57.9,31.3,71.3]}}
];
const basemapStorageKey = 'repeaternet.base-layer';
let mapSourceLayers = [], mapLayerControl, mapSourcePreview, mapSourcePreviewTimer;
map.createPane('coveragePane');map.getPane('coveragePane').style.zIndex=350;
map.createPane('repeaterRoadsPane').style.zIndex=375;
map.createPane('staleRouteSitePane').style.zIndex=395;
const repeaterRoadLayer=L.layerGroup([], {attribution:fallbackBasemaps[0].attribution});
const routeLayer = L.layerGroup().addTo(map), candidateLayer = L.layerGroup().addTo(map), knownLayer = L.layerGroup().addTo(map), coverageLayer = L.layerGroup().addTo(map), predictedCoverageLayer=L.layerGroup(), coverageAreaLayer=L.layerGroup().addTo(map), coverageDiffLayer=L.layerGroup(), inspectionLayer=L.layerGroup().addTo(map), manualLayer = L.layerGroup().addTo(map), targetLayer=L.layerGroup().addTo(map);
let routeLinkLayers=[];
let routeLinkInspector=null, routeLinkPreviewTimer, routeLinkPreviewInFlight=false, routeLinkPreviewStarted=-Infinity;
const plannerMapOverlays = {'Terrain coverage':coverageLayer,'Predicted mesh coverage':predictedCoverageLayer,'Coverage analysis area':coverageAreaLayer,'Coverage difference':coverageDiffLayer,'Coverage targets':targetLayer};
const coverageHover = L.tooltip({direction:'top', className:'coverage-sample-tooltip'});
map.on('mousemove', showCoverageSample).on('mouseout movestart', hideCoverageSample);
for(const group of [predictedCoverageLayer, coverageDiffLayer]){
  group.on('add remove', syncCoverageLegend);
  group.on('remove', hideCoverageSample);
}
rebuildSourceLayers(fallbackBasemaps);
map.on('baselayerchange',event=>{
  const selected = mapSourceLayers.find(item=>item.layer===event.layer);
  if(selected)try{localStorage.setItem(basemapStorageKey,selected.definition.id);}catch{}
});
map.on('moveend',updateSourceLayerLabels);
const markers = {};
const knownMapMarkers = new Map(), manualMapMarkers = new Map();
const routerPolicyLabels = {optional:'Optional',required:'Required',excluded:'Excluded'};
const mapLockSelector = '.map-tools [data-place], [data-map-edit]';
function mapEditsLocked(){
  return busy||routePreflightBusy||coverageBusy||coveragePreflightBusy||coverageLoadingRun||switchingProject;
}
function syncMapEditControls(){
  for(const control of document.querySelectorAll(mapLockSelector))control.disabled=mapEditsLocked();
}
function mapEditControl(control){
  control.dataset.mapEdit='';
  control.disabled=mapEditsLocked();
  return control;
}
function mapPoint(site){
  return [site.latitude,site.longitude];
}
function coordinateLabel(point){
  return point.map(value=>Number(value).toFixed(6)).join(', ');
}
function mapText(tag,content){
  const element=document.createElement(tag);
  element.textContent=content;
  return element;
}
function mapPopupButton(label,handler){
  const button=mapText('button',label);
  button.type='button';
  button.onclick=handler;
  return button;
}
function mapPopupOptions(options={}){
  const toolbar=document.querySelector('.map-tools');
  return {className:'map-popup',autoPanPaddingTopLeft:[12,toolbar.offsetHeight+20],autoPanPaddingBottomRight:[12,28],...options};
}
function fitMapSites(routeOnly=false){
  const routePoints=(result?.route||[]).map(mapPoint);
  const planPoints=[
    ...Object.values(markers).map(marker=>marker.getLatLng()),
    ...manualRouters.map(mapPoint),
    ...selectedKnownRouters().filter(router=>router.policy!=='excluded').map(mapPoint)
  ];
  const points=routeOnly&&routePoints.length?routePoints:[...planPoints,...routePoints];
  pendingMapFit=L.latLngBounds(points.filter(point=>!Array.isArray(point)||point.every(Number.isFinite)));
  if(!map.getSize().x&&!map.getSize().y&&map.getZoom()===undefined&&pendingMapFit.isValid()){
    map.setView(pendingMapFit.getCenter(),12);
  }
  resizeMapViewport();
}
function resizeMapViewport(){
  if(!mapPane.clientWidth||!mapPane.clientHeight)return;
  const top=mapPane.getBoundingClientRect().top+window.scrollY;
  mapWrap.style.setProperty('--map-viewport-height',`${Math.max(240,window.innerHeight-top)}px`);
  map.invalidateSize({pan:false});
  applyPendingMapFit();
}
function applyPendingMapFit(){
  const container=map.getContainer();
  if(!pendingMapFit||!container.clientWidth||!container.clientHeight)return;
  const bounds=pendingMapFit;
  pendingMapFit=null;
  if(!bounds.isValid())return;
  const toolbarBottom=$('map-hint').hidden?document.querySelector('.map-tools').offsetHeight:document.querySelector('.map-top').offsetHeight;
  map.fitBounds(bounds,{paddingTopLeft:[36,toolbarBottom+28],paddingBottomRight:[36,66],maxZoom:14,animate:false});
}
function setMapHint(message='',transient=false){
  const hint=$('map-hint');
  clearTimeout(mapHintTimer);
  hint.textContent=message;
  hint.hidden=!message;
  if(transient&&interactionMode==='browse')mapHintTimer=setTimeout(()=>setMapHint(),4000);
}
function status(message, error=false) {$('status').textContent=message;$('status').className=error?'error':'';const action=$('plan-action-status');if(action){action.textContent=message;action.title=message;action.className=error?'action-status error':'action-status';}}
function planWriteRequest(url, method){
  return (method==='PUT'&&url==='/api/coverage/targets')||
    (method==='POST'&&['/api/projects/autosave','/api/projects/recover-previous','/api/terrain/prepare','/api/optimize'].includes(url));
}
function showProjectConflict(message){
  setProjectSaveState(message,'conflict');
}
async function api(url, options={}){
  const editingProject=loadedSharedProject;
  const writesPlan=planWriteRequest(url,options.method);
  if(editingProject&&(writesPlan||(!url.startsWith('/api/projects')&&url!=='/api/state'))){
    const headers=new Headers(options.headers);
    headers.set('x-repeaternet-project',editingProject.id);
    if(writesPlan)headers.set('x-repeaternet-plan-revision',editingProject.revision);
    options={...options,headers};
  }
  const response=await fetch(url,options);
  if(!response.ok){
    const body=await response.json().catch(()=>({detail:response.statusText}));
    const detail=body.detail??body.error??response.statusText;
    const error=Error(typeof detail==='string'?detail:JSON.stringify(detail));
    error.status=response.status;error.issues=body.issues;
    if(editingProject&&editingProject===loadedSharedProject&&error.status===409&&error.message.includes('Reload the project')){
      error.projectConflict=true;
      showProjectConflict(error.message);
    }
    throw error;
  }
  const data=await response.json();
  if(writesPlan&&editingProject&&editingProject===loadedSharedProject&&data.plan_revision){
    editingProject.revision=data.plan_revision;
  }
  return data;
}
function parseCoordinates(latitude,longitude){
  if(!latitude.trim()||!longitude.trim())return null;
  const lat=Number(latitude),lon=Number(longitude);
  return Number.isFinite(lat)&&Number.isFinite(lon)&&Math.abs(lat)<=90&&Math.abs(lon)<=180?[lat,lon]:null;
}
function validEndpointCoords(name){const parts=$(name).value.split(',');return parts.length===2?parseCoordinates(...parts):null;}
function coords(name){const values=validEndpointCoords(name);if(!values)throw Error('Enter latitude, longitude for endpoint '+name.toUpperCase());return values;}
function mark(name,latlng){
  const label=name.toUpperCase();
  $(name).value=coordinateLabel(latlng);
  if(markers[name])markers[name].setLatLng(latlng);
  else{
    const icon=L.divIcon({className:'site-marker '+(name==='b'?'destination':''),html:label,iconSize:[32,32]});
    markers[name]=L.marker(latlng,{draggable:!mapEditsLocked(),icon,title:'Endpoint '+label});
    markers[name].bindTooltip('',{direction:'top',offset:[0,-12]});
    bindMapSite(markers[name],()=>endpointMapSite(name));
    bindRouteSiteInspector(markers[name],label,point=>editEndpoint(name,point));
    bindLiveCoverageDrag(markers[name],()=>({slot:`endpoint:${name}`,id:label,kind:`endpoint_${name}`,antenna_height_m:liveCoverageSiteHeight(rf?.[`endpoint_${name}`]?.height_agl_m,2)}));
    markers[name].addTo(map);
  }
  markers[name].setTooltipContent(`${label} · ${$(name).value}`);
}
function editEndpoint(name,point,changes){
  if(mapEditsLocked())return;
  mark(name,[point.lat,point.lng]);
  if(changes){
    endpointNames[name]=changes.name;
    rf['endpoint_'+name].height_agl_m=changes.antenna_height_m;
    rf['endpoint_'+name].gain_dbi=changes.gain_dbi;
    rf['endpoint_'+name].terminal_clutter=changes.terminal_clutter;
    buildSettings();
  }
  recordEdit();
  invalidate(changes?'endpoint settings':'endpoint coordinates');
  scheduleCoverageCheck();
}
function clone(value){return JSON.parse(JSON.stringify(value));}
function randomId(){
  if(typeof crypto.randomUUID==='function')return crypto.randomUUID();
  // Plain HTTP on the LAN still provides getRandomValues.
  const bytes=crypto.getRandomValues(new Uint8Array(16));
  bytes[6]=(bytes[6]&0x0f)|0x40; // UUID version 4
  bytes[8]=(bytes[8]&0x3f)|0x80; // RFC UUID variant
  const hex=Array.from(bytes,byte=>byte.toString(16).padStart(2,'0')).join('');
  return `${hex.slice(0,8)}-${hex.slice(8,12)}-${hex.slice(12,16)}-${hex.slice(16,20)}-${hex.slice(20)}`;
}
function setHistoryButtons(){$('undo').disabled=busy||routePreflightBusy||coverageBusy||coveragePreflightBusy||coverageLoadingRun||switchingProject||!undoStack.length;$('redo').disabled=busy||routePreflightBusy||coverageBusy||coveragePreflightBusy||coverageLoadingRun||switchingProject||!redoStack.length;}
function recordEdit(){try{const next=clone(plan());if(lastHistoryPlan&&JSON.stringify(next)!==JSON.stringify(lastHistoryPlan)){undoStack.push(lastHistoryPlan);if(undoStack.length>50)undoStack.shift();redoStack=[];}lastHistoryPlan=next;setHistoryButtons();if(projectInfo) scheduleAutosave();}catch(e){status(e.message,true);}}
function invalidate(reason='planning inputs'){
  if(!routeLinkInspector?.committing)closeRouteLinkInspector();
  closeMapSelection(true);
  $('map-analysis-selection').hidden=true;
  clearTerrainEstimateReview();
  invalidateCoverageEstimate(`${reason} changed. Review a new coverage estimate before previewing or calculating.`);
  setInteractionMode('browse');
  cancelActiveInspection();
  inspectionRequest++;
  inspectionController?.abort();
  coverageCells=[];coverageCursor=0;coverageStale=true;predictedCoverageLayer.clearLayers();coverageDiffLayer.clearLayers();coverageComparison=null;coverageHeightOverrides={};inspectionLayer.clearLayers();$('coverage-inspection').hidden=true;
  updatePlanReadiness();
  if($('coverage-job-status'))setCoverageStatus('Route inputs changed. Recalculate coverage after validating the new route.');
  if(result){resultStale=true;resultStaleReason=reason;renderRouteVerdict();$('result-title').textContent='Previous certified route · stale';$('status').textContent=`${reason} changed. This route is for the previous settings; run a new search to validate the current plan.`;$('status').className='stale';$('exports').hidden=true;for(const layer of routeLayer.getLayers()){if(layer.setStyle)layer.setStyle({color:'var(--map-stale)',dashArray:'5 6'});if(layer.dragging)layer.dragging.disable();if(layer._routeSiteIcon)moveRouteSiteToStalePane(layer);}syncRoutedRouterMarkers();$('alternative-select').disabled=true;syncAnalysisBadge();updateCoverageActions();return;}
  routeLayer.clearLayers();candidateLayer.clearLayers();showRepeaterRoads();$('metrics').replaceChildren();$('hops').hidden=true;$('profile').hidden=true;$('exports').hidden=true;$('alternatives').hidden=true;$('candidate-search-diagnostic').hidden=true;$('route-verdict').hidden=true;$('route-show-map').disabled=true;$('result-title').textContent='Ready for a new route';
}
function updatePlanReadiness(report=terrainReadiness){
  updatePlanStepSummaries(report);
  const card=$('plan-readiness');if(!card)return;
  const copy=$('plan-readiness-copy');let invalid=null;
  for(const name of ['a','b']){const error=$(`${name}-error`),field=$(name);try{coords(name);field.setAttribute('aria-invalid','false');error.hidden=true;error.textContent='';}catch(exception){field.setAttribute('aria-invalid','true');error.textContent=exception.message;error.hidden=false;invalid=name;}}
  const action=$('review-readiness');
  const endpointSummary=$('section-summary-endpoints');if(endpointSummary){const a=$('a').value.trim(),b=$('b').value.trim();endpointSummary.textContent=a&&b?`${a} → ${b}`:'Set start and destination';}
  const terrainSummary=$('section-summary-terrain');if(terrainSummary)terrainSummary.textContent=`${$('dtm-count').textContent}${terrainLoaded?' · ready':' · required'}`;
  if(invalid){card.dataset.readiness='invalid';copy.textContent=`Endpoint ${invalid.toUpperCase()} needs a valid latitude, longitude pair before planning.`;action.textContent='Review endpoints';action.dataset.target='endpoint-setup';}
  else if(!terrainLoaded){card.dataset.readiness='needs-terrain';copy.textContent='Check the two pins on the map, then add ground elevation data for this route.';action.textContent='Open terrain setup';action.dataset.target='terrain-setup';}
  else if(report?.uncovered_sites?.length){card.dataset.readiness='blocked';copy.textContent=`Ground terrain is missing at ${report.uncovered_sites.map(site=>site.name).join(', ')}. Extend or upload terrain before searching.`;action.textContent='Review terrain gaps';action.dataset.target='terrain-setup';}
  else if(report?.covered){card.dataset.readiness='ready';copy.textContent=`Ready to search · ground terrain covers the selected sites${report.surface_available?'; surface obstruction data is also loaded.':'; terrain-only RF estimates.'}`;action.hidden=true;return;}
  else{card.dataset.readiness='checking';copy.textContent='Ground terrain is loaded. Check endpoint and router coverage before starting the RF search.';action.textContent='Open terrain setup';action.dataset.target='terrain-setup';}
  action.hidden=false;
}
$('review-readiness').onclick=()=>{const target=$('review-readiness').dataset.target||'terrain-setup';setPlanSection(target);document.getElementById(target).scrollIntoView({behavior:'smooth',block:'start'});};
function selectedKnownRouters(){return meshcoreRouters.map(router=>({...router,policy:routerPolicies.get(router.id)||router.policy||'excluded'}));}
const mapPinSvg='<svg viewBox="0 0 16 16" aria-hidden="true" focusable="false"><path d="M5.5 1.5h5l-.8 4.2 2.6 3.1H8.8V14L8 15.3 7.2 14V8.8H3.7l2.6-3.1z"/></svg>';
function mapPinBadge(){
  const badge=document.createElement('span');
  badge.className='map-pin-badge';
  badge.innerHTML=mapPinSvg;
  return badge;
}
function shortMapLabel(name){
  const chars=Array.from(String(name||'').trim());
  return chars.length<=9?chars.join(''):chars.slice(0,8).join('')+'…';
}
function mapSiteIcon(type,label,policy,{stale=false}={}){
  const chars=Array.from(label).length;
  const width=type==='existing'?Math.max(30,chars*10+12):type==='manual'?Math.max(36,chars*9+22):30;
  const content=document.createElement('span'),text=mapText('span',label);
  content.className='map-site-content';text.className='map-site-label';content.append(text);
  if(policy==='required')content.append(mapPinBadge());
  return L.divIcon({className:`map-site map-site-${type}${!label?' map-site-dot':''}${policy?' is-'+policy:''}${stale?' is-stale':''}`,html:content,iconSize:[width,30],tooltipAnchor:[0,-12],popupAnchor:[0,-13]});
}
function nextManualRouterName(){
  const numbers=manualRouters.map(router=>/^P(\d+)$/.exec(router.name?.trim()||'')?.[1]).filter(Boolean).map(Number);
  return `P${numbers.length?Math.max(...numbers)+1:1}`;
}
function ensureManualRouterNames(){
  for(const router of manualRouters)if(typeof router.name!=='string'||!router.name.trim())router.name=nextManualRouterName();
}
function manualRouterName(router){return router?.name?.trim()||router?.id||'';}
function routedRouterIds(){
  if(!result?.found||resultStale)return new Set();
  return new Set((result.route||[]).filter(site=>['known','manual'].includes(site.origin)).map(site=>site.id));
}
function syncRoutedRouterMarkers(){
  const routed=routedRouterIds();
  for(const [id,marker] of manualMapMarkers){
    marker.getElement()?.classList.toggle('is-in-route',routed.has(id));
  }
  for(const layer of routeLayer.getLayers()){
    if(!layer.options?.siteId)continue;
    const point=layer.getLatLng();
    const replaced=manualRouters.some(router=>coordinateLabel(mapPoint(router))===coordinateLabel([point.lat,point.lng]));
    if(layer.getElement())layer.getElement().hidden=replaced;
  }
  syncManualRouterTitles();
  renderKnownMapMarkers();
}

function endpointMapSite(key){
  const id=key.toUpperCase(), point=markers[key].getLatLng(), radio=rf?.['endpoint_'+key];
  return {id,key,kind:'endpoint',label:id,name:endpointNames[key]||'Endpoint '+id,
    latitude:point.lat,longitude:point.lng,antenna_height_m:radio?.height_agl_m,
    gain_dbi:radio?.gain_dbi,terminal_clutter:radio?.terminal_clutter};
}
function routerMapSite(router,kind,nearest){
  const radio=rf?.site_overrides?.[router.id];
  const label=kind==='search'?routeSiteLabel(router.id):router.name||routeSiteLabel(router.id);
  const height=kind==='known'&&!router.height_override?rf.router.height_agl_m:router.antenna_height_m;
  return {id:router.id,kind,label,name:kind==='search'?nextManualRouterName():label,
    latitude:router.latitude,longitude:router.longitude,antenna_height_m:height??rf.router.height_agl_m,
    policy:kind==='known'?routerPolicies.get(router.id)||router.policy||'excluded':router.policy||'optional',
    gain_dbi:radio?.gain_dbi??rf.router.gain_dbi,terminal_clutter:router.terminal_clutter,nearest};
}
function findMapSite(id){
  if(id==='A'||id==='B')return endpointMapSite(id.toLowerCase());
  const manual=manualRouters.find(item=>item.id===id);
  if(manual)return routerMapSite(manual,'manual');
  const known=meshcoreRouters.find(item=>item.id===id);
  if(known)return routerMapSite(known,'known');
  const site=result?.route?.find(item=>item.id===id);
  return site&&routerMapSite(site,'search');
}
function mapSiteElement(id){
  return (markers[id.toLowerCase()]||manualMapMarkers.get(id)||knownMapMarkers.get(id)||
    routeLayer.getLayers().find(layer=>layer.options?.siteId===id))?.getElement();
}
function isMapContextKey(event){
  return event.key==='ContextMenu'||(event.shiftKey&&event.key==='F10');
}
function bindMapObject(layer,label,details,edit){
  const keyboardPoint=()=>layer.getLatLng?.()||layer.getCenter();
  const activate=(action,point)=>{
    if(interactionMode==='browse')action(point,()=>layer.getElement());
  };
  layer.on('click',event=>activate(details,event.latlng||keyboardPoint()));
  layer.on('contextmenu',event=>{
    L.DomEvent.stopPropagation(event);
    activate(edit,event.latlng||keyboardPoint());
  });
  const bindElement=()=>{
    const element=layer.getElement();
    // Leaflet reuses marker icons when moving a layer between panes.
    if(element._plannerInteraction)return;
    element._plannerInteraction=true;
    element.tabIndex=0;
    element.setAttribute('role','button');
    element.setAttribute('aria-label',label());
    element.setAttribute('aria-keyshortcuts','Enter Space Shift+F10');
    element.addEventListener('keydown',event=>{
      const action=isMapContextKey(event)?edit:['Enter',' '].includes(event.key)?details:null;
      if(!action)return;
      event.preventDefault();
      event.stopPropagation();
      activate(action,keyboardPoint());
    });
    bindMapLongPress(element,()=>activate(edit,keyboardPoint()));
  };
  layer.on('add',bindElement);
  if(layer.getElement())bindElement();
}
function bindMapLongPress(element,open){
  let press, timer, suppressUntil=0;
  const clear=()=>{clearTimeout(timer);press=null;};
  element.addEventListener('pointerdown',event=>{
    if(event.pointerType!=='touch'||!event.isPrimary||interactionMode!=='browse')return;
    press=event;
    timer=setTimeout(()=>{
      clear();
      suppressUntil=performance.now()+800;
      open();
    },550);
  });
  element.addEventListener('pointermove',event=>{
    if(press&&Math.hypot(event.clientX-press.clientX,event.clientY-press.clientY)>10)clear();
  });
  for(const name of ['pointerup','pointercancel','pointerleave'])element.addEventListener(name,clear);
  element.addEventListener('click',event=>{
    if(performance.now()>=suppressUntil)return;
    event.preventDefault();
    event.stopImmediatePropagation();
  },true);
}
function bindMapSite(marker,describe){
  const opener=()=>mapSiteElement(describe().id)||marker.getElement();
  bindMapObject(marker,()=>describe().label,
    (point)=>showMapSiteDetails(describe(),point,opener),
    (point)=>openMapSiteEditor(describe(),point,opener));
}
function bindMapLink(line,link,index){
  const label=()=>routeSiteLabel(link.source_id)+' → '+routeSiteLabel(link.target_id);
  bindMapObject(line,label,(point,opener)=>{
    showMapDetails(label(),point,opener,content=>{
      appendMapFacts(content,[
        ['Distance · km',formatMetric(link.distance_m/1000,2)],
        ['Usable margin',marginPill(link.worst_margin_db)],
        ['Forward / reverse margin · dB',formatMetric(link.forward.usable_margin_db)+' / '+formatMetric(link.reverse.usable_margin_db)],
        ['Forward / reverse RX · dBm',formatMetric(link.forward.received_power_dbm)+' / '+formatMetric(link.reverse.received_power_dbm)]
      ]);
      content.append(mapText('p',linkIssues(link)),mapPopupButton('Show in analysis',()=>{
        closeMapSelection();
        $('map-analysis-selection').hidden=true;
        openRouteLink(index);
      }));
    });
  },(point,opener)=>{
    const content=document.createElement('div');
    content.append(mapText('strong','Edit link sites'));
    for(const id of [link.source_id,link.target_id]){
      const site=findMapSite(id);
      if(site)content.append(mapEditControl(mapPopupButton('Edit '+site.label,()=>openMapSiteEditor(site,L.latLng(mapPoint(site)),opener))));
    }
    showMapObjectPopup(point,content,opener);
  });
}
function appendMapFacts(content,facts){
  const list=document.createElement('dl');
  list.className='map-object-facts';
  for(const [label,value] of facts){
    const row=document.createElement('div'),reading=document.createElement('dd');
    reading.append(value);
    row.append(mapText('dt',label),reading);
    list.append(row);
  }
  content.append(list);
}
function appendMapSiteFacts(content,site){
  const kind={endpoint:'Endpoint',manual:'Proposed router',known:'MeshCore router',search:'Site found by search'}[site.kind];
  content.append(mapText('p',kind+(site.policy?' · '+routerPolicyLabels[site.policy]:'')+(site.kind==='endpoint'?' · '+site.name:'')));
  appendMapFacts(content,[
    ['Coordinates',coordinateLabel(mapPoint(site))],
    ['Antenna height · m',formatMetric(site.antenna_height_m)],
    ['Antenna gain · dBi',formatMetric(site.gain_dbi)],
    ['Terminal clutter',terminalClutterOptions.find(([value])=>value===(site.terminal_clutter||rf.router.terminal_clutter))?.[1]??'None'],
    ...(Number.isFinite(site.nearest)?[['Nearest road · m',String(Math.round(site.nearest))]]:[])
  ]);
}
const terminalClutterOptions = [['none','None'],['p2108_dom','Nearby trees and buildings (P.2108 from DOM)']];
function showMapSiteDetails(site,point,opener){
  showMapDetails(site.label,point,opener,content=>{
    appendMapSiteFacts(content,site);
    content.append(mapPopupButton('Show in analysis',()=>showSiteInAnalysis(site)));
  });
}
function showSiteInAnalysis(site){
  closeMapSelection();
  if(result?.links?.length){
    const index=result.links.findIndex(link=>link.source_id===site.id||link.target_id===site.id);
    if(index>=0)profile(index);
  }
  setMainView('view-analysis');
  const section=$('map-analysis-selection'),title=mapText('h3',site.label);
  title.tabIndex=-1;
  section.replaceChildren(title);
  appendMapSiteFacts(section,site);
  section.hidden=false;
  title.scrollIntoView({block:'nearest'});
  title.focus({preventScroll:true});
}
function showMapDetails(title,point,opener,fill){
  const content=document.createElement('div');
  content.append(mapText('strong',title));
  fill(content);
  showMapObjectPopup(point,content,opener);
  const panel=$('map-object-details');
  $('map-object-title').textContent=title;
  const body=$('map-object-body');
  body.replaceChildren();
  fill(body);
  panel.hidden=false;
  panel.scrollTop=0;
  keepMapPopupClearOfPanel(panel);
}
function keepMapPopupClearOfPanel(panel){
  const popup=mapObjectPopup?.getElement()?.getBoundingClientRect(), box=panel.getBoundingClientRect();
  if(!popup||popup.right<=box.left||popup.left>=box.right||popup.bottom<=box.top||popup.top>=box.bottom)return;
  const delta=mobileViewport.matches?[0,popup.bottom-box.top+8]:[popup.right-box.left+8,0];
  map.panBy(delta,{animate:false});
}
function showMapObjectPopup(point,content,opener){
  closeRouteLinkInspector();
  closeMapSelection();
  mapSelection={opener};
  if(!['queued','running','preparing','failed','disconnected'].includes($('route-job-card').dataset.state))$('route-job-card').hidden=true;
  const popup=L.popup(mapPopupOptions({maxHeight:Math.min(380,map.getSize().y*.56),closeOnEscapeKey:false}));
  mapObjectPopup=popup;
  popup.on('remove',()=>{
    if(mapObjectPopup!==popup)return;
    closeMapSelection(popup.getElement()?.contains(document.activeElement)||document.activeElement===document.body);
  });
  popup.setLatLng(point).setContent(content).openOn(map);
  syncMapEditControls();
  (content.querySelector('input:not(:disabled), button:not(:disabled)')||popup.getElement().querySelector('.leaflet-popup-close-button'))?.focus({preventScroll:true});
}
function closeMapSelection(restoreFocus=false){
  if(!mapSelection)return;
  const {opener}=mapSelection,popup=mapObjectPopup;
  mapSelection=null;
  mapObjectPopup=null;
  $('map-object-details').hidden=true;
  if(popup?.isOpen())map.closePopup(popup);
  if(restoreFocus){
    const target=opener();
    (target?.checkVisibility()?target:map.getContainer()).focus({preventScroll:true});
  }
}
function openMapSiteFromSidebar(marker,describe){
  if(!marker||mapEditsLocked())return;
  const site=describe();
  setMainView('view-map');
  requestAnimationFrame(()=>{
    map.panTo(marker.getLatLng(),{animate:false});
    openMapSiteEditor(site,marker.getLatLng(),()=>mapSiteElement(site.id));
  });
}
function mapEditorField(form,key,label,value,{min,max,type='number',choices}={}){
  const wrapper=mapText('label',label),control=mapEditControl(document.createElement(choices?'select':'input'));
  control.name=key;
  if(choices){
    for(const [value,label] of Object.entries(choices))control.add(new Option(label,value));
  }else{
    control.type=type;
    if(type==='number'){
      control.min=min;
      control.max=max;
      control.step='any';
    }else control.maxLength=32;
  }
  control.value=value??'';
  control.required=true;
  const error=mapText('small','');
  error.className='field-error';
  error.hidden=true;
  control.addEventListener('input',()=>{
    control.removeAttribute('aria-invalid');
    error.hidden=true;
  });
  wrapper.append(control,error);
  form.append(wrapper);
  return {control,error};
}
function openMapSiteEditor(site,point,opener){
  const content=document.createElement('div'),form=document.createElement('form');
  content.append(mapText('strong','Edit '+site.label),form);
  form.className='map-site-editor';
  form.noValidate=true;
  if(site.kind==='search')form.append(mapText('p','Saving adds this location as a proposed router.'));
  const fields={
    name:mapEditorField(form,'name','Name',site.name,{type:'text'}),
    antenna_height_m:mapEditorField(form,'antenna_height_m','Antenna height · m',site.antenna_height_m,{min:.1,max:site.kind==='known'?500:1000}),
    gain_dbi:mapEditorField(form,'gain_dbi','Antenna gain · dBi',site.gain_dbi,{min:-100,max:100}),
    terminal_clutter:mapEditorField(form,'terminal_clutter','Terminal clutter',site.terminal_clutter||(site.kind==='endpoint'?'none':'default'),{choices:Object.fromEntries(site.kind==='endpoint'?terminalClutterOptions:[['default','Repeater default'],...terminalClutterOptions])})
  };
  if(site.policy)fields.policy=mapEditorField(form,'policy','Use in search',site.policy,{choices:routerPolicyLabels});
  if(site.kind!=='known'){
    const coordinates=document.createElement('div');
    coordinates.className='coordinate-grid';
    form.append(coordinates);
    fields.latitude=mapEditorField(coordinates,'latitude','Latitude',site.latitude.toFixed(6),{min:-90,max:90});
    fields.longitude=mapEditorField(coordinates,'longitude','Longitude',site.longitude.toFixed(6),{min:-180,max:180});
  }
  const actions=document.createElement('div'),save=mapEditControl(mapText('button','Save changes'));
  actions.className='map-editor-actions';
  save.type='submit';
  actions.append(save);
  if(site.kind==='manual')actions.append(mapEditControl(mapPopupButton('Remove',()=>{
    if(mapEditsLocked())return;
    closeMapSelection();
    removeManualRouter(site.id);
    map.getContainer().focus({preventScroll:true});
  })));
  form.append(actions);
  form.onsubmit=event=>{
    event.preventDefault();
    if(mapEditsLocked())return;
    const changes={};
    for(const [key,{control,error}] of Object.entries(fields)){
      const raw=control.value.trim(),numeric=control.type==='number';
      if(!raw||!control.validity.valid||(numeric&&!Number.isFinite(Number(raw)))){
        error.textContent=raw?control.validationMessage:'Enter a value.';
        error.hidden=false;
        control.setAttribute('aria-invalid','true');
        control.focus();
        return;
      }
      changes[key]=numeric?Number(raw):raw;
    }
    closeMapSelection();
    const id=applyMapSiteEdit(site,changes);
    (mapSiteElement(id)||map.getContainer()).focus({preventScroll:true});
  };
  showMapObjectPopup(point,content,opener);
}
function saveSiteRadio(id,changes){
  const overrides=rf.site_overrides??={};
  if(changes)overrides[id]=changes;
  else delete overrides[id];
  if(!Object.keys(overrides).length)delete rf.site_overrides;
}
function applyMapSiteEdit(site,{latitude,longitude,gain_dbi,terminal_clutter,...changes}){
  const point={lat:latitude,lng:longitude};
  if(site.kind==='endpoint'){
    editEndpoint(site.key,point,{...changes,gain_dbi,terminal_clutter});
    return site.id;
  }
  let id=site.id;
  if(site.kind==='search'){
    id='M-'+randomId();
    manualRouters.push({id});
  }
  saveSiteRadio(id,{gain_dbi});
  changes.terminal_clutter=terminal_clutter==='default'?undefined:terminal_clutter;
  if(site.kind==='known')setRouterPolicy(id,changes.policy,{...changes,name_override:true,height_override:true});
  else moveManualRouter(manualRouters.find(item=>item.id===id),point,changes);
  return id;
}
function removeManualRouter(id){
  if(mapEditsLocked())return;
  manualRouters=manualRouters.filter(item=>item.id!==id);
  if(selectedManualRouterId===id)selectedManualRouterId=null;
  saveSiteRadio(id,null);
  recordEdit();
  invalidate('proposed router removed');
  renderManualRouters();
  scheduleCoverageCheck();
}
$('map-object-close').onclick=()=>closeMapSelection(true);



function moveRouteSiteToStalePane(marker){
  if(marker.options.pane==='staleRouteSitePane')return;
  const {type,label,policy}=marker._routeSiteIcon;
  routeLayer.removeLayer(marker);
  marker.options.pane='staleRouteSitePane';
  marker.setIcon(mapSiteIcon(type,label,policy,{stale:true}));
  routeLayer.addLayer(marker);
  marker.dragging?.disable();
}
function updateManualMarker(marker,router){
  const policy=router.policy||'optional';
  marker.options.title=manualRouterName(router);
  marker.setIcon(mapSiteIcon('manual',shortMapLabel(manualRouterName(router)),policy));
  marker.setTooltipContent(mapText('span',`${manualRouterName(router)} · Proposed router · ${routerPolicyLabels[policy]}`));
}
function moveManualRouter(router,point,changes){
  if(mapEditsLocked())return;
  if(changes)Object.assign(router,changes);
  router.latitude=Number(point.lat.toFixed(6));
  router.longitude=Number(point.lng.toFixed(6));
  recordEdit();
  invalidate('proposed router location');
  renderManualRouters();
  scheduleCoverageCheck();
}
function manualRouterTitle(router,routed=routedRouterIds()){
  const policy=router.policy||'optional';
  const title=document.createElement('strong');title.className='manual-router-title';title.title=router.id;
  const chip=mapText('span',shortMapLabel(manualRouterName(router)));chip.className=`map-site-chip is-${policy}`;chip.setAttribute('aria-hidden','true');
  title.append(chip,mapText('span',`${manualRouterName(router)} · ${routerPolicyLabels[policy]}${routed.has(router.id)?' · in route':''}`));
  return title;
}
function syncManualRouterTitles(){
  const routed=routedRouterIds();
  for(const row of document.querySelectorAll('#manual-list [data-manual-router-id]')){
    const router=manualRouters.find(item=>item.id===row.dataset.manualRouterId);
    if(router)row.querySelector('.manual-router-title')?.replaceWith(manualRouterTitle(router,routed));
  }
}
function renderManualRouters(){
  const list=$('manual-list');list.replaceChildren();manualLayer.clearLayers();manualMapMarkers.clear();
  ensureManualRouterNames();
  const routed=routedRouterIds();
  if(!manualRouters.length){const empty=document.createElement('p');empty.className='hint';empty.textContent='No manually placed sites yet.';list.append(empty);return;}
  for(const router of manualRouters){
    const marker=L.marker(mapPoint(router),{draggable:!mapEditsLocked(),title:manualRouterName(router),routerId:router.id}).bindTooltip('');
    updateManualMarker(marker,router);
    bindMapSite(marker,()=>routerMapSite(router,'manual'));
    bindRouteSiteInspector(marker,router.id,point=>moveManualRouter(router,point));
    bindLiveCoverageDrag(marker,()=>({slot:`manual:${router.id}`,id:router.id,kind:'router',antenna_height_m:liveCoverageSiteHeight(router.antenna_height_m,rf?.router?.height_agl_m??3),terminal_clutter:router.terminal_clutter}));
    manualMapMarkers.set(router.id,marker);
    marker.addTo(manualLayer);
    marker.getElement().classList.toggle('is-in-route',routed.has(router.id));
    const row=document.createElement('div');row.className='known-router manual-router'+(selectedManualRouterId===router.id?' selected':'');row.dataset.manualRouterId=router.id;
    const title=manualRouterTitle(router,routed);
    const coordinates=document.createElement('div');coordinates.className='coordinate-grid manual-coordinate-grid';
    const latitudeLabel=document.createElement('label'),latitude=document.createElement('input');latitudeLabel.textContent='Latitude';latitude.type='number';latitude.min='-90';latitude.max='90';latitude.step='any';latitude.value=router.latitude;latitudeLabel.append(latitude);
    const longitudeLabel=document.createElement('label'),longitude=document.createElement('input');longitudeLabel.textContent='Longitude';longitude.type='number';longitude.min='-180';longitude.max='180';longitude.step='any';longitude.value=router.longitude;longitudeLabel.append(longitude);coordinates.append(latitudeLabel,longitudeLabel);
    const move=mapEditControl(document.createElement('button'));move.type='button';move.className='text-button';move.textContent='Apply coordinates';move.onclick=()=>{if(mapEditsLocked())return;const coordinates=parseCoordinates(latitude.value,longitude.value);if(!coordinates){status('Enter a valid latitude (−90 to 90) and longitude (−180 to 180).',true);return;}const [lat,lng]=coordinates;moveManualRouter(router,{lat,lng});};
    const focus=document.createElement('button');focus.type='button';focus.className='text-button';focus.textContent='Show on map';focus.onclick=()=>focusManualRouter(router.id,{pan:true});
    const edit=mapEditControl(mapPopupButton('Edit site',()=>openMapSiteFromSidebar(marker,()=>routerMapSite(router,'manual'))));
    edit.className='text-button';
    row.append(title,coordinates,move,focus,edit);list.append(row);
  }
}
function focusManualRouter(id,{pan=false}={}){selectedManualRouterId=id;for(const row of document.querySelectorAll('#manual-list [data-manual-router-id]'))row.classList.toggle('selected',row.dataset.manualRouterId===id);const router=manualRouters.find(item=>item.id===id),row=[...document.querySelectorAll('#manual-list [data-manual-router-id]')].find(item=>item.dataset.manualRouterId===id);if(pan&&router)map.setView([router.latitude,router.longitude],Math.max(map.getZoom(),12));row?.scrollIntoView({block:'nearest'});}
function routerIsRecent(router){const heard=Date.parse(router.last_heard||''),age=Date.now()-heard;return Number.isFinite(heard)&&age>=0&&age<=7*24*60*60*1000;}
function visibleKnownRouters(){const activity=$('meshcore-filter').value,policy=$('meshcore-policy-filter').value,query=$('meshcore-search').value.trim().toLocaleLowerCase();return meshcoreRouters.filter(router=>{const routerPolicy=routerPolicies.get(router.id)||router.policy||'excluded',matchesActivity=activity==='all'||(activity==='active'&&router.relay_active===true)||(activity==='recent'&&routerIsRecent(router))||(activity==='active_recent'&&router.relay_active===true&&routerIsRecent(router)),matchesPolicy=policy==='all'||routerPolicy===policy,matchesQuery=!query||`${router.name||''} ${router.id}`.toLocaleLowerCase().includes(query);return matchesActivity&&matchesPolicy&&matchesQuery;});}
function setRouterPolicy(id,policy,changes){
  if(mapEditsLocked())return;
  const router=meshcoreRouters.find(item=>item.id===id);
  if(!router)return;
  if(changes)Object.assign(router,changes);
  routerPolicies.set(id,policy);
  recordEdit();
  invalidate(changes?'MeshCore router settings':'existing router selection');
  renderMeshcoreRouters();
  scheduleCoverageCheck();
}
function focusKnownRouter(id,{pan=false,focus=true}={}){selectedKnownRouterId=id;let rows=[...document.querySelectorAll('#meshcore-list [data-router-id]')],row=rows.find(item=>item.dataset.routerId===id);if(!row){$('meshcore-search').value='';$('meshcore-filter').value='all';$('meshcore-policy-filter').value='all';renderMeshcoreRouters();row=[...document.querySelectorAll('#meshcore-list [data-router-id]')].find(item=>item.dataset.routerId===id);status('Filters cleared to show the router selected on the map.');}for(const item of document.querySelectorAll('#meshcore-list [data-router-id]')){const selected=item.dataset.routerId===id;item.classList.toggle('selected',selected);if(selected)item.setAttribute('aria-current','true');else item.removeAttribute('aria-current');}const router=meshcoreRouters.find(item=>item.id===id);if(pan&&router&&mobileViewport.matches)setMainView('view-map');if(pan&&router)map.setView([router.latitude,router.longitude],Math.max(map.getZoom(),12));if(row){row.scrollIntoView({block:'nearest'});if(focus)row.querySelector('select')?.focus({preventScroll:true});}}
function renderKnownMapMarkers(){
  knownLayer.clearLayers();
  knownMapMarkers.clear();
  const routed=routedRouterIds();
  for(const router of meshcoreRouters){
    const policy=routerPolicies.get(router.id)||router.policy||'excluded';
    const label=router.name||router.id, inRoute=routed.has(router.id);
    const marker=L.marker(mapPoint(router),{
      icon:mapSiteIcon('existing',inRoute?shortMapLabel(label):'',policy),
      title:label, zIndexOffset:inRoute?100:0
    }).bindTooltip(mapText('span',label+' · MeshCore router · '+routerPolicyLabels[policy]+(inRoute?' · in route':'')));
    bindMapSite(marker,()=>routerMapSite(router,'known'));
    knownMapMarkers.set(router.id,marker);
    marker.addTo(knownLayer);
    marker.getElement().classList.toggle('is-in-route',inRoute);
  }
}
function renderMeshcoreRouters(){
  const list=$('meshcore-list'),visible=visibleKnownRouters(),included=meshcoreRouters.filter(router=>['optional','required'].includes(routerPolicies.get(router.id)||router.policy)).length,required=meshcoreRouters.filter(router=>(routerPolicies.get(router.id)||router.policy)==='required').length;
  list.replaceChildren();
  $('meshcore-count').textContent=`${visible.length} visible of ${meshcoreRouters.length} saved · ${included} included · ${required} required`;$('meshcore-count').className='';
  const routerSummary=$('section-summary-routers');if(routerSummary)routerSummary.textContent=`${included} included · ${required} required`;
  const includedVisible=visible.filter(router=>['optional','required'].includes(routerPolicies.get(router.id)||router.policy)).length;
  $('meshcore-scope').textContent=`Bulk actions use ${visible.length} visible matching router${visible.length===1?'':'s'} (search and activity/policy filters). ${includedVisible} currently included.`;
  $('include-all').textContent=`Mark ${visible.length} matches optional`;$('include-none').textContent=`Exclude ${includedVisible} included matches`;
  for(const router of visible){
    const row=document.createElement('div');row.className='known-router'+(router.stale?' stale':'')+(selectedKnownRouterId===router.id?' selected':'');row.dataset.routerId=router.id;if(selectedKnownRouterId===router.id)row.setAttribute('aria-current','true');
    const copy=document.createElement('span'),name=document.createElement('strong'),badge=document.createElement('em'),meta=document.createElement('small'),choice=document.createElement('select');const policy=routerPolicies.get(router.id)||router.policy||'excluded';name.textContent=(router.name||router.id)+(router.stale?' · STALE':'');badge.className=`router-policy-badge policy-${policy}`;badge.textContent=policy;const active=router.relay_active===false?' · relay inactive':router.relay_active===true?' · relay active':'';const relayCount=router.relay_count_24h==null?'':' · '+router.relay_count_24h+' relays/24h';meta.textContent=`${router.id} · ${router.corridor_distance_m==null?'saved location':(router.corridor_distance_m/1000).toFixed(1)+' km from route'} · ${router.freshness||'last heard unknown'}${active}${relayCount}`;copy.append(name,badge,meta);
    for(const [value,label] of [['excluded','Excluded'],['optional','Optional'],['required','Required']]){const option=document.createElement('option');option.value=value;option.textContent=label;choice.append(option);}choice.value=routerPolicies.get(router.id)||router.policy||'excluded';choice.disabled=busy||coverageBusy;choice.setAttribute('aria-label',`Search policy for ${router.name||router.id}`);
    choice.onchange=()=>{
      const hadFocus=document.activeElement===choice;
      setRouterPolicy(router.id,choice.value);
      if(hadFocus){
        const replacement=list.querySelector(`[data-router-id="${CSS.escape(router.id)}"] select`);
        (replacement||$('meshcore-policy-filter')).focus({preventScroll:true});
      }
    };
    const focus=document.createElement('button');focus.type='button';focus.className='text-button router-focus';focus.textContent='Show';focus.setAttribute('aria-label',`Show ${router.name||router.id} on map`);focus.onclick=()=>focusKnownRouter(router.id,{pan:true,focus:false});const edit=mapEditControl(mapPopupButton('Edit site',()=>openMapSiteFromSidebar(knownMapMarkers.get(router.id),()=>routerMapSite(router,'known'))));edit.className='text-button';row.append(copy,choice,focus,edit);list.append(row);
  }
  if(!meshcoreRouters.length){const empty=document.createElement('p');empty.className='hint';empty.textContent='Routers within the search corridor will appear here.';list.append(empty);}else if(!visible.length){const empty=document.createElement('p');empty.className='hint';empty.textContent='No saved routers match this search and filter.';list.append(empty);}
  $('include-all').disabled=busy||coverageBusy||!visible.length;$('include-none').disabled=busy||coverageBusy||!visible.some(router=>['optional','required'].includes(routerPolicies.get(router.id)||router.policy));
  renderKnownMapMarkers();
}
for(const name of ['a','b']){
  $(name).addEventListener('input',()=>{clearTerrainEstimateReview();scheduleTerrainSourcePreview();});
  $(name).onchange=()=>{try{editEndpoint(name,L.latLng(coords(name)));}catch(e){status(e.message,true);}};
  $('edit-endpoint-'+name).onclick=()=>openMapSiteFromSidebar(markers[name],()=>endpointMapSite(name));
  mark(name,coords(name));
}
fitMapSites();
for(const name of ['a','b'])$(name).addEventListener('input',()=>updatePlanReadiness());
function setInteractionMode(next){
  if(!['browse','place-a','place-b','place-router','place-target','draw-coverage-area','inspect-coverage'].includes(next))return;
  if(next!=='browse'){setMainView('view-map');closeMapSelection();map.closePopup();}
  if(interactionMode==='draw-coverage-area'&&next!=='draw-coverage-area'){coverageDrawingPoints=[];map.doubleClickZoom.enable();if($('coverage-area'))$('coverage-area').value=coverageSettings.area_mode;}
  if(interactionMode==='inspect-coverage'&&next!=='inspect-coverage'){
    inspectionRequest++;inspectionController?.abort();cancelActiveInspection();inspectionLayer.clearLayers();$('coverage-inspection').hidden=true;
  }
  interactionMode=next;
  const labels={'browse':'','place-a':'Click the map to place endpoint A','place-b':'Click the map to place endpoint B','place-router':'Click the map to place a proposed router','place-target':'Click the map to place a coverage target','draw-coverage-area':'Drawing coverage area · click polygon vertices','inspect-coverage':'Click a location to inspect predicted handheld access'};
  setMapHint(labels[next]);
  for(const button of document.querySelectorAll('.map-tools [data-place]')){
    button.setAttribute('aria-pressed',String(next===`place-${button.dataset.place}`));
  }
  $('cancel-map-mode').hidden=next==='browse';
  if($('coverage-inspect'))$('coverage-inspect').textContent=next==='inspect-coverage'?'Stop inspecting locations':'Inspect a map location';
  if($('coverage-area')){syncCoverageAreaControls();renderCoverageArea();}
}
const mobileViewport=window.matchMedia('(max-width: 650px)');
const mobileDestinationIds=['setup','map','analysis'];
function setMobileDestination(id,persist=true){
  if(!mobileDestinationIds.includes(id))id='map';
  document.querySelector('main').dataset.mobileDestination=id;
  document.querySelectorAll('.mobile-destination-nav [data-mobile-destination]').forEach(button=>button.setAttribute('aria-pressed',String(button.dataset.mobileDestination===id)));
  if(persist)try{localStorage.setItem('repeaternet.mobile-destination',id);}catch{}
  if(id==='map')requestAnimationFrame(()=>map.invalidateSize({pan:false}));
}
try{setMobileDestination(localStorage.getItem('repeaternet.mobile-destination')||'map',false);}catch{setMobileDestination('map',false);}
installNavigationControls(document,{setMainView,setMobileDestination});
let activeWorkspaceId='workspace-plan';
function syncCoverageLegend(){
  const legend=$('coverage-map-legend');
  const hasCoverage=map.hasLayer(predictedCoverageLayer)&&!!predictedCoverageLayer.getLayers().length;
  const hasDifference=map.hasLayer(coverageDiffLayer)&&!!coverageDiffLayer.getLayers().length;
  // Hiding a layer from the layer picker also hides its legend.
  legend.hidden=!hasCoverage&&!hasDifference;
  $('coverage-legend-items').hidden=!hasCoverage;
  $('coverage-difference-items').hidden=!hasDifference;
  renderCoverageLegendHeading(hasCoverage,hasDifference);
}
function syncScenarioEmptyState(){const empty=$('compare-empty-state'),runs=$('coverage-active-run');if(!empty||!runs)return;empty.hidden=[...runs.options].some(option=>option.value);}
function setWorkspace(id){
  const valid=['workspace-plan','workspace-coverage','workspace-compare'];
  if(!valid.includes(id))id='workspace-plan';
  for(const view of valid)$(view).hidden=view!==id;
  activeWorkspaceId=id;syncCoverageLegend();syncScenarioEmptyState();
  document.querySelectorAll('[data-workspace-action]').forEach(action=>action.hidden=action.dataset.workspaceAction!==id);
  document.querySelectorAll('[data-workspace-target]').forEach(button=>{
    const active=button.dataset.workspaceTarget===id;
    button.setAttribute('aria-pressed',String(active));
  });
  if(mobileViewport.matches)setMobileDestination('setup');
  updateCoverageTerrainAction();
  scheduleTerrainSourcePreview();
  try{localStorage.setItem('repeaternet.active-workspace',id);}catch{}
  requestAnimationFrame(()=>{if(!$('view-map').hidden)map.invalidateSize({pan:false});if(!$('view-analysis').hidden&&result?.links.length)profile(selected);});
}
document.querySelectorAll('[data-workspace-target]').forEach(button=>button.addEventListener('click',()=>setWorkspace(button.dataset.workspaceTarget)));
try{setWorkspace(localStorage.getItem('repeaternet.active-workspace')||'workspace-plan');}catch{setWorkspace('workspace-plan');}
const planSectionIds=['endpoint-setup','terrain-setup','router-setup','radio-setup','search-setup'];
function setPlanSection(id,persist=true,{toggle=false}={}){
  if(id!==null&&!planSectionIds.includes(id))id='endpoint-setup';
  const current=planSectionIds.find(sectionId=>$(sectionId)&&!$(sectionId).hidden);
  if(toggle&&current===id)id=null;
  for(const sectionId of planSectionIds){const section=$(sectionId),active=sectionId===id;if(section)section.hidden=!active;const button=document.querySelector(`[data-plan-section="${sectionId}"]`);if(button)button.setAttribute('aria-expanded',String(active));button?.closest('.plan-step')?.classList.toggle('open',active);}
  if(persist)try{localStorage.setItem('repeaternet.plan-section',id||'');}catch{}
  scheduleTerrainSourcePreview();
}
document.querySelectorAll('[data-plan-section]').forEach(button=>button.addEventListener('click',()=>{setPlanSection(button.dataset.planSection,true,{toggle:true});if(!$(button.dataset.planSection).hidden)requestAnimationFrame(()=>button.scrollIntoView({block:'nearest',behavior:'smooth'}));}));
try{const saved=localStorage.getItem('repeaternet.plan-section');setPlanSection(saved===''?null:saved||'endpoint-setup',false);}catch{setPlanSection('endpoint-setup',false);}
function setStepState(sectionId,state){const badge=document.querySelector(`[data-step="${sectionId}"] .step-index`);if(!badge)return;badge.dataset.stepState=state;const index=planSectionIds.indexOf(sectionId)+1;badge.textContent=state==='done'?'✓':state==='blocked'?'!':String(index);}
function updatePlanStepSummaries(report=terrainReadiness){
  const endpointsValid=validEndpointCoords('a')&&validEndpointCoords('b');
  setStepState('endpoint-setup',endpointsValid?'done':'blocked');
  setStepState('terrain-setup',!terrainLoaded?'todo':report?.uncovered_sites?.length?'blocked':'done');
  const routerSummary=$('section-summary-routers');
  if(routerSummary&&typeof candidates!=='undefined'&&candidates){const included=meshcoreRouters.filter(router=>['optional','required'].includes(routerPolicies.get(router.id)||router.policy)).length,manual=manualRouters.filter(router=>(router.policy||'optional')!=='excluded').length;routerSummary.textContent=included||manual?`${included} existing · ${manual} proposed by you`:meshcoreRouters.length?`${meshcoreRouters.length} found · none included`:'Planner proposes new sites';setStepState('router-setup',included||manual?'done':'todo');}
  if(rf&&$('section-summary-radio')){$('section-summary-radio').textContent=`${formatMetric(Number(rf.frequency_mhz),1)} MHz · ${diffractionModelName(rf.diffraction_model)}`;}
  if(candidates&&$('section-summary-search')){const objective={minimum_routers:'Fewest hops',fewest_new_installations:'Fewest new installations',minimum_infrastructure:'Minimum infrastructure',maximum_reliability:'Maximum reliability'}[candidates.priority]||'Custom objective';const effort={quick:'quick',balanced:'balanced',thorough:'thorough'}[searchEffort]||searchEffort;$('section-summary-search').textContent=`${objective} · ${effort}${candidates.road_access_enabled?' · near roads':''}`;}
}
const coverageSectionIds=['display','range','handheld','runs'];
function setCoverageSection(id,persist=true){if(!coverageSectionIds.includes(id))id='display';for(const sectionId of coverageSectionIds){const active=sectionId===id,panel=document.querySelector(`[data-coverage-panel="${sectionId}"]`),button=document.querySelector(`[data-coverage-section="${sectionId}"]`);if(panel)panel.hidden=!active;if(button)button.setAttribute('aria-pressed',String(active));}if(persist)try{localStorage.setItem('repeaternet.coverage-section',id);}catch{}}
document.querySelectorAll('[data-coverage-section]').forEach(button=>button.addEventListener('click',()=>setCoverageSection(button.dataset.coverageSection)));
try{setCoverageSection(localStorage.getItem('repeaternet.coverage-section')||'display',false);}catch{setCoverageSection('display',false);}
const mainViewIds=['view-map','view-analysis'];
function setMainView(id,persist=true){
  if(!mainViewIds.includes(id))id='view-map';
  const unchanged=$('workspace').dataset.mainView===id;
  for(const viewId of mainViewIds){const active=viewId===id,pane=$(viewId),tab=document.querySelector(`[data-main-view-target="${viewId}"]`);pane.hidden=!active;tab.setAttribute('aria-selected',String(active));tab.tabIndex=active?0:-1;}
  $('workspace').dataset.mainView=id;
  if(id==='view-analysis')closeMapSelection();
  if(id==='view-analysis'&&!['queued','running','preparing','failed','disconnected'].includes($('route-job-card').dataset.state))$('route-job-card').hidden=true;
  if(persist)try{localStorage.setItem('repeaternet.main-view',id);}catch{}
  if(persist&&mobileViewport.matches)setMobileDestination(id==='view-map'?'map':'analysis');
  if(unchanged)return;
  requestAnimationFrame(()=>{if(id==='view-map')map.invalidateSize({pan:false});else if(result?.links.length)profile(selected);});
}
const mainViewNav=document.querySelector('.main-view-nav');
document.querySelector('.main-view-nav').addEventListener('keydown',event=>{if(!['ArrowLeft','ArrowRight','Home','End'].includes(event.key))return;const tabs=[...document.querySelectorAll('[data-main-view-target]')];let index=tabs.indexOf(document.activeElement);index=event.key==='Home'?0:event.key==='End'?tabs.length-1:(index+(event.key==='ArrowRight'?1:-1)+tabs.length)%tabs.length;event.preventDefault();tabs[index].focus();setMainView(tabs[index].dataset.mainViewTarget);});
try{setMainView(localStorage.getItem('repeaternet.main-view')||'view-map',false);}catch{setMainView('view-map',false);}
$('route-show-map').onclick=()=>setMainView('view-map');
$('inspection-show-map').onclick=()=>setMainView('view-map');
window.addEventListener('keydown',event=>{
  if(document.querySelector('dialog[open]'))return;
  if(event.key==='Escape'){
    if(mapSelection||mapObjectPopup?.isOpen()){
      closeMapSelection(true);
      event.preventDefault();
      return;
    }
    const popup=map._popup;
    if(popup?.isOpen()&&popup!==mapContextPopup&&popup.getElement()?.contains(document.activeElement)){
      const opener=popup._source?.getElement?.();
      map.closePopup(popup);
      (opener?.isConnected?opener:map.getContainer()).focus({preventScroll:true});
      event.preventDefault();
      return;
    }
    if(routeLinkInspector){closeRouteLinkInspector(true);event.preventDefault();}
    if(mapContextPopup?.isOpen()){const inside=mapContextPopup.getElement()?.contains(document.activeElement);map.closePopup(mapContextPopup);if(inside)map.getContainer().focus({preventScroll:true});}
    if(interactionMode!=='browse'){
      setInteractionMode('browse');
      event.preventDefault();
    }
    return;
  }
  if(event.defaultPrevented||!(event.ctrlKey||event.metaKey)||event.altKey||
    document.activeElement?.closest('input, textarea, select, [contenteditable]:not([contenteditable="false"])'))return;
  const key=event.key.toLowerCase();
  const action=key==='z'?(event.shiftKey?'redo':'undo'):key==='y'&&event.ctrlKey&&!event.shiftKey?'redo':null;
  if(action&&!$(action).disabled){
    event.preventDefault();
    $(action).click();
  }
});
document.querySelectorAll('[data-place]').forEach(button=>button.onclick=()=>{
  if(!mapEditsLocked())setInteractionMode(`place-${button.dataset.place}`);
});
function placeMapSite(mode,point){
  if(!rf||mapEditsLocked())return;
  if(mode==='place-router'){
    const router=addManualRouterAt(point.lat,point.lng);
    if(!router)return;
    focusManualRouter(router.id);
    setMapHint('Proposed router added. Right-click or long-press its marker to edit it.',true);
  }else{
    editEndpoint(mode==='place-a'?'a':'b',point);
    setMapHint('Endpoint placed. Drag its marker to fine-tune.',true);
  }
}
map.on('contextmenu',event=>openMapContextMenu(event.latlng));
map.getContainer().addEventListener('keydown',event=>{
  if(event.target!==map.getContainer()||!isMapContextKey(event))return;
  event.preventDefault();
  openMapContextMenu(map.getCenter());
});
function openMapContextMenu(latlng){
  closeMapSelection();
  const point=latlng.wrap(),menu=document.createElement('div');
  for(const [mode,label] of [
    ['place-a','Set start (A) here'],['place-b','Set destination (B) here'],['place-router','Add proposed router here']
  ]){
    const button=mapEditControl(mapPopupButton(label,()=>{
      map.closePopup(mapContextPopup);
      placeMapSite(mode,point);
    }));
    button.disabled||=!rf;
    menu.append(button);
  }
  menu.append(mapPopupButton('Copy coordinates',async()=>{
    try{
      await navigator.clipboard.writeText(coordinateLabel([point.lat,point.lng]));
      map.closePopup(mapContextPopup);
      if(interactionMode==='browse')setMapHint('Coordinates copied.',true);
    }catch{
      if(interactionMode==='browse')setMapHint('Could not copy coordinates.',true);
    }
  }));
  mapContextPopup=L.popup(mapPopupOptions({className:'map-popup map-context-popup'})).setLatLng(point).setContent(menu).openOn(map);
  menu.querySelector('button:not(:disabled)')?.focus({preventScroll:true});
}
map.on('click',e=>{
  if(mapEditsLocked())return;
  if(interactionMode==='draw-coverage-area'){coverageDrawingPoints.push(L.latLng(Number(e.latlng.lat.toFixed(6)),Number(e.latlng.lng.toFixed(6))));renderCoverageArea();syncCoverageAreaControls();return;}
  if(interactionMode==='inspect-coverage'){inspectAt(e.latlng);return;}
  if(interactionMode==='place-target'){void placeCoverageTarget(e.latlng).catch(error=>setTargetStatus(error.message,true));return;}
  if(['place-a','place-b','place-router'].includes(interactionMode))placeMapSite(interactionMode,e.latlng.wrap());
});
function addManualRouterAt(latitude,longitude,height=rf.router.height_agl_m){if(String(latitude).trim()===''||String(longitude).trim()===''){status('Enter both a latitude and longitude before adding a router.',true);return null;}const lat=Number(latitude),lon=Number(longitude),antennaHeight=Number(height);if(!Number.isFinite(lat)||Math.abs(lat)>90||!Number.isFinite(lon)||Math.abs(lon)>180){status('Enter a valid latitude (−90 to 90) and longitude (−180 to 180).',true);return null;}if(!Number.isFinite(antennaHeight)||antennaHeight<.1||antennaHeight>1000){status('Antenna height must be between 0.1 and 1,000 m.',true);return null;}const router={id:'M-'+randomId(),latitude:Number(lat.toFixed(6)),longitude:Number(lon.toFixed(6)),antenna_height_m:antennaHeight,policy:'optional',name:nextManualRouterName()};manualRouters.push(router);renderManualRouters();recordEdit();invalidate('proposed router added');scheduleCoverageCheck();return router;}
$("add-router").onclick=()=>setInteractionMode('place-router');
$("add-router-coordinate").onclick=()=>{const router=addManualRouterAt($('router-latitude').value,$('router-longitude').value,$('router-height').value||rf.router.height_agl_m);if(router){$('router-latitude').value='';$('router-longitude').value='';status(`${manualRouterName(router)} added at entered coordinates.`);}};
$('cancel-map-mode').onclick=()=>{setInteractionMode('browse');$('coverage-area').value=coverageSettings.area_mode;};
$('fit').onclick=()=>fitMapSites();
const definitions=[['frequency_mhz','Frequency · MHz'],['tx_power_dbm','TX power · dBm'],['receiver_sensitivity_dbm','Sensitivity · dBm'],['fade_margin_db','Fade margin · dB']];
const fieldHelp = {
  frequency_mhz: 'Use the operating frequency shared by the radios on this route.',
  tx_power_dbm: 'Transmitter output before cable loss and antenna gain.',
  receiver_sensitivity_dbm: 'Weakest signal the receiver can decode at your chosen radio settings.',
  fade_margin_db: 'Signal reserve for changing conditions. A larger reserve makes links harder to qualify.',
  height_agl_m: 'Measure from the ground to the antenna.',
  gain_dbi: 'Use the gain listed for the antenna at your operating frequency.',
  feed_loss_db: 'Signal lost in the cable and connectors.',
  k_factor: 'Effective Earth radius multiplier used to model atmospheric refraction.',
  required_fresnel_clearance: 'Fraction of the first Fresnel zone that should be clear of obstacles.',
  miscellaneous_loss_db: 'Extra loss beyond the cable and antenna assumptions.',
  corridor_width_m: 'Outer limit of the search. Candidate sites are dense within half the route length of the line (at least 5 km) and sparse beyond, so longer routes get more sites.',
  grid_spacing_m: 'Smaller spacing explores more possible repeater locations.',
  maximum_new_routers: 'Limits the sites the search can add. Existing, proposed and required routers do not use this budget.',
  maximum_hops: 'Limits the number of links per route, including links through existing, proposed and required routers.',
  refine_radius_m: 'How far the search may move a generated router after the route is found, to strengthen its links.',
  refine_step_m: 'Spacing between positions tested during refinement.',
  final_sample_step_m: 'Distance between terrain checks along a final link.'
};
function input(parent,obj,key,label){const l=document.createElement('label');l.textContent=label;const i=document.createElement('input');i.type='number';i.step='any';i.value=obj[key]??'';i.dataset.key=key;const bounds={frequency_mhz:[.1,null],k_factor:[.1,null],required_fresnel_clearance:[0,1],height_agl_m:[.1,1000],grid_spacing_m:[.1,null],cell_size_m:[.1,null],coarse_sample_step_m:[.1,null],medium_sample_step_m:[.1,null],refine_step_m:[.1,null],corridor_width_m:[.1,null],maximum_neighbors_per_site:[1,200],refine_radius_m:[0,2000],maximum_road_distance_m:[10,20000]}[key];if(bounds){if(bounds[0]!==null)i.min=String(bounds[0]);if(bounds[1]!==null)i.max=String(bounds[1]);}i.onchange=()=>{obj[key]=i.value===''?null:Number(i.value);recordEdit();invalidate(label.toLowerCase());};l.append(i);
  if(fieldHelp[key]){
    const hint=document.createElement('small');
    hint.className='setting-hint';
    hint.textContent=fieldHelp[key];
    l.append(hint);
  }
  parent.append(l);return i;}
function group(parent,obj,defs){const box=document.createElement('div');box.className='fields';defs.forEach(([key,label])=>input(box,obj,key,label));parent.append(box);}
function select(parent,obj,key,label,options,id){const l=document.createElement('label');l.textContent=label;const s=document.createElement('select');for(const [value,text] of options)s.add(new Option(text,value));s.value=obj[key];if(id){s.id=id;mapEditControl(s);}s.onchange=()=>{if(mapEditsLocked()){s.value=obj[key];return;}obj[key]=s.value;recordEdit();invalidate(label.toLowerCase());if(id||obj===candidates)scheduleCoverageCheck();};l.append(s);parent.append(l);}
function details(title,parent=$('settings')){const d=document.createElement('details'),s=document.createElement('summary');s.textContent=title;d.append(s);parent.append(d);return d;}
function diffractionModelName(model){
  return {delta_bullington:'Delta-Bullington',bullington:'Bullington',deygout3:'Deygout 3-edge'}[model]||'Unknown model';
}
function validatedSetting(parent,obj,key,label,id,min,max=null,step='any'){
  const field=mapEditControl(input(parent,obj,key,label)),error=mapText('span',max===null?`Enter a whole number of at least ${min}.`:`Enter a value between ${min} and ${max}.`);
  field.id=id;field.min=min;if(max!==null)field.max=max;field.step=step;field.required=true;
  error.id=`${id}-error`;error.className='field-error';error.hidden=true;error.setAttribute('role','alert');
  field.after(error);field.setAttribute('aria-describedby',error.id);
  field.oninput=()=>{if(field.validity.valid){error.hidden=true;field.removeAttribute('aria-invalid');}};
  field.onchange=()=>{
    if(mapEditsLocked()){field.value=obj[key];return;}
    if(!validateRfSetting(field))return;
    obj[key]=field.valueAsNumber;
    recordEdit();
    invalidate(label.split(' · ')[0].toLowerCase());
    scheduleCoverageCheck();
  };
}
function validateRfSetting(field){
  if(field.value.trim()&&field.validity.valid)return true;
  const step=field.closest('.plan-step-body');
  if(step){setWorkspace('workspace-plan');setPlanSection(step.id);}
  for(let panel=field.closest('details');panel;panel=panel.parentElement.closest('details'))panel.open=true;
  $(`${field.id}-error`).hidden=false;
  field.setAttribute('aria-invalid','true');
  field.focus();
  return false;
}
function buildPropagationSettings(parent){
  const block=document.createElement('div');block.id='propagation-model-settings';block.className='propagation-model-settings';
  block.append(mapText('h3','Propagation model'));
  select(block,rf,'diffraction_model','Diffraction model',[
    ['delta_bullington','Delta-Bullington (recommended)'],
    ['bullington','Bullington (previous default)'],
    ['deygout3','Deygout 3-edge (route search; live coverage uses delta-Bullington)']
  ],'diffraction-model');
  select(block,rf,'obstacle_surface','Diffraction surface',[
    ['ground','Ground (DTM, recommended)'],
    ['surface','Surface with trees and buildings (DOM, very pessimistic)']
  ],'obstacle-surface');
  validatedSetting(block,rf,'model_correction_db','Model correction · dB','model-correction-db',0,20);
  const help=details('Help: propagation model',block);help.id='propagation-model-help';help.className='help-disclosure';
  const {terminal_clutter_radius_m:radius,model_correction_db:correction,coarse_step_allowance_db:allowances}=calibration;
  const coarserSteps=allowances.slice(1).map(([step,db])=>`${formatMetric(db)} dB at ${step} m`).join(' and ');
  help.append(
    mapText('p','Bullington represents terrain as a single edge. Delta-Bullington adds a smooth-Earth correction. Deygout considers up to three edges in route search. Live coverage uses Delta-Bullington in its place.'),
    mapText('p',`Ground elevation (DTM) is recommended. Trees do not behave like sharp terrain edges. Using surface elevation (DOM) for diffraction overestimated loss by about 21 dB in the calibration. Terminal clutter uses surface height above ground within ${radius} m of the site. It improved predictions for rooftop and mast relays. Handheld predictions did not improve.`),
    mapText('p',`The initial calibration found predictions a few dB too optimistic, so the default correction adds ${formatMetric(correction)} dB of loss. Saved coverage also allows for ridges missed by coarse sampling (calibration found ${coarserSteps} relative to the fine-step baseline). These values may change after more measurements.`)
  );
  parent.append(block);
}
function buildSettings(){
  const box=$('settings'),searchBox=$('search-settings');box.replaceChildren();searchBox.replaceChildren();
  const intent=document.createElement('div');intent.className='intent-controls';
  const title=document.createElement('h3');title.textContent='Route preferences';
  intent.append(title);
  select(intent,candidates,'infrastructure_policy','Which sites can the route use?',[['existing_and_proposed','Use existing + proposed'],['existing_only','Existing only'],['proposed_only','Proposed only']]);
  select(intent,candidates,'priority','Route objective',[['minimum_routers','Fewest hops'],['fewest_new_installations','Fewest new installations'],['minimum_infrastructure','Minimum infrastructure cost'],['maximum_reliability','Maximum reliability']]);
  const routeLimits=document.createElement('div');routeLimits.className='fields';
  validatedSetting(routeLimits,candidates,'maximum_new_routers','Maximum new routers','maximum-new-routers',0,null,1);
  validatedSetting(routeLimits,candidates,'maximum_hops','Maximum hops','maximum-hops',1,null,1);
  intent.append(routeLimits);
  searchBox.append(intent);
  const roadEnabled=$('road-access-enabled'),roadOptions=$('road-access-options'),roadFields=$('road-access-fields');
  roadFields.replaceChildren();
  input(roadFields,candidates,'maximum_road_distance_m','Max distance to road (m)');
  select(roadFields,candidates,'road_access_classes','Road classes',[['public_roads','Public roads'],['all_drivable','Public roads and tracks']]);
  roadEnabled.checked=candidates.road_access_enabled;
  roadOptions.hidden=!roadEnabled.checked;
  roadEnabled.onchange=()=>{candidates.road_access_enabled=roadEnabled.checked;roadOptions.hidden=!roadEnabled.checked;recordEdit();invalidate('road access filter');};
  group(box,rf,definitions.slice(0,1));
  const equipment=details('Transmitter and receiver specifications');
  group(equipment,rf,definitions.slice(1));
  select(equipment,rf,'validation_mode','Link validation',[['rf_propagation','RF propagation + diffraction'],['strict_los','Strict line of sight + Fresnel']]);
  let advanced=details('Antenna & propagation assumptions');
  for(const [key,label] of [['endpoint_a','Endpoint A'],['endpoint_b','Endpoint B'],['router','Repeaters']]){
    const role=document.createElement('fieldset');role.className='propagation-role';
    role.append(mapText('legend',label));
    group(role,rf[key],[['height_agl_m','Height AGL · m'],['gain_dbi','Gain · dBi'],['feed_loss_db','Feed loss · dB']]);
    select(role,rf[key],'terminal_clutter','Terminal clutter',terminalClutterOptions,`terminal-clutter-${key.replace('_','-')}`);
    advanced.append(role);
  }
  group(advanced,rf,[['k_factor','Earth k-factor'],['required_fresnel_clearance','Fresnel clearance fraction · 0–1'],['miscellaneous_loss_db','Miscellaneous loss · dB']]);
  buildPropagationSettings(advanced);
  advanced=details('Advanced search limits',searchBox);
  const advancedNote=document.createElement('p');advancedNote.className='hint';advancedNote.textContent='Lower limits can speed up a search but may exclude useful paths. Units are metres unless stated.';advanced.append(advancedNote);
  group(advanced,candidates,[['corridor_width_m','Corridor half-width · m (each side and around endpoints)'],['grid_spacing_m','Candidate grid spacing · m'],['maximum_link_distance_m','Maximum hop length · m (blank = auto)'],['refine_radius_m','Refinement radius · m'],['refine_step_m','Refinement step · m']]);
  validatedSetting(advanced,candidates,'final_sample_step_m','Final terrain sample step · m','final-sample-step-m',0.1,calibration.certification_max_step_m);
}
function sourceHtmlText(value){
  const text=document.createElement('span');
  text.textContent=value??'';
  return text.innerHTML;
}
function sourceMapTileOptions(definition){
  const options={
    attribution:sourceHtmlText(definition.attribution),
    minZoom:definition.min_zoom??0,
    opacity:definition.opacity??1,
    keepBuffer:4,
    updateWhenZooming:false,
  };
  if(definition.max_zoom!=null)options.maxZoom=definition.max_zoom;
  if(definition.subdomains!=null)options.subdomains=definition.subdomains;
  return options;
}
function makeSourceMapLayer(definition,options=sourceMapTileOptions(definition)){
  const {url,kind,wms}=definition;
  let layer;
  if(kind==='xyz')layer=L.tileLayer(url,options);
  else if(kind==='wms')layer=L.tileLayer.wms(url,{...wms,...options});
  else throw Error('Unsupported map layer kind.');
  if(!definition.overlay)retainRecentBasemapTiles(layer);
  return layer;
}
function retainRecentBasemapTiles(layer){
  const recent=new Map();
  const createTile=layer.createTile,removeTile=layer._removeTile;
  layer.createTile=function(coords,done){
    const url=this.getTileUrl(coords),tile=recent.get(url);
    if(!tile)return createTile.call(this,coords,done);
    recent.delete(url);
    L.Util.requestAnimFrame(()=>{
      if(tile.isConnected)done(null,tile);
    });
    return tile;
  };
  layer._removeTile=function(key){
    const tile=this._tiles[key]?.el;
    if(!tile?.classList.contains('leaflet-tile-loaded')||!tile.naturalWidth){
      removeTile.call(this,key);
      return;
    }
    recent.set(tile.getAttribute('src'),tile);
    if(recent.size>128)recent.delete(recent.keys().next().value);
    // TileLayer clears the image source before removing it. Detach loaded images
    // through GridLayer so a quick return to their zoom can reuse the same pixels.
    L.GridLayer.prototype._removeTile.call(this,key);
  };
  layer.on('remove',()=>recent.clear());
}
function sourceCoverageBoxes(definition){
  const coverage=definition.coverage;
  if(coverage?.type==='bbox')return [coverage.bbox];
  if(coverage?.type==='multi_bbox')return coverage.bboxes;
  if(coverage?.type==='polygon')return coverage.rings.map(ring=>{
    const longitude=ring.map(point=>point[0]),latitude=ring.map(point=>point[1]);
    return [Math.min(...longitude),Math.min(...latitude),Math.max(...longitude),Math.max(...latitude)];
  });
  return definition.coverage_bbox&&coverage?.type!=='global'?[definition.coverage_bbox]:[];
}
function sourceRingIntersectsView(ring,west,south,east,north){
  let inside=false;
  for(let index=0;index<ring.length;index++){
    const [x,y]=ring[index], [nextX,nextY]=ring[(index+1)%ring.length];
    if(x>=west&&x<=east&&y>=south&&y<=north)return true;
    if((y>south)!==(nextY>south)&&west<x+(south-y)*(nextX-x)/(nextY-y))inside=!inside;
    for(const edge of [west,east]){
      if(x===nextX||edge<Math.min(x,nextX)||edge>Math.max(x,nextX))continue;
      const latitude=y+(edge-x)*(nextY-y)/(nextX-x);
      if(latitude>=south&&latitude<=north)return true;
    }
    for(const edge of [south,north]){
      if(y===nextY||edge<Math.min(y,nextY)||edge>Math.max(y,nextY))continue;
      const longitude=x+(edge-y)*(nextX-x)/(nextY-y);
      if(longitude>=west&&longitude<=east)return true;
    }
  }
  return inside;
}
function sourceIntersectsView(definition){
  const boxes=sourceCoverageBoxes(definition);
  if(!boxes.length||map.getZoom()==null)return true;
  const bounds=map.getBounds(),south=bounds.getSouth(),north=bounds.getNorth();
  const west=L.Util.wrapNum(bounds.getWest(),[-180,180]),width=bounds.getEast()-bounds.getWest();
  const spans=width>=360?[[-180,180]]:[[west,Math.min(180,west+width)]];
  if(width<360&&west+width>180)spans.push([-180,west+width-360]);
  return boxes.some(([x,y,nextX,nextY],index)=>spans.some(([left,right])=>
    y<=north&&nextY>=south&&x<=right&&nextX>=left&&
    (definition.coverage?.type!=='polygon'||sourceRingIntersectsView(definition.coverage.rings[index],left,south,right,north))));
}
function updateSourceLayerLabels(){
  const labels=mapLayerControl?.getContainer()?.querySelectorAll('[data-source-layer-index]')||[];
  for(const label of labels)label.hidden=sourceIntersectsView(mapSourceLayers[Number(label.dataset.sourceLayerIndex)].definition);
}
function clearMapSourcePreview(){
  clearTimeout(mapSourcePreviewTimer);
  if(mapSourcePreview)map.removeLayer(mapSourcePreview);
  mapSourcePreview=null;
}
function rebuildSourceLayers(definitions,{preserveBase=true}={}){
  const previous=new Map(mapSourceLayers.map(item=>[item.definition.id,item]));
  const next=definitions.filter(definition=>definition.enabled!==false).map(definition=>{
    const options=sourceMapTileOptions(definition);
    const key=JSON.stringify([definition.kind,definition.url,definition.wms,!!definition.overlay,options]);
    const prior=previous.get(definition.id);
    const layer=prior?.key===key?prior.layer:makeSourceMapLayer(definition,options);
    return {definition,key,layer};
  });
  const active=mapSourceLayers.filter(item=>map.hasLayer(item.layer));
  const current=preserveBase?active.find(item=>!item.definition.overlay)?.definition.id:null;
  let remembered;
  try{remembered=localStorage.getItem(basemapStorageKey);}catch{}
  clearMapSourcePreview();
  if(mapLayerControl){
    map.off('click',mapLayerControl.collapse,mapLayerControl);
    mapLayerControl.remove();
  }
  mapSourceLayers=next;
  const bases=mapSourceLayers.filter(item=>!item.definition.overlay);
  const base=bases.find(item=>item.definition.id===current)
    ||bases.find(item=>item.definition.id===remembered)
    ||bases.find(item=>item.definition.default&&sourceIntersectsView(item.definition))
    ||bases.find(item=>sourceIntersectsView(item.definition));
  const overlays=mapSourceLayers.filter(({definition})=>
    definition.overlay&&active.some(item=>item.definition.overlay&&item.definition.id===definition.id)
  );
  const visibleLayers=new Set((base?[base,...overlays]:overlays).map(item=>item.layer));
  for(const {layer} of active)if(!visibleLayers.has(layer))map.removeLayer(layer);
  for(const layer of visibleLayers)if(!map.hasLayer(layer))layer.addTo(map);
  mapLayerControl=L.control.layers({},plannerMapOverlays);
  const entries=[...mapSourceLayers.entries()];
  entries.sort(([,a],[,b])=>Number(!!a.definition.overlay)-Number(!!b.definition.overlay));
  for(const [index,{definition,layer}] of entries){
    const label=`${sourceHtmlText(definition.name)}<span data-source-layer-index="${index}"${sourceIntersectsView(definition)?' hidden':''}> (outside view)</span>`;
    if(definition.overlay)mapLayerControl.addOverlay(layer,label);
    else mapLayerControl.addBaseLayer(layer,label);
  }
  mapLayerControl.addTo(map);
  bindSourceLayerKeyboard(mapLayerControl);
}
function bindSourceLayerKeyboard(control){
  const container=control.getContainer(),toggle=container.querySelector('.leaflet-control-layers-toggle');
  container.addEventListener('keydown',event=>{
    if(event.target===toggle&&['Enter',' '].includes(event.key)){
      event.preventDefault();
      control.expand();
      (container.querySelector('input:checked:not(:disabled)')||container.querySelector('input:not(:disabled)'))?.focus();
    }else if(event.key==='Escape'){
      event.preventDefault();event.stopPropagation();
      control.collapse();toggle.focus();
    }
  });
  container.addEventListener('click',event=>{
    if(event.detail===0&&event.target.matches('.leaflet-control-layers-selector'))event.target.focus();
  });
}
map.on('layeradd layerremove',()=>requestAnimationFrame(updateSourceLayerLabels));
function sourceCoverageBounds(definition){
  return sourceCoverageBoxes(definition).map(box=>`[${box.join(', ')}]`).join(' · ');
}
function sourceCoverageLabel(definition){
  if(definition.region?.trim())return definition.region.trim();
  const bounds=sourceCoverageBounds(definition);
  return bounds?`Bounds ${bounds}`:'Global';
}
function updateTerrainSourceSelect(){
  const select=$('terrain-source');
  const options=[new Option('Automatic · best available','auto')];
  const providers=dataSources?.terrain||[];
  for(const provider of providers.filter(item=>item.enabled!==false)){
    options.push(new Option(`${provider.name} · ${provider.resolution_m} m · ${sourceCoverageLabel(provider)}`,provider.id));
  }
  if(!options.some(option=>option.value===terrainOptions.source)){
    const name=providers.find(provider=>provider.id===terrainOptions.source)?.name;
    options.push(new Option(name?`${name} · unavailable`:'Saved terrain source · unavailable',terrainOptions.source));
    options.at(-1).disabled=true;
  }
  select.replaceChildren(...options);select.value=terrainOptions.source;
  $('terrain-fallback-label').hidden=terrainOptions.source==='auto';
}
function syncTerrainControls(){
  $('terrain-resolution').value=terrainOptions.requested_resolution_m;
  $('terrain-resolution').removeAttribute('aria-invalid');
  $('terrain-resolution-error').hidden=true;
  $('terrain-auto').checked=terrainOptions.auto_resolution;
  $('terrain-dom').checked=terrainOptions.include_dom;
  $('terrain-fallback').checked=terrainOptions.fallback;
  updateTerrainSourceSelect();
}
function readTerrainControls(){
  terrainOptions={...terrainOptions,
    auto_resolution:$('terrain-auto').checked,include_dom:$('terrain-dom').checked,
    source:$('terrain-source').value,fallback:$('terrain-fallback').checked};
  const resolution=$('terrain-resolution').value.trim();
  if(resolution&&$('terrain-resolution').validity.valid)terrainOptions.requested_resolution_m=Number(resolution);
  $('terrain-fallback-label').hidden=terrainOptions.source==='auto';
}
function validTerrainResolution(){
  const field=$('terrain-resolution'),error=$('terrain-resolution-error');
  const valid=field.value.trim()!==''&&field.validity.valid;
  field.setAttribute('aria-invalid',String(!valid));
  error.textContent=valid?'':'Enter a whole number from 1 to 1,000 m.';
  error.hidden=valid;
  if(!valid){field.closest('details').open=true;field.focus();}
  return valid;
}
function editTerrainControls(event){
  if(mapEditsLocked()||(event.target.id==='terrain-resolution'&&!validTerrainResolution()))return;
  readTerrainControls();recordEdit();invalidate('terrain settings');scheduleCoverageCheck();
}
function setTerrainDomNote(){
  const copy=$('terrain-dom-note');
  copy.textContent=terrainEstimateReview?.estimate.dom_unavailable_reason||terrainPreviewDomNote;
  copy.hidden=!copy.textContent;
}
function clearTerrainEstimateReview(){
  terrainEstimateReview=null;
  $('terrain-estimate').hidden=true;
  $('prepare-terrain').hidden=true;
  setTerrainDomNote();
}
function terrainSourceRequest({a,b,known_routers,manual_routers,terrain}){
  return {a,b,known_routers,manual_routers,terrain};
}
function scheduleTerrainSourcePreview(){
  const snapshot={a:validEndpointCoords('a'),b:validEndpointCoords('b'),known_routers:selectedKnownRouters(),manual_routers:manualRouters,terrain:terrainOptions};
  const ready=dataSources&&!$('workspace-plan').hidden&&!$('terrain-setup').hidden&&snapshot.a&&snapshot.b;
  const key=ready?JSON.stringify(snapshot):null;
  if(key===terrainPreviewKey)return;
  terrainPreviewKey=key;
  clearTimeout(terrainPreviewTimer);
  const request=++terrainPreviewRequest;
  const copy=$('terrain-source-preview');
  copy.hidden=!ready;
  copy.textContent=ready?'Checking sources for this route…':'';
  terrainPreviewDomNote='';
  setTerrainDomNote();
  if(ready)terrainPreviewTimer=setTimeout(()=>void previewTerrainSources(snapshot,request),300);
}
async function previewTerrainSources(snapshot,request=++terrainPreviewRequest){
  clearTimeout(terrainPreviewTimer);
  if(!dataSources)return;
  const copy=$('terrain-source-preview');
  try{
    const preview=await api('/api/data-sources/preview',sourceJsonRequest('POST',terrainSourceRequest(snapshot)));
    if(request!==terrainPreviewRequest)return;
    const caveats=[...new Set((preview.sources||[]).filter(source=>source.surface_model&&source.role!=='fallback').map(source=>source.name))];
    const caveat=caveats.length?` Surface models such as ${caveats.join(', ')} can include trees and buildings, so ground clearance is approximate.`:'';
    copy.textContent=`For this route: ${preview.summary}${/[.!?]$/.test(preview.summary)?'':'.'}${caveat}`;
    terrainPreviewDomNote=preview.dom_available===false?'No surface terrain (DOM) source covers this route.':'';
  }catch(error){
    if(request!==terrainPreviewRequest)return;
    copy.textContent=`Could not preview terrain sources. ${error.message}`;
    terrainPreviewDomNote='';
  }
  copy.hidden=false;
  setTerrainDomNote();
}
function scheduleCoverageCheck(){scheduleTerrainSourcePreview();clearTimeout(coverageTimer);terrainReadiness=null;updatePlanReadiness(null);coverageTimer=setTimeout(()=>updateTerrainCoverage(),350);}
function sourceJsonRequest(method,body){
  return {method,headers:{'Content-Type':'application/json'},body:JSON.stringify(body)};
}
function applySourceRegistry(registry){
  rebuildSourceLayers(registry.basemaps,{preserveBase:!!dataSources});
  dataSources=registry;
  dataSourcesRevision++;
  terrainPreviewKey=undefined;
  clearTerrainEstimateReview();
  updateTerrainSourceSelect();
  scheduleTerrainSourcePreview();
}
let sourceRegistryRequest;
function loadDataSources(){
  if(sourceRegistryRequest)return sourceRegistryRequest;
  sourceRegistryRequest=api('/api/data-sources').then(registry=>{
    applySourceRegistry(registry);
    dataSourcesError='';
  }).catch(error=>{
    dataSourcesError=`Data sources could not be loaded. ${error.message}`;
  }).finally(()=>{sourceRegistryRequest=null;});
  return sourceRegistryRequest;
}

const sourceKindLabels={wcs:'WCS',cog:'COG',cog_tiles:'COG tiles',xyz:'XYZ map tiles',wms:'WMS map'};
let sourceDialogOpener, sourceEditorOpener, sourceEditorLocation, sourceSavePending=false;
function sourceDialogStatus(message=''){
  const copy=$('data-sources-status');
  copy.textContent=message;
  copy.hidden=!message;
}
function sourceIssueSummary(issues){
  return issues.map(issue=>`${issue.path?issue.path+': ':''}${issue.message}`).join(' ');
}
function sourceRowButton(definition,action,handler){
  const button=document.createElement('button');
  button.type='button';
  button.textContent=action;
  button.dataset.sourceId=definition.id;
  button.dataset.sourceAction=action;
  button.setAttribute('aria-label',`${action} ${definition.name}`);
  button.onclick=handler;
  return button;
}
function renderDataSourceRows(){
  for(const [key,listId] of [['terrain','terrain-source-list'],['basemaps','map-source-list']]){
    const list=$(listId);
    list.replaceChildren();
    for(const definition of dataSources?.[key]||[]){
      const index=dataSources.custom[key].findIndex(item=>item.id===definition.id);
      const builtin=index<0,enabled=definition.enabled!==false;
      const row=document.createElement('li'), title=document.createElement('strong'),meta=document.createElement('p');
      title.textContent=definition.name;
      const specs=[sourceKindLabels[definition.kind],sourceCoverageLabel(definition)];
      if(key==='terrain')specs.push(`${definition.resolution_m} m`,`Priority ${definition.priority??0}`);
      else specs.push(`Zoom ${definition.min_zoom??0}–${definition.max_zoom??18}`,definition.overlay?'Overlay':'Base layer');
      specs.push(builtin?'Built-in':'Custom');
      if(definition.surface_model)specs.push('Surface model');
      meta.textContent=specs.join(' · ');
      const bounds=sourceCoverageBounds(definition);
      if(bounds&&definition.region)meta.title=`Bounds ${bounds}`;
      const controls=document.createElement('div');
      controls.className='source-row-actions';
      const toggle=sourceRowButton(definition,'Enabled',event=>void toggleDataSource(key,definition,index,event.currentTarget));
      toggle.setAttribute('aria-pressed',String(enabled));
      toggle.textContent=enabled?'Enabled':'Disabled';
      controls.append(toggle);
      if(key==='terrain'&&enabled){
        const result=document.createElement('p');
        result.setAttribute('role','status');
        result.hidden=true;
        controls.append(sourceRowButton(definition,'Test',event=>void probeDataSource(definition,event.currentTarget,result)));
        row.append(title,meta,controls,result);
      }else{
        if(key==='basemaps')controls.append(sourceRowButton(definition,'Preview',()=>previewMapSource(definition)));
        row.append(title,meta,controls);
      }
      if(!builtin){
        controls.append(sourceRowButton(definition,'Edit',event=>openSourceEditor({key,index},event.currentTarget)));
        controls.append(sourceRowButton(definition,'Remove',event=>void removeDataSource(key,index,definition,event.currentTarget)));
      }
      list.append(row);
    }
    if(!list.childElementCount){
      const empty=document.createElement('li');
      empty.textContent='No sources to show.';
      list.append(empty);
    }
  }
  $('add-data-source').disabled=!dataSources;
}
function focusSourceDialogControl(opener){
  const matching=opener?.dataset.sourceId&&[...$('data-sources-list').querySelectorAll('[data-source-action]')]
    .find(button=>button.dataset.sourceId===opener.dataset.sourceId&&button.dataset.sourceAction===opener.dataset.sourceAction);
  (matching||((opener?.isConnected&&opener.checkVisibility()&&!opener.disabled)?opener:null)||$('add-data-source')).focus();
}
async function openDataSources(event){
  const dialog=$('data-sources-dialog');
  sourceDialogOpener=event.currentTarget;
  $('data-sources-title').textContent='Data sources';
  $('add-data-source').hidden=false;
  $('data-source-editor').hidden=true;
  $('data-sources-list').hidden=false;
  sourceEditorLocation=null;
  renderDataSourceRows();
  sourceDialogStatus(dataSources?'':'Loading data sources…');
  dialog.showModal();
  $('data-sources-title').focus();
  if(!dataSources)await loadDataSources();
  if(!dialog.open||!$('data-source-editor').hidden)return;
  if(!dataSources)$('add-data-source').disabled=true;
  else if(!document.activeElement.closest('.source-list'))renderDataSourceRows();
  sourceDialogStatus(dataSourcesError||sourceIssueSummary(dataSources?.errors||[]));
}
async function saveSourceRegistry(custom){
  const registry=await api('/api/data-sources',sourceJsonRequest('PUT',custom));
  applySourceRegistry(registry);
  invalidateCoverageEstimate('Data sources changed. Review a new coverage estimate before calculating.');
}
async function changeSourceRegistry(custom,opener,message){
  if(sourceSavePending||mapEditsLocked())return;
  sourceSavePending=true;
  sourceDialogStatus('Saving data sources…');
  try{
    await saveSourceRegistry(custom);
    if(!$('data-sources-dialog').open)return;
    renderDataSourceRows();
    focusSourceDialogControl(opener);
    sourceDialogStatus(message);
  }catch(error){sourceDialogStatus(error.issues?.length?sourceIssueSummary(error.issues):error.message);}
  finally{sourceSavePending=false;}
}
async function toggleDataSource(key,definition,index,opener){
  const custom=clone(dataSources.custom),enabled=definition.enabled!==false;
  if(index>=0)custom[key][index].enabled=!enabled;
  else{
    const disabled=new Set(custom.disabled_builtins||[]);
    if(enabled)disabled.add(definition.id);
    else disabled.delete(definition.id);
    custom.disabled_builtins=[...disabled];
  }
  await changeSourceRegistry(custom,opener,`${definition.name} ${enabled?'disabled':'enabled'}.`);
}
async function removeDataSource(key,index,definition,opener){
  if(sourceSavePending||mapEditsLocked())return;
  const impact=key==='terrain'?' Plans that pin it will need another terrain source.':'';
  if(!await askActionDialog({title:`Remove ${definition.name}?`,copy:`Remove the custom source ${definition.name} from all projects on this server.${impact}`,confirm:'Remove source',danger:true}))return;
  const custom=clone(dataSources.custom);
  custom[key].splice(index,1);
  await changeSourceRegistry(custom,opener,`${definition.name} removed.`);
}
async function probeDataSource(definition,button,result){
  if(button.getAttribute('aria-disabled')==='true')return;
  button.setAttribute('aria-disabled','true');
  result.hidden=false;
  result.textContent='Testing at the map centre…';
  try{
    const {lat,lng}=map.getCenter().wrap();
    const probe=await api(`/api/data-sources/${encodeURIComponent(definition.id)}/probe`,sourceJsonRequest('POST',{latitude:lat,longitude:lng}));
    const details=[];
    if(Number.isFinite(probe.valid_fraction))details.push(`${(probe.valid_fraction*100).toFixed(1)}% valid`);
    if(Number.isFinite(probe.min_m)&&Number.isFinite(probe.max_m))details.push(`Min ${probe.min_m.toFixed(1)} m · max ${probe.max_m.toFixed(1)} m`);
    if(Number.isFinite(probe.elapsed_ms))details.push(`${probe.elapsed_ms} ms`);
    result.textContent=[probe.message|| (probe.ok?'Test complete':'Test failed'),...details].join(' · ');
  }catch(error){result.textContent=`Test failed. ${error.message}`;}
  finally{button.removeAttribute('aria-disabled');}
}
function previewMapSource(definition){
  try{
    clearMapSourcePreview();
    mapSourcePreview=makeSourceMapLayer(definition).addTo(map).bringToFront();
    mapSourcePreviewTimer=setTimeout(clearMapSourcePreview,15000);
    $('data-sources-dialog').close();
    setMainView('view-map');
    setMapHint(`Previewing ${definition.name} for 15 seconds${sourceIntersectsView(definition)?'':' · outside the current view'}.`);
    clearTimeout(mapHintTimer);
    mapHintTimer=setTimeout(()=>setMapHint(),15000);
  }catch(error){sourceDialogStatus(error.message);}
}

function makeSourceField(path,label,type,value,{placeholder,readOnly=false}={}){
  const wrapper=document.createElement('div'),caption=document.createElement('label');
  const field=document.createElement(Array.isArray(type)?'select':type==='json'?'textarea':'input');
  field.id=`source-field-${path.replaceAll('.','-')}`;
  field.dataset.sourcePath=path;
  field.dataset.sourceType=Array.isArray(type)?'select':type;
  if(Array.isArray(type)){
    for(const [key,copy] of type)field.add(new Option(copy,key));
  }else if(type==='checkbox'){
    field.type='checkbox';
    field.checked=!!value;
    caption.className='check-line';
  }else if(type==='json'){
    field.rows=2;
    field.spellcheck=false;
  }else{
    field.type='text';
    if(type==='number')field.inputMode='decimal';
  }
  if(type!=='checkbox')field.value=type==='json'&&value!==undefined?JSON.stringify(value):value??'';
  field.readOnly=readOnly;
  if(placeholder)field.placeholder=placeholder;
  caption.htmlFor=field.id;
  caption.append(type==='checkbox'?field:label);
  caption.append(type==='checkbox'?label:field);
  wrapper.append(caption);
  return wrapper;
}
function sourceProductFields(kind){
  if(kind==='cog')return [['url','GeoTIFF URL','text']];
  if(kind==='cog_tiles')return [
    ['url_template','Tile URL template','text',undefined,'https://example.org/dem/{NS}{lat2}{EW}{lon3}.tif'],
    ['tile_degrees','Tile size · degrees','number',1]
  ];
  return [
    ['endpoint','WCS endpoint','text'],
    ['coverage','Coverage name','text'],
    ['version','WCS version',[['1.0.0','1.0.0'],['2.0.1','2.0.1']],'1.0.0'],
    ['format','Format',[['GeoTIFF','GeoTIFF'],['image/tiff','image/tiff']],'GeoTIFF'],
    ['native_crs','Native CRS codes · JSON array','json',undefined,'["EPSG:25833"]'],
    ['axis_labels','Axis labels · JSON array','json',undefined,'["E","N"]'],
    ['params','Extra request parameters · JSON object','json'],
    ['per_crs','Per-CRS endpoints and coverage names · JSON object','json']
  ];
}
function renderSourceFields(definition){
  const fields=$('data-source-fields');
  fields.replaceChildren();
  const {kind}=definition;
  const terrain=['wcs','cog','cog_tiles'].includes(kind);
  const secretHeaders=Object.values(sourceEditorLocation?.original?.headers||{}).includes('***');
  const add=(path,label,type,value,options)=>fields.appendChild(makeSourceField(path,label,type,value,options));
  add('kind','Kind',Object.entries(sourceKindLabels),kind).querySelector('select').onchange=event=>{
    const next={kind:event.target.value};
    for(const key of ['id','name','attribution']){
      next[key]=$(`source-field-${key}`).value;
    }
    next.enabled=$('source-field-enabled').checked;
    clearSourceEditorIssues();
    renderSourceFields(next);
    $('source-field-kind').focus();
  };
  add('name','Name','text',definition.name);
  add('id','Source ID','text',definition.id,{placeholder:'example-elevation',readOnly:secretHeaders});
  add('attribution','Attribution','text',definition.attribution);
  add('enabled','Enabled','checkbox',definition.enabled!==false);
  const coverage=definition.coverage||{type:'global'};
  const coverageType=add('coverage.type','Coverage',[['global','Global'],['bbox','Bounding box'],['multi_bbox','Multiple bounding boxes'],['polygon','Polygon']],coverage.type).querySelector('select');
  add('region','Region name · optional','text',definition.region,{placeholder:'Norway'});
  const coverageFields=[
    ['bbox',add('coverage.bbox','Bounds [west, south, east, north] · JSON array','json',coverage.bbox,{placeholder:'[4, 57.9, 31.3, 71.3]'})],
    ['multi_bbox',add('coverage.bboxes','Bounding boxes · JSON array','json',coverage.bboxes)],
    ['polygon',add('coverage.rings','Outer rings [longitude, latitude] · JSON array','json',coverage.rings)]
  ];
  coverageType.onchange=()=>{
    for(const [shape,wrapper] of coverageFields){
      wrapper.hidden=coverageType.value!==shape;
      wrapper.querySelector('textarea').disabled=wrapper.hidden;
    }
  };
  coverageType.onchange();
  if(terrain){
    add('resolution_m','Native resolution · m','number',definition.resolution_m);
    add('priority','Priority','number',definition.priority??0);
    add('surface_model','Ground data is a surface model','checkbox',definition.surface_model);
    add('license','Licence','text',definition.license);
    add('homepage','Homepage URL','text',definition.homepage);
    add('max_concurrency','Maximum concurrent requests','number',definition.max_concurrency);
    add('headers','HTTP headers · JSON object','json',definition.headers);
    if(kind==='wcs')add('crs_rule','Target CRS rule',[['','Automatic'],['nearest_utm','Nearest UTM zone'],['norway_utm','Norway UTM zones']],definition.crs_rule);
    if(kind==='cog_tiles')add('missing_tile','Missing tiles',[['nodata','No data'],['sea_level','Sea level (0 m)']],definition.missing_tile??'nodata');
    for(const [product,label] of [['dtm','Ground (DTM)'],['dom','Surface (DOM)']]){
      const group=document.createElement('fieldset'),legend=document.createElement('legend');
      legend.textContent=label;
      const path=`products.${product}`,value=definition.products?.[product];
      const toggle=makeSourceField(path,`Offer ${product==='dtm'?'ground':'surface'} elevation`,'checkbox',!!value||(!definition.products&&product==='dtm'));
      toggle.querySelector('input').dataset.sourceType='product';
      group.append(legend,toggle);
      const productFields=document.createElement('fieldset');
      productFields.className='source-product-fields';
      for(const [key,fieldLabel,type,defaultValue,placeholder] of sourceProductFields(kind)){
        productFields.append(makeSourceField(`${path}.${key}`,fieldLabel,type,value?.[key]??defaultValue,{placeholder}));
      }
      group.append(productFields);
      const input=toggle.querySelector('input');
      input.onchange=()=>{productFields.hidden=productFields.disabled=!input.checked;};
      input.onchange();
      fields.append(group);
    }
  }else{
    add('url',kind==='xyz'?'Tile URL template':'WMS URL','text',definition.url);
    if(kind==='xyz')add('subdomains','Tile subdomains','text',definition.subdomains,{placeholder:'abc'});
    add('min_zoom','Minimum zoom','number',definition.min_zoom??0);
    add('max_zoom','Maximum zoom','number',definition.max_zoom??18);
    add('opacity','Opacity · 0 to 1','number',definition.opacity);
    add('overlay','Overlay','checkbox',definition.overlay);
    add('default','Default base layer','checkbox',definition.default);
    if(kind==='wms'){
      add('wms.layers','WMS layers','text',definition.wms?.layers);
      add('wms.styles','WMS styles','text',definition.wms?.styles);
      add('wms.version','WMS version','text',definition.wms?.version??'1.3.0');
      add('wms.format','WMS format','text',definition.wms?.format??'image/png');
      add('wms.transparent','Transparent WMS tiles','checkbox',definition.wms?.transparent??true);
    }
  }
}
function readSourceFields(){
  const definition={},issues=[];
  for(const field of $('data-source-fields').querySelectorAll('[data-source-path]')){
    const {sourcePath:path,sourceType:type}=field.dataset;
    if(field.matches(':disabled')||type==='product')continue;
    const raw=field.value.trim(),parts=path.split('.'),key=parts.pop();
    let value=type==='checkbox'?field.checked:raw;
    if((type==='json'||type==='number')&&raw){
      try{
        value=JSON.parse(raw);
        if(type==='number'&&!Number.isFinite(value))throw Error();
      }catch{
        issues.push({path,message:type==='number'?'Enter a number.':'Enter valid JSON.'});
        continue;
      }
    }else if(!raw&&type!=='checkbox')continue;
    const object=parts.reduce((parent,part)=>(parent[part]??={}),definition);
    object[key]=value;
  }
  return {definition,issues};
}
function readSourceJson(){
  const raw=$('data-source-json').value;
  try{
    const definition=JSON.parse(raw);
    if(!definition||Array.isArray(definition)||typeof definition!=='object')throw Error();
    return {definition,issues:[]};
  }catch{return {issues:[{path:'$json',message:'Enter a valid JSON object.'}]};}
}
function clearSourceEditorIssues(){
  const editor=$('data-source-editor');
  for(const field of editor.querySelectorAll('[aria-invalid]')){
    field.removeAttribute('aria-invalid');field.removeAttribute('aria-describedby');
  }
  for(const error of editor.querySelectorAll('.field-error'))error.remove();
  sourceDialogStatus();
}
function showSourceEditorIssues(issues,prefix=''){
  clearSourceEditorIssues();
  const editor=$('data-source-editor'),json=$('data-source-json'),jsonOpen=$('data-source-json-panel').open;
  const fields=[...editor.querySelectorAll('[data-source-path]')].filter(input=>!input.matches(':disabled')&&input.checkVisibility());
  const unmatched=[];
  let first=null;
  for(const issue of issues){
    const path=(issue.path||'').replace(/^\$\./,'');
    const local=prefix?(path===prefix?'':path.startsWith(prefix+'.')?path.slice(prefix.length+1):null):path;
    const parent=fields.filter(input=>local?.startsWith(input.dataset.sourcePath+'.')||local?.startsWith(input.dataset.sourcePath+'['))
      .sort((a,b)=>b.dataset.sourcePath.length-a.dataset.sourcePath.length)[0];
    const field=local===null?null:jsonOpen?json:fields.find(input=>input.dataset.sourcePath===local)
      ||parent||fields.find(input=>input.dataset.sourcePath.startsWith(local+'.'));
    if(!field){unmatched.push(issue);continue;}
    const errorId=`${field.id}-error`;
    let error=$(errorId);
    if(!error){
      error=document.createElement('p');error.id=errorId;error.className='field-error';
      (field===json?field:field.closest('label')).after(error);
    }
    error.textContent=[error.textContent,issue.message].filter(Boolean).join(' ');
    field.setAttribute('aria-invalid','true');
    field.setAttribute('aria-describedby',errorId);
    first??=field;
  }
  sourceDialogStatus(unmatched.length?sourceIssueSummary(unmatched):'Review the highlighted fields.');
  (first||$('data-sources-status')).focus();
}
function openSourceEditor(location,opener){
  if(sourceSavePending||mapEditsLocked()||!dataSources)return;
  const definition=location?dataSources.custom[location.key][location.index]:{kind:'wcs'};
  sourceEditorLocation={...location,original:definition};
  sourceEditorOpener=opener;
  clearSourceEditorIssues();
  $('data-source-json-panel').open=false;
  $('data-source-fields').hidden=false;
  renderSourceFields(definition);
  $('data-sources-list').hidden=true;
  $('add-data-source').hidden=true;
  $('data-source-editor').hidden=false;
  $('data-sources-title').textContent=location?'Edit source':'Add source';
  $('source-field-kind').focus();
}
function closeSourceEditor(){
  $('data-source-editor').hidden=true;
  $('data-sources-list').hidden=false;
  $('add-data-source').hidden=false;
  $('data-sources-title').textContent='Data sources';
  sourceEditorLocation=null;
  sourceDialogStatus();
  focusSourceDialogControl(sourceEditorOpener);
}
function toggleSourceJson(event){
  event.preventDefault();
  const panel=$('data-source-json-panel'),fields=$('data-source-fields'),json=$('data-source-json');
  if(panel.open){
    const {definition,issues}=readSourceJson();
    if(issues.length){showSourceEditorIssues(issues);return;}
    if(!Object.hasOwn(sourceKindLabels,definition.kind)){
      showSourceEditorIssues([{path:'kind',message:'Choose a supported source kind.'}]);return;
    }
    clearSourceEditorIssues();
    renderSourceFields(definition);
    fields.hidden=false;panel.open=false;
  }else{
    const {definition,issues}=readSourceFields();
    if(issues.length){showSourceEditorIssues(issues);return;}
    clearSourceEditorIssues();
    json.value=JSON.stringify(definition,null,2);
    panel.open=true;fields.hidden=true;json.focus();
  }
}
async function submitSourceEditor(event){
  event.preventDefault();
  if(sourceSavePending||mapEditsLocked())return;
  const dialog=$('data-sources-dialog'),location=sourceEditorLocation;
  const {definition,issues}=$('data-source-json-panel').open?readSourceJson():readSourceFields();
  if(issues.length){showSourceEditorIssues(issues);return;}
  const {key,index,original}=location;
  const group=['xyz','wms'].includes(definition.kind)?'basemaps':'terrain';
  if(group==='terrain'&&definition.id==='auto'){
    showSourceEditorIssues([{path:'id',message:'Choose an ID other than auto, which is reserved for automatic selection.'}]);return;
  }
  if(definition.id!==original.id&&Object.values(original.headers||{}).includes('***')&&Object.values(definition.headers||{}).includes('***')){
    showSourceEditorIssues([{path:'id',message:'To change this ID, first replace the redacted header values.'}]);return;
  }
  const custom=clone(dataSources.custom);
  if(key&&key!==group)custom[key].splice(index,1);
  const destination=key===group?index:custom[group].length;
  custom[group][destination]=definition;
  const prefix=`${group}[${destination}]`;
  const current=()=>dialog.open&&sourceEditorLocation===location;
  clearSourceEditorIssues();
  sourceDialogStatus('Validating source…');
  sourceSavePending=true;
  const editor=$('data-source-editor'),fields=$('data-source-fields'),jsonPanel=$('data-source-json-panel');
  editor.setAttribute('aria-busy','true');
  fields.inert=jsonPanel.inert=true;
  try{
    const validation=await api('/api/data-sources/validate',sourceJsonRequest('POST',custom));
    if(!current())return;
    if(!validation.ok){
      fields.inert=jsonPanel.inert=false;
      showSourceEditorIssues(validation.issues||[],prefix);
      return;
    }
    sourceDialogStatus('Saving source…');
    await saveSourceRegistry(custom);
    if(!dialog.open)return;
    renderDataSourceRows();
    if(current()){
      if(key)sourceEditorOpener.dataset.sourceId=definition.id;
      closeSourceEditor();
    }else focusSourceDialogControl(document.activeElement);
    sourceDialogStatus(`${definition.name} saved.`);
  }catch(error){
    fields.inert=jsonPanel.inert=false;
    if(current()&&error.issues?.length)showSourceEditorIssues(error.issues,prefix);
    else if(dialog.open)sourceDialogStatus(error.message);
  }finally{
    sourceSavePending=false;
    fields.inert=jsonPanel.inert=false;
    editor.removeAttribute('aria-busy');
  }
}
$('open-data-sources').onclick=openDataSources;
$('close-data-sources').onclick=()=>$('data-sources-dialog').close();
$('data-sources-dialog').addEventListener('close',()=>{
  const opener=sourceDialogOpener;
  const menu=opener?.closest('.project-menu');
  const summary=menu?.querySelector('summary');
  (menu?.open&&opener.checkVisibility()&&!opener.disabled?opener:summary)?.focus();
});
$('add-data-source').onclick=event=>openSourceEditor(null,event.currentTarget);
$('cancel-data-source').onclick=closeSourceEditor;
$('data-source-editor').onsubmit=submitSourceEditor;
$('data-source-json-panel').querySelector('summary').onclick=toggleSourceJson;
async function updateTerrainCoverage(requestPlan=plan()){coverageLayer.clearLayers();if(!terrainLoaded){$('coverage-note').textContent='Upload or prepare ground terrain to see coverage and check sites.';$('coverage-note').className='hint';return null;}try{const coverage=await api('/api/terrain/coverage',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify(requestPlan)});for(const polygon of coverage.dtm_outlines)L.polygon(polygon,{color:'#29805d',weight:1,fill:false}).addTo(coverageLayer);for(const polygon of coverage.dom_outlines)L.polygon(polygon,{color:'#4382b5',weight:1,dashArray:'5 4',fill:false}).addTo(coverageLayer);for(const gap of coverage.line_gaps){if(gap.length>1)L.polyline(gap,{color:'#c8463a',weight:4,dashArray:'4 6'}).addTo(coverageLayer);else if(gap.length)L.circleMarker(gap[0],{radius:5,color:'#c8463a',fillColor:'#e99a90',fillOpacity:.8}).addTo(coverageLayer);}for(const site of coverage.uncovered_sites)L.circleMarker([site.latitude,site.longitude],{radius:8,color:'#b52222',fillColor:'#ed7770',fillOpacity:.9}).bindTooltip(`${site.name}: ${site.reason==='no_data'?'terrain nodata':'outside terrain coverage'}`).addTo(coverageLayer);const missing=coverage.uncovered_sites.map(site=>site.name);$('coverage-note').textContent=missing.length?`Ground coverage missing at ${missing.join(', ')}. Add terrain tiles before searching.`:`Ground terrain covers selected sites${coverage.line_gaps.length?`; ${coverage.line_gaps.length} nodata gap(s) sampled along the direct corridor`:''}. ${coverage.surface_available?'Surface data loaded.':'Surface obstruction data unavailable — terrain only.'}`;$('coverage-note').className=missing.length?'hint error':'hint';return coverage;}catch(e){$('coverage-note').textContent=e.message;$('coverage-note').className='hint error';return null;}}
function syncCoverageInputs(){if(!$('coverage-mode'))return;$('coverage-mode').value=coverageSettings.mode;$('coverage-area').value=coverageSettings.area_mode;$('coverage-cell-size').value=coverageSettings.cell_size_m;$('coverage-buffer').value=coverageSettings.area_buffer_m;$('coverage-profile-step').value=coverageSettings.profile_step_m;$('coverage-profile-limit').value=coverageSettings.maximum_profile_samples||4096;$('coverage-endpoints').checked=coverageSettings.include_endpoints;const c=coverageSettings.client;$('client-height').value=c.height_agl_m;$('client-tx').value=c.tx_power_dbm;$('client-gain').value=c.gain_dbi;$('client-feed').value=c.feed_loss_db;$('client-sensitivity').value=c.sensitivity_dbm;$('client-loss').value=c.miscellaneous_loss_db;syncCoverageAreaControls();renderCoverageArea();}
function validateMeshBuffer(){
  const input=$('coverage-buffer');
  // CoverageSettings::validate accepts every positive finite f64 up to the maximum.
  if(input.value.trim()&&input.validity.valid&&Number(input.value)>0)return true;
  focusProjectTransitionError(input,'Enter a mesh buffer above zero and at most 500,000 m.');
  return false;
}
function readCoverageInputs(){coverageSettings={...coverageSettings,mode:$('coverage-mode').value,area_mode:$('coverage-area').value,cell_size_m:Number($('coverage-cell-size').value),area_buffer_m:Number($('coverage-buffer').value),profile_step_m:Number($('coverage-profile-step').value),maximum_profile_samples:Number($('coverage-profile-limit').value),include_endpoints:$('coverage-endpoints').checked,client:{...coverageSettings.client,name:'Handheld · custom',height_agl_m:Number($('client-height').value),tx_power_dbm:Number($('client-tx').value),gain_dbi:Number($('client-gain').value),feed_loss_db:Number($('client-feed').value),sensitivity_dbm:Number($('client-sensitivity').value),miscellaneous_loss_db:Number($('client-loss').value)}};}
function syncCoverageAreaControls(){const drawing=interactionMode==='draw-coverage-area',drawn=drawing||coverageSettings.area_mode==='drawn';$('coverage-area-actions').hidden=!drawn;$('coverage-area-hint').hidden=!drawn;$('coverage-draw-area').textContent=drawing?'Cancel drawing':'Draw area on map';$('coverage-finish-area').disabled=!drawing||coverageDrawingPoints.length<3;$('coverage-clear-area').disabled=drawing||!coverageSettings.area_polygon_wgs84;if(drawn&&!drawing)$('coverage-area-hint').textContent=coverageSettings.area_polygon_wgs84?'Drawn polygon selected. Coverage samples outside this boundary will be excluded.':'Choose Draw area on map, click at least three vertices, then finish the polygon.';}
function renderCoverageArea(){const drawing=interactionMode==='draw-coverage-area';coverageAreaLayer.clearLayers();const coordinates=drawing?coverageDrawingPoints.map(point=>[point.lat,point.lng]):coverageSettings.area_mode==='drawn'?(coverageSettings.area_polygon_wgs84||[]).map(point=>[point[1],point[0]]):[];if(coordinates.length>=3)L.polygon(coordinates,{color:drawing?'#c27b31':'#21644e',weight:2,dashArray:'7 5',fillColor:drawing?'#dda456':'#4a9a77',fillOpacity:.08,interactive:false}).addTo(coverageAreaLayer);else if(coordinates.length>=2)L.polyline(coordinates,{color:'#c27b31',weight:2,dashArray:'7 5',interactive:false}).addTo(coverageAreaLayer);for(const point of coordinates)L.circleMarker(point,{radius:3,color:'#7c4d22',weight:1,fillOpacity:.9,interactive:false}).addTo(coverageAreaLayer);}
function startCoverageAreaDrawing(){if(busy||routePreflightBusy||coverageBusy||coverageLoadingRun)return;if(interactionMode==='draw-coverage-area'){setInteractionMode('browse');$('coverage-area').value=coverageSettings.area_mode;syncCoverageAreaControls();return;}invalidateCoverageEstimate('Coverage area drawing started. Finish or cancel the polygon, then review a fresh estimate.');$('coverage-area').value='drawn';coverageDrawingPoints=[];map.doubleClickZoom.disable();setInteractionMode('draw-coverage-area');$('coverage-area-hint').textContent='Click at least three map points around the area, then select Finish polygon.';}
function finishCoverageArea(){if(interactionMode!=='draw-coverage-area'||coverageDrawingPoints.length<3)return;const ring=coverageDrawingPoints.map(point=>[Number(point.lng.toFixed(6)),Number(point.lat.toFixed(6))]);ring.push([...ring[0]]);coverageSettings.area_mode='drawn';coverageSettings.area_polygon_wgs84=ring;coverageDrawingPoints=[];map.doubleClickZoom.enable();$('coverage-area').value='drawn';setInteractionMode('browse');syncCoverageAreaControls();renderCoverageArea();coverageSettingsChanged({target:{id:'coverage-area'}});setMapHint('Coverage area saved. Calculate a preview or estimate its full grid.',true);}
function clearCoverageArea(){if(interactionMode==='draw-coverage-area')setInteractionMode('browse');coverageSettings.area_polygon_wgs84=null;if(coverageSettings.area_mode==='drawn')coverageSettings.area_mode='mesh';$('coverage-area').value=coverageSettings.area_mode;syncCoverageAreaControls();renderCoverageArea();coverageSettingsChanged({target:{id:'coverage-area'}});}
function coverageModeMetrics(cell){
  const direction=coverageSettings.mode==='downlink'?'downlink':coverageSettings.mode==='uplink'?'uplink':'two_way';
  let count=0,margin=null,bestSourceId=null;
  for(const source of cell.sources||[]){
    if(!source['valid_'+direction])continue;
    count++;
    const value=source[direction+'_margin_db'];
    if(margin===null||value>margin){margin=value;bestSourceId=source.source_id;}
  }
  return {count,margin,bestSourceId,unknown:cell.unknown_sources||0};
}
const servingPalette=['#287b64','#4169a1','#b24f55','#8a68a8','#9b6b32','#287f91','#775542','#5b8543'];
function servingColor(sourceId){const ids=(coverageDisplayInfo||coverageJob)?.source_ids||[];let index=ids.indexOf(sourceId);if(index<0){index=[...String(sourceId||'')].reduce((hash,char)=>(hash*31+char.charCodeAt(0))>>>0,7);}return servingPalette[index%servingPalette.length];}
function coverageCellPaint(cell){
  if(cell.state==='outside_area'||cell.state==='not_evaluated')return null;
  if(cell.state==='unknown_terrain')return 1;
  if(cell.state==='unresolved')return 2;
  const {count,margin,bestSourceId}=coverageModeMetrics(cell);
  if(!count)return null;
  if(coverageSettings.mode==='best_source')return coverageRgb(servingColor(bestSourceId));
  if(coverageSettings.mode==='overlap')return coverageRgb(coverageOverlapColors[Math.min(count,3)-1]);
  return Number.isFinite(margin)&&margin>=0?coverageMarginPaint(margin):null;
}
function coverageDifferencePaint(cell){
  const change=cell.reference_connected_change;
  if(change==='unknown')return 1;
  if(change==='unresolved')return 2;
  return coverageChangeColors[change]?coverageRgb(coverageChangeColors[change]):null;
}
function addCoverageRasterCells(cells){
  let raster=predictedCoverageLayer.getLayers()[0];
  if(!raster){
    const categorical=['best_source','overlap'].includes(coverageSettings.mode);
    raster=createCoverageRaster(coverageDisplayInfo||coverageJob,Number($('coverage-opacity').value)/100,coverageCellPaint,categorical);
    if(!raster)return;
    raster.addTo(predictedCoverageLayer);
    renderCoverageLegend();
  }
  raster.addCells(cells);
}
function redrawCoverageCells(){
  hideCoverageSample();
  predictedCoverageLayer.clearLayers();
  const cells=coveragePreviewActive?coveragePreviewCells:coverageCells;
  if(cells.length)addCoverageRasterCells(cells);
  renderCoverageLegend();
}
function renderCoverageDifference(report){
  coverageDiffLayer.clearLayers();
  const raster=createCoverageRaster(report,Number($('coverage-opacity').value)/100,coverageDifferencePaint,true);
  if(raster){raster.addCells(report.cells);raster.addTo(coverageDiffLayer);}
  if(!map.hasLayer(coverageDiffLayer))coverageDiffLayer.addTo(map);
  renderCoverageLegend();
}
function hideCoverageSample(){coverageHover.remove();}
function showCoverageSample(event){
  if(interactionMode!=='browse'||$('coverage-opacity').value==='0'||event.originalEvent.target.closest?.('.leaflet-interactive')){hideCoverageSample();return;}
  const diff=map.hasLayer(coverageDiffLayer)&&coverageDiffLayer.getLayers()[0]?.cellAt(event.latlng);
  const cell=map.hasLayer(predictedCoverageLayer)&&predictedCoverageLayer.getLayers()[0]?.cellAt(event.latlng);
  const change=diff&&coverageDifferenceLabels[diff.reference_connected_change];
  let details=change;
  if(!details&&cell&&!['outside_area','not_evaluated'].includes(cell.state)){
    const {count,margin,bestSourceId,unknown}=coverageModeMetrics(cell);
    const marginLabel=Number.isFinite(margin)?` · ${margin.toFixed(1)} dB`:'';
    if(cell.state==='unknown_terrain')details='Unknown terrain';
    else if(cell.state==='unresolved')details=`Unresolved profile limit · ${cell.unresolved_sources||0} source(s) need more samples`;
    else if(coverageSettings.mode==='best_source'&&bestSourceId)details=`Best source ${routeSiteLabel(bestSourceId)}${marginLabel}`;
    else details=count?`${count} usable source(s)${marginLabel}`:'No usable link';
    if(unknown&&count)details+=` · ${unknown} source(s) unknown`;
    const client=(coverageDisplayInfo||coverageJob)?.settings?.client||coverageSettings.client;
    if(coveragePreviewActive)details=`Approximate preview · ${details}`;
    details+=` · handheld ${client.height_agl_m} m AGL · ${client.tx_power_dbm} dBm TX / ${client.sensitivity_dbm} dBm sensitivity`;
  }
  if(!details){hideCoverageSample();return;}
  coverageHover.setContent(mapText('span',details)).setLatLng(event.latlng);
  if(!map.hasLayer(coverageHover))coverageHover.addTo(map);
}
const coverageDifferenceLabels={gained:'Newly connected',lost:'Lost connection',retained:'Still connected',unknown:'Unknown in either run',unresolved:'Unresolved profile limit'};
function renderCoverageLegendHeading(hasCoverage,hasDifference){
  $('coverage-legend-heading').textContent=hasCoverage
    ? coveragePreviewActive?'Approximate coverage preview':coverageStale?'Stale coverage':'Coverage'
    :'Coverage difference';
  $('coverage-difference-items').classList.toggle('with-coverage',hasCoverage&&hasDifference);
  const sources=(coverageDisplayInfo||coverageJob)?.source_ids||[];
  $('coverage-legend-search-wrap').hidden=!hasCoverage||coverageSettings.mode!=='best_source'||sources.length<=8;
}
function coverageLegendItem(legend,color,label){
  const item=document.createElement('span'),swatch=document.createElement('i');
  swatch.style.background=color;
  item.append(swatch,document.createTextNode(label));
  legend.append(item);
}
function coveragePatternSwatch(pattern){
  const [light,dark]=coveragePatterns[pattern];
  return `repeating-linear-gradient(135deg,${light} 0 4px,${dark} 4px 6px)`;
}
function renderCoverageMarginScale(legend){
  const scale=document.createElement('div'),label=document.createElement('strong');
  const ramp=document.createElement('div'),ticks=document.createElement('div');
  scale.className='coverage-margin-scale';
  label.textContent='Usable margin · dB';
  ramp.className='coverage-margin-ramp';
  ramp.style.background=`linear-gradient(to right,${coverageMarginStops.map(([db,rgb])=>`rgb(${rgb.join(',')}) ${db/15*100}%`).join(',')})`;
  ticks.className='coverage-margin-ticks';
  for(const value of ['0','5','10','15+'])ticks.append(mapText('span',value));
  scale.append(label,ramp,ticks);
  legend.append(scale);
}
function renderCoverageLegend(){
  const legend=$('coverage-legend-items');
  legend.replaceChildren();
  const mode=coverageSettings.mode;
  if(mode==='overlap'){
    coverageOverlapColors.forEach((color,i)=>coverageLegendItem(legend,color,['1 usable router','2 usable routers','3+ usable routers'][i]));
  }else if(mode==='best_source'){
    const sources=(coverageDisplayInfo||coverageJob)?.source_ids||[];
    const query=$('coverage-legend-search').value.trim().toLocaleLowerCase();
    for(const id of sources){
      const label=routeSiteLabel(id);
      if(!query||label.toLocaleLowerCase().includes(query))coverageLegendItem(legend,servingColor(id),label);
    }
  }else renderCoverageMarginScale(legend);
  coverageLegendItem(legend,'transparent','Clear · no usable signal or outside area');
  coverageLegendItem(legend,coveragePatternSwatch('unknown'),'Unknown terrain');
  coverageLegendItem(legend,coveragePatternSwatch('unresolved'),'Unresolved profile limit');
  const diff=$('coverage-difference-items');
  diff.replaceChildren(mapText('strong','Coverage difference'));
  for(const [change,label] of Object.entries(coverageDifferenceLabels)){
    coverageLegendItem(diff,coverageChangeColors[change]||coveragePatternSwatch(change),label);
  }
  syncCoverageLegend();
}
function renderCoverageSources(){const list=$('coverage-sources');if(!list)return;list.replaceChildren();const sources=(result?.route||[]).filter(site=>!['endpoint_a','endpoint_b','client'].includes(site.kind));if(!result?.found||!result?.active_alternative_id){const message=document.createElement('p');message.className='hint';message.textContent='Find a certified route to choose repeater sources.';list.append(message);$('coverage-source-count').textContent='No selected route';return;}if(!sources.length){if(coverageSettings.source_ids.length){coverageSettings.source_ids=[];recordEdit();}const message=document.createElement('p');message.className='hint';message.textContent='This route has no repeater routers. Enable endpoint sources if you want to evaluate the two route ends.';list.append(message);$('coverage-source-count').textContent='No repeater routers';return;}const validIds=new Set(sources.map(site=>site.id));if(coverageSettings.source_ids.some(id=>!validIds.has(id))){coverageSettings.source_ids=[];recordEdit();}const allSelected=coverageSettings.source_ids.length===0,selectedCount=allSelected?sources.length:coverageSettings.source_ids.length;$('coverage-source-count').textContent=allSelected?`${sources.length} available`:`${selectedCount} selected of ${sources.length}`;for(const site of sources){const label=document.createElement('label');label.className='coverage-source';const checkbox=document.createElement('input');checkbox.type='checkbox';checkbox.checked=allSelected||coverageSettings.source_ids.includes(site.id);checkbox.disabled=busy||coverageBusy||coverageLoadingRun;checkbox.dataset.sourceId=site.id;checkbox.onchange=updateCoverageSourceSelection;const name=document.createElement('span');name.textContent=`${site.id} · ${site.origin==='known'?'existing':'proposed'}`;label.append(checkbox,name);list.append(label);}}
function renderHeightScenarioControls(){const list=$('coverage-height-sources');if(!list)return;list.replaceChildren();const all=(result?.route||[]).filter(site=>!['endpoint_a','endpoint_b','client'].includes(site.kind)),chosen=new Set(coverageSettings.source_ids),sites=all.filter(site=>!chosen.size||chosen.has(site.id));for(const site of sites){const label=document.createElement('label');label.className='coverage-height-source';const name=document.createElement('span');name.textContent=`${site.id} · current ${Number(site.antenna_height_m).toFixed(1)} m`;const input=document.createElement('input');input.type='number';input.min='0.1';input.max='500';input.step='0.1';input.value=coverageHeightOverrides[site.id]??site.antenna_height_m;input.dataset.heightSource=site.id;input.dataset.originalHeight=site.antenna_height_m;input.setAttribute('aria-label',`${site.id} scenario antenna height in metres`);input.oninput=updateHeightScenarioButtons;label.append(name,input);list.append(label);}const ready=!!result?.found&&!resultStale&&!!sites.length&&!busy&&!coverageBusy;$('coverage-height-run').disabled=!ready;updateHeightScenarioButtons();}
function updateCoverageSourceSelection(){const all=[...document.querySelectorAll('#coverage-sources [data-source-id]')],selected=all.filter(input=>input.checked).map(input=>input.dataset.sourceId);if(!selected.length){const changed=[...all].find(input=>!input.checked);if(changed)changed.checked=true;setCoverageStatus('Choose at least one router source, or include endpoints.');return;}coverageSettings.source_ids=selected.length===all.length?[]:selected;renderCoverageSources();coverageSettingsChanged({target:{id:'coverage-sources'}});}
function resetFailureScenario(){for(const checkbox of document.querySelectorAll('[data-failure-source]'))checkbox.checked=false;$('failure-summary').textContent='Scenario reset. The saved coverage matrix is unchanged; run Analyze failures to recalculate the baseline summary.';renderFailureControls();}
function coverageRunLabel(run){
  const time=run.finished_at??run.created_at,date=time?new Date(time*1000).toLocaleString():'Date unknown';
  const spacing=run.effective_cell_size_m??run.requested_cell_size_m,grid=spacing?`${Number(spacing).toLocaleString()} m grid`:'Grid spacing unknown';
  const overrides=Object.keys(run.source_height_overrides||{}).length,heights=overrides?`${overrides} height override${overrides===1?'':'s'}`:'Plan heights';
  const cells=run.requested_cells?`${Number(run.requested_cells).toLocaleString()} cells`:'cells unknown',stale=run.stale?' · stale':'',search=run.route_search_complete===false?` · route search stopped at ${run.achieved_path_count??'?'}/${run.requested_path_count??'?'} paths`:'';
  return `${date} · ${grid} · ${heights} · ${run.state} · ${run.source_ids?.length||0} sources · ${cells} · ${run.alternative_id||'route'}${search}${stale}`;
}
async function loadSavedCoverageRun(jobId=$('coverage-active-run').value){
  if(!jobId||coverageBusy||coverageLoadingRun)return;const request=++coverageLoadRequest;coveragePollRequest++;coverageLoadingRun=true;if(interactionMode!=='browse')setInteractionMode('browse');inspectionRequest++;inspectionController?.abort();cancelActiveInspection();syncCoverageLocks();$('coverage-load-progress').hidden=false;$('coverage-load-progress').removeAttribute('value');$('coverage-load-status').textContent='Opening saved coverage run…';
  setCoverageResultProgress(0,0,false);
  try{
    const job=await api(`/api/coverage/jobs/${encodeURIComponent(jobId)}`);if(request!==coverageLoadRequest)return;
    if(['queued','running'].includes(job.state)){coverageJob=job;coverageDisplayInfo=job;coverageCells=[];coverageCursor=0;coveragePreviewActive=false;coveragePreviewCells=[];coverageStale=!!job.stale;predictedCoverageLayer.clearLayers();if(!map.hasLayer(predictedCoverageLayer))predictedCoverageLayer.addTo(map);$('coverage-load-progress').hidden=true;$('coverage-load-status').textContent='This saved run is still active; reconnecting to calculation and streamed cells.';coverageBusyState(true);$('coverage-cancel').hidden=false;void pollCoverage(jobId);return;}
    coverageJob=job;coverageDisplayInfo=job;coverageCells=[];coverageCursor=0;coveragePreviewActive=false;coveragePreviewCells=[];coverageStale=!!job.stale;predictedCoverageLayer.clearLayers();if(!map.hasLayer(predictedCoverageLayer))predictedCoverageLayer.addTo(map);
    let page=await api(`/api/coverage/jobs/${encodeURIComponent(jobId)}/cells?cursor=0&limit=192`);if(request!==coverageLoadRequest)return;const total=page.stored_cells||0,requested=page.requested_cells||job.requested_cells||0;let announcedPercent=-1;
    while(true){
      if(page.cells.length){appendCoverageCells(page.cells);coverageCursor=page.next_cursor??coverageCursor+page.cells.length;}
      $('coverage-load-progress').max=Math.max(1,total);$('coverage-load-progress').value=Math.min(coverageCursor,total);const percent=total?Math.floor(coverageCursor/total*4)*25:100;if(percent>announcedPercent){$('coverage-load-status').textContent=`Loading saved run · ${coverageCursor.toLocaleString()} of ${total.toLocaleString()} stored cells${job.stale?' · stale':''}.`;announcedPercent=percent;}
      if(page.next_cursor===null||page.next_cursor===undefined)break;
      page=await api(`/api/coverage/jobs/${encodeURIComponent(jobId)}/cells?cursor=${coverageCursor}&limit=192`);if(request!==coverageLoadRequest)return;
    }
    $('coverage-load-progress').hidden=true;
    coverageStale=!!job.stale||!!page.stale;renderCoverageLegend();renderFailureControls();renderHeightScenarioControls();renderCoverageContext();
    if(!coverageStale&&job.unknown_cells)noteCoverageTerrainGap();
    const complete=job.state==='complete'&&coverageCursor===requested;
    $('coverage-load-status').textContent=`${complete?'All saved cells loaded':`Partial result · ${coverageCursor.toLocaleString()} of ${requested.toLocaleString()} requested cells stored`} · ${job.state}${coverageStale?' · stale historical run':''}. No RF calculation was started.`;
    setCoverageStatus($('coverage-load-status').textContent,coverageStale||!complete);await refreshCoverageRunSelectors();
  }catch(error){if(request==coverageLoadRequest){$('coverage-load-status').textContent=`Could not fully load this saved run: ${error.message}. Any visible cells are partial.`;setCoverageStatus($('coverage-load-status').textContent,true);}}
  finally{if(request===coverageLoadRequest){$('coverage-load-progress').hidden=true;coverageLoadingRun=false;syncCoverageLocks();updateCoverageActions();}}
}
function updateHeightScenarioButtons(){const inputs=[...document.querySelectorAll('[data-height-source]')],changed=inputs.some(input=>Math.abs(Number(input.value)-Number(input.dataset.originalHeight))>.0001),ready=!!result?.found&&!resultStale&&!busy&&!coverageBusy;$('coverage-height-apply').disabled=!ready||!changed;$('coverage-height-run').disabled=!ready||!inputs.length;}
function applyCoverageHeightPreset(value){for(const input of document.querySelectorAll('[data-height-source]'))input.value=String(value);updateHeightScenarioButtons();}
const updateTerrainCoverageCore=updateTerrainCoverage;
async function checkRouteTerrain(requestPlan){
  if(!terrainLoaded)return null;
  const coverage=await api('/api/terrain/coverage?route_only=true',{
    method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify(requestPlan)
  });
  terrainReadiness=coverage;
  updatePlanReadiness(coverage);
  return coverage;
}
updateTerrainCoverage=async(requestPlan)=>{
  if(!requestPlan){
    if(!validEndpointCoords('a')||!validEndpointCoords('b'))return null;
    requestPlan=plan();
  }
  const coverage=await updateTerrainCoverageCore(requestPlan);
  if(coverage&&coverageTerrainLoaded){
    await checkRouteTerrain(requestPlan).catch(()=>{terrainReadiness=null;updatePlanReadiness(null);});
  }else{
    terrainReadiness=coverage;
    updatePlanReadiness(coverage);
  }
  return coverage;
};
function setTargetStatus(message,error=false){$('coverage-target-summary').textContent=message;$('coverage-target-summary').className=error?'hint error':'hint';}
async function saveCoverageTargets(features){const previous=coverageTargets;coverageTargets=features;try{const saved=await api('/api/coverage/targets',{method:'PUT',headers:{'Content-Type':'application/json'},body:JSON.stringify({type:'FeatureCollection',features})});coverageTargets=saved.targets.features;coverageTargetStates=new Map();coverageTargetStatesRunId=null;renderCoverageTargets();recordEdit();return coverageTargets;}catch(error){coverageTargets=previous;renderCoverageTargets();throw error;}}
async function placeCoverageTarget(latlng){setInteractionMode('browse');const name=await askActionDialog({title:'Name point target',copy:'The point will be saved to this project for assessment against a coverage run.',label:'Target name',value:'Coverage target',confirm:'Add target'});if(!name){setMapHint('Point target placement cancelled.',true);return;}await savePointCoverageTarget(latlng.lat,latlng.lng,name);}
async function savePointCoverageTarget(latitude,longitude,name){if(String(latitude).trim()===''||String(longitude).trim()==='')throw Error('Enter both a latitude and longitude before adding a target.');const lat=Number(latitude),lon=Number(longitude),normalizedName=String(name||'').trim();if(!Number.isFinite(lat)||Math.abs(lat)>90||!Number.isFinite(lon)||Math.abs(lon)>180)throw Error('Enter a valid latitude (−90 to 90) and longitude (−180 to 180).');if(!normalizedName)throw Error('Enter a name for this target.');const feature={type:'Feature',id:'T-'+randomId(),geometry:{type:'Point',coordinates:[Number(lon.toFixed(7)),Number(lat.toFixed(7))]},properties:{name:normalizedName,minimum_margin_db:0,reference_id:null}};await saveCoverageTargets([...coverageTargets,feature]);setTargetStatus('Target saved. Assess it against a completed coverage run when ready.');return feature;}
async function importCoverageTargets(event){const file=event.target.files?.[0];event.target.value='';if(!file)return;try{const form=new FormData();form.append('file',file);const imported=await api('/api/coverage/targets/import',{method:'POST',body:form});await saveCoverageTargets([...coverageTargets,...imported.features]);setTargetStatus(`${imported.features.length} target(s) imported and saved.`);}catch(error){setTargetStatus(error.message,true);}}
let cachedCoverageRuns=[];
let selectedCoverageTargetId=null;
let coverageTargetStatesRunId=null;
let editingCoverageTargetId=null;
let coverageFailureContextJobId=null;
let coverageFailureRequest=0;
let coverageFailurePending=false;
function activeCoverageRun(){return cachedCoverageRuns.find(run=>run.job_id===$('coverage-active-run').value)||null;}
function sameJsonValue(left,right){
  if(Object.is(left,right))return true;
  if(left===null||right===null||typeof left!=='object'||typeof right!=='object')return false;
  if(Array.isArray(left)||Array.isArray(right))return Array.isArray(left)&&Array.isArray(right)&&left.length===right.length&&left.every((value,index)=>sameJsonValue(value,right[index]));
  const leftKeys=Object.keys(left).sort(),rightKeys=Object.keys(right).sort();
  return leftKeys.length===rightKeys.length&&leftKeys.every((key,index)=>key===rightKeys[index]&&sameJsonValue(left[key],right[key]));
}
function coverageRunCompatibility(baseline,scenario){
  if(!baseline||!scenario)return ['Select both a baseline and a scenario run.'];
  const reasons=[];
  if(baseline.job_id===scenario.job_id)reasons.push('Choose two different runs.');
  if(baseline.state!=='complete'||scenario.state!=='complete')reasons.push('Both runs must be complete.');
  if(!sameJsonValue(baseline.terrain_fingerprint,scenario.terrain_fingerprint))reasons.push('Terrain differs.');
  if(baseline.terrain_current===false||scenario.terrain_current===false)reasons.push('Terrain changed since one of the runs.');
  if(!sameJsonValue(baseline.radio_fingerprint,scenario.radio_fingerprint))reasons.push('RF or propagation settings differ.');
  if(!sameJsonValue(baseline.model_version,scenario.model_version))reasons.push('Coverage model version differs.');
  for(const key of ['profile_step_m','maximum_profile_samples','client'])if(!sameJsonValue(baseline.settings?.[key],scenario.settings?.[key]))reasons.push('Client radio or profile resolution differs.');
  for(const key of ['rows','columns','effective_cell_size_m','area_bounds_wgs84'])if(!sameJsonValue(baseline[key],scenario[key]))reasons.push('Grid bounds or cell spacing differs.');
  const shared=(baseline.router_ids||[]).filter(id=>(scenario.router_ids||[]).includes(id)&&(baseline.source_ids||[]).includes(id)&&(scenario.source_ids||[]).includes(id));
  if(!shared.length)reasons.push('No shared selected router can serve as the reference.');
  return [...new Set(reasons)];
}
function updateComparisonReferenceOptions(runs){
  const baseline=runs.find(run=>run.job_id===$('coverage-baseline-run').value),scenario=runs.find(run=>run.job_id===$('coverage-scenario-run').value),reference=$('coverage-compare-reference'),previous=reference.value,shared=(baseline?.router_ids||[]).filter(id=>(scenario?.router_ids||[]).includes(id)&&(baseline?.source_ids||[]).includes(id)&&(scenario?.source_ids||[]).includes(id));
  for(const option of $('coverage-baseline-run').options){const run=runs.find(item=>item.job_id===option.value),reasons=coverageRunCompatibility(run,scenario);option.textContent=`${coverageRunLabel(run)}${run&&!reasons.length?' · compatible':reasons.length?` · ${reasons.join(' ')}`:''}`;}
  for(const option of $('coverage-scenario-run').options){const run=runs.find(item=>item.job_id===option.value),reasons=coverageRunCompatibility(baseline,run);option.textContent=`${coverageRunLabel(run)}${run&&!reasons.length?' · compatible':reasons.length?` · ${reasons.join(' ')}`:''}`;}
  reference.replaceChildren();for(const id of shared){const option=document.createElement('option');option.value=id;option.textContent=id;reference.append(option);}reference.value=shared.includes(previous)?previous:(shared[0]||'');
  const reasons=coverageRunCompatibility(baseline,scenario);$('coverage-compare-compatibility').textContent=reasons.length?`Cannot compare yet: ${reasons.join(' ')}`:`Compatible grid, terrain, radio and client assumptions. ${shared.length} shared reference router(s); the server rechecks compatibility when submitted.`;
  if(coverageComparison&&(coverageComparison.baseline_job_id!==baseline?.job_id||coverageComparison.scenario_job_id!==scenario?.job_id)){coverageDiffLayer.clearLayers();if(map.hasLayer(coverageDiffLayer))map.removeLayer(coverageDiffLayer);coverageComparison=null;$('coverage-compare-run-metrics').hidden=true;$('coverage-compare-summary').textContent='Comparison selection changed. Run Compare to replace the previous difference layer.';}
  $('coverage-compare-run').disabled=!!reasons.length||!reference.value||busy||coverageBusy;
}
function renderComparisonRunMetrics(baseline,scenario){
  const container=$('coverage-compare-run-metrics');container.replaceChildren();
  for(const [label,run] of [['Baseline',baseline],['Scenario',scenario]]){
    const card=document.createElement('section'),title=document.createElement('h4'),details=document.createElement('p');card.className='coverage-run-metric';title.textContent=`${label} · ${run.alternative_id||'route'} · ${run.job_id.slice(0,8)}`;
    const client=run.settings?.client||{},overrides=run.source_height_overrides||{};
    details.textContent=`${run.source_ids?.length||0} selected sources (${(run.router_ids||[]).length} routers) · ${client.height_agl_m??'—'} m handheld · ${run.effective_cell_size_m??'—'} m grid · ${Number(run.covered_cells||0).toLocaleString()} covered / ${Number(run.evaluated_cells||0).toLocaleString()} evaluated · ${Number(run.unknown_cells||0).toLocaleString()} unknown · ${Number(run.unresolved_cells||0).toLocaleString()} unresolved · ${Object.keys(overrides).length} temporary height overrides.`;card.append(title,details);container.append(card);
  }
  container.hidden=false;
}
function updateCoverageActiveRunContext(){
  const run=activeCoverageRun(),display=coverageDisplayInfo||coverageJob,loaded=!!run&&display?.job_id===run.job_id;
  if(coverageFailureContextJobId!==(run?.job_id||null)){coverageFailureContextJobId=run?.job_id||null;coverageFailureRequest++;coverageFailurePending=false;$('failure-summary').textContent=run?`Failure analysis will use selected run ${run.job_id.slice(0,8)}.`:'Select a saved run before analyzing router failures.';}
  if(coverageTargetStatesRunId&&coverageTargetStatesRunId!==run?.job_id){coverageTargetStates=new Map();coverageTargetStatesRunId=null;$('coverage-target-results').replaceChildren();}
  const context=run?`${coverageRunLabel(run)}${loaded?' · loaded on map':' · not loaded on map'}`:'No saved coverage run selected.';
  $('coverage-active-run-context').textContent=run?`Selected run · ${context}. This run is the shared context for exact inspection, target assessment and exports.`:'Select a saved run to use it for inspection, targets and exports.';
  $('coverage-target-run-summary').textContent=run?`Target assessment · ${context}. ${run.stale?'This run is stale and must be recalculated before assessment.':''}`:'Target assessment · choose a completed saved run in the Coverage workspace.';
  $('coverage-export-run-summary').textContent=run?`Export context · ${context}. Partial and stale exports remain separately opt-in.`:'Export context · choose a saved run in the Coverage workspace.';
  $('coverage-assess-targets').disabled=!coverageTargets.length||!run||run.state!=='complete'||run.stale||busy||coverageBusy;
  const canExport=!!run&&(run.state==='complete'||$('coverage-export-partial').checked)&&(!run.stale||$('coverage-export-stale').checked);
  for(const id of ['coverage-export-geojson','coverage-export-json','coverage-export-html'])$(id).disabled=!canExport||busy||coverageBusy;
  updateCoverageExportScenarios(run);updateComparisonReferenceOptions(coverageRunMetadata.filter(item=>item.state==='complete'));renderFailureControls();
}
function renderFailureControls(){
  const list=$('failure-sources'),reference=$('failure-reference');if(!list||!reference)return;
  const run=activeCoverageRun(),ids=(run?.source_ids||[]).filter(id=>run.router_ids?.includes(id));
  const ready=run?.state==='complete'&&!run.stale&&ids.length>0,disabled=!ready||busy||coverageBusy;
  const sameRun=list.dataset.jobId===(run?.job_id||''),checked=new Set(sameRun?[...list.querySelectorAll(':checked')].map(input=>input.value):[]);
  const referenceId=sameRun?reference.value:'',active=document.activeElement,focusedId=list.contains(active)?active.value:null;
  list.dataset.jobId=run?.job_id||'';
  list.replaceChildren();reference.replaceChildren();
  for(const id of ids){
    const option=document.createElement('option'),label=document.createElement('label'),checkbox=document.createElement('input'),name=document.createElement('span');
    option.value=id;option.textContent=routeSiteLabel(id);reference.append(option);
    label.className='coverage-source';checkbox.type='checkbox';checkbox.value=id;checkbox.dataset.failureSource='true';checkbox.disabled=disabled;checkbox.checked=checked.has(id);
    name.textContent=`Disable ${option.textContent}`;label.append(checkbox,name);list.append(label);
    if(id===focusedId&&!disabled&&checkbox.checkVisibility())checkbox.focus({preventScroll:true});
  }
  if(ids.includes(referenceId))reference.value=referenceId;
  $('failure-run').disabled=disabled;$('failure-run').setAttribute('aria-disabled',String(coverageFailurePending));$('failure-reset').disabled=disabled;
  if(!ids.length){const note=document.createElement('p');note.className='hint';note.textContent=run?'The selected saved run has no reusable repeater reference.':'Select a completed coverage run first.';list.append(note);}
}
async function analyzeFailureScenario(){
  const run=activeCoverageRun();if(!run||run.state!=='complete'||run.stale||coverageFailurePending)return;const request=++coverageFailureRequest,failed_ids=[...document.querySelectorAll('[data-failure-source]:checked')].map(input=>input.value);coverageFailurePending=true;$('failure-run').setAttribute('aria-disabled','true');$('failure-summary').textContent=`Analyzing saved coverage run ${run.job_id.slice(0,8)} and its certified router links…`;
  try{const report=await api(`/api/coverage/jobs/${encodeURIComponent(run.job_id)}/scenario`,{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({failed_ids,reference_id:$('failure-reference').value})});if(request!==coverageFailureRequest||activeCoverageRun()?.job_id!==run.job_id)return;const components=report.components.map((items,index)=>`C${index+1}: ${items.join(', ')}`).join(' · ')||'No surviving routers';$('failure-summary').textContent=`Run ${run.job_id.slice(0,8)} · ${report.failed_ids.length} disabled · ${report.lost_local_cells} locally covered cell(s) lost · ${report.lost_reference_connected_cells} reference-connected cell(s) lost · ${report.unknown_cells} unknown · ${report.unresolved_cells||0} unresolved · ${report.unresolved_links||0} unresolved backbone edge(s) · ${report.outside_area_cells||0} outside. Reference ${report.reference_available?'available':'unavailable'}. ${report.disconnected_source_ids.length?`Not connected: ${report.disconnected_source_ids.join(', ')}. `:''}${components}.`;}catch(error){if(request===coverageFailureRequest)$('failure-summary').textContent=error.message;}finally{if(request===coverageFailureRequest){coverageFailurePending=false;renderFailureControls();}}
}
async function runCoverageComparison(){
  const baseline=coverageRunMetadata.find(run=>run.job_id===$('coverage-baseline-run').value),scenario=coverageRunMetadata.find(run=>run.job_id===$('coverage-scenario-run').value),reference_id=$('coverage-compare-reference').value,reasons=coverageRunCompatibility(baseline,scenario);
  if(reasons.length||!reference_id){$('coverage-compare-compatibility').textContent=`Cannot compare yet: ${[...reasons,...(!reference_id?['Select a shared reference router.']:[])].join(' ')}`;return;}
  try{
    $('coverage-compare-run').disabled=true;$('coverage-compare-summary').textContent=`Comparing baseline ${baseline.job_id.slice(0,8)} with scenario ${scenario.job_id.slice(0,8)}…`;
    const report=await api('/api/coverage/compare',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({baseline_job_id:baseline.job_id,scenario_job_id:scenario.job_id,reference_id})});coverageComparison=report;renderCoverageDifference(report);renderComparisonRunMetrics(baseline,scenario);const c=report.counts,client=baseline.settings?.client,overrides=scenario.source_height_overrides||{},baselineOverrides=baseline.source_height_overrides||{},added=scenario.source_ids.filter(id=>!baseline.source_ids.includes(id)),removed=baseline.source_ids.filter(id=>!scenario.source_ids.includes(id)),heightChanges=Object.keys({...baselineOverrides,...overrides}).filter(id=>baselineOverrides[id]!==overrides[id]).map(id=>`${id} ${baselineOverrides[id]??'plan'}→${overrides[id]??'plan'} m`);
    const assumptionChanges=[...(added.length?[`added sources ${added.join(', ')}`]:[]),...(removed.length?[`removed sources ${removed.join(', ')}`]:[]),...(heightChanges.length?[`height changes ${heightChanges.join(', ')}`]:[])];
    $('coverage-compare-summary').textContent=`Baseline ${baseline.alternative_id} (${baseline.job_id.slice(0,8)}) → scenario ${scenario.alternative_id} (${scenario.job_id.slice(0,8)}). Scenario changes: ${assumptionChanges.join('; ')||'no source or height changes in the saved context'}. Shared client ${client?.height_agl_m??'—'} m AGL, ${baseline.effective_cell_size_m} m grid. Reference-connected: ${c.gained_reference_connected} gained · ${c.lost_reference_connected} lost · ${c.retained_reference_connected} retained; local: ${c.gained_local} gained · ${c.lost_local} lost; ${c.unknown} unknown · ${c.unresolved||0} unresolved. ${c.evaluated_cells} common evaluated samples · ${Number(report.approximate_sampled_area_km2).toFixed(2)} km² approximate sampled area.`;
  }catch(error){$('coverage-compare-summary').textContent=error.message;}finally{updateComparisonReferenceOptions(coverageRunMetadata.filter(item=>item.state==='complete'));}
}
async function refreshCoverageRunSelectors(){
  try{
    const response=await api('/api/coverage/jobs'),all=response.jobs||[],runs=all.filter(run=>run.state==='complete'),displayable=all.filter(run=>['complete','cancelled','interrupted'].includes(run.state)),active=$('coverage-active-run'),baseline=$('coverage-baseline-run'),scenario=$('coverage-scenario-run');
    coverageRunMetadata=all;cachedCoverageRuns=all;comparisonRuns=runs;
    const oldActive=active.value,oldBaseline=baseline.value,oldScenario=scenario.value;
    active.replaceChildren();for(const run of displayable){const option=document.createElement('option');option.value=run.job_id;option.textContent=coverageRunLabel(run);active.append(option);}
    active.value=displayable.some(run=>run.job_id===coverageJob?.job_id)?coverageJob.job_id:displayable.some(run=>run.job_id===oldActive)?oldActive:displayable[0]?.job_id||'';
    for(const selector of [baseline,scenario]){selector.replaceChildren();for(const run of runs){const option=document.createElement('option');option.value=run.job_id;option.textContent=coverageRunLabel(run);selector.append(option);}}
    baseline.value=runs.some(run=>run.job_id===oldBaseline)?oldBaseline:(runs[1]?.job_id||runs[0]?.job_id||'');
    scenario.value=runs.some(run=>run.job_id===oldScenario)?oldScenario:(runs[0]?.job_id||'');
    $('coverage-load-run').disabled=!active.value||coverageBusy||coverageLoadingRun||busy;
    updateCoverageActiveRunContext();await refreshCoverageTargetReports();
  }catch(error){$('coverage-compare-summary').textContent=error.message;}
}
function updateCoverageExportScenarios(baseline){
  const select=$('coverage-export-scenario'),previous=select.value;
  select.replaceChildren();
  const none=document.createElement('option');none.value='';none.textContent='No comparison';select.append(none);
  if(baseline){
    for(const run of cachedCoverageRuns){
      if(run.state!=='complete'||run.job_id===baseline.job_id)continue;
      const option=document.createElement('option'),reasons=coverageRunCompatibility(baseline,run);
      option.value=run.job_id;option.disabled=!!reasons.length;
      option.textContent=`${coverageRunLabel(run)}${reasons.length?` · incompatible: ${reasons.join(' ')}`:' · compatible'}`;
      select.append(option);
    }
  }
  select.value=[...select.options].some(option=>option.value===previous&&!option.disabled)?previous:'';
  if(previous&&!select.value)$('coverage-export-status').textContent='Comparison cleared. Choose a compatible run to include a comparison.';
  updateCoverageExportReferences();
}
function updateCoverageExportReferences(){
  const baseline=activeCoverageRun(),scenario=cachedCoverageRuns.find(run=>run.job_id===$('coverage-export-scenario').value),select=$('coverage-export-reference'),previous=select.value,allowed=new Set((baseline?.source_ids||[]).filter(id=>(baseline?.router_ids||[]).includes(id)&&(scenario?.source_ids||[]).includes(id)&&(scenario?.router_ids||[]).includes(id)));
  select.replaceChildren();for(const id of [...allowed].sort()){const option=document.createElement('option');option.value=id;option.textContent=id;select.append(option);}select.value=allowed.has(previous)?previous:[...allowed][0]||'';
  const reasons=baseline&&scenario?coverageRunCompatibility(baseline,scenario):[];
  if(scenario&&reasons.length)$('coverage-export-status').textContent=`Comparison unavailable: ${reasons.join(' ')}`;
}
async function downloadCoverageExport(extension){
  const run=activeCoverageRun();if(!run){$('coverage-export-status').textContent='Select a saved coverage run in the Coverage workspace first.';return;}
  if(run.state!=='complete'&&!$('coverage-export-partial').checked){$('coverage-export-status').textContent='This run is partial; enable the explicit partial-results option before exporting.';return;}
  if(run.stale&&!$('coverage-export-stale').checked){$('coverage-export-status').textContent='This run is stale; enable the explicit stale-results option before exporting.';return;}
  const params=new URLSearchParams();if($('coverage-export-partial').checked)params.set('include_partial','true');if($('coverage-export-stale').checked)params.set('include_stale','true');
  if($('coverage-export-target-report').value)params.set('target_report_id',$('coverage-export-target-report').value);
  if(extension!=='geojson'&&$('coverage-export-scenario').value){const scenario=cachedCoverageRuns.find(item=>item.job_id===$('coverage-export-scenario').value),reasons=coverageRunCompatibility(run,scenario);if(reasons.length){$('coverage-export-status').textContent=`Comparison cannot be included: ${reasons.join(' ')}`;return;}if(!$('coverage-export-reference').value){$('coverage-export-status').textContent='Choose a shared reference router for the selected comparison.';return;}params.set('scenario_job_id',$('coverage-export-scenario').value);params.set('reference_id',$('coverage-export-reference').value);}
  const query=params.toString(),path=extension==='html'?'report.html':'export.'+extension,url='/api/coverage/jobs/'+encodeURIComponent(run.job_id)+'/'+path+(query?'?'+query:'');$('coverage-export-status').textContent=`Preparing ${coverageRunLabel(run)} for export…`;
  try{const response=await fetch(url);if(!response.ok){const error=await response.json().catch(()=>({detail:response.statusText}));throw Error(error.detail||response.statusText);}const blob=await response.blob(),download=URL.createObjectURL(blob),link=document.createElement('a');link.href=download;link.download='predicted-mesh-coverage-'+run.job_id+'.'+extension;link.click();setTimeout(()=>URL.revokeObjectURL(download),1000);$('coverage-export-status').textContent=`Export downloaded for ${run.alternative_id||'saved route'} · ${run.job_id.slice(0,8)}${$('coverage-export-target-report').value?` · target report ${$('coverage-export-target-report').value.slice(0,8)}`:''}. The output records saved assumptions and explicit unknown/partial states.`;}catch(error){$('coverage-export-status').textContent=error.message;}
}
function renderCoverageTargets(){
  const list=$('coverage-target-list');if(!list)return;list.replaceChildren();targetLayer.clearLayers();
  if(editingCoverageTargetId&&!coverageTargets.some(item=>item.id===editingCoverageTargetId))editingCoverageTargetId=null;
  if(!coverageTargets.length){const empty=document.createElement('p');empty.className='hint';empty.textContent='No saved targets. Place a point or import GeoJSON.';list.append(empty);}
  else{
    const collection={type:'FeatureCollection',features:coverageTargets};
    L.geoJSON(collection,{style:feature=>{const state=coverageTargetStates.get(feature.id),selected=selectedCoverageTargetId===feature.id,color=state==='pass'?'#287b64':state==='fail'?'#ce796d':state==='unresolved'?'#9476b7':state?'#a66d24':'#397aa8';return {color,weight:selected?5:3,fillColor:color,fillOpacity:.17};},pointToLayer:(feature,latlng)=>{const state=coverageTargetStates.get(feature.id),selected=selectedCoverageTargetId===feature.id,color=state==='pass'?'#287b64':state==='fail'?'#ce796d':state==='unresolved'?'#9476b7':state?'#a66d24':'#397aa8';return L.circleMarker(latlng,{radius:selected?10:7,color,fillColor:color,fillOpacity:.85,weight:selected?4:2});},onEachFeature:(feature,layer)=>{const tip=document.createElement('span');tip.textContent=`${feature.properties?.name||feature.id} · ≥ ${Number(feature.properties?.minimum_margin_db||0).toFixed(1)} dB${coverageTargetStates.has(feature.id)?` · ${coverageTargetStates.get(feature.id)}`:''}`;layer.bindTooltip(tip);}}).addTo(targetLayer);
    for(const feature of coverageTargets){
      const row=document.createElement('div'),copy=document.createElement('span'),name=document.createElement('strong'),meta=document.createElement('small'),focus=document.createElement('button'),remove=document.createElement('button');
      row.className='known-router coverage-target-row'+(selectedCoverageTargetId===feature.id?' selected':'');row.setAttribute('aria-current',selectedCoverageTargetId===feature.id?'true':'false');
      const geometry=feature.geometry.type;name.textContent=feature.properties.name;meta.textContent=`${geometry} · ≥ ${Number(feature.properties.minimum_margin_db||0).toFixed(1)} dB${feature.properties.reference_id?` · connected to ${feature.properties.reference_id}`:''}${coverageTargetStates.has(feature.id)?` · ${coverageTargetStates.get(feature.id)}`:''}`;copy.append(name,meta);
      focus.type='button';focus.className='router-focus';focus.textContent='Focus';focus.setAttribute('aria-label',`Focus ${feature.properties.name} on the map`);focus.onclick=()=>focusCoverageTarget(feature.id);
      if(geometry==='Point'){const edit=document.createElement('button');edit.type='button';edit.className='text-button';edit.textContent='Edit';edit.onclick=()=>editCoverageTarget(feature.id);row.append(copy,focus,edit,remove);}else row.append(copy,focus,remove);
      remove.className='text-button';remove.textContent='Remove';remove.onclick=()=>{const next=coverageTargets.filter(item=>item.id!==feature.id);void saveCoverageTargets(next).catch(error=>setTargetStatus(error.message,true));};list.append(row);
    }
  }
  $('coverage-target-editor').hidden=!editingCoverageTargetId;$('coverage-assess-targets').disabled=!coverageTargets.length||busy||coverageBusy||!activeCoverageRun()||activeCoverageRun()?.state!=='complete'||activeCoverageRun()?.stale;
}
function focusCoverageTarget(targetId){
  if(mobileViewport.matches)setMainView('view-map');
  const feature=coverageTargets.find(item=>item.id===targetId);if(!feature)return;selectedCoverageTargetId=targetId;renderCoverageTargets();let found=null;targetLayer.eachLayer(layer=>{if(layer.feature?.id===targetId)found=layer;});
  if(found){if(typeof found.getBounds==='function'&&found.getBounds().isValid())map.fitBounds(found.getBounds().pad(.35));else if(typeof found.getLatLng==='function')map.setView(found.getLatLng(),Math.max(map.getZoom(),13));}
  setTargetStatus(`${feature.properties.name} selected; the map is focused on its geometry. ${coverageTargetStates.get(targetId)?`Assessment: ${coverageTargetStates.get(targetId)}.`:''}`);
}
function editCoverageTarget(targetId){const feature=coverageTargets.find(item=>item.id===targetId);if(!feature||feature.geometry.type!=='Point')return;selectedCoverageTargetId=targetId;editingCoverageTargetId=targetId;renderCoverageTargets();$('coverage-target-edit-name').value=feature.properties.name;$('coverage-target-edit-margin').value=Number(feature.properties.minimum_margin_db||0);$('coverage-target-editor').hidden=false;$('coverage-target-edit-name').focus();}
async function saveEditedCoverageTarget(){const feature=coverageTargets.find(item=>item.id===editingCoverageTargetId);if(!feature||feature.geometry.type!=='Point')return;const name=$('coverage-target-edit-name').value.trim(),margin=Number($('coverage-target-edit-margin').value);if(!name||name.length>120){setTargetStatus('Target name must contain 1–120 characters.',true);return;}if(!Number.isFinite(margin)||margin< -100||margin>100){setTargetStatus('Minimum margin must be between −100 and 100 dB.',true);return;}const updated=coverageTargets.map(item=>item.id===feature.id?{...item,properties:{...item.properties,name,minimum_margin_db:margin}}:item);try{await saveCoverageTargets(updated);selectedCoverageTargetId=feature.id;editingCoverageTargetId=null;$('coverage-target-editor').hidden=true;renderCoverageTargets();setTargetStatus(`${name} saved. Previous assessments were cleared because the target criterion changed.`);}catch(error){setTargetStatus(error.message,true);}}
async function assessSavedTargets(){
  const button=$('coverage-assess-targets'),run=activeCoverageRun();button.disabled=true;setTargetStatus('Assessing exact point/road links and saved area-grid samples…');
  try{if(!run)throw Error('Select a completed coverage run in the Coverage workspace first.');if(run.state!=='complete'||run.stale)throw Error('Recalculate the selected stale or incomplete run before assessing targets.');const report=await api(`/api/coverage/jobs/${encodeURIComponent(run.job_id)}/targets`,{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({road_spacing_m:Number($('coverage-road-spacing').value)})});coverageTargetStates=new Map(report.targets.map(item=>[item.id,item.state]));coverageTargetStatesRunId=run.job_id;renderCoverageTargets();const counts={pass:0,fail:0,unknown:0,unresolved:0,needs_finer_sampling:0,outside_analysed_area:0};for(const item of report.targets)counts[item.state]=(counts[item.state]||0)+1;setTargetStatus(`${counts.pass||0} pass · ${counts.fail||0} fail · ${counts.unresolved||0} unresolved · ${(counts.unknown||0)+(counts.needs_finer_sampling||0)+(counts.outside_analysed_area||0)} unknown or needs finer sampling · run ${run.job_id.slice(0,8)} · report ${report.report_id.slice(0,8)}.`);const results=$('coverage-target-results');results.replaceChildren();for(const item of report.targets){const row=document.createElement('div'),title=document.createElement('strong'),message=document.createElement('small'),focus=document.createElement('button');row.className='known-router';title.textContent=`${item.name} · ${item.state.replaceAll('_',' ')}`;message.textContent=item.message;focus.type='button';focus.className='router-focus';focus.textContent='Focus target';focus.onclick=()=>focusCoverageTarget(item.id);row.append(title,message,focus);results.append(row);}await refreshCoverageTargetReports();}catch(error){setTargetStatus(error.message,true);}finally{button.disabled=!coverageTargets.length||!activeCoverageRun()||busy||coverageBusy;}
}
async function refreshCoverageTargetReports(){
  const data=await api('/api/coverage/target-reports'),reports=data.reports||[],baselineSelect=$('coverage-target-baseline'),scenarioSelect=$('coverage-target-scenario'),oldBaseline=baselineSelect.value,oldScenario=scenarioSelect.value,activeId=$('coverage-active-run').value;coverageTargetReports=reports;
  for(const select of [baselineSelect,scenarioSelect]){select.replaceChildren();for(const report of reports){const option=document.createElement('option');option.value=report.report_id;option.textContent=`${new Date(report.assessed_at*1000).toLocaleString()} · ${report.pass_count}/${report.target_count} pass · ${report.unresolved_count||0} unresolved · run ${report.coverage_job_id.slice(0,8)}`;select.append(option);}}
  const exportTarget=$('coverage-export-target-report'),oldExportTarget=exportTarget.value;exportTarget.replaceChildren();const noReport=document.createElement('option');noReport.value='';noReport.textContent='No target report';exportTarget.append(noReport);
  for(const report of reports.filter(item=>item.coverage_job_id===activeId)){const option=document.createElement('option');option.value=report.report_id;option.textContent=`${new Date(report.assessed_at*1000).toLocaleString()} · ${report.pass_count}/${report.target_count} pass · ${report.unresolved_count||0} unresolved`;exportTarget.append(option);}
  exportTarget.value=[...exportTarget.options].some(option=>option.value===oldExportTarget)?oldExportTarget:'';
  const activeReports=reports.filter(report=>report.coverage_job_id===activeId);baselineSelect.value=reports.some(report=>report.report_id===oldBaseline)?oldBaseline:activeReports[0]?.report_id||reports[0]?.report_id||'';
  scenarioSelect.value=reports.some(report=>report.report_id===oldScenario&&report.report_id!==baselineSelect.value)?oldScenario:reports.find(report=>report.report_id!==baselineSelect.value)?.report_id||'';
  for(const option of baselineSelect.options){const report=reports.find(item=>item.report_id===option.value),other=reports.find(item=>item.report_id===scenarioSelect.value),reasons=targetReportCompatibility(report,other);option.textContent=`${new Date(report.assessed_at*1000).toLocaleString()} · ${report.pass_count}/${report.target_count} pass · run ${report.coverage_job_id.slice(0,8)}${reasons.length?` · incompatible: ${reasons.join(' ')}`:' · compatible'}`;}
  for(const option of scenarioSelect.options){const report=reports.find(item=>item.report_id===option.value),other=reports.find(item=>item.report_id===baselineSelect.value),reasons=targetReportCompatibility(other,report);option.textContent=`${new Date(report.assessed_at*1000).toLocaleString()} · ${report.pass_count}/${report.target_count} pass · run ${report.coverage_job_id.slice(0,8)}${reasons.length?` · incompatible: ${reasons.join(' ')}`:' · compatible'}`;}
  updateTargetCompareButton();
}
function targetReportCompatibility(baseline,scenario){
  if(!baseline||!scenario)return ['Select two saved assessment reports.'];const reasons=[];
  for(const key of ['target_fingerprint','terrain_fingerprint','radio_fingerprint','model_version','grid_compatibility','client_profile','profile_step_m','road_sample_spacing_m'])if(!sameJsonValue(baseline[key],scenario[key]))reasons.push(key==='target_fingerprint'?'Saved target names or criteria differ.':key==='terrain_fingerprint'?'Terrain differs.':key==='radio_fingerprint'?'RF or propagation settings differ.':key==='model_version'?'Coverage model differs.':key==='road_sample_spacing_m'?'Road sampling spacing differs.':'Grid, client or profile assumptions differ.');
  return [...new Set(reasons)];
}
let targetComparisonKey=null,targetComparisonPending=false;
function targetComparisonSelection(){
  return JSON.stringify({baseline_report_id:$('coverage-target-baseline').value,scenario_report_id:$('coverage-target-scenario').value});
}
function updateTargetCompareButton(){
  const reports=coverageTargetReports,baseline=reports.find(item=>item.report_id===$('coverage-target-baseline').value),scenario=reports.find(item=>item.report_id===$('coverage-target-scenario').value),reasons=targetReportCompatibility(baseline,scenario);
  $('coverage-target-compare').disabled=!!reasons.length||busy||coverageBusy||targetComparisonPending;
  if(targetComparisonKey===targetComparisonSelection())return;
  targetComparisonKey=null;
  $('coverage-target-comparison-results').replaceChildren();
  $('coverage-target-compare-summary').textContent=reasons.length?`Cannot compare these reports: ${reasons.join(' ')}`:`Compatible saved reports · run ${baseline.coverage_job_id.slice(0,8)} versus ${scenario.coverage_job_id.slice(0,8)}. The server rechecks all assumptions.`;
}
async function runCoverageTargetComparison(){
  const selection=targetComparisonSelection(),rows=$('coverage-target-comparison-results'),summary=$('coverage-target-compare-summary');
  targetComparisonKey=selection;targetComparisonPending=true;updateTargetCompareButton();
  rows.replaceChildren();summary.textContent='Comparing saved target reports…';
  try{
    const report=await api('/api/coverage/target-reports/compare',{method:'POST',headers:{'Content-Type':'application/json'},body:selection});
    if(targetComparisonKey!==selection)return;
    for(const item of report.targets){const row=document.createElement('div'),title=document.createElement('strong'),details=document.createElement('small'),focus=document.createElement('button');row.className='known-router';title.textContent=`${item.name} · ${item.change}`;details.textContent=`${item.baseline_state} → ${item.scenario_state}`;focus.type='button';focus.className='router-focus';focus.textContent='Focus target';focus.onclick=()=>focusCoverageTarget(item.id);row.append(title,details,focus);rows.append(row);}
    const baseline=coverageTargetReports.find(item=>item.report_id===$('coverage-target-baseline').value),scenario=coverageTargetReports.find(item=>item.report_id===$('coverage-target-scenario').value);
    summary.textContent=`${baseline?.coverage_job_id?.slice(0,8)||'baseline'} → ${scenario?.coverage_job_id?.slice(0,8)||'scenario'} · ${report.counts.improved} improved · ${report.counts.regressed} regressed · ${report.counts.unchanged} unchanged · ${report.counts.uncertain} uncertain.`;
  }catch(error){if(targetComparisonKey===selection)summary.textContent=error.message;}
  finally{targetComparisonPending=false;updateTargetCompareButton();}
}
function updateCoverageTargetInputs(){
  $('coverage-add-target').onclick=()=>{if(busy||coverageBusy)return;setInteractionMode('place-target');};
  $('add-target-coordinate').onclick=async()=>{try{await savePointCoverageTarget($('target-latitude').value,$('target-longitude').value,$('target-name').value);$('target-latitude').value='';$('target-longitude').value='';setTargetStatus('Point target saved from entered coordinates.');}catch(error){setTargetStatus(error.message,true);}};
  $('coverage-target-file').onchange=importCoverageTargets;$('coverage-assess-targets').onclick=assessSavedTargets;$('coverage-target-compare').onclick=runCoverageTargetComparison;
  $('coverage-target-baseline').onchange=updateTargetCompareButton;$('coverage-target-scenario').onchange=updateTargetCompareButton;
  $('coverage-export-scenario').onchange=updateCoverageExportReferences;$('coverage-export-geojson').onclick=()=>downloadCoverageExport('geojson');$('coverage-export-json').onclick=()=>downloadCoverageExport('json');$('coverage-export-html').onclick=()=>downloadCoverageExport('html');
  $('coverage-active-run').onchange=()=>{$('coverage-load-run').disabled=!$('coverage-active-run').value||coverageBusy||coverageLoadingRun||busy;if($('coverage-active-run').value)$('coverage-load-status').textContent='Run selected. Choose Show saved run on map to display it.';inspectionRequest++;inspectionController?.abort();cancelActiveInspection();inspectionLayer.clearLayers();if(interactionMode==='inspect-coverage')setInteractionMode('browse');updateCoverageActiveRunContext();void refreshCoverageTargetReports();};
  $('coverage-export-partial').onchange=updateCoverageActiveRunContext;$('coverage-export-stale').onchange=updateCoverageActiveRunContext;
  $('coverage-target-save-edit').onclick=()=>void saveEditedCoverageTarget();$('coverage-target-cancel-edit').onclick=()=>{editingCoverageTargetId=null;$('coverage-target-editor').hidden=true;};
  void refreshCoverageRunSelectors();
}
let coverageTargetReports=[];
async function applyCoverageHeightScenario(){const edits=new Map();for(const input of document.querySelectorAll('[data-height-source]')){const height=Number(input.value),original=Number(input.dataset.originalHeight);if(!Number.isFinite(height)||height<.1||height>500){$('coverage-compare-summary').textContent='Plan antenna heights must be between 0.1 and 500 m AGL.';return;}if(Math.abs(height-original)>.0001)edits.set(input.dataset.heightSource,height);}if(!edits.size)return;for(const [siteId,height] of edits){const existing=meshcoreRouters.find(router=>router.id===siteId);if(existing){existing.antenna_height_m=height;existing.height_override=true;continue;}const manual=manualRouters.find(router=>router.id===siteId);if(manual){manual.antenna_height_m=height;manual.policy='required';continue;}const site=(result?.route||[]).find(item=>item.id===siteId);if(site)manualRouters.push({id:'M-'+randomId(),latitude:site.latitude,longitude:site.longitude,antenna_height_m:height,policy:'required'});}coverageHeightOverrides={};renderManualRouters();renderMeshcoreRouters();recordEdit();invalidate();scheduleCoverageCheck();$('coverage-compare-summary').textContent='Scenario heights are now explicit plan edits. Required proposed sites were pinned; use Undo to restore the previous plan.';}
async function startCoverageHeightScenario(){const overrides={};for(const input of document.querySelectorAll('[data-height-source]')){const value=Number(input.value),original=Number(input.dataset.originalHeight);if(!Number.isFinite(value)||value<.1||value>500){$('coverage-compare-summary').textContent='Scenario heights must be between 0.1 and 500 m AGL.';return;}if(Math.abs(value-original)>.0001)overrides[input.dataset.heightSource]=value;}if(!Object.keys(overrides).length){$('coverage-compare-summary').textContent='Change at least one router height to create a scenario.';return;}coverageHeightOverrides=overrides;invalidateCoverageEstimate('Height scenario changed.');$('coverage-compare-summary').textContent='Confirm once to calculate the height scenario.';setWorkspace('workspace-coverage');await requestCoverageAction({sourceHeightOverrides:overrides});}
const routeRender=render;render=(...args)=>{routeRender(...args);renderCoverageSources();renderFailureControls();renderHeightScenarioControls();renderCoverageContext();};
function coveragePayload(settings=coverageSettings,sourceHeightOverrides={}){if(!result||resultStale||!result.found||!result.active_alternative_id)throw Error('Find a certified route before calculating mesh coverage.');if(settings.area_mode==='drawn'&&!settings.area_polygon_wgs84)throw Error('Draw and finish a polygon coverage area first.');const bounds=map.getBounds();return {snapshot_version:result.snapshot_version,alternative_id:result.active_alternative_id,settings,source_height_overrides:sourceHeightOverrides,map_bounds:[bounds.getSouth(),bounds.getWest(),bounds.getNorth(),bounds.getEast()]};}
function setCoverageStatus(text,error=false){syncCoverageLegend();$('coverage-job-status').textContent=text;$('coverage-job-status').title=text;$('coverage-job-status').className=error?'action-status error':'action-status';if(error||/^(Estimating|Estimate ready|Approximate preview|Settings changed|Map extent changed|Coverage calculation complete|Coverage calculation cancelled|Coverage calculation failed|Coverage operation cancelled|Identical saved coverage reused|\d+ covered of )/.test(text))$('coverage-announcement').textContent=text;}
function renderCoverageContext(){
  const sources=(result?.route||[]).filter(site=>!['endpoint_a','endpoint_b','client'].includes(site.kind)),selected=(coverageSettings.source_ids.length||sources.length)+(coverageSettings.include_endpoints?2:0),display=coverageDisplayInfo||coverageJob;
  const searchStatus=result?.search_complete?'search complete':result?.found?`search stopped early · ${result.achieved_path_count}/${result.requested_path_count} paths`:'search not complete';
  const current=result?`${resultStale?'Stale current route':'Current route'} · ${result.active_alternative_id||'selected mesh'} · ${sources.length} repeater source(s)${coverageSettings.include_endpoints?' + endpoints':''} · ${searchStatus}`:'Find a certified route to choose a mesh.';
  const overrides=Object.keys(display?.source_height_overrides||{}).length,shownState=display?.stale?'stale historical snapshot':display?.state==='cancelled'?'cancelled/partial snapshot':display?.preview?'approximate preview':'saved snapshot';
  const shown=display?`Displayed coverage · ${display.alternative_id||'saved mesh'} · ${display.job_id?.slice(0,8)||'preview'} · ${display.source_ids?.length||0} saved sources · ${display.effective_cell_size_m?.toFixed(0)||'—'} m cells${overrides?` · ${overrides} height override(s)`:''}${display.route_search_complete===false?` · selected route from stopped search (${display.achieved_path_count??'?'}/${display.requested_path_count??'?'} paths)`:''} · ${shownState}`:'No coverage layer loaded yet.';
  const client=display?.settings?.client||coverageSettings.client;
  $('coverage-context-route').textContent=`${current} · ${selected} source(s) selected. ${shown}`;
  $('coverage-context-radio').textContent=`${display?.settings?'Saved run':'Calculation'} client · ${client.height_agl_m} m AGL · ${client.tx_power_dbm} dBm TX · ${client.gain_dbi} dBi antenna · ${client.sensitivity_dbm} dBm sensitivity · ${client.miscellaneous_loss_db||0} dB other loss.`;
  $('coverage-endpoints-note').textContent=sources.length?`Include endpoints to add both route ends to the ${coverageSettings.source_ids.length?'selected':'available'} repeater sources.`:'This direct route has no repeater sites. Enable endpoints to evaluate access from the two selected sites.';
  updateCoverageActions();
}
function coverageReviewKey(payload){const normalized=clone(payload);normalized.settings={...normalized.settings,mode:'two_way'};if(normalized.settings.area_mode!=='view')delete normalized.map_bounds;return JSON.stringify(normalized);}
function coverageEstimateIsCurrent(approved=coverageEstimate){if(!approved)return false;try{return approved.key===coverageReviewKey(coveragePayload(coverageSettings,approved.payload.source_height_overrides));}catch{return false;}}
function invalidateChangedCoverageExtent(){if(coverageSettings.area_mode==='view'&&coverageEstimate&&!coverageEstimateIsCurrent())invalidateCoverageEstimate('Map extent changed. Calculate again to use this new area.');updateCoverageTerrainAction();}
function coverageRouteReady(){return !!(result&&!resultStale&&result.found&&result.active_alternative_id);}
function updateCoverageActions(){const ready=coverageRouteReady(),locked=busy||routePreflightBusy||coverageBusy||coveragePreflightBusy||coverageLoadingRun||switchingProject;$('coverage-no-route').hidden=ready;$('coverage-start').disabled=!ready||locked;$('coverage-preview').disabled=!ready||locked;$('coverage-inspect').disabled=!ready||locked;updateCoverageTerrainAction(locked);}

function coverageTerrainKey(payload){
  return JSON.stringify([projectInfo?.active_id,coverageReviewKey(payload)]);
}
function currentCoverageTerrainRequest(){
  try{const payload=coveragePayload();return {payload,key:coverageTerrainKey(payload)};}catch{return null;}
}
function setCoverageTerrainStatus(message,error=false){
  const status=$('coverage-terrain-status');
  status.textContent=message;status.hidden=!message;status.className=error?'hint error':'hint';
}
function noteCoverageTerrainGap(){
  coverageTerrainUnknownKey=currentCoverageTerrainRequest()?.key;
  updateCoverageTerrainAction();
}
function updateCoverageTerrainAction(locked=$('coverage-start').disabled){
  const button=$('coverage-terrain-download');
  const current=currentCoverageTerrainRequest();
  button.hidden=!current||!!coverageTerrainJobId||!(
    coverageTerrainUnknownKey===current.key
    || (coverageTerrainCheck?.key===current.key&&coverageTerrainCheck.estimate?.tile_count>0)
  );
  const estimate=coverageTerrainCheck?.key===current?.key?coverageTerrainCheck?.estimate:null;
  button.textContent=terrainFromCache(estimate)?cachedTerrainAction(estimate):'Download terrain for coverage area';
  button.disabled=locked;
  if(!current||locked||$('workspace-coverage').hidden||coverageTerrainCheck?.key===current.key)return;
  coverageTerrainCheck=current;
  setTimeout(async()=>{
    if(coverageTerrainCheck!==current||currentCoverageTerrainRequest()?.key!==current.key)return;
    if($('coverage-start').disabled){coverageTerrainCheck=null;return;}
    try{
      current.estimate=await api('/api/coverage/terrain/estimate',{
        method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify(current.payload)
      });
    }catch{
      // The download action reports errors when requested explicitly.
    }
    if(coverageTerrainCheck===current)updateCoverageTerrainAction();
  },250);
}
async function requestCoverageTerrain(){
  if($('coverage-terrain-download').disabled||!validateMeshBuffer())return;
  readCoverageInputs();
  const current=currentCoverageTerrainRequest();
  if(!current)return;
  coveragePreflightBusy=true;syncCoverageLocks();
  setCoverageTerrainStatus('Estimating missing terrain for the selected area…');
  try{
    await autosaveNow();
    const request={method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify(current.payload)};
    const estimate=await api('/api/coverage/terrain/estimate',request);
    if(currentCoverageTerrainRequest()?.key!==current.key)throw Error('The coverage area changed. Review a new terrain estimate.');
    if(!estimate.tile_count){
      setCoverageTerrainStatus('Installed terrain tiles already span this area. Any remaining gaps can be caused by missing elevation values or paths outside the area.');
      return;
    }
    const copy = coverageTerrainEstimateCopy(estimate);
    const action = terrainFromCache(estimate) ? cachedTerrainAction(estimate) : 'Download terrain';
    const confirmed=await askActionDialog({title:action+' for coverage area?',copy,confirm:action,terrainEstimate:estimate});
    if(!confirmed){setCoverageTerrainStatus('');return;}
    if(currentCoverageTerrainRequest()?.key!==current.key)throw Error('The coverage area changed. Review a new terrain estimate.');
    const job=await api('/api/coverage/terrain/prepare',request);
    if(!job.job_id){setCoverageTerrainStatus(job.stage);return;}
    coverageTerrainJobId=job.job_id;
    if(interactionMode==='inspect-coverage')setInteractionMode('browse');
    renderCoverageTerrainProgress(job);
    void pollCoverageTerrain();
  }catch(error){setCoverageTerrainStatus(error.message,true);}
  finally{if(!coverageTerrainJobId){coveragePreflightBusy=false;syncCoverageLocks();}}
}
function renderCoverageTerrainProgress(job){
  const progress=$('coverage-terrain-progress');
  progress.hidden=false;progress.max=job.total;progress.value=job.done;
  $('coverage-terrain-cancel').hidden=false;
  $('coverage-terrain-reconnect').hidden=true;
  setCoverageTerrainStatus(`${job.stage} · ${job.done}/${job.total} product tiles`);
  updateCoverageTerrainAction();
}
async function pollCoverageTerrain(){
  const jobId=coverageTerrainJobId;
  if(!jobId)return;
  $('coverage-terrain-reconnect').hidden=true;
  try{
    const {coverage_terrain_job:job}=await refresh();
    if(coverageTerrainJobId!==jobId)return;
    if(job?.job_id===jobId&&['queued','preparing'].includes(job.state)){
      renderCoverageTerrainProgress(job);setTimeout(pollCoverageTerrain,800);return;
    }
    coverageTerrainJobId=null;coveragePreflightBusy=false;
    $('coverage-terrain-progress').hidden=true;$('coverage-terrain-cancel').hidden=true;
    $('coverage-terrain-reconnect').hidden=true;
    if(job?.job_id===jobId&&job.state==='complete'){
      invalidateCoverageEstimate('');
      if(coverageDisplayInfo){coverageDisplayInfo={...coverageDisplayInfo,stale:true,route_current:false};coverageStale=true;renderCoverageLegend();}
      inspectionLayer.clearLayers();$('coverage-inspection').hidden=true;
      renderCoverageContext();renderFailureControls();
      setCoverageTerrainStatus('Coverage terrain is ready. Live coverage updates automatically; calculate saved runs again to use it.');
      void updateTerrainCoverage();
      void refreshCoverageRunSelectors().catch(()=>{});
    }else{
      setCoverageTerrainStatus(job?.job_id===jobId?job.stage:'The terrain download is no longer active.',job?.state!=='cancelled');
    }
    syncCoverageLocks();
  }catch(error){
    $('coverage-terrain-reconnect').hidden=false;
    setCoverageTerrainStatus(`${error.message}. Reconnect to check the terrain download.`,true);
  }
}
async function cancelCoverageTerrain(){
  const jobId=coverageTerrainJobId;
  if(!jobId)return;
  try{
    await api(`/api/coverage/terrain/${encodeURIComponent(jobId)}/cancel`,{method:'POST'});
    if(jobId===coverageTerrainJobId)setCoverageTerrainStatus('Cancelling coverage terrain preparation…');
  }catch(error){if(jobId===coverageTerrainJobId)setCoverageTerrainStatus(error.message,true);}
}
function invalidateCoverageEstimate(message='Coverage inputs changed. Calculate again to use the new settings.'){
  coverageRevision++;coverageEstimateRequest++;coverageEstimate=null;
  coverageTerrainCheck=null;coverageTerrainUnknownKey=null;
  updateCoverageActions();
  if(message&&$('coverage-job-status'))setCoverageStatus(message);
}
function coverageSettingsChanged(event){if(!validateMeshBuffer())return;readCoverageInputs();const modeOnly=event?.target?.id==='coverage-mode';if(!modeOnly&&interactionMode==='inspect-coverage')setInteractionMode('browse');inspectionRequest++;inspectionController?.abort();cancelActiveInspection();inspectionLayer.clearLayers();$('coverage-inspection').hidden=true;if(!modeOnly)coverageStale=true;coverageDiffLayer.clearLayers();coverageComparison=null;syncCoverageAreaControls();renderCoverageArea();if(modeOnly){redrawCoverageCells();}else{invalidateCoverageEstimate();coverageCells=[];coveragePreviewCells=[];coverageCursor=0;coverageDisplayInfo=null;coveragePreviewActive=false;predictedCoverageLayer.clearLayers();}recordEdit();renderCoverageContext();if(!modeOnly)setCoverageStatus('Settings changed. Calculate again when ready.');else setCoverageStatus('Coverage display mode updated; no RF work was started.');}
function coverageEstimateSummary(approved){
  const {payload,estimate}=approved,settings=payload.settings,sources=estimate.source_count||estimate.source_ids?.length||0;
  const sourceLimit=Math.min(settings.maximum_sources,64),evaluationLimit=Math.min(settings.maximum_evaluations,250000),cellLimit=Math.min(settings.maximum_cells,16384,Math.floor(evaluationLimit/Math.max(1,sources))),sampleLimit=Math.min(settings.maximum_profile_samples,16384);
  const mode={mesh:'selected mesh + buffer',view:'current map extent',drawn:'drawn polygon'}[settings.area_mode]||settings.area_mode;
  const terrainGuidance=estimate.unknown_cells?' You can download terrain for the coverage area before calculating.':'';
  const surfaceUse=estimate.surface_available
    ? rf.obstacle_surface==='surface'
      ? 'Diffraction uses surface elevations where available. Surface data also supports enabled terminal clutter.'
      : 'Diffraction uses ground elevations. Surface data supports enabled terminal clutter.'
    : 'Surface data is unavailable, so the prediction uses ground elevations.';
  const routeSearchNote=estimate.route_search_complete===false?` Coverage describes this certified route only; the route search stopped with ${estimate.achieved_path_count}/${estimate.requested_path_count} requested independent paths, so unexplored alternatives are not included.`:'';
  return `Area: ${mode}. ${sources} source(s) · ${estimate.requested_area_cells.toLocaleString()} in-area cells at ${estimate.effective_cell_size_m.toFixed(0)} m · ${estimate.outside_area_cells.toLocaleString()} outside · ${estimate.planned_evaluations.toLocaleString()} planned source evaluations. Terrain known at ${estimate.terrain_available_cells.toLocaleString()} cells; ${estimate.unknown_cells.toLocaleString()} cells have unknown terrain. Limits: ${sourceLimit} sources, ${cellLimit.toLocaleString()} cells / ${evaluationLimit.toLocaleString()} evaluations, ${sampleLimit.toLocaleString()} profile samples per path · ${settings.profile_step_m} m profile step. The sampling allowance accounts for the profile step when it is coarser than the native terrain. ${surfaceUse}${terrainGuidance}${Object.keys(payload.source_height_overrides||{}).length?` ${Object.keys(payload.source_height_overrides).length} changed router height(s) and affected certified backbone links will be revalidated.`:''}${routeSearchNote}`;
}
async function estimateCoverage(sourceHeightOverrides){
  readCoverageInputs();coverageEstimate=null;updateCoverageActions();const payload=clone(coveragePayload(coverageSettings,sourceHeightOverrides)),key=coverageReviewKey(payload),request=++coverageEstimateRequest,revision=coverageRevision;
  const approved={key,payload,revision};
  await autosaveNow();
  if(request!==coverageEstimateRequest||revision!==coverageRevision||!coverageEstimateIsCurrent(approved))throw Error('Coverage settings changed while preparing the estimate. Calculate again to use the current inputs.');
  const estimate=await api('/api/coverage/estimate',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify(payload)});
  if(request!==coverageEstimateRequest||revision!==coverageRevision||!coverageEstimateIsCurrent(approved))throw Error('Coverage settings changed while estimating. Calculate again to use the current inputs.');
  coverageEstimate={...approved,estimate};updateCoverageActions();
  if(estimate.unknown_cells)noteCoverageTerrainGap();
  setCoverageStatus('Estimate ready. Confirm once to start the requested coverage operation.');return coverageEstimate;
}
async function requestCoverageAction({preview=false,sourceHeightOverrides}={}){
  if(busy||routePreflightBusy||coverageBusy||coveragePreflightBusy||coverageLoadingRun||switchingProject)return;
  if(!coverageRouteReady()){setCoverageStatus('Find a certified route before calculating coverage.',true);return;}
  if(!validateMeshBuffer())return;
  const hasRepeater=result.route.some(site=>!['endpoint_a','endpoint_b','client'].includes(site.kind));
  if(!hasRepeater&&!coverageSettings.include_endpoints){
    setCoverageSection('runs');
    $('coverage-endpoints').focus();
    setCoverageStatus('This route connects the endpoints directly. Include endpoints as sources to calculate coverage.',true);
    return;
  }
  coveragePreflightBusy=true;syncCoverageLocks();setCoverageStatus('Estimating the selected area and calculation size…');
  try{
    const approved=await estimateCoverage(sourceHeightOverrides),summary=coverageEstimateSummary(approved);
    const copy=preview?`Bounded approximate preview for the same area and sources. Full-request estimate: ${summary} The preview is approximate and is not a saved full result.`:summary;
    const confirmed=await askActionDialog({title:preview?'Run a quick coverage preview?':'Calculate mesh coverage?',copy,confirm:preview?'Run preview':'Calculate coverage'});
    if(!confirmed){setCoverageStatus('Coverage operation cancelled; no calculation was started.');return;}
    if(approved!==coverageEstimate||approved.revision!==coverageRevision||!coverageEstimateIsCurrent())throw Error('Coverage inputs changed after the estimate. Calculate again to approve the current settings.');
    coveragePreflightBusy=false;syncCoverageLocks();
    if(preview)await previewCoverage(approved);else await startCoverage(approved);
  }catch(error){setCoverageStatus(error.message,true);}
  finally{if(coveragePreflightBusy){coveragePreflightBusy=false;syncCoverageLocks();}}
}
async function previewCoverage(approved=coverageEstimate){
  if(!approved||approved!==coverageEstimate||!coverageEstimateIsCurrent()||approved.revision!==coverageRevision){setCoverageStatus('Coverage inputs changed. Start a fresh preview to approve the current settings.',true);return;}
  const {payload,revision}=approved;coverageBusyState(true);setCoverageStatus('Calculating a bounded approximate preview…');
  try{
    await autosaveNow();const preview=await api('/api/coverage/preview',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify(payload)});
    if(revision!==coverageRevision||!coverageEstimateIsCurrent(approved))return;
    if(preview.unknown_cells)noteCoverageTerrainGap();
    coverageCells=[];coverageCursor=0;coverageDisplayInfo=preview;coveragePreviewCells=preview.cells;coveragePreviewActive=true;coverageStale=false;predictedCoverageLayer.clearLayers();if(!map.hasLayer(predictedCoverageLayer))predictedCoverageLayer.addTo(map);addCoverageRasterCells(preview.cells);renderCoverageContext();setCoverageStatus(`Approximate preview · ${preview.requested_cells} cells at ${preview.effective_cell_size_m.toFixed(0)} m · ${preview.covered_cells} covered · ${preview.unknown_cells} unknown · ${preview.unresolved_cells||0} unresolved · ${preview.elapsed_seconds.toFixed(2)} s. This is not the full saved result.`);
  }catch(error){setCoverageStatus(error.message,true);}finally{coverageBusyState(false);updateCoverageActions();}
}
function coverageBusyState(value){coverageBusy=value;if(value&&interactionMode==='inspect-coverage')setInteractionMode('browse');if(value)cancelActiveInspection();syncCoverageLocks();}
function syncAlternativeControls(){
  const locked=busy||coverageBusy||coveragePreflightBusy||coverageLoadingRun||switchingProject||alternativeSelecting||resultStale;
  const selector=$('alternative-select');
  syncAlternativeRows();
  document.querySelector('.alternative-table')?.classList.toggle('locked',locked);
  selector.disabled=locked;
}
function syncCoverageLocks(){
  const locked=busy||routePreflightBusy||coverageBusy||coveragePreflightBusy||coverageLoadingRun||switchingProject;
  for(const el of document.querySelectorAll('aside input:not([data-display-only]), aside select:not([data-display-only]), [data-place], [data-clear], [data-height-preset], #open, #import-plan, #load-meshcore, #include-all, #include-none, #estimate-terrain, #prepare-terrain, #add-router, #add-router-coordinate, #add-target-coordinate, #project-select, #new-project, #rename-project, #recover-project, #duplicate-project, #archive-project, #delete-project, #retry-save, #optimize, #coverage-preview, #coverage-start, #coverage-inspect, #coverage-draw-area, #coverage-finish-area, #coverage-clear-area, #coverage-height-run, #coverage-height-apply, #coverage-compare-run, #failure-run, #failure-reset, #coverage-add-target, #coverage-load-run'))el.disabled=locked;
  $('coverage-cancel').disabled=busy;$('coverage-assess-targets').disabled=!coverageTargets.length||locked;$('coverage-mode').disabled=switchingProject;$('coverage-opacity').disabled=switchingProject;syncAlternativeControls();
  document.querySelectorAll('[data-workspace-target]').forEach(button=>button.disabled=switchingProject);Object.values(markers).forEach(marker=>locked?marker.dragging.disable():marker.dragging.enable());
  for(const layer of routeLayer.getLayers())if(layer.dragging){if(locked||resultStale||!layer._plannerDraggable)layer.dragging.disable();else layer.dragging.enable();}
  for(const layer of manualLayer.getLayers())if(layer.dragging)locked?layer.dragging.disable():layer.dragging.enable();
  syncMapEditControls();
  $('coverage-load-run').disabled=!$('coverage-active-run').value||locked;$('coverage-reconnect').disabled=!coverageBusy||busy||switchingProject;setHistoryButtons();updateCoverageActions();
}
function appendCoverageCells(cells){if(coveragePreviewActive){predictedCoverageLayer.clearLayers();coveragePreviewActive=false;coveragePreviewCells=[];}coverageDisplayInfo=coverageJob;coverageCells.push(...cells);addCoverageRasterCells(cells);syncCoverageLegend();}
function setCoverageOpacity(value){
  const opacity=Number(value)/100;
  $('coverage-opacity-value').value=`${value}%`;
  $('coverage-opacity').setAttribute('aria-valuetext',`${value}%`);
  for(const group of [predictedCoverageLayer,coverageDiffLayer])group.getLayers()[0]?.setOpacity(opacity);
  if(!opacity)hideCoverageSample();
}
function setCoverageResultProgress(loaded,total,visible=true){const bar=$('coverage-result-progress');bar.hidden=!visible||!total;if(total){bar.max=total;bar.value=Math.min(loaded,total);}else bar.removeAttribute('value');$('coverage-result-progress-copy').hidden=!visible;$('coverage-result-progress-copy').textContent=total?`Loading saved cells · ${loaded.toLocaleString()} of ${total.toLocaleString()}`:'Waiting for stored coverage cells…';}
async function pollCoverage(jobId,request=++coveragePollRequest){
  try{
    const job=await api(`/api/coverage/jobs/${encodeURIComponent(jobId)}`);if(request!==coveragePollRequest)return;coverageJob=job;if(!coveragePreviewActive)coverageDisplayInfo=job;
    let page=await api(`/api/coverage/jobs/${encodeURIComponent(jobId)}/cells?cursor=${coverageCursor}&limit=192`);if(request!==coveragePollRequest)return;
    if(page.cells.length){appendCoverageCells(page.cells);coverageCursor=page.next_cursor??coverageCursor+page.cells.length;}
    const running=['queued','running'].includes(job.state),stored=page.stored_cells||job.stored_cells||0,total=page.requested_cells||job.requested_cells||0;
    setCoverageResultProgress(coverageCursor,stored,stored>0||!running);renderCoverageContext();
    if(running){setCoverageStatus(`${job.stage} · ${job.done||0}/${job.total||job.requested_cells||'?'} grid cells computed · ${coverageCursor.toLocaleString()} saved cells loaded so far${job.stale?' · STALE':''}`,!!job.stale);$('coverage-reconnect').hidden=true;setTimeout(()=>void pollCoverage(jobId,request),800);return;}
    while(page.next_cursor!==null&&page.next_cursor!==undefined){setCoverageStatus(`Calculation ${job.state}; loading saved cells · ${coverageCursor.toLocaleString()} of ${stored.toLocaleString()}.`);page=await api(`/api/coverage/jobs/${encodeURIComponent(jobId)}/cells?cursor=${coverageCursor}&limit=192`);if(request!==coveragePollRequest)return;if(page.cells.length){appendCoverageCells(page.cells);coverageCursor=page.next_cursor??coverageCursor+page.cells.length;}setCoverageResultProgress(coverageCursor,page.stored_cells||stored,true);}
    setCoverageResultProgress(0,0,false);
    coverageBusyState(false);$('coverage-cancel').hidden=true;$('coverage-reconnect').hidden=true;coverageStale=!!job.stale||job.state==='cancelled';renderCoverageLegend();renderFailureControls();renderHeightScenarioControls();renderCoverageContext();
    if(!job.stale&&job.unknown_cells)noteCoverageTerrainGap();
    if(job.state==='complete'){const fullyLoaded=coverageCursor===total;setCoverageStatus(`${coverageRunReused?'Identical saved coverage reused; no RF recalculation was needed. ':''}${job.covered_cells||0} covered of ${job.evaluated_cells||0} evaluated in-area cells · ${job.unknown_cells||0} unknown · ${job.unresolved_cells||0} unresolved profiles · ${job.outside_area_cells||0} outside · ${fullyLoaded?'all requested samples loaded':`partial storage · ${coverageCursor.toLocaleString()} of ${total.toLocaleString()} requested samples loaded`} · ${job.source_ids?.length||0} sources · ${job.effective_cell_size_m?.toFixed(0)||'—'} m cells.${job.stale?' This is a stale historical result.':''}`,!!job.stale||!fullyLoaded);}
    else if(job.state==='cancelled')setCoverageStatus(`Coverage calculation cancelled; ${coverageCursor.toLocaleString()} of ${total.toLocaleString()} requested samples are available.${job.stale?' This historical result is stale.':''}`,true);
    else if(job.state==='failed')setCoverageStatus(job.stage||'Coverage calculation failed.',true);
    else setCoverageStatus(`${job.stage||`Coverage job ${job.state}`} · ${coverageCursor.toLocaleString()} of ${total.toLocaleString()} samples loaded.`,job.state!=='complete');
    await refreshCoverageRunSelectors();
  }catch(error){
    if(request!==coveragePollRequest)return;
    setCoverageResultProgress(0,0,false);
    $('coverage-reconnect').hidden=false;setCoverageStatus(`${error.message}. The saved result may still be available; reconnect to resume status and cell loading.`,true);
  }
}
async function startCoverage(approved=coverageEstimate){
  if(!approved||approved!==coverageEstimate||!coverageEstimateIsCurrent()||approved.revision!==coverageRevision){setCoverageStatus('Coverage inputs changed. Start a fresh calculation to approve the current settings.',true);return;}
  const {payload,revision}=approved,keepPreview=coveragePreviewActive;coveragePollRequest++;coverageBusyState(true);
  try{
    await autosaveNow();if(revision!==coverageRevision||!coverageEstimateIsCurrent(approved))throw Error('Coverage settings changed after the estimate. Calculate again to approve the current settings.');
    coverageCells=[];coverageCursor=0;coverageStale=false;coverageRunReused=false;coverageDiffLayer.clearLayers();coverageComparison=null;
    if(!keepPreview){coverageDisplayInfo=null;predictedCoverageLayer.clearLayers();}if(!map.hasLayer(predictedCoverageLayer))predictedCoverageLayer.addTo(map);
    const job=await api('/api/coverage/jobs',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify(payload)});
    coverageJob=job;if(!keepPreview)coverageDisplayInfo=job;coverageRunReused=!!job.reused;$('coverage-cancel').hidden=false;setCoverageStatus(`${keepPreview?'Approximate preview shown while full calculation runs. ':''}${job.stage} · waiting for bounded planner capacity`);renderCoverageContext();void pollCoverage(job.job_id);
  }catch(error){coverageBusyState(false);setCoverageStatus(error.message,true);}
}
function cancelCoverage(){if(!coverageJob)return;api(`/api/coverage/jobs/${encodeURIComponent(coverageJob.job_id)}/cancel`,{method:'POST'}).then(()=>setCoverageStatus('Cancelling coverage; completed cells will remain available.')).catch(e=>setCoverageStatus(e.message,true));}
let saveRevision=0,savedRevision=0,saveQueue=Promise.resolve(),saveState='saved';
function setProjectSaveState(message,state){
  saveState=state;
  const conflict=state==='conflict', button=$('retry-save');
  $('project-storage').textContent=message;
  $('project-storage').dataset.saveState=conflict?'failed':state;
  button.hidden=state!=='failed'&&!conflict;
  button.textContent=conflict?'Reload project':'Retry save';
}
function showProjectSaved(){setProjectSaveState(`Saved · ${formatBytes(projectInfo.storage_usage_bytes||0)} in this workspace`,'saved');}
function renderProjects(data){if(!data)return;projectInfo=data;const selector=$('project-select'),items=data.items||[];selector.replaceChildren();for(const item of items){const option=document.createElement('option');option.value=item.id;option.textContent=item.name;selector.append(option);}selector.value=data.active_id||'';if(saveState!=='failed'&&saveState!=='conflict'&&saveRevision===savedRevision)setProjectSaveState(`Saved · ${formatBytes(data.storage_usage_bytes||0)} in this workspace`,'saved');$('recover-project').hidden=!data.has_previous_revision;renderArchivedProjects(data.archived_items||[]);updatePlanReadiness();}
function renderArchivedProjects(items){const list=$('archived-project-list');if(!list)return;list.replaceChildren();$('archived-project-count').textContent=String(items.length);if(!items.length){const empty=document.createElement('p');empty.className='hint';empty.textContent='No archived projects.';list.append(empty);return;}for(const item of items){const row=document.createElement('div'),name=document.createElement('span'),restore=document.createElement('button');row.className='archived-project-item';name.textContent=`${item.name} · archived ${new Date(item.updated_at*1000).toLocaleDateString()}`;restore.type='button';restore.textContent='Restore';restore.onclick=()=>void restoreProject(item);row.append(name,restore);list.append(row);}}
function formatBytes(value){if(value<1024)return `${value} B`;const units=['KiB','MiB','GiB'];let size=value/1024,index=0;while(size>=1024&&index<units.length-1){size/=1024;index++;}return `${size.toFixed(1)} ${units[index]}`;}
function scheduleAutosave(){
  const revision=++saveRevision;
  clearTimeout(autosaveTimer);
  if(saveState==='conflict')return;
  setProjectSaveState('Saving changes…','saving');
  autosaveTimer=setTimeout(()=>void autosaveNow(revision).catch(()=>{}),500);
}
function retryAutosaveAfterJob(projectId,revision){
  clearTimeout(autosaveTimer);
  autosaveTimer=setTimeout(async()=>{
    const pending=()=>projectInfo?.active_id===projectId&&saveRevision===revision&&savedRevision<revision;
    if(!pending())return;
    try{
      const {job,coverage_job,coverage_terrain_job}=await api('/api/state');
      if(!pending())return;
      if([job,coverage_job,coverage_terrain_job].some(item=>['queued','running','preparing'].includes(item?.state))){
        retryAutosaveAfterJob(projectId,revision);
        return;
      }
    }catch(error){
      if(pending())setProjectSaveState(`Save failed · ${error.message}`,'failed');
      return;
    }
    await autosaveNow(revision).catch(()=>{});
  },1000);
}
async function autosaveNow(revision=saveRevision){
  clearTimeout(autosaveTimer);
  if(!projectInfo)return;
  const projectId=projectInfo.active_id,current=()=>projectInfo?.active_id===projectId&&revision===saveRevision;
  if(saveState!=='conflict')setProjectSaveState('Saving changes…','saving');
  let saved;
  try{
    const snapshot=clone(plan());
    const request=saveQueue.then(()=>api('/api/projects/autosave',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({project_id:projectId,plan:snapshot})}));
    saveQueue=request.catch(()=>{});
    saved=await request;
  }catch(error){
    if(current()){
      if(error.status===409&&error.message==='Plan inputs are locked while a job is active'){
        if(savedRevision<revision){
          setProjectSaveState('Changes will save when the job finishes','saving');
          retryAutosaveAfterJob(projectId,revision);
        }else showProjectSaved();
      }else if(!error.projectConflict)setProjectSaveState(`Save failed · ${error.message}`,'failed');
    }
    throw error;
  }
  if(!current())return saved;
  projectInfo.has_previous_revision=saved.has_previous_revision;savedRevision=revision;$('recover-project').hidden=!saved.has_previous_revision;
  showProjectSaved();
  return saved;
}
async function flushAutosave(){
  await saveQueue;
  if(savedRevision<saveRevision)await autosaveNow();
}
$('retry-save').onclick=async()=>{
  if(saveState!=='conflict'){
    await autosaveNow().catch(()=>{});
    return;
  }
  if(savedRevision<saveRevision&&!await askActionDialog({title:'Reload project?',copy:'Reloading discards your unsaved edits on this page.',confirm:'Reload project'}))return;
  location.reload();
};
async function refresh(){const data=await api('/api/state');if(data.projects.shared){
  if(!loadedSharedProject)loadedSharedProject={id:data.projects.active_id,revision:data.projects.plan_revision};
  if(loadedSharedProject.id!==data.projects.active_id){
    const message='The active project changed. Reload the project.';
    showProjectConflict(message);
    const error=Error(message);error.projectConflict=true;throw error;
  }
}terrainLoaded=data.terrain.dtm.length>0;coverageTerrainTileCounts={dtm:data.coverage_terrain?.dtm.length||0,dom:data.coverage_terrain?.dom.length||0};coverageTerrainLoaded=!!(coverageTerrainTileCounts.dtm||coverageTerrainTileCounts.dom);for(const kind of ['dtm','dom']){const count=$(kind+'-count');count.dataset.tileCount=data.terrain[kind].length;count.textContent=data.terrain[kind].length+' '+kind.toUpperCase()+' tiles';}$('surface-note').textContent=data.terrain.dom.length?'Surface terrain loaded — includes obstruction geometry.':'Surface obstruction data unavailable — terrain only.';if(!terrainLoaded){coverageLayer.clearLayers();$('coverage-note').textContent='Upload or prepare ground terrain to see coverage and check sites.';}$('coverage-note').className='hint';const cache=data.rf_cache||{};$('cache-stats').textContent=`RF cache · ${cache.entries||0} saved · ${cache.hits||0} reused · ${cache.misses||0} computed · ${cache.evictions||0} evicted`;renderProjects(data.projects);return data;}
function setBusy(value){busy=value;$('optimize').disabled=value||routePreflightBusy;$('cancel').hidden=!value;$('cancel').textContent=activeJobKind==='terrain'?'Cancel terrain preparation':'Cancel optimization';$('stop-keep').hidden=!value||activeJobKind!=='route';$('stop-keep').disabled=!value||lastSnapshotVersion===0;$('clear-cache').disabled=value||routePreflightBusy;document.querySelectorAll('aside input:not([data-display-only]), aside select:not([data-display-only]), [data-place], [data-clear], #open, #import-plan, #load-meshcore, #include-all, #include-none, #estimate-terrain, #prepare-terrain, #add-router, #add-router-coordinate, #add-target-coordinate, #project-select, #new-project, #rename-project, #recover-project, #duplicate-project, #archive-project, #delete-project').forEach(el=>el.disabled=value);Object.values(markers).forEach(marker=>value?marker.dragging.disable():marker.dragging.enable());for(const layer of routeLayer.getLayers())if(layer.dragging){if(value||resultStale||!layer._plannerDraggable)layer.dragging.disable();else layer.dragging.enable();}renderMeshcoreRouters();renderManualRouters();$('alternative-select').disabled=value||resultStale;setHistoryButtons();syncCoverageLocks();}
function formatElapsed(){const seconds=Math.max(0,Math.floor((Date.now()-(jobStartedAt||Date.now()))/1000));return seconds<60?`${seconds} s`:`${Math.floor(seconds/60)} min ${String(seconds%60).padStart(2,'0')} s`;}
function updateJobCard({kind='route',state='running',phase='',message='',progress=null,retry=false}){
  const card=$('route-job-card'),bar=$('progress'),active=['queued','running','preparing'].includes(state);card.hidden=false;card.dataset.state=state;
  $('job-phase').textContent=phase||`${kind==='terrain'?'Terrain preparation':'Route search'} · ${state}`;$('job-message').textContent=message;$('job-elapsed').textContent=active?`Elapsed · ${formatElapsed()}`:'';$('job-elapsed').hidden=!active;$('dismiss-job').hidden=active;
  $('job-progress-copy').textContent=progress?.known?`${progress.done.toLocaleString()} of ${progress.total.toLocaleString()} ${progress.unit||'steps'}`:active?'Progress details are not reported for this phase.':'';
  if(progress?.known){bar.max=progress.total;bar.value=Math.min(progress.done,progress.total);bar.hidden=!active;}else{bar.removeAttribute('value');bar.hidden=!active;}
  if(!active&&!retry&&!['failed','disconnected'].includes(state)&&$('workspace').dataset.mainView==='view-analysis')card.hidden=true;
  $('cancel').hidden=!active;$('cancel').textContent=kind==='terrain'?'Cancel terrain preparation':'Cancel optimization';$('stop-keep').hidden=!active||kind!=='route';$('retry-job').hidden=!retry;$('view-results').hidden=active||kind!=='route'||!result?.links?.length;
}
$('view-results').onclick=()=>{setMainView('view-analysis');if(!['queued','running','preparing'].includes($('route-job-card').dataset.state))$('route-job-card').hidden=true;};
$('dismiss-job').onclick=()=>{$('route-job-card').hidden=true;};
let jobCardHideTimer=null;new MutationObserver(()=>{clearTimeout(jobCardHideTimer);const card=$('route-job-card');if(!card.hidden&&card.dataset.state==='complete')jobCardHideTimer=setTimeout(()=>{if(card.dataset.state==='complete')card.hidden=true;},15000);}).observe($('route-job-card'),{attributes:true,attributeFilter:['data-state','hidden']});
$('compare-open-coverage').onclick=()=>{setWorkspace('workspace-coverage');setCoverageSection('runs');const details=$('saved-run-settings');if(details)details.open=true;};
new MutationObserver(syncScenarioEmptyState).observe($('coverage-active-run'),{childList:true});
{const menu=document.querySelector('.project-menu');const close=()=>{menu.open=false;};menu.querySelector('.project-menu-actions').addEventListener('click',event=>{if(event.target.closest('button')&&!event.target.closest('#archived-projects'))close();});document.addEventListener('click',event=>{if(menu.open&&!menu.contains(event.target))close();});document.addEventListener('keydown',event=>{if(event.key==='Escape'&&menu.open){close();menu.querySelector('summary').focus();}});}
function searchScope(job){const scope=job.resolved_search||{};const known=scope.known_router_count||0;const generated=scope.generates_candidates===false?'no generated sites':'sites scaled to route length';return `${job.search_effort||searchEffort} · ${generated} + ${known} existing`;}
function showJobConnectionError(error){const bar=$('progress'),known=!bar.hidden&&bar.hasAttribute('value'),progress={known,done:known?bar.value:0,total:known?bar.max:0,unit:activeJobKind==='terrain'?'tiles':'steps'};updateJobCard({kind:activeJobKind||'route',state:'disconnected',phase:'Connection interrupted',message:`${error.message}. The calculation may still be running on the server. Reconnect to read its status; this will not start another calculation.`,progress,retry:true});$('job-progress-copy').textContent=known?`Last known progress · ${progress.done} of ${progress.total} ${progress.unit}`:'Progress resumes after reconnecting.';}
$('retry-job').onclick=()=>{if(activeJobKind==='terrain')void pollTerrainJob();else if(activeJobKind==='route')void poll();};
for(const kind of ['dtm','dom'])$(kind).onchange=async e=>{setBusy(true);try{for(const file of e.target.files){status('Uploading '+file.name+'…');const data=new FormData();data.append('file',file);const uploaded=await api('/api/terrain/'+kind,{method:'POST',body:data});map.fitBounds(uploaded.bounds);}invalidate();await refresh();await updateTerrainCoverage();status('Terrain uploaded. Coverage is shown on the map.');}catch(e){status(e.message,true);}finally{setBusy(false);$(kind).value='';}};
document.querySelectorAll('[data-clear]').forEach(button=>button.onclick=async()=>{
  const kind=button.dataset.clear,count=Number($(kind+'-count').dataset.tileCount||0);
  if(!count&&!coverageTerrainLoaded)return;
  const tiles=`${kind==='dtm'?'ground (DTM)':'surface (DOM)'} tile${count===1?'':'s'}`;
  const title=count?`Clear ${count} ${tiles}?`:'Clear supplemental coverage terrain?';
  const routeCopy=count?`This removes all ${count} route ${tiles} from this project, including downloaded and uploaded files.`:`The route terrain set has 0 ${tiles}.`;
  const coverageCopy=coverageTerrainLoaded?` This also clears the supplemental coverage terrain set (${coverageTerrainTileCounts.dtm} DTM, ${coverageTerrainTileCounts.dom} DOM).`:'';
  if(!await askActionDialog({title,copy:routeCopy+coverageCopy,confirm:'Clear tiles',danger:true}))return;
  try{await api('/api/terrain/'+kind,{method:'DELETE'});invalidate();await refresh();await updateTerrainCoverage();}catch(error){status(error.message,true);}
});
for(const id of ['terrain-resolution','terrain-auto','terrain-dom','terrain-source','terrain-fallback'])$(id).onchange=editTerrainControls;
function terrainFromCache(estimate){
  return estimate?.cache_total>0&&estimate.cache_hits===estimate.cache_total;
}
function cachedTerrainAction(estimate){
  return 'Use '+estimate.cache_hits+' cached terrain '+(estimate.cache_hits===1?'tile':'tiles');
}
function coverageTerrainEstimateCopy(estimate){
  return estimate.missing_area_km2.toLocaleString(undefined,{maximumFractionDigits:1})+' km² missing · '+estimate.summary+'. '+terrainEstimateCost(estimate);
}
function terrainEstimateCost(estimate){
  const missing=estimate.cache_total-estimate.cache_hits;
  const download=missing>0
    ? missing+' to fetch · about '+estimate.estimated_uncached_raw_mib.toLocaleString(undefined,{maximumFractionDigits:1})+' MiB uncompressed'
    : 'No download needed';
  return `${estimate.cache_hits}/${estimate.cache_total} tiles already cached · ${download} · about ${estimate.estimated_disk_mib.toFixed(0)} MiB for the workspace and cache · ${estimate.available_disk_mib.toFixed(0)} MiB free on the server.`;
}
function renderTerrainEstimateDetail(container,estimate){
  container.replaceChildren();
  if(estimate.sources?.length){
    const list=document.createElement('ul');
    list.className='terrain-estimate-sources';
    for(const source of estimate.sources){
      const product=source.product==='dom'?'Surface (DOM)':'Ground (DTM)';
      const tiles=`${source.tiles} ${source.tiles===1?'tile':'tiles'}`;
      const row=document.createElement('li');
      row.textContent=`${source.name} · ${product} · ${tiles} · ${source.resolution_m} m${source.surface_model?' · Surface model may include trees and buildings':''}`;
      if(source.attribution){
        const attribution=document.createElement('small');
        attribution.textContent=source.attribution;
        row.append(attribution);
      }
      list.append(row);
    }
    container.append(list);
  }
  const messages=[estimate.dom_unavailable_reason&&`Ground only. ${estimate.dom_unavailable_reason}`,...(estimate.warnings||[])];
  for(const message of messages.filter(Boolean)){
    const warning=document.createElement('p');
    warning.className='hint';
    warning.textContent=message;
    container.append(warning);
  }
  container.hidden=!container.childElementCount;
}
async function estimateTerrain(){
  if(mapEditsLocked()||!validTerrainResolution())return;
  const button=$('estimate-terrain');button.disabled=true;
  clearTerrainEstimateReview();
  try{
    const operation=createRouteOperation(),revision=dataSourcesRevision;
    void previewTerrainSources(operation.snapshot);
    const estimate=await api('/api/terrain/estimate',sourceJsonRequest('POST',operation.snapshot));
    if(revision!==dataSourcesRevision||!routeOperationIsCurrent(operation))return;
    terrainEstimateReview={estimate,operation};
    $('terrain-estimate-copy').textContent=`${estimate.summary}. ${terrainEstimateCost(estimate)}`;
    renderTerrainEstimateDetail($('terrain-estimate-detail'),estimate);
    setTerrainDomNote();
    $('terrain-estimate').hidden=false;$('prepare-terrain').hidden=false;
    $('prepare-terrain').textContent=terrainFromCache(estimate)?cachedTerrainAction(estimate):'Download terrain';
    status('Terrain estimate ready. Review it before starting.');
  }catch(error){status(error.message,true);}
  finally{button.disabled=mapEditsLocked();}
}
$('estimate-terrain').onclick=estimateTerrain;
async function startTerrainPreparation(operation=createRouteOperation()){
  if(busy||activeJobKind)throw Error('Another planner job is already active.');
  await flushAutosave();
  if(busy||activeJobKind||coverageBusy||coveragePreflightBusy||coverageLoadingRun||switchingProject)throw Error('Another planner operation is active.');
  if(!routeOperationIsCurrent(operation))throw Error('The plan changed before terrain preparation. Review the current settings and start again.');
  activeJobId=null;activeJobKind='terrain';jobStartedAt=Date.now();preparingTerrain=true;setBusy(true);
  updateJobCard({kind:'terrain',state:'queued',phase:'Submitting terrain preparation',message:'Sending the approved terrain request.'});$('cancel').hidden=true;
  try{
    const job=await api('/api/terrain/prepare',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify(operation.snapshot)});
    activeJobId=job.job_id;jobStartedAt=(job.started_at||Date.now()/1000)*1000;
    updateJobCard({kind:'terrain',state:job.state||'queued',phase:job.stage||'Terrain preparation queued',message:'Preparing requested terrain tiles.',progress:{known:Number.isFinite(job.total)&&job.total>0,done:job.done||0,total:job.total||0,unit:'tiles'}});
    status('Preparing terrain…');void pollTerrainJob();
  }catch(error){activeJobId=null;activeJobKind=null;preparingTerrain=false;setBusy(false);throw error;}
}
$('prepare-terrain').onclick=async()=>{
  const review=terrainEstimateReview;
  if(!review||mapEditsLocked()||!validTerrainResolution())return;
  const {estimate,operation}=review;
  const action=terrainFromCache(estimate)?cachedTerrainAction(estimate):'Download terrain';
  if(!await askActionDialog({title:action+'?',copy:`${estimate.summary}. ${terrainEstimateCost(estimate)}`,confirm:action}))return;
  if(review!==terrainEstimateReview)return;
  try{await startTerrainPreparation(operation);}
  catch(error){if(error.status===409)clearTerrainEstimateReview();if(!busy)updateJobCard({kind:'terrain',state:'failed',phase:'Terrain preparation did not start',message:error.message});status(error.message,true);}
  finally{syncCoverageLocks();}
};
function routeOperationIsCurrent(operation){return operation.projectId===(projectInfo?.active_id||null)&&operation.revision===saveRevision&&validEndpointCoords('a')&&validEndpointCoords('b')&&operation.fingerprint===JSON.stringify(plan());}
async function pollTerrainJob(){
  try{
    const expectedJobId=activeJobId,{job}=await refresh();if(activeJobKind!=='terrain'||job.job_id!==expectedJobId)return;
    if(job.started_at)jobStartedAt=job.started_at*1000;
    const running=['queued','preparing'].includes(job.state),progress={known:Number.isFinite(job.total)&&job.total>0,done:job.done||0,total:job.total||0,unit:'tiles'};
    updateJobCard({kind:'terrain',state:job.state,phase:job.stage||'Terrain preparation',message:running?'Terrain preparation is in progress.':job.stage||'Terrain preparation finished.',progress});
    if(running){status(`${job.stage} · ${progress.known?`${progress.done}/${progress.total} tiles`:'tile progress not reported'}`);setTimeout(pollTerrainJob,800);return;}
    preparingTerrain=false;setBusy(false);activeJobKind=null;
    if(job.state==='terrain_ready'){
      if(loadedSharedProject&&job.plan_revision)loadedSharedProject.revision=job.plan_revision;
      const continuation=pendingRouteContinuation;pendingRouteContinuation=null;
      updateJobCard({kind:'terrain',state:'complete',phase:'Terrain ready',message:'Ground terrain is ready.'});
      invalidate();const coverage=await updateTerrainCoverage(continuation?.snapshot);$('terrain-estimate').hidden=true;$('prepare-terrain').hidden=true;
      if(continuation){
        if(!routeOperationIsCurrent(continuation)){setRoutePreflightBusy(false);status('Terrain is ready, but the route inputs changed during preparation. Review Terrain and start the search again.',true);void updateTerrainCoverage();return;}
        try{
          if(!coverage?.covered)throw Error('Terrain preparation finished, but ground terrain is still missing at one or more selected sites. Upload terrain for the gaps, then start the route search again.');
          await startRouteSearch(continuation,coverage);
        }catch(error){setRoutePreflightBusy(false);status(error.message,true);updateJobCard({kind:'route',state:'failed',phase:'Route search did not start',message:error.message});}
      }else status('Terrain is ready. Check the coverage outline, then run the RF search.');
    }else if(job.state==='cancelled'){
      pendingRouteContinuation=null;setRoutePreflightBusy(false);
      updateJobCard({kind:'terrain',state:'cancelled',phase:'Terrain preparation cancelled',message:'The previous terrain remains available.'});status('Terrain preparation cancelled; the previous terrain remains available.',true);
    }else if(job.state==='failed'){
      pendingRouteContinuation=null;setRoutePreflightBusy(false);
      updateJobCard({kind:'terrain',state:'failed',phase:'Terrain preparation failed',message:job.stage||'The server could not prepare terrain.'});status(job.stage,true);
    }
  }catch(error){showJobConnectionError(error);}
}
function preserveMeshCoreEdits(router,prior){
  if(!prior)return {...router,stale:false};
  const {name,height_override,antenna_height_m}=prior;
  return {...router,...(prior.name_override?{name,name_override:true}:{}),
    ...(height_override?{height_override,antenna_height_m}:{}),terminal_clutter:prior.terminal_clutter,stale:false};
}
async function loadMeshcore(){const button=$('load-meshcore');button.disabled=true;status('Loading nearby MeshCore routers from CoreScope…');try{const previous=new Map(routerPolicies),oldById=new Map(meshcoreRouters.map(router=>[router.id,router])),data=await api('/api/meshcore',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({a:coords('a'),b:coords('b'),corridor_width_m:candidates.corridor_width_m})}),fresh=data.routers.map(router=>preserveMeshCoreEdits(router,oldById.get(router.id))),available=new Set(fresh.map(router=>router.id)),stale=meshcoreRouters.filter(router=>!available.has(router.id)).map(router=>({...router,stale:true,corridor_distance_m:null}));meshcoreRouters=[...fresh,...stale];routerPolicies=new Map(meshcoreRouters.map(router=>[router.id,previous.get(router.id)||oldById.get(router.id)?.policy||router.policy||'excluded']));recordEdit();invalidate('existing router set');renderMeshcoreRouters();scheduleCoverageCheck();status(`${data.count} nearby routers refreshed; ${stale.length} saved router(s) are now marked stale.`);}catch(e){status(e.message,true);}finally{button.disabled=busy;}}
$('load-meshcore').onclick=loadMeshcore;
for(const id of ['include-all','include-none'])$(id).onclick=()=>{const policy=id==='include-all'?'optional':'excluded';for(const router of visibleKnownRouters())routerPolicies.set(router.id,policy);recordEdit();invalidate('existing router policies');renderMeshcoreRouters();scheduleCoverageCheck();};
$('meshcore-filter').onchange=renderMeshcoreRouters;$('meshcore-policy-filter').onchange=renderMeshcoreRouters;$('meshcore-search').oninput=renderMeshcoreRouters;
for(const id of ['coverage-mode','coverage-cell-size','coverage-buffer','coverage-profile-step','coverage-profile-limit','coverage-endpoints','client-height','client-tx','client-gain','client-feed','client-sensitivity','client-loss'])$(id).onchange=coverageSettingsChanged;
$('coverage-area').addEventListener('change',event=>{const requested=event.target.value;if(interactionMode==='draw-coverage-area'){setInteractionMode('browse');event.target.value=requested;}if(event.target.value==='drawn'&&!coverageSettings.area_polygon_wgs84){startCoverageAreaDrawing();return;}coverageSettingsChanged({target:event.target});});
$('coverage-draw-area').onclick=startCoverageAreaDrawing;$('coverage-finish-area').onclick=finishCoverageArea;$('coverage-clear-area').onclick=clearCoverageArea;
$('coverage-opacity').oninput=event=>setCoverageOpacity(event.target.value);
$('coverage-preview').onclick=()=>void requestCoverageAction({preview:true});$('coverage-start').onclick=()=>void requestCoverageAction();$('coverage-find-route').onclick=()=>{setWorkspace('workspace-plan');const target=$('review-readiness').hidden?'endpoint-setup':($('review-readiness').dataset.target||'endpoint-setup');setPlanSection(target);setMainView('view-map');if(mobileViewport.matches)setMobileDestination('setup');document.querySelector(`[data-plan-section="${target}"]`).focus();};$('coverage-cancel').onclick=cancelCoverage;
$('coverage-terrain-download').onclick=requestCoverageTerrain;
$('coverage-terrain-cancel').onclick=cancelCoverageTerrain;
$('coverage-terrain-reconnect').onclick=pollCoverageTerrain;
$('failure-run').onclick=analyzeFailureScenario;$('failure-reset').onclick=resetFailureScenario;
$('coverage-height-run').onclick=startCoverageHeightScenario;$('coverage-compare-run').onclick=runCoverageComparison;
$('coverage-height-apply').onclick=applyCoverageHeightScenario;
$('coverage-baseline-run').onchange=()=>updateComparisonReferenceOptions(comparisonRuns);$('coverage-scenario-run').onchange=()=>updateComparisonReferenceOptions(comparisonRuns);
$('coverage-load-run').onclick=()=>void loadSavedCoverageRun();$('coverage-reconnect').onclick=()=>{if(coverageJob?.job_id)void pollCoverage(coverageJob.job_id);};$('coverage-legend-search').oninput=renderCoverageLegend;
map.on('moveend',invalidateChangedCoverageExtent);
document.querySelectorAll('[data-height-preset]').forEach(button=>button.onclick=()=>applyCoverageHeightPreset(Number(button.dataset.heightPreset)));
function plan(){return {format:'repeaternet-web',version:1,a:coords('a'),b:coords('b'),rf,candidates,terrain:terrainOptions,coverage:coverageSettings,coverage_targets:{type:'FeatureCollection',features:clone(coverageTargets)},search_effort:searchEffort,known_routers:selectedKnownRouters(),manual_routers:clone(manualRouters),endpoint_names:clone(endpointNames)};}
function loadRfSettings(saved){
  const settings={...baseRf,...saved};
  if(!Object.hasOwn(saved,'obstacle_surface')&&saved.diffraction_model==='bullington'){
    settings.diffraction_model=baseRf.diffraction_model;
  }
  for(const role of ['endpoint_a','endpoint_b','router'])settings[role]={...baseRf[role],...saved[role]};
  return settings;
}
$('save').onclick=()=>{try{const blob=new Blob([JSON.stringify(plan(),null,2)],{type:'application/json'}),url=URL.createObjectURL(blob),a=document.createElement('a');a.href=url;a.download='route.webplan.json';a.click();setTimeout(()=>URL.revokeObjectURL(url),1000);}catch(e){status(e.message,true);}};
function load(data){if(!Array.isArray(data.a)||!Array.isArray(data.b)||!data.rf||!data.candidates)throw Error('Choose a RepeaterNet .webplan.json file');rf=loadRfSettings(data.rf);endpointNames=clone(data.endpoint_names||{});candidates={...baseCandidates,...data.candidates};delete candidates.maximum_solution_routers;if(!candidates.infrastructure_policy)candidates.infrastructure_policy='existing_and_proposed';terrainOptions={...defaultTerrainOptions,...(data.terrain||{})};for(const [key,defaultValue] of Object.entries(defaultTerrainOptions)){if(typeof terrainOptions[key]!==typeof defaultValue)terrainOptions[key]=defaultValue;}coverageSettings={...JSON.parse(JSON.stringify(defaultCoverageSettings)),...(data.coverage||{}),client:{...defaultCoverageSettings.client,...(data.coverage?.client||{})}};coverageTargets=Array.isArray(data.coverage_targets?.features)?clone(data.coverage_targets.features):[];coverageTargetStates=new Map();syncTerrainControls();syncCoverageInputs();searchEffort=data.search_effort||'balanced';$('search-effort').value=searchEffort;meshcoreRouters=Array.isArray(data.known_routers)?data.known_routers.map(router=>({...router,provenance:router.provenance||'saved plan'})):[];routerPolicies=new Map(meshcoreRouters.map(router=>[router.id,router.policy||'optional']));if(!meshcoreRouters.some(router=>router.id===selectedKnownRouterId))selectedKnownRouterId=null;manualRouters=Array.isArray(data.manual_routers)?clone(data.manual_routers):[];ensureManualRouterNames();mark('a',data.a);mark('b',data.b);coords('a');coords('b');buildSettings();renderMeshcoreRouters();renderManualRouters();renderCoverageTargets();invalidate();$('fit').click();scheduleCoverageCheck();}
async function applyHistory(from,to){
  if(!from.length)return;
  const invalid=['a','b'].find(name=>!validEndpointCoords(name));
  if(invalid){focusProjectTransitionError($(invalid),`Correct Endpoint ${invalid.toUpperCase()} before using undo or redo.`);return;}
  to.push(clone(plan()));load(from.pop());lastHistoryPlan=clone(plan());setHistoryButtons();
  if(!projectInfo)return;
  scheduleAutosave();
  if(!result)return;
  const previousResult=result,revision=saveRevision,current=()=>revision===saveRevision&&result===previousResult&&!switchingProject&&!busy&&!routePreflightBusy;
  try{
    const saved=await autosaveNow(revision);
    if(!current()||!saved?.route_result_available)return;
    const restored=await api('/api/result');
    if(current()&&restored.job_id===previousResult.job_id)showCertifiedRoute(restored);
  }catch(error){
    if(current()&&savedRevision===revision&&error.status!==404&&error.status!==409)status(error.message,true);
  }
}
$('undo').onclick=()=>applyHistory(undoStack,redoStack);
$('redo').onclick=()=>applyHistory(redoStack,undoStack);
$('alternative-select').onchange=chooseAlternative;$('analysis-link-select').onchange=event=>profile(Number(event.target.value));
$('import-plan').onclick=()=>{if(!busy&&!switchingProject)$('open').click();};
$('open').onchange=async e=>{try{load(JSON.parse(await e.target.files[0].text()));undoStack=[];redoStack=[];lastHistoryPlan=clone(plan());setHistoryButtons();scheduleAutosave();status('Plan opened and saved to this project. Upload matching terrain if needed.');}catch(e){status(e.message,true);}e.target.value='';};
function emptyProjectPlan(){return {format:'repeaternet-web',version:1,a:[59.9139,10.7522],b:[59.95,10.85],rf:clone(baseRf),candidates:clone(baseCandidates),terrain:{...defaultTerrainOptions},coverage:clone(defaultCoverageSettings),coverage_targets:{type:'FeatureCollection',features:[]},search_effort:'balanced',known_routers:[],manual_routers:[]};}
function showPlanSummary(data){const summary=data.projects?.previous_result;if(summary){const prefix=summary.stale?'Saved result is stale':'Last certified result';status(`${prefix}: ${summary.router_count} router(s), ${summary.alternatives?.length||0} alternative(s). Rerun to restore the full route and profiles.`);}else if(data.job.state==='interrupted'){status('Previous search was interrupted by restart. The saved inputs are ready to rerun.',true);}else status('Project ready. Changes save automatically.');}
function showCertifiedRoute(saved){
  result=saved;resultStale=false;resultStaleReason='';lastSnapshotVersion=saved.snapshot_version;
  render();
}
async function restoreProjectResult(data){
  const {job}=data;
  if(!['complete','stopped'].includes(job.state)||!job.snapshot_version)return false;
  let saved;
  try{saved=await api('/api/result');}
  catch(error){if(error.status===404||error.status===409)return false;throw error;}
  if(saved.job_id!==job.job_id||saved.input_revision!==job.input_revision)return false;
  showCertifiedRoute(saved);
  if(result.found)fitMapSites(true);
  return true;
}
async function installProject(response){
  if(loadedSharedProject)loadedSharedProject={id:response.id,revision:response.plan_revision};
  coverageTerrainCheck=null;coverageTerrainUnknownKey=null;setCoverageTerrainStatus('');
  for(const control of document.querySelectorAll('[data-project-transition-invalid]'))clearProjectTransitionError(control);
  setInteractionMode('browse');result=null;resultStale=false;resultStaleReason='';lastSnapshotVersion=0;activeJobId=null;activeJobKind=null;jobStartedAt=null;$('route-job-card').hidden=true;$('retry-job').hidden=true;coveragePollRequest++;coverageLoadRequest++;coverageLoadingRun=false;coverageJob=null;coverageDisplayInfo=null;coveragePreviewActive=false;coveragePreviewCells=[];coverageCells=[];coverageCursor=0;coverageStale=false;terrainReadiness=null;$('coverage-load-progress').hidden=true;$('coverage-result-progress').hidden=true;$('coverage-result-progress-copy').hidden=true;
  saveRevision++;savedRevision=saveRevision;saveState='saved';setProjectSaveState('Saved automatically','saved');
  coverageBusyState(false);predictedCoverageLayer.clearLayers();if(map.hasLayer(predictedCoverageLayer))map.removeLayer(predictedCoverageLayer);resetRouteResultView();
  if(response.plan?.rf&&response.plan?.candidates)load(response.plan);else load(emptyProjectPlan());undoStack=[];redoStack=[];lastHistoryPlan=clone(plan());setHistoryButtons();
  const data=await refresh();
  if(!await restoreProjectResult(data))showPlanSummary(data);
  if(terrainLoaded)await updateTerrainCoverage();
  renderCoverageContext();await refreshCoverageRunSelectors();
}
let projectTransitionErrorSequence=0;
const projectTransitionValidationListeners=new WeakMap();
function clearProjectTransitionError(control){
  const error=control?.dataset.projectTransitionInvalid?$(control.dataset.projectTransitionInvalid):null;
  if(error){const errorId=error.id;error.remove();const describedBy=(control.getAttribute('aria-describedby')||'').split(/\s+/).filter(id=>id&&id!==errorId);if(describedBy.length)control.setAttribute('aria-describedby',describedBy.join(' '));else control.removeAttribute('aria-describedby');}
  if(control?.dataset.projectTransitionInvalid){control.removeAttribute('aria-invalid');delete control.dataset.projectTransitionInvalid;}
  const notice=$('project-transition-status');notice.textContent='';notice.hidden=true;
}
function focusProjectTransitionError(control,message){
  const workspace=control?.closest('.workspace-view');
  if(workspace)setWorkspace(workspace.id);
  const planSection=control?.closest('.plan-step-body');if(planSection)setPlanSection(planSection.id);
  const coveragePanel=control?.closest('[data-coverage-panel]');if(coveragePanel)setCoverageSection(coveragePanel.dataset.coveragePanel);
  for(let details=control?.closest('details');details;details=details.parentElement?.closest('details'))details.open=true;
  if(control){
    clearProjectTransitionError(control);
    const error=document.createElement('span');error.className='field-error project-transition-field-error';error.id=`project-transition-error-${++projectTransitionErrorSequence}`;error.setAttribute('role','alert');error.textContent=message;
    control.after(error);control.dataset.projectTransitionInvalid=error.id;control.setAttribute('aria-invalid','true');
    const describedBy=(control.getAttribute('aria-describedby')||'').split(/\s+/).filter(Boolean);
    control.setAttribute('aria-describedby',[...describedBy,error.id].join(' '));
    const previousListener=projectTransitionValidationListeners.get(control);if(previousListener){control.removeEventListener('input',previousListener);control.removeEventListener('change',previousListener);}
    const clearWhenValid=()=>{if(control.validity.valid)clearProjectTransitionError(control);};projectTransitionValidationListeners.set(control,clearWhenValid);
    control.addEventListener('input',clearWhenValid);control.addEventListener('change',clearWhenValid);
    control.scrollIntoView({behavior:'smooth',block:'center'});control.focus({preventScroll:true});
  }
  const notice=$('project-transition-status');notice.textContent=message;notice.hidden=false;
}
async function transitionProject(operation,{allowInvalidCurrentPlan=false}={}){
  if(busy||routePreflightBusy||coverageBusy||coveragePreflightBusy||coverageLoadingRun||switchingProject)return false;
  if(!allowInvalidCurrentPlan){
    for(const endpoint of ['a','b']){if(!validEndpointCoords(endpoint)){focusProjectTransitionError($(endpoint),`Correct Endpoint ${endpoint.toUpperCase()} before changing projects.`);return false;}}
    if(!validateMeshBuffer())return false;
    const invalid=document.querySelector('aside input:not([data-height-source]):invalid, aside select:invalid');
    if(invalid){const labelElement=invalid.labels?.[0]?.cloneNode(true);labelElement?.querySelectorAll('.project-transition-field-error').forEach(error=>error.remove());const label=labelElement?.textContent?.trim()||invalid.getAttribute('aria-label')||invalid.dataset.key||'router setting';focusProjectTransitionError(invalid,`Correct ${label} before changing projects. ${invalid.validationMessage}`);return false;}
  }
  $('project-transition-status').hidden=true;
  switchingProject=true;syncCoverageLocks();setHistoryButtons();
  try{while(savedRevision<saveRevision)await autosaveNow(saveRevision);await operation();return true;}
  finally{switchingProject=false;syncCoverageLocks();setHistoryButtons();}
}
async function activateProject(id){if(!id||id===projectInfo?.active_id)return;try{const switched=await transitionProject(async()=>{const response=await api(`/api/projects/${encodeURIComponent(id)}/activate`,{method:'POST'});await installProject(response);});if(!switched)$('project-select').value=projectInfo?.active_id||'';}catch(error){status(error.message,true);await refresh().catch(()=>{});}}
$('project-select').onchange=event=>activateProject(event.target.value);
function askActionDialog({title,copy,label='',value='',confirm='Continue',danger=false,terrainEstimate}){
  const dialog=$('action-dialog'),form=$('action-dialog-form'),fieldLabel=$('action-dialog-field-label'),field=$('action-dialog-value'),confirmButton=$('action-dialog-confirm'),cancelButton=$('action-dialog-cancel');
  const returnFocus=document.activeElement instanceof HTMLElement?document.activeElement:null,fallbackFocus=returnFocus?.closest('details')?.querySelector(':scope > summary')||null;
  $('action-dialog-title').textContent=title;$('action-dialog-copy').textContent=copy;fieldLabel.hidden=!label;fieldLabel.firstChild.textContent=label;field.value=value;field.required=!!label;field.setCustomValidity('');confirmButton.textContent=confirm;confirmButton.classList.toggle('danger-button',danger);
  const detail=$('action-dialog-detail');
  if(terrainEstimate)renderTerrainEstimateDetail(detail,terrainEstimate);
  else{detail.replaceChildren();detail.hidden=true;}
  return new Promise(resolve=>{
    const onSubmit=event=>{event.preventDefault();if(label&&!field.value.trim()){field.setCustomValidity(`${label} is required.`);field.reportValidity();return;}dialog.close('confirm');};
    const onInput=()=>field.setCustomValidity('');
    const onCancel=()=>dialog.close('cancel');
    const onClose=()=>{form.removeEventListener('submit',onSubmit);field.removeEventListener('input',onInput);cancelButton.removeEventListener('click',onCancel);{const usable=returnFocus?.isConnected&&!returnFocus.matches(':disabled')&&!returnFocus.closest('[hidden]')&&(returnFocus.checkVisibility?returnFocus.checkVisibility():returnFocus.getClientRects().length>0);const target=usable?returnFocus:fallbackFocus;target?.focus({preventScroll:true});requestAnimationFrame(()=>{if(target&&(document.activeElement===document.body||!document.activeElement))target.focus({preventScroll:true});});}resolve(dialog.returnValue==='confirm'?(label?field.value.trim():true):null);};
    form.addEventListener('submit',onSubmit);field.addEventListener('input',onInput);cancelButton.addEventListener('click',onCancel);dialog.addEventListener('close',onClose,{once:true});dialog.showModal();if(label)field.focus();else cancelButton.focus();
  });
}
$('new-project').onclick=async()=>{const name=await askActionDialog({title:'Create a project',copy:'Projects keep route settings, terrain and coverage work together.',label:'Project name',value:'New project',confirm:'Create'});if(!name)return;try{await transitionProject(async()=>{const response=await api('/api/projects',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({name,plan:emptyProjectPlan()})});await installProject(response);},{allowInvalidCurrentPlan:true});}catch(error){status(error.message,true);}};
$('rename-project').onclick=async()=>{const name=await askActionDialog({title:'Rename project',copy:'This changes the project name, not its saved plan.',label:'Project name',value:projectInfo?.active_name||'',confirm:'Save name'});if(!name)return;try{await api(`/api/projects/${encodeURIComponent(projectInfo.active_id)}`,{method:'PATCH',headers:{'Content-Type':'application/json'},body:JSON.stringify({name})});await refresh();status('Project renamed.');}catch(error){status(error.message,true);}};
$('duplicate-project').onclick=async()=>{const name=await askActionDialog({title:'Duplicate project',copy:'The copy starts with the current saved plan and settings.',label:'Copy name',value:`${projectInfo?.active_name||'Project'} copy`,confirm:'Duplicate'});if(!name)return;try{await transitionProject(async()=>{const response=await api(`/api/projects/${encodeURIComponent(projectInfo.active_id)}/duplicate`,{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({name})});await installProject(response);});}catch(error){status(error.message,true);}};
function replacementProject(oldId){return (projectInfo.items||[]).find(item=>item.id!==oldId);}
async function openReplacementProject(replacement,name){
  const response=replacement
    ?await api(`/api/projects/${encodeURIComponent(replacement.id)}/activate`,{method:'POST'})
    :await api('/api/projects',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({name,plan:emptyProjectPlan()})});
  await installProject(response);
}
$('archive-project').onclick=async()=>{
  const oldId=projectInfo.active_id,oldName=projectInfo.active_name,replacement=replacementProject(oldId);
  const answer=await askActionDialog({title:'Archive project?',copy:replacement?`The plan and terrain remain recoverable. The app will switch to ${replacement.name} first.`:'This is the last active project. A replacement project will be created so you can continue; the archived plan and terrain remain recoverable.',label:replacement?'':'Replacement project name',value:replacement?'':'New project',confirm:replacement?'Archive':'Create and archive'});
  if(!answer)return;
  const replacementName=replacement?null:answer;
  try{await transitionProject(async()=>{
    await openReplacementProject(replacement,replacementName);
    await api(`/api/projects/${encodeURIComponent(oldId)}/archive`,{method:'POST'});
    await refresh();status(`Archived ${oldName}. You can restore it from Archived projects.`);
  });}catch(error){status(error.message,true);}
};
async function restoreProject(item){
  const name=await askActionDialog({title:'Restore archived project?',copy:'Restoring returns the saved plan and terrain to the active project list.',label:'Project name',value:item.name,confirm:'Restore'});if(!name)return;
  try{await transitionProject(async()=>{const response=await api(`/api/projects/${encodeURIComponent(item.id)}/restore`,{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({name})});await installProject(response);await refresh();status(`${name} restored and opened.`);});}catch(error){status(error.message,true);}
}
$('delete-project').onclick=async()=>{
  const oldId=projectInfo.active_id,replacement=replacementProject(oldId);
  if(!await askActionDialog({title:'Delete project permanently?',copy:'This permanently removes the project and its terrain files. This cannot be undone.',confirm:'Delete project',danger:true}))return;
  try{await transitionProject(async()=>{
    await openReplacementProject(replacement,`Untitled ${Date.now()}`);
    await api(`/api/projects/${encodeURIComponent(oldId)}`,{method:'DELETE'});
    await refresh();
  });}catch(error){status(error.message,true);}
};
$('recover-project').onclick=async()=>{if(!await askActionDialog({title:'Recover previous plan?',copy:'The current revision will become the recovery copy.',confirm:'Recover plan'}))return;try{await transitionProject(async()=>{const response=await api('/api/projects/recover-previous',{method:'POST'});await installProject(response);});}catch(error){status(error.message,true);}};
function createRouteOperation(){const snapshot=clone(plan());return {id:randomId(),projectId:projectInfo?.active_id||null,revision:saveRevision,snapshot,fingerprint:JSON.stringify(snapshot)};}
async function startRouteSearch(operation,preflightCoverage=null){
  if(busy||coverageBusy||coverageLoadingRun||switchingProject)throw Error('Another planner operation is active. Wait for it to finish before searching.');
  if(!routeOperationIsCurrent(operation))throw Error('The route inputs changed during preparation. Review them and start the search again.');
  const coverage=preflightCoverage||await checkRouteTerrain(operation.snapshot);
  if(!routeOperationIsCurrent(operation))throw Error('The route inputs changed during terrain checking. Start the search again to use the latest settings.');
  if(!coverage?.covered)throw Error('Ground terrain is missing at one or more selected sites. Upload terrain for the gaps, then start the route search again.');
  await flushAutosave();
  if(!routeOperationIsCurrent(operation))throw Error('The route inputs changed before submission. Start the search again to use the latest settings.');
  const body=clone(operation.snapshot);setRoutePreflightBusy(false);invalidate('new route search started');lastSnapshotVersion=0;activeJobId=null;activeJobKind='route';jobStartedAt=Date.now();setBusy(true);
  updateJobCard({kind:'route',state:'queued',phase:'Submitting route search',message:'Sending the saved plan to the planner.'});$('cancel').hidden=true;
  try{
    const job=await api('/api/optimize',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify(body)});
    activeJobId=job.job_id;jobStartedAt=(job.started_at||Date.now()/1000)*1000;preparingTerrain=false;
    updateJobCard({kind:'route',state:job.state||'queued',phase:job.stage||'Route search queued',message:'Planner accepted the route search.',progress:{known:Number.isFinite(job.total)&&job.total>0,done:job.done||0,total:job.total||0,unit:'steps'}});void poll();
  }catch(error){activeJobId=null;activeJobKind=null;setBusy(false);updateJobCard({kind:'route',state:'failed',phase:'Route search did not start',message:error.message});throw error;}
}
$('optimize').onclick=async()=>{
  if(busy||routePreflightBusy||coverageBusy||coverageLoadingRun||switchingProject)return;
  for(const id of ['model-correction-db','final-sample-step-m','maximum-new-routers','maximum-hops']){
    if(!validateRfSetting($(id)))return;
  }
  searchEffort=$('search-effort').value;
  for(const name of ['a','b']){
    try{coords(name);}catch(error){
      setPlanSection('endpoint-setup');
      updatePlanReadiness();
      status(error.message,true);
      $(name).focus();
      return;
    }
  }
  if(!terrainLoaded&&!$('terrain-resolution-error').hidden){setPlanSection('terrain-setup');validTerrainResolution();return;}
  const operation=createRouteOperation();setRoutePreflightBusy(true);
  try{
    status('Checking terrain coverage…');
    const coverage=await checkRouteTerrain(operation.snapshot);
    if(!routeOperationIsCurrent(operation))throw Error('Route settings changed during terrain checking. Start again to use the latest settings.');
    if(coverage?.covered){await startRouteSearch(operation,coverage);return;}
    await flushAutosave();
    if(!routeOperationIsCurrent(operation))throw Error('Route settings changed before the terrain estimate. Start again to use the latest settings.');
    status('Estimating terrain for this route…');
    const revision=dataSourcesRevision;
    void previewTerrainSources(operation.snapshot);
    const estimate=await api('/api/terrain/estimate',sourceJsonRequest('POST',operation.snapshot));
    if(!routeOperationIsCurrent(operation))throw Error('Route settings changed while estimating terrain. Start again to use the latest settings.');
    const resolution=`Requested resolution ${operation.snapshot.terrain.requested_resolution_m} m${operation.snapshot.terrain.auto_resolution?' · automatic coarsening enabled':''}.`;
    const action=terrainFromCache(estimate)?cachedTerrainAction(estimate):'Download terrain';
    const confirmed=await askActionDialog({title:action+' and find this route?',copy:`${estimate.summary}. ${resolution} ${terrainEstimateCost(estimate)}`,terrainEstimate:estimate,confirm:action+' and find route'});
    if(!confirmed){status('Route search cancelled.');return;}
    if(revision!==dataSourcesRevision)throw Error('Data sources changed. Estimate again before preparing terrain.');
    if(!routeOperationIsCurrent(operation))throw Error('Route settings changed before terrain preparation. Start again to approve the current settings.');
    pendingRouteContinuation=operation;status('Starting terrain preparation…');await startTerrainPreparation(operation);
  }catch(error){pendingRouteContinuation=null;status(error.message,true);}
  finally{if(routePreflightBusy&&!pendingRouteContinuation&&!busy)setRoutePreflightBusy(false);}
};
$('search-effort').onchange=()=>{searchEffort=$('search-effort').value;recordEdit();invalidate('search effort');};
$('cancel').onclick=async()=>{if(preparingTerrain){pendingRouteContinuation=null;setRoutePreflightBusy(false);}try{await api('/api/cancel',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({job_id:activeJobId})});updateJobCard({kind:activeJobKind||'route',state:'running',phase:'Cancellation requested',message:'Waiting for the planner to confirm cancellation.'});status(preparingTerrain?'Cancelling terrain preparation…':'Cancelling…');}catch(error){status(error.message,true);}};
$('stop-keep').onclick=async()=>{try{await api('/api/stop-and-keep',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({job_id:activeJobId})});$('stop-keep').disabled=true;updateJobCard({kind:'route',state:'running',phase:'Stop and keep requested',message:'The planner is finishing with its latest certified route.'});status('Stopping and keeping the certified route…');}catch(error){status(error.message,true);}};
$('clear-cache').onclick=async()=>{try{const data=await api('/api/cache',{method:'DELETE'});await refresh();status(`${data.deleted_entries} saved RF calculations cleared.`);}catch(e){status(e.message,true);}};
async function poll(){
  try{
    const expectedJobId=activeJobId,{job}=await refresh();
    if(activeJobKind!=='route'||job.job_id!==expectedJobId)return;
    if(job.started_at)jobStartedAt=job.started_at*1000;
    const running=['running','queued'].includes(job.state),progress={known:Number.isFinite(job.total)&&job.total>0,done:job.done||0,total:job.total||0,unit:'steps'},preview=result&&!result.search_complete?` · certified route · ${result.achieved_path_count}/${result.requested_path_count} paths`:'';
    updateJobCard({kind:'route',state:job.state,phase:job.stage||'Route search',message:running?`${searchScope(job)}${preview}`:job.stage||'Route search finished.',progress});
    if(job.snapshot_version>lastSnapshotVersion){
      const snapshot=await api('/api/result');
      if(activeJobKind!=='route'||activeJobId!==expectedJobId)return;
      if(snapshot.job_id===job.job_id&&snapshot.input_revision===job.input_revision){result=snapshot;resultStale=false;resultStaleReason='';lastSnapshotVersion=job.snapshot_version;render();$('stop-keep').disabled=false;}
    }
    if(running){status(`${job.stage} · ${searchScope(job)}${preview}`);setTimeout(poll,800);return;}
    if(job.state==='complete'){
      let completedResult=result;
      if(completedResult?.job_id!==job.job_id||completedResult?.input_revision!==job.input_revision)completedResult=await api('/api/result');
      if(activeJobKind!=='route'||activeJobId!==expectedJobId)return;
      if(completedResult.job_id!==job.job_id||completedResult.input_revision!==job.input_revision)throw Error('The completed route snapshot is not available yet. Reconnect to load the matching result.');
      result=completedResult;resultStale=false;resultStaleReason='';render();if(result.found)fitMapSites(true);activeJobKind=null;setBusy(false);
      updateJobCard({kind:'route',state:'complete',phase:'Route search complete',message:result.found?'A certified route is ready for review.':'No viable route was found within the selected search limits.',progress});
      return;
    }
    activeJobKind=null;setBusy(false);
    updateJobCard({kind:'route',state:job.state,phase:job.state==='stopped'?'Search stopped with certified route':job.state==='cancelled'?'Search cancelled':job.state==='failed'?'Route search failed':job.stage||'Search finished',message:job.state==='stopped'?'The latest certified route was kept.':job.state==='cancelled'?'Any earlier displayed result remains marked stale.':job.stage||'The route search finished.',progress});
    if(job.state==='stopped'){if(result?.found)fitMapSites(true);status('Stopped with the latest certified route.');}
    else if(job.state==='failed')status(job.stage,true);
    else if(job.state==='cancelled')status('Search cancelled; any displayed previous route remains marked stale.',true);
  }catch(error){showJobConnectionError(error);}
}
function analysisMarginBand(db){if(!Number.isFinite(db))return {cls:'unknown',label:'unknown'};if(db>=10)return {cls:'strong',label:'strong'};if(db>=5)return {cls:'good',label:'good'};if(db>=0)return {cls:'marginal',label:'marginal'};return {cls:'fail',label:'fails'};}
function analysisSiteLabel(id){try{if(typeof routeSiteLabel==='function'){const label=routeSiteLabel(id);if(label)return label;}}catch{}const site=result?.route?.find(item=>item.id===id);if(site?.kind==='endpoint_a')return 'A';if(site?.kind==='endpoint_b')return 'B';return id;}
function marginPill(db,suffix=' dB'){const band=analysisMarginBand(db),pill=document.createElement('span');pill.className=`margin-pill margin-${band.cls}`;pill.textContent=Number.isFinite(db)?`${db.toFixed(1)}${suffix}`:'—';pill.title=`${band.label} margin`;return pill;}
function renderAlternativeChoices(){
  const alternatives=result?.alternatives||[],panel=$('alternatives'),selector=$('alternative-select'),cards=$('alternative-cards');selector.replaceChildren();cards.replaceChildren();
  if(!alternatives.length){panel.hidden=true;return;}
  selector.value=result.active_alternative_id||alternatives.find(item=>item.selected)?.id||alternatives[0].id;
  const objective={minimum_routers:'fewest hops',fewest_new_installations:'fewest new installations',minimum_infrastructure:'lowest infrastructure cost',maximum_reliability:'independent paths and RF quality before router count'}[candidates.priority]||'configured objective';
  const table=document.createElement('table'),head=document.createElement('thead'),body=document.createElement('tbody');table.className='alternative-table';table.setAttribute('aria-label','Route alternatives');head.innerHTML='<tr><th>Alternative</th><th>Existing · new</th><th>Primary length</th><th>Weakest link</th><th>Primary weakest</th><th>LOS-blocked hops</th><th>Independent paths</th></tr>';table.append(head,body);cards.append(table);
  for(const [index,item] of alternatives.entries()){
    const routers=`${item.router_count} ${item.router_count===1?'router':'routers'}`,weakest=Number.isFinite(item.minimum_margin_db)?`${item.minimum_margin_db.toFixed(1)} dB weakest`:'no margin';
    const option=document.createElement('option');option.value=item.id;option.textContent=`${item.name||`Alternative ${index+1}`} · ${weakest} · ${(item.primary_distance_m/1000).toFixed(1)} km · ${item.achieved_path_count}/${item.requested_path_count} paths`;selector.append(option);
    const row=document.createElement('tr');row.dataset.alternativeId=item.id;row.tabIndex=0;
    const cells=[item.name||`Alternative ${index+1}`,`${item.existing_router_count} · ${item.proposed_router_count}`,`${(item.primary_distance_m/1000).toFixed(2)} km`,marginPill(item.minimum_margin_db),Number.isFinite(item.primary_minimum_margin_db)?marginPill(item.primary_minimum_margin_db):'—',Number.isFinite(item.primary_los_blocked_hops)?String(item.primary_los_blocked_hops):'—',`${item.achieved_path_count}/${item.requested_path_count}${item.resilient?' ✓':''}`];
    for(const value of cells){const cell=document.createElement('td');if(value instanceof Node)cell.append(value);else cell.textContent=value;row.append(cell);}
    if(!item.resilient)row.lastChild.classList.add('shortfall');if(!item.search_complete)row.firstChild.append(' (search incomplete)');
    const choose=()=>{if($('alternative-select').disabled||item.id===$('alternative-select').value)return;$('alternative-select').value=item.id;void chooseAlternative();};row.onclick=choose;row.onkeydown=event=>{if(event.key==='Enter'||event.key===' '){event.preventDefault();choose();}};body.append(row);
  }
  selector.value=result.active_alternative_id||alternatives.find(item=>item.selected)?.id||alternatives[0].id;selector.hidden=false;const active=alternatives.find(item=>item.id===selector.value);$('alternative-summary').textContent=active?`${alternatives.length} alternatives, ranked for ${objective}.`:'';syncAlternativeRows();$('alternative-comparison-summary').textContent=`Compare all ${alternatives.length} route alternatives`;panel.hidden=false;syncAlternativeControls();
}
async function chooseAlternative(){if(resultStale||busy||coverageBusy||coverageLoadingRun||switchingProject||alternativeSelecting)return;const id=$('alternative-select').value;if(!id)return;alternativeSelecting=true;syncAlternativeControls();try{result=await api(`/api/alternatives/${encodeURIComponent(id)}/select`,{method:'POST'});resultStale=false;resultStaleReason='';coverageHeightOverrides={};invalidateCoverageEstimate('Selected route alternative changed. Review a new coverage estimate for this mesh.');coverageCells=[];coverageCursor=0;coverageStale=true;predictedCoverageLayer.clearLayers();setCoverageStatus('Route alternative changed. Recalculate coverage to match the selected mesh.');render();}catch(e){render();status(e.message,true);}finally{alternativeSelecting=false;syncAlternativeControls();}}
function pinMovedRouter(site,point){
  const router={id:'M-'+randomId(),name:nextManualRouterName(),antenna_height_m:site.antenna_height_m,policy:'required'};
  manualRouters.push(router);
  moveManualRouter(router,point.wrap());
  focusManualRouter(router.id);
  setMapHint(`${routeSiteLabel(site.id)} pinned as required router ${manualRouterName(router)}. The grey marker shows the old route; run a new search to validate the route.`,true);
}
function routeSiteLabel(siteId){
  if(labelledRoute!==result){
    labelledRoute=result;
    routeSiteLabels=new Map();
    let proposedCount=0,existingCount=0;
    const primary=result?.alternatives?.find(alternative=>alternative.id===result.active_alternative_id)?.route||[];
    const knownNames=new Map(meshcoreRouters.map(router=>[router.id,router.name?.trim()]));
    const manualNames=new Map(manualRouters.map(router=>[router.id,manualRouterName(router)]));
    for(const site of [...primary,...(result?.route||[])]){
      if(routeSiteLabels.has(site.id))continue;
      let label=site.id;
      if(site.kind==='endpoint_a')label='A';
      else if(site.kind==='endpoint_b')label='B';
      else if(site.kind!=='client'){
        if(site.origin==='known'){
          const name=Array.from(knownNames.get(site.id)||'');
          label=name.length?(name.length<=9?name.join(''):name.slice(0,8).join('')+'…'):`E${++existingCount}`;
        }else if(site.origin==='manual')label=shortMapLabel(manualNames.get(site.id)||site.name||site.id);
        else label=`R${++proposedCount}`;
      }
      routeSiteLabels.set(site.id,label);
    }
  }
  return routeSiteLabels.get(siteId)??siteId;
}
function routeLinkMapStyle(link,stale=resultStale){
  const margin=link.worst_margin_db;
  const band=stale?'stale':!link.valid||margin<0?'invalid':margin>=10?'strong':margin>=5?'good':'marginal';
  return {color:`var(--map-${band})`,dashArray:band==='stale'||band==='invalid'?'5 6':null};
}
function routeMapLinkTooltip(link){
  return mapText('span',`${routeSiteLabel(link.source_id)} → ${routeSiteLabel(link.target_id)} · ${(link.distance_m/1000).toFixed(2)} km · ${link.worst_margin_db.toFixed(1)} dB · ${linkIssues(link)}`);
}
function routeSiteNeighbors(id){
  const primary=result?.alternatives?.find(item=>item.id===result.active_alternative_id)?.route||[];
  const index=primary.findIndex(site=>site.id===id);
  if(index<0)return [];
  return [primary[index-1],primary[index+1]].filter(Boolean).flatMap(neighbor=>{
    const source=primary[index],target=neighbor;
    const linkIndex=result.links.findIndex(link=>
      (link.source_id===source.id&&link.target_id===target.id)||
      (link.source_id===target.id&&link.target_id===source.id));
    if(linkIndex<0)return [];
    const link=result.links[linkIndex];
    const sites=link.source_id===source.id?[source,target]:[target,source];
    return [{link,sites,line:routeLinkLayers[linkIndex]}];
  });
}
function bindRouteSiteInspector(marker,id,move){
  let originalPoint;
  marker.on('dragstart',()=>{
    originalPoint=marker.getLatLng();
    closeMapSelection();
    if(!mapEditsLocked()&&!resultStale&&interactionMode==='browse')openRouteLinkInspector(id,marker);
  });
  marker.on('drag',()=>{
    const inspector=routeLinkInspector;
    if(!mapEditsLocked()&&!resultStale&&inspector?.marker===marker){
      queueRouteLinkPreview(inspector,marker.getLatLng().wrap());
    }
  });
  marker.on('dragend',()=>{
    const inspector=routeLinkInspector?.marker===marker?routeLinkInspector:null;
    const point=marker.getLatLng().wrap();
    if(mapEditsLocked()){
      if(originalPoint)marker.setLatLng(originalPoint);
      return;
    }
    // Evaluate exactly the coordinates that the plan will save.
    const finalPoint=L.latLng(Number(point.lat.toFixed(6)),Number(point.lng.toFixed(6)));
    if(inspector)inspector.committing=true;
    try{move(finalPoint);}finally{if(inspector)inspector.committing=false;}
    if(inspector){
      for(const item of inspector.links)restoreRouteInspectorLine(item);
      setRouteInspectorStatus('Link preview · run a new search to validate the route.');
      queueRouteLinkPreview(inspector,finalPoint);
    }
  });
}
function openRouteLinkInspector(id,marker){
  if(routeLinkInspector?.id===id&&routeLinkInspector.marker===marker){
    return true;
  }
  const links=routeSiteNeighbors(id);
  if(!links.length)return false;
  // A finished job toast sits where the inspector opens; the inspector takes over that corner.
  if(!['queued','running','preparing','failed','disconnected'].includes($('route-job-card').dataset.state))$('route-job-card').hidden=true;
  closeRouteLinkInspector();
  map.closePopup();
  routeLinkInspector={id,marker,links,route:result,projectId:projectInfo?.active_id};
  const panel=$('route-link-inspector'),container=$('route-link-inspector-links'),title=$('route-link-inspector-title');
  title.textContent=`Links at ${routeSiteLabel(id)}`;
  container.replaceChildren();
  for(const item of links){
    const card=document.createElement('article');
    item.body=document.createElement('div');
    card.append(mapText('h3',item.sites.map(site=>routeSiteLabel(site.id)).join(' – ')),item.body);
    container.append(card);
  }
  setRouteInspectorStatus(resultStale?'Previous route · stale.':'');
  panel.hidden=false;
  panel.scrollTop=0;
  for(const item of links)renderRouteInspectorLink(item,item.link);
  return true;
}
function setRouteInspectorStatus(message){
  const status=$('route-link-inspector-status');
  if(status.textContent!==message)status.textContent=message;
  status.hidden=!message;
}
function restoreRouteInspectorLine({link,sites,line}){
  line?.setLatLngs(sites.map(mapPoint));
  line?.setStyle(routeLinkMapStyle(link));
  line?.setTooltipContent(routeMapLinkTooltip(link));
}
function closeRouteLinkInspector(restoreFocus=false){
  if(!routeLinkInspector)return;
  const {marker,links,id:routeLinkInspectorId} = routeLinkInspector;
  routeLinkInspector=null;
  clearTimeout(routeLinkPreviewTimer);
  routeLinkPreviewTimer=null;
  for(const item of links)restoreRouteInspectorLine(item);
  $('route-link-inspector').hidden=true;
  if(restoreFocus){
    const icon=mapSiteElement(routeLinkInspectorId)||marker.getElement();
    (icon?.checkVisibility()?icon:map.getContainer()).focus({preventScroll:true});
  }
}
function queueRouteLinkPreview(inspector,point){
  inspector.pending=point;
  if(!resultStale){
    setRouteInspectorStatus('Live link preview');
    for(const item of inspector.links){
      item.line?.setLatLngs(item.sites.map(site=>site.id===inspector.id?point:mapPoint(site)));
    }
  }
  scheduleRouteLinkPreview();
}
function scheduleRouteLinkPreview(){
  if(!routeLinkInspector?.pending||routeLinkPreviewInFlight||routeLinkPreviewTimer!=null)return;
  const wait=Math.max(0,125-(performance.now()-routeLinkPreviewStarted));
  routeLinkPreviewTimer=setTimeout(evaluateRouteLinkPreview,wait);
}
async function evaluateRouteLinkPreview(){
  routeLinkPreviewTimer=null;
  const inspector=routeLinkInspector;
  if(!inspector?.pending||mapEditsLocked())return;
  const point=inspector.pending;
  inspector.pending=null;
  routeLinkPreviewInFlight=true;
  routeLinkPreviewStarted=performance.now();
  try{
    for(const item of inspector.links){
      if(routeLinkInspector!==inspector||result!==inspector.route||mapEditsLocked())break;
      const [source,target]=item.sites.map(({id,latitude,longitude,antenna_height_m,kind})=>({
        id,latitude:id===inspector.id?point.lat:latitude,longitude:id===inspector.id?point.lng:longitude,
        antenna_height_m,kind
      }));
      let link,error;
      try{
        link=await api('/api/link/evaluate',{method:'POST',headers:{'Content-Type':'application/json'},
          body:JSON.stringify({source,target,include_profile:true,project_id:inspector.projectId})});
      }catch(e){error=e.message;}
      if(routeLinkInspector!==inspector||result!==inspector.route||mapEditsLocked())break;
      renderRouteInspectorLink(item,link,error);
      if(!resultStale&&item.line){
        item.line.setStyle(routeLinkMapStyle(link??{valid:false}));
        item.line.setTooltipContent(link?routeMapLinkTooltip(link):mapText('span',error));
      }
    }
  }finally{
    routeLinkPreviewInFlight=false;
    scheduleRouteLinkPreview();
  }
}
function renderRouteInspectorLink(item,link,error){
  const body=item.body;
  body.replaceChildren();
  if(error){
    const message=mapText('p',error);
    message.className='error';
    message.role='status';
    body.append(message);
    return;
  }
  const metrics=document.createElement('dl');
  const fresnel=link.minimum_fresnel_clearance_ratio;
  for(const [label,value] of [
    ['Distance · km',formatMetric(link.distance_m/1000,2)],
    ['Usable margin',marginPill(link.worst_margin_db)],
    ['Line of sight clear',link.los_clear?'Yes':'No'],
    ['Min. Fresnel clearance',Number.isFinite(fresnel)?`${(fresnel*100).toFixed(0)}%`:'—'],
  ]){
    const field=document.createElement('div'),reading=document.createElement('dd');
    reading.append(value);
    field.append(mapText('dt',label),reading);
    metrics.append(field);
  }
  body.append(metrics);
  if(!link.profile)return;
  const clearance=link.required_fresnel_clearance??result.rf_settings?.required_fresnel_clearance??rf.required_fresnel_clearance;
  const canvas=document.createElement('canvas'),legend=document.createElement('div');
  canvas.setAttribute('role','img');
  canvas.setAttribute('aria-label','Elevation profile along the link');
  legend.className='chart-legend';
  const lines=routeProfileLines(link.profile,clearance);
  for(const {label,color,dash} of lines){
    const item=document.createElement('span'),swatch=document.createElement('i');
    if(dash)swatch.style.borderTop=`3px dashed ${color}`;
    else swatch.style.backgroundColor=color;
    item.append(swatch,label);
    legend.append(item);
  }
  body.append(canvas,legend);
  drawRouteProfileChart(canvas,link,lines,78);
}
$('route-link-inspector-close').onclick=()=>closeRouteLinkInspector(true);
function openRouteLink(index){
  setMainView('view-analysis');
  profile(index,true);
}
function linkIssues(link){const issues=[];if(!link.los_clear)issues.push('LOS obstructed');if(!link.fresnel_clear)issues.push('Fresnel clearance not met');if(!link.forward.valid)issues.push('forward budget failed');if(!link.reverse.valid)issues.push('reverse budget failed');return link.valid?`Passes configured validation criteria${issues.length?` · ${issues.slice(0,2).join(', ')}`:''}`:issues.length?`Not certified · ${issues.join(', ')}`:'Not certified · radio criteria not met';}
function resetRouteResultView(){
  closeRouteLinkInspector();
  $('prediction-spread').hidden=true;
  closeMapSelection(true);
  $('map-analysis-selection').hidden=true;
  routeLayer.clearLayers();candidateLayer.clearLayers();routeLinkLayers=[];
  $('metrics').replaceChildren();$('route-verdict').hidden=true;$('alternatives').hidden=true;$('hops').hidden=true;$('hops-disclosure').hidden=true;$('profile').hidden=true;$('exports').hidden=true;$('candidate-search-diagnostic').hidden=true;$('rf-validation-note').hidden=true;$('analysis-link-picker').hidden=true;
  $('route-show-map').disabled=true;$('result-title').textContent='A clear view of your connection.';$('status').textContent='Find a route to see alternatives, hops, link budgets and terrain profiles here.';$('status').className='';
  syncAnalysisBadge();showRepeaterRoads();syncRoutedRouterMarkers();
}
function syncAlternativeRows(){const id=$('alternative-select').value;document.querySelectorAll('.alternative-table tbody tr').forEach(row=>{const on=row.dataset.alternativeId===id;row.classList.toggle('selected',on);row.setAttribute('aria-selected',String(on));});}
function syncAnalysisBadge(){const badge=$('analysis-badge');badge.hidden=!result;badge.textContent=resultStale?'Stale':'Ready';badge.dataset.state=resultStale?'stale':'ready';}
function showRepeaterRoads(){
  repeaterRoadLayer.clearLayers();
  const active=result?.alternatives?.find(item=>item.id===result.active_alternative_id);
  const lines=(active?.road_polyline_indices||[]).map(index=>result.road_polylines?.[index]).filter(Boolean);
  $('show-repeater-roads-control').hidden=!lines.length;
  if(!lines.length||!$('show-repeater-roads').checked){
    if(map.hasLayer(repeaterRoadLayer))map.removeLayer(repeaterRoadLayer);
    return;
  }
  const style={pane:'repeaterRoadsPane',interactive:false};
  L.polyline(lines,{...style,color:'#2e3037',weight:4}).addTo(repeaterRoadLayer);
  L.polyline(lines,{...style,color:'#ffe1a1',weight:2}).addTo(repeaterRoadLayer);
  if(!map.hasLayer(repeaterRoadLayer))repeaterRoadLayer.addTo(map);
}
$('show-repeater-roads').onchange=showRepeaterRoads;
function renderRouteVerdict({links,band,margin,requested,achieved,resilient}={}){
  const verdict=$('route-verdict');
  verdict.hidden=!result?.found;
  if(verdict.hidden)return;
  if(resultStale){
    verdict.dataset.state='stale';
    verdict.textContent=`Stale route. This result is for previous inputs (${resultStaleReason||'planning inputs'} changed). Run a new search to validate the current plan.`;
    return;
  }
  const weak=links.filter(link=>link.valid&&link.worst_margin_db<5).length,failed=links.filter(link=>!link.valid).length,parts=[];
  let state='ok';
  if(failed){state='fail';parts.push(`${failed} link${failed===1?'':'s'} fail the configured validation.`);}
  else if(band.cls==='marginal'){state='warn';parts.push(`All links pass, but the weakest link has only ${margin.toFixed(1)} dB margin and may drop out in poor conditions.`);}
  else parts.push(`All ${links.length} links pass with at least ${Number.isFinite(margin)?margin.toFixed(1):'—'} dB margin.`);
  if(Number.isFinite(requested)&&!resilient){if(state==='ok')state='warn';parts.push(`Only ${achieved} of ${requested} requested independent paths found.`);}
  if(weak&&band.cls!=='marginal')parts.push(`${weak} link${weak===1?' is':'s are'} below 5 dB.`);
  if(!result.search_complete)parts.push('Search incomplete; a better route may exist.');
  verdict.dataset.state=state;verdict.textContent=parts.join(' ');
}
function render(){
  closeRouteLinkInspector();
  $('prediction-spread').hidden=true;
  closeMapSelection(true);
  $('map-analysis-selection').hidden=true;
  routeLayer.clearLayers();routeLinkLayers=[];$('metrics').replaceChildren();$('hops').querySelector('tbody').replaceChildren();$('profile').hidden=true;$('analysis-link-picker').hidden=true;$('hops-disclosure').hidden=true;$('rf-validation-note').hidden=true;
  syncAnalysisBadge();showRepeaterRoads();$('route-show-map').disabled=!result?.links?.length;syncRoutedRouterMarkers();
  const candidateDiagnostic=$('candidate-search-diagnostic');
  candidateDiagnostic.textContent=result.found?(result.diagnostics||[]).filter(line=>line.startsWith('Road access filter kept ')||line.startsWith('Candidate sites:')||line.startsWith('Effective candidate grid spacing:')).join(' '):'';
  candidateDiagnostic.hidden=!candidateDiagnostic.textContent;
  $('result-title').textContent=resultStale?'Previous certified route · stale':!result.found?'No viable route found':result.search_complete?'Your connection, mapped.':'Certified route · search incomplete';renderAlternativeChoices();
  if(!result.found){renderRouteVerdict();$('hops').hidden=true;$('exports').hidden=true;status(resultStale?'Previous search result is stale.':result.diagnostics.join(' · ')||'Try a wider corridor or different antenna heights.');return;}$('rf-validation-note').hidden=false;
  const links=result.links,active=(result.alternatives||[]).find(item=>item.id===result.active_alternative_id),margin=active?.minimum_margin_db??Math.min(...links.map(link=>link.worst_margin_db)),distance=active?.primary_distance_m??links.reduce((sum,link)=>sum+link.distance_m,0);
  const band=analysisMarginBand(margin),requested=active?.requested_path_count??result.requested_path_count,achieved=active?.achieved_path_count??result.achieved_path_count,resilient=active?.resilient??(achieved>=requested),existing=result.existing_router_count||0,proposed=result.proposed_router_count??result.router_count;
  const tiles=[[`${result.router_count}`,`Routers · ${existing} existing, ${proposed} new`,''],[(distance/1000).toFixed(1)+' km','Primary route length',''],[Number.isFinite(margin)?margin.toFixed(1)+' dB':'—',`Weakest link · ${band.label}`,`margin-${band.cls}`],[Number.isFinite(requested)?`${achieved}/${requested}`:'—',resilient?'Independent paths · met':'Independent paths · shortfall',resilient?'margin-strong':'margin-marginal']];
  for(const [value,label,cls] of tiles){const metric=document.createElement('div'),strong=document.createElement('strong'),small=document.createElement('small');metric.className='metric '+cls;strong.textContent=value;small.textContent=label;metric.append(strong,small);$('metrics').append(metric);}
  renderRouteVerdict({links,band,margin,requested,achieved,resilient});
  const spread=$('prediction-spread'),fade=result.rf_settings?.fade_margin_db;
  spread.textContent=`At an unmeasured site, signal strength often differs from the prediction by about ±${formatMetric(calibration.new_site_sigma_db)} dB. The ${formatMetric(fade)} dB fade margin covers changes over time (σ ≈ ${formatMetric(calibration.time_variability_sigma_db)} dB).`;
  spread.hidden=!Number.isFinite(fade);
  {const diagnostic=$('candidate-search-diagnostic'),took=`Search took ${Number(result.elapsed_seconds||0).toFixed(1)} s.`;diagnostic.textContent=diagnostic.textContent?`${diagnostic.textContent} · ${took}`:took;diagnostic.hidden=false;$('rf-validation-note').hidden=true;}
  const linkSelector=$('analysis-link-select');linkSelector.replaceChildren();links.forEach((link,index)=>{const option=document.createElement('option');option.value=String(index);option.textContent=`${index+1}. ${analysisSiteLabel(link.source_id)} → ${analysisSiteLabel(link.target_id)} · ${(link.distance_m/1000).toFixed(2)} km · ${link.worst_margin_db.toFixed(1)} dB`;linkSelector.append(option);});$('analysis-link-picker').hidden=!links.length;
  const sites=Object.fromEntries(result.route.map(site=>[site.id,site]));
  result.route.filter(site=>!['endpoint_a','endpoint_b','client'].includes(site.kind)).forEach(site=>{
    if(!mapPoint(site).every(Number.isFinite)||['manual','known'].includes(site.origin))return;
    const label=routeSiteLabel(site.id),type='proposed';
    const marker=L.marker(mapPoint(site),{
      draggable:!mapEditsLocked()&&!resultStale,
      icon:mapSiteIcon(type,label,null,{stale:resultStale}),title:label,zIndexOffset:100,siteId:site.id,
      ...(resultStale?{pane:'staleRouteSitePane'}:{})
    }).bindTooltip(mapText('span',label+' · Site found by search'));
    marker._routeSiteIcon={type,label};
    marker._plannerDraggable=true;
    bindMapSite(marker,()=>routerMapSite(site,'search',active?.nearest_road_distance_m?.[site.id]));
    bindLiveCoverageDrag(marker,()=>({slot:'route:'+site.id,id:site.id,kind:'router',antenna_height_m:liveCoverageSiteHeight(site.antenna_height_m,rf?.router?.height_agl_m??3),terminal_clutter:site.terminal_clutter}));
    bindRouteSiteInspector(marker,site.id,point=>{
      marker.setLatLng(mapPoint(site));
      pinMovedRouter(site,point);
    });
    marker.addTo(routeLayer);
  });
  links.forEach((link,index)=>{
    const a=sites[link.source_id],b=sites[link.target_id];
    if(a&&b&&[...mapPoint(a),...mapPoint(b)].every(Number.isFinite)){
      const line=L.polyline([mapPoint(a),mapPoint(b)],{...routeLinkMapStyle(link),weight:4}).addTo(routeLayer);
      routeLinkLayers[index]=line;
      line.bindTooltip(routeMapLinkTooltip(link));
      bindMapLink(line,link,index);
    }
    const row=document.createElement('tr'),reason=linkIssues(link),resultCell=document.createElement('span');resultCell.className=link.valid?'link-result pass':'link-result fail';resultCell.textContent=link.valid?'Pass':'Fail';resultCell.title=reason;const linkName=document.createElement('span');linkName.textContent=`${analysisSiteLabel(link.source_id)} → ${analysisSiteLabel(link.target_id)}`;linkName.title=`${link.source_id} → ${link.target_id}`;for(const value of [linkName,(link.distance_m/1000).toFixed(2)+' km',marginPill(link.worst_margin_db),link.minimum_fresnel_clearance_ratio===null?'—':(link.minimum_fresnel_clearance_ratio*100).toFixed(0)+'%',link.los_clear?'Clear':'Obstructed',resultCell]){const cell=document.createElement('td');if(value instanceof Node)cell.append(value);else cell.textContent=value;row.append(cell);}row.tabIndex=0;row.setAttribute('aria-selected',String(index===selected));row.onclick=()=>profile(index);row.onkeydown=event=>{if(event.key==='Enter'||event.key===' '){event.preventDefault();profile(index);}};$('hops').querySelector('tbody').append(row);
  });
  $('hops').hidden=false;$('hops-disclosure').hidden=false;$('exports').hidden=resultStale;status(resultStale?`Previous route is stale because ${resultStaleReason||'planning inputs'} changed. Run a new search before exporting.`:result.search_complete?'Calculation complete. Choose an alternative or link to inspect its profile.':`Certified route · ${result.achieved_path_count}/${result.requested_path_count} requested independent paths found to date.`);$('status').className=resultStale?'stale':'';showCandidates();if(links.length)profile(Math.min(selected,links.length-1));
}
function showCandidates(){candidateLayer.clearLayers();if($('show-candidates').checked&&result)result.candidates.filter(site=>site.origin!=='known'&&mapPoint(site).every(Number.isFinite)).forEach(site=>L.circleMarker(mapPoint(site),{radius:3,color:resultStale?'#68737b':'#778e7d',weight:1,interactive:false}).addTo(candidateLayer));}
$('show-candidates').onchange=showCandidates;
function formatMetric(value,digits=1){return Number.isFinite(value)?value.toFixed(digits):'—';}
function routeProfileLines(profileData,clearance){
  const curved=values=>values.map((value,i)=>value+profileData.earth_bulge_m[i]);
  return [
    {label:'Ground terrain',color:'#94785d',values:curved(profileData.dtm_elevation_m)},
    ...(profileData.surface_available?[{label:'Surface',color:'#508c62',values:curved(profileData.surface_elevation_m)}]:[]),
    {label:'Line of sight',color:'#457bab',values:profileData.los_elevation_m},
    {label:`${(clearance*100).toFixed(0)}% Fresnel lower edge`,color:'#b36e14',dash:[5,3],
      values:profileData.los_elevation_m.map((value,i)=>value-clearance*profileData.fresnel_radius_m[i])}
  ];
}
function drawRouteProfileChart(canvas,link,lines,height){
  const width=Math.max(200,canvas.clientWidth),ratio=devicePixelRatio||1;
  canvas.width=width*ratio;canvas.height=height*ratio;
  const ctx=canvas.getContext('2d');
  ctx.scale(ratio,ratio);
  let minimum=Infinity,maximum=-Infinity;
  for(const {values} of lines)for(const value of values){
    if(Number.isFinite(value)){minimum=Math.min(minimum,value);maximum=Math.max(maximum,value);}
  }
  minimum-=5;maximum+=5;
  const span=maximum-minimum,x=i=>45+(width-60)*i/Math.max(1,lines[0].values.length-1),y=value=>height-22-(value-minimum)/span*(height-34);
  ctx.font='12px Segoe UI';
  const ticks=height<120?3:5;
  for(let i=0;i<ticks;i++){
    const value=minimum+span*i/(ticks-1);
    ctx.fillStyle='#51655b';ctx.fillText(`${value.toFixed(0)} m`,0,y(value));
    ctx.strokeStyle='#dfe6df';ctx.beginPath();ctx.moveTo(43,y(value));ctx.lineTo(width,y(value));ctx.stroke();
  }
  for(const {values,color,dash=[]} of lines){
    ctx.strokeStyle=color;ctx.lineWidth=2;ctx.setLineDash(dash);ctx.beginPath();
    values.forEach((value,i)=>i?ctx.lineTo(x(i),y(value)):ctx.moveTo(x(i),y(value)));
    ctx.stroke();
  }
  ctx.fillStyle='#51655b';ctx.fillText('0 km',45,height-4);
  ctx.textAlign='right';ctx.fillText(`${(link.distance_m/1000).toFixed(2)} km`,width-2,height-4);
}
function renderLinkBudget(link,assumptions,clearance,clearanceRatio){
  const model=diffractionModelName(link.diffraction_model??assumptions.diffraction_model);
  const directionLabel=budget=>`${analysisSiteLabel(budget.source_id)} → ${analysisSiteLabel(budget.target_id)}`;
  $('budget-forward').textContent=`Forward · ${directionLabel(link.forward)}`;
  $('budget-reverse').textContent=`Reverse · ${directionLabel(link.reverse)}`;
  const body=$('budget-table').querySelector('tbody');
  body.replaceChildren();
  const fields=[
    ['TX power','tx_power_dbm','dBm'],
    ['Path loss','total_path_loss_db','dB'],
    ['Diffraction loss','diffraction_loss_db','dB'],
    ['Model correction','model_correction_db','dB'],
    ['Terminal clutter','terminal_clutter_db','dB'],
    ['Sampling allowance','step_allowance_db','dB'],
    ['Received','received_power_dbm','dBm'],
    ['Sensitivity','sensitivity_dbm','dBm'],
    ['Usable margin','usable_margin_db','dB']
  ];
  for(const [label,key,unit] of [['Diffraction model'],...fields]){
    const row=body.insertRow(),heading=mapText('th',label);
    heading.scope='row';row.append(heading);
    for(const budget of [link.forward,link.reverse])row.insertCell().textContent=key?`${formatMetric(budget[key])} ${unit}`:model;
  }
  const details=[
    ...fields,
    ['TX gain','tx_gain_dbi','dBi'],['TX feed loss','tx_feed_loss_db','dB'],
    ['Free-space loss','fspl_db','dB'],['Path clutter','clutter_loss_db','dB'],
    ['RX gain','rx_gain_dbi','dBi'],['RX feed loss','rx_feed_loss_db','dB'],
    ['Raw margin','raw_margin_db','dB'],
    ['Departure angle','departure_angle_deg','°'],['Arrival angle','arrival_angle_deg','°']
  ];
  const directions=[['Forward',link.forward],['Reverse',link.reverse]].map(([direction,budget])=>
    [`${direction} · ${directionLabel(budget)}`,...details.map(([label,key,unit])=>`${label} · ${formatMetric(budget[key])} ${unit}`)].join('\n'));
  const surface=assumptions.obstacle_surface==='surface'&&link.profile.surface_available?'Surface where available':'Ground';
  $('budget').textContent=[
    `Saved RF assumptions · ${formatMetric(assumptions.frequency_mhz)} MHz · k-factor ${formatMetric(assumptions.k_factor,2)} · Fresnel clearance ${(clearance*100).toFixed(0)}% · ${assumptions.validation_mode||'RF propagation'}`,
    `Diffraction model · ${model}\nDiffraction surface · ${surface}`,
    ...directions,
    `Link checks · ${linkIssues(link)} · ${clearanceRatio}`
  ].join('\n\n');
}
function profile(index,focusRow=false){
  if(!result?.links?.[index])return;selected=index;$('analysis-link-select').value=String(index);const link=result.links[index],profileData=link.profile;if(!profileData)return;$('profile').hidden=false;$('profile-title').textContent=`${analysisSiteLabel(link.source_id)} → ${analysisSiteLabel(link.target_id)} · ${(link.distance_m/1000).toFixed(2)} km`;
  document.querySelectorAll('#hops tbody tr').forEach((row,i)=>{row.classList.toggle('selected',i===index);row.setAttribute('aria-selected',String(i===index));if(i===index&&focusRow){row.focus({preventScroll:true});row.scrollIntoView({block:'nearest'});}});
  routeLinkLayers.forEach((line,i)=>line.setStyle({weight:i===index?6:4,opacity:i===index?1:.76}));
  const assumptions=result.rf_settings||rf,clearance=Number(assumptions.required_fresnel_clearance??rf.required_fresnel_clearance);
  drawRouteProfileChart($('chart'),link,routeProfileLines(profileData,clearance),230);
  const clearanceRatio=Number.isFinite(link.minimum_fresnel_clearance_ratio)?`${(link.minimum_fresnel_clearance_ratio*100).toFixed(1)}% minimum Fresnel clearance`: 'Fresnel clearance unavailable';
  const clearanceLocation=Number.isFinite(link.minimum_clearance_distance_m)?` at ${formatMetric(link.minimum_clearance_distance_m/1000,2)} km`:'';
  $('profile-description').textContent=`Terrain profile for ${analysisSiteLabel(link.source_id)} to ${analysisSiteLabel(link.target_id)}, ${(link.distance_m/1000).toFixed(2)} km. ${clearanceRatio}${clearanceLocation}. Fresnel boundary uses ${(clearance*100).toFixed(0)}% clearance from the saved RF assumptions. ${linkIssues(link)}.`;
  renderLinkBudget(link,assumptions,clearance,clearanceRatio);
}
const drawRouteProfile=profile;
profile=(index,focusRow=false)=>{const liveRf=rf;if(result?.rf_settings)rf={...rf,...result.rf_settings};try{return drawRouteProfile(index,focusRow);}finally{rf=liveRf;}};
window.addEventListener('resize',()=>{requestAnimationFrame(()=>{resizeMapViewport();invalidateChangedCoverageExtent();if(result?.links.length)profile(selected);});});
renderCoverageTargets();
updateCoverageTargetInputs();
initLiveCoverage();
void loadDataSources();
(async()=>{
  try{
    const data=await refresh();rf=data.rf;candidates=data.candidates;calibration=data.calibration;baseRf=clone(rf);baseCandidates=clone(candidates);
    if(data.plan)load(data.plan);else{buildSettings();syncTerrainControls();syncCoverageInputs();renderManualRouters();}
    renderCoverageLegend();renderCoverageSources();renderFailureControls();renderHeightScenarioControls();renderCoverageContext();lastHistoryPlan=clone(plan());undoStack=[];redoStack=[];setHistoryButtons();
    const progress={known:Number.isFinite(data.job.total)&&data.job.total>0,done:data.job.done||0,total:data.job.total||0};let restoredResult=false;
    if(data.job.terrain_estimate&&['queued','preparing'].includes(data.job.state)){
      activeJobId=data.job.job_id;activeJobKind='terrain';preparingTerrain=true;jobStartedAt=(data.job.started_at||Date.now()/1000)*1000;setBusy(true);updateJobCard({kind:'terrain',state:data.job.state,phase:data.job.stage||'Terrain preparation active',message:'Reconnecting to the active terrain preparation job.',progress:{...progress,unit:'tiles'}});void pollTerrainJob();
    }else if(['queued','running'].includes(data.job.state)){
      activeJobId=data.job.job_id;activeJobKind='route';lastSnapshotVersion=0;jobStartedAt=(data.job.started_at||Date.now()/1000)*1000;setBusy(true);updateJobCard({kind:'route',state:data.job.state,phase:data.job.stage||'Route search active',message:'Reconnecting to the active route search.',progress:{...progress,unit:'steps'}});void poll();
    }else if(['complete','stopped','failed','cancelled','interrupted'].includes(data.job.state)){
      activeJobId=data.job.job_id;activeJobKind='route';jobStartedAt=(data.job.started_at||Date.now()/1000)*1000;
      const completed=data.job.state==='complete',stopped=data.job.state==='stopped',failed=data.job.state==='failed';
      updateJobCard({kind:'route',state:data.job.state,phase:completed?'Last route search complete':stopped?'Search stopped with certified route':failed?'Last route search failed':data.job.state==='cancelled'?'Last route search cancelled':'Previous search interrupted',message:completed?'Loading the saved route result.':stopped?'The latest certified route was kept.':failed?data.job.stage||'The previous search failed.':data.job.state==='cancelled'?'The previous search was cancelled.':'The previous search did not finish before the server restarted.',progress:{...progress,unit:'steps'}});
      if(completed||stopped){restoredResult=await restoreProjectResult(data);if(restoredResult)updateJobCard({kind:'route',state:data.job.state,phase:completed?'Last route search complete':'Search stopped with certified route',message:result.found?'The saved certified route is ready for review.':'No viable route was found within the previous search limits.',progress:{...progress,unit:'steps'}});}
      if(restoredResult&&completed)$('route-job-card').hidden=true;
      activeJobKind=null;
    }
    if((data.projects.previous_result||data.job.state==='interrupted')&&!restoredResult)showPlanSummary(data);
    if(['failed','cancelled','interrupted'].includes(data.job.state))status(data.job.stage||$('job-message').textContent,data.job.state==='failed');
    if(data.coverage_terrain_job&&['queued','preparing'].includes(data.coverage_terrain_job.state)){
      coverageTerrainJobId=data.coverage_terrain_job.job_id;coveragePreflightBusy=true;
      syncCoverageLocks();renderCoverageTerrainProgress(data.coverage_terrain_job);void pollCoverageTerrain();
    }
    if(data.coverage_job){coverageJob=data.coverage_job;const active=['queued','running'].includes(coverageJob.state);coverageBusyState(active);$('coverage-cancel').hidden=!active;coverageCursor=0;pollCoverage(coverageJob.job_id);}
    if(terrainLoaded)void updateTerrainCoverage();await refreshCoverageRunSelectors();
  }catch(error){status(error.message,true);}
})();
