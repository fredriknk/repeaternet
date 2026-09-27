/* Exact-point terrain and client-link inspection. Loaded after app.js. */
function cancelActiveInspection(){
  const jobId=activeInspectionId;
  activeInspectionId=null;
  if(jobId)api(`/api/coverage/inspect/${encodeURIComponent(jobId)}/cancel`,{method:'POST'}).catch(()=>{});
}

function enterCoverageInspect(){
  if(!result||resultStale||!result.search_complete){setCoverageStatus('Finish a route search before inspecting locations.',true);return;}
  inspectCoverage=!inspectCoverage;
  $('coverage-inspect').textContent=inspectCoverage?'Stop inspecting locations':'Inspect a map location';
  $('map-hint').textContent=inspectCoverage?'Click any location to inspect predicted handheld access':'Map inspection stopped';
  if(!inspectCoverage){inspectionRequest++;inspectionController?.abort();cancelActiveInspection();inspectionLayer.clearLayers();$('coverage-inspection').hidden=true;}
}

async function waitForInspection(jobId,requestId,signal){
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
    setCoverageStatus(`${job.stage} · ${job.source_count} router link(s) · client height ${coverageSettings.client.height_agl_m} m`);
    await new Promise((resolve,reject)=>{const done=()=>{signal.removeEventListener('abort',abort);resolve();},abort=()=>{clearTimeout(timer);signal.removeEventListener('abort',abort);reject(new DOMException('Aborted','AbortError'));},timer=setTimeout(done,250);signal.addEventListener('abort',abort,{once:true});if(signal.aborted)abort();});
  }
  return null;
}

async function inspectAt(latlng){
  const requestId=++inspectionRequest;
  inspectionController?.abort();
  cancelActiveInspection();
  inspectionController=new AbortController();
  inspectionLayer.clearLayers();
  const latitude=Number(latlng.lat.toFixed(6)),longitude=Number(latlng.lng.toFixed(6));
  setCoverageStatus(`Inspecting ${latitude.toFixed(5)}, ${longitude.toFixed(5)}…`);
  try{
    readCoverageInputs();
    await autosaveNow();
    const job=await api('/api/coverage/inspect',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({...coveragePayload(),latitude,longitude}),signal:inspectionController.signal});
    if(requestId!==inspectionRequest||!inspectCoverage)return;
    activeInspectionId=job.job_id;
    const response=await waitForInspection(job.job_id,requestId,inspectionController.signal);
    if(!response||requestId!==inspectionRequest||!inspectCoverage)return;
    renderCoverageInspection(response,latitude,longitude);
  }catch(error){if(error.name!=='AbortError'&&requestId===inspectionRequest)setCoverageStatus(error.message,true);}
}

function renderCoverageInspection(response,latitude,longitude){
  $('coverage-inspection').hidden=false;
  $('inspection-title').textContent=`${latitude.toFixed(5)}, ${longitude.toFixed(5)} · handheld at ${response.client.height_agl_m} m AGL`;
  const point=[latitude,longitude];
  if(response.state==='unknown_terrain'){
    $('inspection-summary').textContent=response.message;
    $('inspection-detail').textContent='No RF conclusion is available without ground terrain at this point.';
    $('inspection-chart').hidden=true;
    L.circleMarker(point,{radius:7,color:'#888',fillColor:'#b9c0bd',fillOpacity:.8}).addTo(inspectionLayer);
    setCoverageStatus('Unknown terrain at the selected location.');
    return;
  }
  if(response.state==='unknown_links'){
    $('inspection-summary').textContent=response.message||'Terrain data is missing along every candidate path; coverage is unknown.';
    $('inspection-detail').textContent='The client location has known ground elevation, but no router path could be evaluated because the path terrain is incomplete.';
    $('inspection-chart').hidden=true;
    L.circleMarker(point,{radius:7,color:'#888',fillColor:'#b9c0bd',fillOpacity:.8}).addTo(inspectionLayer);
    setCoverageStatus('Unknown terrain along all candidate router paths.');
    return;
  }
  const chosen=response.sources.find(source=>source.source_id===response.selected_source_id);
  const summary=chosen?`${chosen.source_id} · ${(chosen.distance_m/1000).toFixed(2)} km · downlink ${chosen.downlink_margin_db.toFixed(1)} dB · uplink ${chosen.uplink_margin_db.toFixed(1)} dB · ${chosen.valid_two_way?'two-way usable':'not usable in both directions'}`:'No candidate mesh router is available for this location.';
  $('inspection-summary').textContent=summary+(response.selected_source_component?` · selected mesh component ${response.selected_source_component} (${response.selected_source_component_size} node(s))`:'');
  $('inspection-chart').hidden=!response.profile_link?.profile;
  $('inspection-detail').textContent=`Ground ${response.ground_elevation_m.toFixed(1)} m · ${response.surface_sample_available?`surface ${response.surface_elevation_m.toFixed(1)} m`:'surface terrain unavailable here'}\n${response.sources.map(source=>source.rejection==='unknown_terrain'?`${source.source_id}: path terrain unknown`:`${source.source_id}: down ${source.downlink_margin_db.toFixed(1)} dB, up ${source.uplink_margin_db.toFixed(1)} dB · ${source.rejection||'usable'}`).join('\n')}`;
  L.circleMarker(point,{radius:7,color:'#193e72',fillColor:'#6da3df',fillOpacity:.9}).addTo(inspectionLayer);
  if(response.selected_source){const source=response.selected_source;L.polyline([[source.latitude,source.longitude],point],{color:chosen?.valid_two_way?'#176750':'#b44735',dashArray:'5 5',weight:3}).addTo(inspectionLayer);}
  if(response.profile_link?.profile)drawCoverageInspectionProfile(response.profile_link);
  const popup=document.createElement('div'),heading=document.createElement('strong'),copy=document.createElement('div');
  heading.textContent=chosen?chosen.source_id:'No usable mesh link';
  copy.textContent=chosen?`Downlink ${chosen.downlink_margin_db.toFixed(1)} dB · uplink ${chosen.uplink_margin_db.toFixed(1)} dB`:'Terrain was evaluated for some links, but no source link is usable.';
  popup.append(heading,copy);L.popup().setLatLng(point).setContent(popup).openOn(map);
  setCoverageStatus(response.selected_source_id?`Inspection complete. Best same-router two-way margin: ${chosen.two_way_margin_db.toFixed(1)} dB.`:'Inspection complete; no serving router found.');
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

const previousInvalidateForInspection=invalidate;
invalidate=()=>{cancelActiveInspection();previousInvalidateForInspection();};
const previousCoverageSettingsChanged=coverageSettingsChanged;
coverageSettingsChanged=event=>{cancelActiveInspection();previousCoverageSettingsChanged(event);};
const previousCoverageBusyState=coverageBusyState;
coverageBusyState=value=>{if(value)cancelActiveInspection();previousCoverageBusyState(value);};
const previousInstallProjectForInspection=installProject;
installProject=async response=>{cancelActiveInspection();return previousInstallProjectForInspection(response);};
$('coverage-inspect').onclick=enterCoverageInspect;
for(const id of ['coverage-mode','coverage-area','coverage-cell-size','coverage-buffer','coverage-profile-step','coverage-endpoints','client-height','client-tx','client-gain','client-feed','client-sensitivity','client-loss'])$(id).onchange=coverageSettingsChanged;
