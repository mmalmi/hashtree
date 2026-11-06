const {chromium}=require(process.cwd()+'/node_modules/@playwright/test');
const fs=require('node:fs');
const assert=require('node:assert/strict');
const version=require(process.cwd()+'/node_modules/@playwright/test/package.json').version;assert.equal(version,'1.60.0');
assert.equal(process.version,JSON.parse(fs.readFileSync('/stage/evidence/image-toolchain.json')).node);
(async()=>{const browser=await chromium.launch({args:['--disable-features=WebRtcHideLocalIpsWithMdns','--allow-loopback-in-peer-connection']});try{
 assert.equal(browser.version(),'148.0.7778.96');
 const page=await browser.newPage();const denied=await page.evaluate(async()=>{
  const source=String.raw`(async()=>{const outcomes=[];for(const host of ['203.0.113.1','[2001:db8::1]']){try{await fetch('https://'+host+'/raw-worker-proof');outcomes.push({host,https:'escaped'});}catch(e){outcomes.push({host,https:'rejected',error:e.name});}outcomes.push(await new Promise(resolve=>{const s=new WebSocket('wss://'+host+'/raw-worker-proof');let failed=false;s.onerror=()=>{failed=true;};s.onopen=()=>{s.close();resolve({host,wss:'escaped'});};s.onclose=()=>resolve({host,wss:failed?'rejected':'close-without-error'});}));}postMessage(outcomes);})()`;
  return await new Promise((resolve,reject)=>{const url=URL.createObjectURL(new Blob([source],{type:'application/javascript'})),worker=new Worker(url);const timer=setTimeout(()=>{worker.terminate();URL.revokeObjectURL(url);reject(Error('Raw worker denial timed out'));},8000);worker.onmessage=e=>{clearTimeout(timer);worker.terminate();URL.revokeObjectURL(url);resolve(e.data);};worker.onerror=e=>{clearTimeout(timer);worker.terminate();URL.revokeObjectURL(url);reject(Error(e.message));};});
 });assert.equal(denied.length,4);assert(denied.every(v=>v.https==='rejected'||v.wss==='rejected'));
 const result=await page.evaluate(async()=>{
  const a=new RTCPeerConnection({iceServers:[]}),b=new RTCPeerConnection({iceServers:[]}),candidates=[];
  const pendingA=[],pendingB=[];
  a.onicecandidate=e=>{if(e.candidate){candidates.push({...e.candidate.toJSON(),address:e.candidate.address});if(b.remoteDescription)b.addIceCandidate(e.candidate);else pendingB.push(e.candidate);}};
  b.onicecandidate=e=>{if(e.candidate){candidates.push({...e.candidate.toJSON(),address:e.candidate.address});if(a.remoteDescription)a.addIceCandidate(e.candidate);else pendingA.push(e.candidate);}};
  try {
   const channel=a.createDataChannel('unsigned-loopback-proof');
   const roundtrip=new Promise((resolve,reject)=>{const timer=setTimeout(()=>reject(Error('Loopback browser datachannel unavailable')),12000);b.ondatachannel=e=>e.channel.onmessage=m=>e.channel.send(m.data);channel.onopen=()=>channel.send('loopback-only');channel.onmessage=e=>{clearTimeout(timer);resolve(e.data);};});
   await a.setLocalDescription(await a.createOffer());await b.setRemoteDescription(a.localDescription);for(const c of pendingB)await b.addIceCandidate(c);
   await b.setLocalDescription(await b.createAnswer());await a.setRemoteDescription(b.localDescription);for(const c of pendingA)await a.addIceCandidate(c);
   const echoed=await roundtrip;if(echoed!=='loopback-only')throw Error('Wrong echo');
   if(!candidates.length||candidates.some(c=>!['127.0.0.1','::1'].includes(c.address)))throw Error('Unexpected ICE candidates');
   return {echoed,candidates};
  }finally{a.close();b.close();}
 });fs.writeFileSync(process.argv[2],JSON.stringify({passed:true,node:process.version,playwright:version,browser:browser.version(),rawWorkerDenial:denied,...result},null,2)+'\n');console.log('PASS non-root Chromium unsigned loopback datachannel');
}finally{await browser.close();}})().catch(e=>{console.error(e);process.exitCode=1;});
