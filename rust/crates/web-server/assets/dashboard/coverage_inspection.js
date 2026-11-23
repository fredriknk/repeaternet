/* Exact-point terrain and client-link inspection. Loaded after app.js. */
let inspectionStartQueue=Promise.resolve();

function startCurrentInspection(payload,isCurrent){
  // Creating a job changes server state: aborting fetch can lose the job ID
  // without stopping that work. Serialize starts and cancel obsolete replies
  // before dispatching the next click, so arrival order matches click order.
  const request=inspectionStartQueue.then(async()=>{
    if(!isCurrent())return null;
    const job=await api('/api/coverage/inspect',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify(payload)});
    if(!isCurrent()){
      await api(`/api/coverage/inspect/${encodeURIComponent(job.job_id)}/cancel`,{method:'POST'}).catch(()=>{});
      return null;
    }
    return job;
  });
  inspectionStartQueue=request.catch(()=>{});
  return request;
}

function cancelActiveInspection(){
  const jobId=activeInspectionId;
  activeInspectionId=null;
  if(jobId)api(`/api/coverage/inspect/${encodeURIComponent(jobId)}/cancel`,{method:'POST'}).catch(()=>{});
}

function enterCoverageInspect(){
  if(!result||resultStale||!result.found||!result.active_alternative_id){setCoverageStatus('Find a certified route before inspecting locations.',true);return;}
  const display=coverageDisplayInfo||coverageJob;
  if(display&&(display.snapshot_version!==result.snapshot_version||display.alternative_id!==result.active_alternative_id||(display.stale&&!display.route_current)||display.route_current===false)){
    setCoverageStatus('The displayed coverage belongs to a stale or different route snapshot. Load coverage for the current route before inspecting locations.',true);return;
  }
  const selectedRunId=$('coverage-active-run').value;
  if(selectedRunId&&!display?.preview&&display?.job_id!==selectedRunId){
    setCoverageStatus('Load the selected saved run on the map before inspecting a location, so its client and router-height assumptions are used.',true);return;
  }
  setInteractionMode(interactionMode==='inspect-coverage'?'browse':'inspect-coverage');
}

async function waitForInspection(jobId,requestId,signal,showStatus=setCoverageStatus){
  while(requestId===inspectionRequest){
    const job=await api(`/api/coverage/inspect/${encodeURIComponent(jobId)}`,{signal});
    if(requestId!==inspectionRequest)return null;
    if(job.state==='complete'){
      activeInspectionId=null;
      if(job.stale)throw Error('The route, terrain, or client settings changed; inspect again.');
      return job.result;
    }
    if(job.state==='failed')throw Error(job.stage);
    if(['cancelled','superseded'].includes(job.state)){activeInspectionId=null;return null;}
    const client=(coverageDisplayInfo||coverageJob)?.settings?.client||coverageSettings.client;
    showStatus(`${job.stage} · ${job.source_count} router link(s) · client height ${client.height_agl_m} m${(coverageDisplayInfo||coverageJob)?.stale?' · saved historical run assumptions':''}`);
    await new Promise((resolve,reject)=>{const done=()=>{signal.removeEventListener('abort',abort);resolve();},abort=()=>{clearTimeout(timer);signal.removeEventListener('abort',abort);reject(new DOMException('Aborted','AbortError'));},timer=setTimeout(done,250);signal.addEventListener('abort',abort,{once:true});if(signal.aborted)abort();});
  }
  return null;
}

async function inspectAt(latlng){
  const requestId=++inspectionRequest;
  inspectionController?.abort();
  cancelActiveInspection();
  const controller=new AbortController();
  inspectionController=controller;
  const isCurrent=()=>requestId===inspectionRequest&&interactionMode==='inspect-coverage'&&!controller.signal.aborted;
  inspectionLayer.clearLayers();
  const latitude=Number(latlng.lat.toFixed(6)),longitude=Number(latlng.lng.toFixed(6));
  setCoverageStatus(`Inspecting ${latitude.toFixed(5)}, ${longitude.toFixed(5)}…`);
  try{
    readCoverageInputs();
    await autosaveNow();
    if(!isCurrent())return;
    const display=coverageDisplayInfo||coverageJob,selectedRunId=$('coverage-active-run').value;
    const savedRun=display?.job_id&&display.job_id===selectedRunId?display:null;
    const payload=coveragePayload(savedRun?.settings||coverageSettings,savedRun?.source_height_overrides);
    if(savedRun)payload.coverage_job_id=savedRun.job_id;
    const job=await startCurrentInspection({...payload,latitude,longitude},isCurrent);
    if(!job)return;
    if(!isCurrent()){
      await api(`/api/coverage/inspect/${encodeURIComponent(job.job_id)}/cancel`,{method:'POST'}).catch(()=>{});
      return;
    }
    activeInspectionId=job.job_id;
    const response=await waitForInspection(job.job_id,requestId,controller.signal);
    if(!response||!isCurrent())return;
    setMainView('view-analysis');
    renderCoverageInspection(response,latitude,longitude);
    $('coverage-inspection').scrollIntoView({block:'start'});
    $('inspection-title').focus({preventScroll:true});
  }catch(error){if(error.name!=='AbortError'&&requestId===inspectionRequest)setCoverageStatus(error.message,true);}
}

