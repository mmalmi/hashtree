const fs=require('node:fs'),http=require('node:http'),os=require('node:os'),path=require('node:path');
const assert=require('node:assert/strict'),crypto=require('node:crypto');
const app=process.cwd(),pw=require(app+'/node_modules/@playwright/test');
const {WebSocketServer}=require(app+'/node_modules/ws');
const version=require(app+'/node_modules/@playwright/test/package.json').version;
assert.equal(version,'1.64.1');
const core=path.dirname(require.resolve('playwright-core/package.json',{paths:[app+'/node_modules/@playwright/test']}));
assert.equal(JSON.parse(fs.readFileSync(core+'/package.json')).version,'1.64.1');
const browserMetadata=JSON.parse(fs.readFileSync(core+'/browsers.json'));
assert.deepEqual(browserMetadata,JSON.parse(fs.readFileSync('/stage/procedure/playwright-browsers.json')));
assert.equal(process.version,JSON.parse(fs.readFileSync('/stage/evidence/image-toolchain.json')).node);
const result={passed:false,node:process.version,playwright:version,browserMetadata,engines:{},captureInvoked:false};
const sha=p=>crypto.createHash('sha256').update(fs.readFileSync(p)).digest('hex');
result.image=JSON.parse(fs.readFileSync('/stage/procedure/manifest.json')).image;
result.packageFiles={playwrightTest:sha(app+'/node_modules/@playwright/test/package.json'),playwrightCore:sha(core+'/package.json'),browserMetadata:sha(core+'/browsers.json')};
function executables(){return [...new Set(fs.readdirSync('/proc').filter(p=>/^\d+$/.test(p)).flatMap(p=>{
 try{const executable=fs.readlinkSync('/proc/'+p+'/exe');return executable.startsWith('/ms-playwright/')?[executable]:[];}catch{return [];}
}))].map(path=>({path,sha256:sha(path)}));}
(async()=>{
 const server=http.createServer((_req,res)=>res.end('<!doctype html><title>Unsigned Chat boundary proof</title>'));
 const wss=new WebSocketServer({server,path:'/echo'});wss.on('connection',s=>s.on('message',(data,binary)=>s.send(data,{binary})));
 await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));
 const origin=`http://127.0.0.1:${server.address().port}`;
 try {
 for(const engine of ['chromium','firefox','webkit']){
  const record=result.engines[engine]={passed:false,engine,origin};let browser,context;
  try{
   if(engine==='webkit'){
    // Mirror the unchanged file suite's persistent iPhone context (OPFS enabled).
    context=await pw.webkit.launchPersistentContext(fs.mkdtempSync(path.join(os.tmpdir(),'chat-unsigned-webkit-')),{...pw.devices['iPhone 13'],baseURL:origin,acceptDownloads:true});
    browser=context.browser();record.contextMode='persistent-iPhone13';
   }else{
    browser=await pw[engine].launch(engine==='chromium'?{channel:'chromium',args:['--disable-features=WebRtcHideLocalIpsWithMdns','--allow-loopback-in-peer-connection']}:{});
    context=await browser.newContext();record.contextMode='standard';
   }
   assert(browser,'No browser identity');record.browser=browser.version();
   record.channel=engine==='chromium'?'chromium':null;
   const page=await context.newPage();
   if(engine==='chromium'){
    assert.equal(process.env.FIPS_DUMMY,'1');await context.grantPermissions(['microphone'],{origin});
   }
   await page.goto(origin+'/');
   record.executables=executables();assert(record.executables.length>0);
   const revision=browserMetadata.browsers.find(b=>b.name===engine).revision;
   assert(record.executables.some(e=>e.path.startsWith(`/ms-playwright/${engine}-${revision}/`)),'Expected engine revision executable missing');
   if(engine==='chromium')assert(record.executables.every(e=>!e.path.includes('headless_shell')),'Full Chromium channel required');
   record.metadataBrowserVersion=browserMetadata.browsers.find(b=>b.name===engine).browserVersion;
   // Record actual image binaries; upstream npm version metadata need not match a patched image.
   record.mediaPermission={requested:engine==='chromium',origin:engine==='chromium'?origin:null,captureInvoked:false};
   if(engine==='chromium'){
    record.mediaPermission.state=await page.evaluate(async()=>(await navigator.permissions.query({name:'microphone'})).state);
    assert.equal(record.mediaPermission.state,'granted');
   }
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
   record.loopbackWebSocket=await page.evaluate(async()=>await new Promise((resolve,reject)=>{
    const socket=new WebSocket(location.origin.replace('http:','ws:')+'/echo');
    const timer=setTimeout(()=>{socket.close();reject(Error('Unsigned local WebSocket unavailable'));},3000);
    socket.onopen=()=>socket.send('unsigned-local-echo');socket.onmessage=e=>{clearTimeout(timer);socket.close();resolve(e.data);};socket.onerror=()=>{clearTimeout(timer);socket.close();reject(Error('Unsigned local WebSocket failed'));};
   }));assert.equal(record.loopbackWebSocket,'unsigned-local-echo');
   if(engine==='chromium'){
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

   }else{
    record.rtc={required:false,reason:'Selected direct-file suite explicitly disables RTC and uses the loopback routed WebSocket seed; remaining WebKit projects exercise UI only'};
   }
   record.passed=true;
  }catch(error){record.error=String(error);}
  finally{await context?.close().catch(()=>{});await browser?.close().catch(()=>{});}
 }
 result.passed=Object.values(result.engines).every(r=>r.passed);
 assert(result.passed,JSON.stringify(Object.fromEntries(Object.entries(result.engines).map(([k,v])=>[k,v.error??'passed']))));
 console.log('PASS all three exact engines, raw-worker denials, local WebSocket and full-Chromium unsigned RTC');
 }finally{
  fs.writeFileSync(process.argv[2],JSON.stringify(result,null,2)+'\n');
  for(const socket of wss.clients)socket.terminate();await new Promise(resolve=>wss.close(resolve));await new Promise(resolve=>server.close(resolve));
 }
})().catch(error=>{console.error(error);process.exitCode=1;});
