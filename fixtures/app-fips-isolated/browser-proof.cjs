const {chromium}=require(process.cwd()+'/node_modules/@playwright/test');
const fs=require('node:fs'),http=require('node:http'),assert=require('node:assert/strict');
const version=require(process.cwd()+'/node_modules/@playwright/test/package.json').version;
assert.equal(version,'1.58.1');
assert.equal(process.version,JSON.parse(fs.readFileSync('/stage/evidence/image-toolchain.json')).node);
const crypto=require('node:crypto');
const record={passed:false,node:process.version,playwright:version};
(async()=>{
 const server=http.createServer((_req,res)=>res.end('<!doctype html><title>Unsigned namespace proof</title>'));
 await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));let browser;
 try {
  browser=await chromium.launch({args:['--disable-features=WebRtcHideLocalIpsWithMdns','--allow-loopback-in-peer-connection']});
  record.browser=browser.version();assert.equal(record.browser,'145.0.7632.6');
  record.actualBrowserExecutables=[];
  for(const pid of fs.readdirSync('/proc').filter(p=>/^\d+$/.test(p))){
   try{const status=fs.readFileSync('/proc/'+pid+'/status','utf8');if(Number(status.match(/^PPid:\s+(\d+)/m)?.[1])!==process.pid)continue;const exe=fs.readlinkSync('/proc/'+pid+'/exe');if(!exe.startsWith('/ms-playwright/'))continue;record.actualBrowserExecutables.push({pid:Number(pid),path:exe,sha256:crypto.createHash('sha256').update(fs.readFileSync(exe)).digest('hex')});}catch(error){if(error.code!=='ENOENT')throw error;}
  }
  assert.equal(record.actualBrowserExecutables.length,1,'Identify actual launched browser executable');
  const origin=`http://127.0.0.1:${server.address().port}`;
  const page=await browser.newPage();
  const permission=process.env.FIPS_MEDIA_PERMISSION==='1';
  if(permission){assert.equal(process.env.FIPS_DUMMY,'1');await page.context().grantPermissions(['microphone'],{origin});}
  await page.goto(origin+'/');
  record.mediaPermission={requested:permission,origin:permission?origin:null,captureInvoked:false,state:await page.evaluate(async()=>(await navigator.permissions.query({name:'microphone'})).state)};
  if(permission)assert.equal(record.mediaPermission.state,'granted');
  record.rawWorkerDenial=await page.evaluate(async()=>{
   const source=String.raw`(async()=>{const outcomes=[];for(const host of ['203.0.113.1','[2001:db8::1]']){try{await fetch('https://'+host+'/raw-worker-proof');outcomes.push({host,https:'escaped'});}catch(e){outcomes.push({host,https:'rejected',error:e.name});}outcomes.push(await new Promise(resolve=>{const s=new WebSocket('wss://'+host+'/raw-worker-proof');let failed=false;s.onerror=()=>{failed=true;};s.onopen=()=>{s.close();resolve({host,wss:'escaped'});};s.onclose=()=>resolve({host,wss:failed?'rejected':'close-without-error'});}));}postMessage(outcomes);})()`;
   return await new Promise((resolve,reject)=>{
    const url=URL.createObjectURL(new Blob([source],{type:'application/javascript'})),worker=new Worker(url);
    const close=()=>{worker.terminate();URL.revokeObjectURL(url);};
    const timer=setTimeout(()=>{close();reject(Error('Raw worker denial timed out'));},8000);
    worker.onmessage=e=>{clearTimeout(timer);close();resolve(e.data);};worker.onerror=e=>{clearTimeout(timer);close();reject(Error(e.message));};
   });
  });
  assert.equal(record.rawWorkerDenial.length,4);assert(record.rawWorkerDenial.every(v=>v.https==='rejected'||v.wss==='rejected'));
  record.rtc=await page.evaluate(async()=>{
   const a=new RTCPeerConnection({iceServers:[]}),b=new RTCPeerConnection({iceServers:[]}),candidates=[],candidateErrors=[],states=[];
   const pendingA=[],pendingB=[];
   const admit=(peer,candidate)=>peer.addIceCandidate(candidate).catch(e=>candidateErrors.push(e.message));
   a.onicecandidate=e=>{if(e.candidate){candidates.push({...e.candidate.toJSON(),address:e.candidate.address});if(b.remoteDescription)void admit(b,e.candidate);else pendingB.push(e.candidate);}};
   b.onicecandidate=e=>{if(e.candidate){candidates.push({...e.candidate.toJSON(),address:e.candidate.address});if(a.remoteDescription)void admit(a,e.candidate);else pendingA.push(e.candidate);}};
   const state=peer=>({connection:peer.connectionState,ice:peer.iceConnectionState,gathering:peer.iceGatheringState,signaling:peer.signalingState});
   for(const [name,peer] of [['a',a],['b',b]]){
    for(const event of ['connectionstatechange','iceconnectionstatechange','icegatheringstatechange','signalingstatechange'])peer.addEventListener(event,()=>states.push({name,event,at:performance.now(),...state(peer)}));
    peer.onicecandidateerror=e=>candidateErrors.push({name,url:e.url,address:e.address,code:e.errorCode,text:e.errorText});
   }
   try {
    const channel=a.createDataChannel('unsigned-loopback-proof');
    const roundtrip=new Promise(resolve=>{
     const timer=setTimeout(()=>resolve({error:'Loopback browser datachannel unavailable'}),12000);
     b.ondatachannel=e=>e.channel.onmessage=m=>e.channel.send(m.data);
     channel.onopen=()=>channel.send('loopback-only');channel.onmessage=e=>{clearTimeout(timer);resolve({echoed:e.data});};
    });
    await a.setLocalDescription(await a.createOffer());await b.setRemoteDescription(a.localDescription);for(const c of pendingB)await admit(b,c);
    await b.setLocalDescription(await b.createAnswer());await a.setRemoteDescription(b.localDescription);for(const c of pendingA)await admit(a,c);
    const result=await roundtrip;return {...result,candidates,candidateErrors,states,a:state(a),b:state(b),origin:location.origin};
   }catch(error){return {error:String(error),candidates,candidateErrors,states,a:state(a),b:state(b),origin:location.origin};}
   finally{a.close();b.close();}
  });
  record.candidateAddressesAllowed=record.rtc.candidates.every(c=>['127.0.0.1','::1','192.0.2.1'].includes(c.address));
  assert(record.candidateAddressesAllowed,'Unexpected ICE candidate address');
  assert.equal(record.rtc.echoed,'loopback-only',JSON.stringify(record.rtc));
  assert(record.rtc.candidates.length);
  const {grantIsolatedPermission}=require('/stage/procedure/permission-context.ts');
  const appEvidence=process.env.FIPS_APP_EVIDENCE;
  process.env.FIPS_APP_EVIDENCE=appEvidence+'/unsigned-permission-controls';fs.mkdirSync(process.env.FIPS_APP_EVIDENCE);
  record.permissionPreservation=[];
  try {
   for(const permissions of [undefined,[],['clipboard-read','clipboard-write'],['camera','microphone'],['geolocation']]){
    const context=await browser.newContext({permissions});
    try{await grantIsolatedPermission(context);record.permissionPreservation.push({originalPermissions:permissions??'unset',passed:true});}finally{await context.close();}
   }
   const late=await browser.newContext();
   try{
    await grantIsolatedPermission(late);
    await late.grantPermissions(['geolocation']);
    const probe=await late.newPage();const owned=process.env.FIPS_PERMISSION_ORIGIN+'/__late_permission_control';
    await probe.route(owned,r=>r.fulfill({contentType:'text/html',body:'<!doctype html>'}));await probe.goto(owned);
    const states=await probe.evaluate(async()=>Object.fromEntries(await Promise.all(['microphone','geolocation','camera','clipboard-read','clipboard-write'].map(async name=>[name,(await navigator.permissions.query({name})).state]))));
    assert.equal(states.microphone,'granted');assert.equal(states.geolocation,'granted');
    for(const name of ['camera','clipboard-read','clipboard-write'])assert.notEqual(states[name],'granted');
    record.permissionPreservation.push({originalPermissions:'late-global-geolocation',states,passed:true});
   }finally{await late.close();}
  } finally {process.env.FIPS_APP_EVIDENCE=appEvidence;}
  record.passed=true;console.log('PASS non-root Chromium raw-worker denial and unsigned local datachannel');
 }catch(error){record.error=String(error);throw error;}
 finally{fs.writeFileSync(process.argv[2],JSON.stringify(record,null,2)+'\n');await browser?.close();await new Promise(resolve=>server.close(resolve));}
})().catch(e=>{console.error(e);process.exitCode=1;});