function renderCoverageInspection(response,latitude,longitude){
  $('coverage-inspection').hidden=false;
  const selectedRun=$('coverage-active-run').selectedOptions[0]?.textContent;
  $('inspection-title').textContent=`${selectedRun||'Current mesh'} · ${latitude.toFixed(5)}, ${longitude.toFixed(5)} · handheld at ${response.client.height_agl_m} m AGL`;
  const point=[latitude,longitude];
  if(response.state==='unknown_terrain'){
    noteCoverageTerrainGap();
    $('inspection-summary').textContent=response.message;
    $('inspection-detail').textContent='No RF conclusion is available without ground terrain at this point.';
    $('inspection-chart').hidden=true;
    L.circleMarker(point,{radius:7,color:'#888',fillColor:'#b9c0bd',fillOpacity:.8}).addTo(inspectionLayer);
    setCoverageStatus('Unknown terrain at the selected location.');
    return;
  }
  if(response.state==='unknown_links'){
    noteCoverageTerrainGap();
    $('inspection-summary').textContent=response.message||'Terrain data is missing along every candidate path; coverage is unknown.';
    $('inspection-detail').textContent='The client location has known ground elevation, but no router path could be evaluated because the path terrain is incomplete.';
    $('inspection-chart').hidden=true;
    L.circleMarker(point,{radius:7,color:'#888',fillColor:'#b9c0bd',fillOpacity:.8}).addTo(inspectionLayer);
    setCoverageStatus('Unknown terrain along all candidate router paths.');
    return;
  }
  const unresolved=response.sources.filter(source=>source.rejection==='profile_sample_limit');
  if(response.state==='unresolved'){
    $('inspection-summary').textContent=response.message||'No candidate path fits the configured terrain-profile sample limit.';
    $('inspection-detail').textContent=response.sources.map(source=>`${source.source_id}: ${source.rejection_detail||'profile sample limit exceeded'}`).join('\n');
    $('inspection-chart').hidden=true;
    L.circleMarker(point,{radius:7,color:'#765497',fillColor:'#b8a4d1',fillOpacity:.85}).addTo(inspectionLayer);
    setCoverageStatus('Inspection unresolved: raise the profile step or sample limit to evaluate these paths.');
    return;
  }
  const chosen=response.sources.find(source=>source.source_id===response.selected_source_id);
  const summary=chosen?`${chosen.source_id} · ${(chosen.distance_m/1000).toFixed(2)} km · downlink ${chosen.downlink_margin_db.toFixed(1)} dB · uplink ${chosen.uplink_margin_db.toFixed(1)} dB · ${chosen.valid_two_way?'two-way usable':'not usable in both directions'}`:'No candidate mesh router is available for this location.';
  $('inspection-summary').textContent=summary+(response.selected_source_component?` · selected mesh component ${response.selected_source_component} (${response.selected_source_component_size} node(s))`:'')+(unresolved.length?` · ${unresolved.length} other router path(s) unresolved by the profile sample limit`:response.message?` · ${response.message}`:'');
  $('inspection-chart').hidden=!response.profile_link?.profile;
  $('inspection-detail').textContent=`Ground ${response.ground_elevation_m.toFixed(1)} m · ${response.surface_sample_available?`surface ${response.surface_elevation_m.toFixed(1)} m`:'surface terrain unavailable here'}\n${response.sources.map(source=>source.rejection==='unknown_terrain'?`${source.source_id}: path terrain unknown`:source.rejection==='profile_sample_limit'?`${source.source_id}: ${source.rejection_detail||'profile sample limit exceeded'}`:`${source.source_id}: down ${source.downlink_margin_db.toFixed(1)} dB, up ${source.uplink_margin_db.toFixed(1)} dB · ${source.rejection||'usable'}`).join('\n')}`;
  L.circleMarker(point,{radius:7,color:'#193e72',fillColor:'#6da3df',fillOpacity:.9}).addTo(inspectionLayer);
  if(response.selected_source){const source=response.selected_source;L.polyline([[source.latitude,source.longitude],point],{color:chosen?.valid_two_way?'#176750':'#b44735',dashArray:'5 5',weight:3}).addTo(inspectionLayer);}
  if(response.profile_link?.profile)drawCoverageInspectionProfile(response.profile_link);
  const popup=document.createElement('div'),heading=document.createElement('strong'),copy=document.createElement('div');
  heading.textContent=chosen?chosen.source_id:'No usable mesh link';
  copy.textContent=chosen?`Downlink ${chosen.downlink_margin_db.toFixed(1)} dB · uplink ${chosen.uplink_margin_db.toFixed(1)} dB`:'Terrain was evaluated for some links, but no source link is usable.';
  popup.append(heading,copy);L.popup().setLatLng(point).setContent(popup).openOn(map);
  setCoverageStatus(response.selected_source_id?`Inspection complete. Best same-router two-way margin: ${chosen.two_way_margin_db.toFixed(1)} dB.${unresolved.length?` ${unresolved.length} other path(s) remain unresolved.`:''}`:unresolved.length?'Inspection incomplete: candidate paths exceed the configured profile sample limit.':'Inspection complete; no serving router found.');
}

function drawCoverageInspectionProfile(link){
  const p=link.profile,canvas=$('inspection-chart'),width=Math.max(200,canvas.clientWidth),height=190,ratio=devicePixelRatio||1;
  canvas.width=width*ratio;canvas.height=height*ratio;
  const ctx=canvas.getContext('2d');ctx.scale(ratio,ratio);
  const ground=p.dtm_elevation_m.map((v,i)=>v+p.earth_bulge_m[i]),surface=p.surface_elevation_m.map((v,i)=>v+p.earth_bulge_m[i]),fresnel=p.los_elevation_m.map((v,i)=>v-rf.required_fresnel_clearance*p.fresnel_radius_m[i]);
  const all=[...ground,...(p.surface_available?surface:[]),...p.los_elevation_m,...fresnel].filter(Number.isFinite),min=Math.min(...all)-8,max=Math.max(...all)+8,span=max-min||1,x=i=>42+(width-50)*i/Math.max(1,ground.length-1),y=v=>height-22-(v-min)/span*(height-35);
  ctx.font='10px Segoe UI';
  for(let i=0;i<4;i++){const value=min+span*i/3;ctx.fillStyle='#88968c';ctx.fillText(`${value.toFixed(0)}m`,0,y(value));ctx.strokeStyle='#dfe6df';ctx.beginPath();ctx.moveTo(40,y(value));ctx.lineTo(width,y(value));ctx.stroke();}
  for(const [values,color] of [[ground,'#94785d'],...(p.surface_available?[[surface,'#508c62']]:[]),[p.los_elevation_m,'#457bab'],[fresnel,'#d69a45']]){ctx.strokeStyle=color;ctx.lineWidth=2;ctx.beginPath();values.forEach((value,i)=>i?ctx.lineTo(x(i),y(value)):ctx.moveTo(x(i),y(value)));ctx.stroke();}
  ctx.fillStyle='#88968c';ctx.fillText('0 km',42,height-4);ctx.fillText(`${(link.distance_m/1000).toFixed(2)} km`,width-54,height-4);
}

$('coverage-inspect').onclick=enterCoverageInspect;


let coverageProbe=null;

function mapProbeContext(){
  const display=coverageDisplayInfo||coverageJob;
  if(map.hasLayer(predictedCoverageLayer)&&predictedCoverageLayer.getLayers().length&&
    (display?.preview||display?.state==='complete')&&coverageRouteReady()&&display.route_current!==false){
    return {kind:'saved',display,client:display.settings.client,mode:coverageSettings.mode};
  }
  if(liveCoverage.layer&&map.hasLayer(liveCoverage.layer)&&liveCoverageVisibleRasters().length){
    return {kind:'live',client:clone(coverageSettings.client),mode:liveCoverage.settings.mode};
  }
  return null;
}
function appendMapProbeAction(menu,point){
  if(!mapProbeContext())return;
  menu.append(mapPopupButton('Analyse links from here',()=>{
    map.closePopup(mapContextPopup);
    void openCoverageProbe(point);
  }));
}
async function openCoverageProbe(point){
  const context=mapProbeContext();
  if(!context||mapEditsLocked())return;
  if(interactionMode!=='browse')setInteractionMode('browse');
  closeMapSelection();
  const controller=new AbortController(),layer=L.layerGroup().addTo(map);
  const probe={...context,point,controller,layer,panels:[],projectId:projectInfo?.active_id,rows:[]};
  coverageProbe=probe;
  L.circleMarker(point,{radius:8,color:'#233d70',fillColor:'#fff',fillOpacity:1,weight:3,interactive:false}).addTo(layer);
  showMapDetails('Handheld probe',point,()=>map.getContainer(),content=>appendCoverageProbe(content,probe),
    ()=>clearCoverageProbe(probe));
  try{
    const rows=probe.kind==='saved'?await savedProbeLinks(probe):await liveProbeLinks(probe);
    if(coverageProbe!==probe||!rows)return;
    probe.rows=rows.sort((a,b)=>b.margin-a.margin);
    for(const row of probe.rows){
      const style=routeLinkMapStyle({valid:true,worst_margin_db:row.margin},false);
      L.polyline([mapPoint(row.site),point],{...style,weight:3,dashArray:'8 4 2 4',interactive:false}).addTo(layer);
    }
    updateCoverageProbe(probe,probe.rows.length?'':probe.message||'No usable links at this location.');
  }catch(error){
    if(coverageProbe===probe&&error.name!=='AbortError')updateCoverageProbe(probe,error.message);
  }
}
function clearCoverageProbe(probe){
  if(coverageProbe!==probe)return;
  coverageProbe=null;
  probe.controller.abort();
  probe.profileController?.abort();
  if(probe.kind==='saved'){
    inspectionRequest++;
    cancelActiveInspection();
  }
  probe.layer.remove();
}
function appendCoverageProbe(content,probe){
  appendMapFacts(content,[
    ['Coordinates',coordinateLabel([probe.point.lat,probe.point.lng])],
    ['Handheld height · m',formatMetric(probe.client.height_agl_m)],
    ['Handheld gain · dBi',formatMetric(probe.client.gain_dbi)]
  ]);
  const status=mapText('p','Checking links…'),list=document.createElement('div'),actions=document.createElement('div');
  status.setAttribute('role','status');
  actions.className='map-editor-actions';
  actions.append(
    mapEditControl(mapPopupButton('Add proposed router',()=>{
      if(mapEditsLocked())return;
      const router=addManualRouterAt(probe.point.lat,probe.point.lng);
      if(router)mapSiteElement(router.id)?.focus({preventScroll:true});
    })),
    mapEditControl(mapPopupButton('Add coverage target',async()=>{
      if(mapEditsLocked()||probe.addingTarget)return;
      probe.addingTarget=true;
      try{
        await savePointCoverageTarget(probe.point.lat,probe.point.lng,'Coverage target');
        if(coverageProbe===probe){
          closeMapSelection(true);
          setMapHint('Coverage target saved.',true);
        }
      }catch(error){
        if(coverageProbe===probe)updateCoverageProbe(probe,error.message);
      }finally{
        probe.addingTarget=false;
      }
    }))
  );
  content.append(status,list,actions);
  probe.panels.push({status,list});
}
function updateCoverageProbe(probe,message){
  for(const {status,list} of probe.panels){
    status.textContent=message;
    status.hidden=!message;
    list.replaceChildren();
    for(const row of probe.rows){
      const item=document.createElement('article'),overview=document.createElement('div'),profileBody=document.createElement('div');
      overview.className='map-probe-link';
      overview.append(mapText('strong',row.label),marginPill(row.margin),
        mapText('span',formatMetric(row.distance/1000,2)+' km'),
        mapPopupButton('Show profile',()=>void openCoverageProbeProfile(probe,row,profileBody)));
      item.append(overview,profileBody);
      list.append(item);
    }
  }
  resizeCoverageProbePopup();
}
function resizeCoverageProbePopup(){
  const focused=document.activeElement;
  const popup=mapObjectPopup;
  const restore=popup?.getElement()?.contains(focused);
  popup?.update();
  if(restore&&focused.checkVisibility())focused.focus({preventScroll:true});
  keepMapDetailsInView();
}
function coverageProbeMargin(mode,source){
  if(mode==='downlink')return source.valid_downlink?source.downlink_margin_db:null;
  if(mode==='uplink')return source.valid_uplink?source.uplink_margin_db:null;
  return source.valid_two_way?source.two_way_margin_db:null;
}
function coverageProbeLabel(site){
  return findMapSite(site.id)?.label||site.name||routeSiteLabel(site.id);
}
async function savedProbeInspection(probe,sourceId,signal){
  const requestId=++inspectionRequest;
  cancelActiveInspection();
  const settings=probe.display.settings;
  const payload=coveragePayload(settings,probe.display.source_height_overrides);
  if(!probe.display.preview)payload.coverage_job_id=probe.display.job_id;
  if(sourceId)payload.profile_source_id=sourceId;
  payload.latitude=probe.point.lat;
  payload.longitude=probe.point.lng;
  const isCurrent=()=>coverageProbe===probe&&requestId===inspectionRequest&&!signal.aborted;
  const job=await startCurrentInspection(payload,isCurrent);
  if(!job||!isCurrent())return null;
  activeInspectionId=job.job_id;
  return waitForInspection(job.job_id,requestId,signal,()=>{});
}
async function savedProbeLinks(probe){
  const response=await savedProbeInspection(probe,null,probe.controller.signal);
  if(!response)return null;
  probe.message=response.message;
  probe.servingProfile=response.profile_link;
  const sites=new Map((response.source_sites||[]).map(site=>[site.id,site]));
  return response.sources.flatMap(source=>{
    const site=sites.get(source.source_id);
    const margin=coverageProbeMargin(probe.mode,source);
    return site&&margin!==null&&margin>=0?[{site,label:coverageProbeLabel(site),margin,distance:source.distance_m}]:[];
  });
}
async function liveProbeLinks(probe){
  const visible=liveCoverageSites().filter(site=>!liveCoverage.hidden.has(site.slot)&&
    map.distance(probe.point,mapPoint(site))<=liveCoverage.settings.radius_km*1000);
  const rows=[];
  let next=0,failures=0;
  const evaluate=async()=>{
    while(next<visible.length&&!probe.controller.signal.aborted){
      const site=visible[next++];
      try{
        const link=await liveProbeLink(probe,site,false,probe.controller.signal);
        const clear=rf.validation_mode!=='strict_los'||(link.los_clear&&link.fresnel_clear);
        const margin=coverageProbeMargin(probe.mode,{
          valid_downlink:clear&&link.forward.valid,downlink_margin_db:link.forward.usable_margin_db,
          valid_uplink:clear&&link.reverse.valid,uplink_margin_db:link.reverse.usable_margin_db,
          valid_two_way:link.valid,two_way_margin_db:link.worst_margin_db
        });
        if(margin!==null&&margin>=0)rows.push({site,label:site.label,margin,distance:link.distance_m});
      }catch(error){
        if(error.name==='AbortError')return;
        failures++;
      }
    }
  };
  await Promise.all([evaluate(),evaluate()]);
  if(failures===visible.length&&failures)probe.message='Terrain data is missing at this point or along the candidate links.';
  return rows;
}
function liveProbeLink(probe,site,profile,signal){
  const target={id:'coverage-probe',kind:'client',latitude:probe.point.lat,longitude:probe.point.lng,
    antenna_height_m:probe.client.height_agl_m};
  return requestMapLink(site,target,{projectId:probe.projectId,client:probe.client,profile,signal});
}
async function openCoverageProbeProfile(probe,row,body){
  probe.profileController?.abort();
  const controller=new AbortController();
  probe.profileController=controller;
  body.textContent='Loading terrain profile…';
  try{
    const profile=probe.kind==='live'?await liveProbeLink(probe,row.site,true,controller.signal):
      probe.servingProfile?.source_id===row.site.id?probe.servingProfile:
        (await savedProbeInspection(probe,row.site.id,controller.signal))?.profile_link;
    if(coverageProbe!==probe||controller.signal.aborted)return;
    if(profile?.profile)renderRouteInspectorLink({body},profile);
    else body.textContent='Profile unavailable for this link.';
    if(mapObjectPopup?.getElement()?.contains(body))resizeCoverageProbePopup();
  }catch(error){
    if(coverageProbe===probe&&error.name!=='AbortError')body.textContent=error.message;
  }finally{
    if(controller.signal.aborted&&body.textContent==='Loading terrain profile…')body.textContent='';
  }
}
