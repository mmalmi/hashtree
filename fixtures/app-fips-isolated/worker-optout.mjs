import {createDenyProxy, assertWorkerNetworkBlocked} from './standalone-browser-boundary.ts';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { once } from 'node:events';
import { readFile, writeFile } from 'node:fs/promises';
import { createRequire } from 'node:module';
import path from 'node:path';
import { pathToFileURL } from 'node:url';
const app = process.argv[2], proof = process.argv[3];
const require = createRequire(path.join(app, 'package.json'));
const { chromium, expect } = require('@playwright/test');
const { SocialGraph } = require('nostr-social-graph');
const {WebSocketServer}=require('ws');
const upstreams=[],frames=[];const relay=new WebSocketServer({host:'127.0.0.1',port:0});await once(relay,'listening');
relay.on('connection',(ws,request)=>{upstreams.push(new URL(request.url,'http://local').searchParams.get('upstream'));ws.on('message',raw=>{let f;try{f=JSON.parse(String(raw));}catch{return;}frames.push(f);if(f[0]==='REQ')ws.send(JSON.stringify(['EOSE',f[1]]));});});
const relayUrl=`ws://127.0.0.1:${relay.address().port}`;
const prelude=`;(()=>{const Original=globalThis.WebSocket;globalThis.WebSocket=class extends Original{constructor(url,protocols){const original=String(url),u=new URL(original);super(u.protocol==='wss:'?${JSON.stringify(relayUrl)}+'/?upstream='+encodeURIComponent(original):url,protocols)}}})();`;
const { finalizeEvent, getPublicKey, nip19 } = require('nostr-tools');
const secret = new Uint8Array(32).fill(42), owner = getPublicKey(secret);
const graph = new SocialGraph(owner);
graph.handleEvent([3, 10000].map(kind => finalizeEvent({kind,created_at:300,content:'',tags:[['p',getPublicKey(new Uint8Array(32).fill(kind===3?43:44))]]},secret)),true);
const bytes = [...await graph.toBinary()];
const root=path.join(app,'dist');
const server=createServer(async(req,res)=>{
 const pathname=new URL(req.url,'http://localhost').pathname;
 if(pathname==='/__fixture'){res.end('<!doctype html><title>Fixture</title>');return;}
 if(pathname.startsWith('/htree/')){res.setHeader('content-type','text/html');res.end('<!doctype html><title>Local child</title><p>Local child</p>');return;}
 const file=path.resolve(root,`.${pathname==='/'?'/index.html':pathname}`);
 if(!file.startsWith(root+path.sep)){res.writeHead(403).end();return;}
 try{let body=await readFile(file);if(/relayWorkerEntry-.*\.js$/.test(file))body=Buffer.concat([Buffer.from(prelude),body]);res.setHeader('content-type', {'.html':'text/html','.js':'application/javascript','.css':'text/css','.wasm':'application/wasm'}[path.extname(file)]??'application/octet-stream');res.end(body);}catch{res.writeHead(404).end();}
}).listen(0,'127.0.0.1');await once(server,'listening');
const host=`fixture.${nip19.npubEncode(owner)}.iris.localhost:${server.address().port}`;
const boundary=await createDenyProxy();
const browser=await chromium.launch({headless:true,proxy:{server:boundary.url,bypass:'127.0.0.1,localhost,[::1],'+host}});
const context=await browser.newContext({serviceWorkers:'allow'});
let graphHttp=0;const graphFilters=[];const init=[];
try{
 await context.route('https://graph-api.iris.to/**',route=>{graphHttp++;return route.abort();});
 await context.addInitScript(prelude);
 await context.addInitScript(()=>{
  window.__workerPolicies=[];const OriginalWorker=window.Worker;
  window.Worker=class extends OriginalWorker{postMessage(message,...args){if(message?.type==='init')window.__workerPolicies.push({graphReconciliation:message.config?.graphReconciliation,relays:message.config?.relays});return super.postMessage(message,...args);}};
 });
 const rawPage=await context.newPage();await assertWorkerNetworkBlocked(rawPage,boundary);await rawPage.close();
 const page=await context.newPage();page.on('pageerror',error=>console.log('pageerror',error.message));page.on('response',response=>{if(response.status()>=400)console.log('httpfail',response.status(),new URL(response.url()).pathname);});await page.goto(`http://${host}/__fixture`);
 await page.evaluate(async bytes=>{
  const db=await new Promise((resolve,reject)=>{const r=indexedDB.open('hashtree-social-graph',10);r.onupgradeneeded=()=>r.result.createObjectStore('socialGraph',{keyPath:'id'});r.onsuccess=()=>resolve(r.result);r.onerror=()=>reject(r.error);});
  try{await new Promise((resolve,reject)=>{const tx=db.transaction('socialGraph','readwrite');tx.objectStore('socialGraph').put({id:'main',data:new Uint8Array(bytes),updatedAt:Date.now()});tx.oncomplete=resolve;tx.onerror=()=>reject(tx.error);});}finally{db.close();}
 },bytes);
 await page.goto(`http://${host}/index.html`,{waitUntil:'load'});
 await page.waitForTimeout(1500);
 console.log('diagnostic',JSON.stringify({url:page.url(),title:await page.title(),text:(await page.locator('body').innerText()).slice(0,400),workers:page.workers().length,upstreams,policies:await page.evaluate(()=>window.__workerPolicies)}));
 await page.screenshot({path:path.join(proof,'worker-diagnostic.png')});
 await expect.poll(()=>page.evaluate(()=>window.__workerPolicies),{timeout:10000}).toContainEqual(expect.objectContaining({graphReconciliation:false}));
 await expect.poll(()=>upstreams.some(url=>url?.replace(/\/+$/,'')==='wss://relay.ditto.pub')).toBe(true);
 await page.locator('iframe.site-frame').waitFor({state:'attached'});
 await page.waitForTimeout(5500);
 init.push(...await page.evaluate(()=>window.__workerPolicies));
 const after=await page.evaluate(async()=>{const db=await new Promise((resolve,reject)=>{const r=indexedDB.open('hashtree-social-graph');r.onsuccess=()=>resolve(r.result);r.onerror=()=>reject(r.error);});try{return await new Promise((resolve,reject)=>{const r=db.transaction('socialGraph').objectStore('socialGraph').get('main');r.onsuccess=()=>resolve([...r.result.data]);r.onerror=()=>reject(r.error);});}finally{db.close();}});
 for(const frame of frames){const filters=frame[0]==='REQ'?frame.slice(2):frame[0]==='NEG-OPEN'?[frame[2]]:[];for(const filter of filters)if(filter.kinds?.some(k=>k===3||k===10000))graphFilters.push(filter);}
 assert.equal(init[0].relays[0],'wss://relay.ditto.pub');assert.equal(graphHttp,0);assert.equal(frames.filter(f=>f[0]==='NEG-OPEN'||f[0]==='NEG-MSG').length,0);for(const filter of graphFilters){assert.deepEqual(filter.kinds,[3,10000]);assert(filter.authors.length<=65);assert(filter.authors.includes('336f319763657d6b0e65a5b5876719e8c8dcdcf9396852be71ee26b73368b29b'));}assert(graphFilters.length<=24);assert.deepEqual(after,bytes);
 await page.screenshot({path:path.join(proof,'worker-optout.png')});
 await writeFile(path.join(proof,'worker-optout.json'),JSON.stringify({status:'passed',init,upstreams,graphHttp,graphFilters,rawWorkerDenied:true,graphOptOutScope:'NIP77 catch-up and cold HTTP bootstrap; documented permanent live list subscription remains',retainedPersonalGraphBytes:bytes.length,retainedBytesUnchanged:true},null,2)+'\n');
 console.log('PASS actual Sites worker NEG/bootstrap opt-out; bounded pre-existing live lists retained; personal graph unchanged');
}finally{await context.close();await browser.close();for(const ws of relay.clients)ws.terminate();await new Promise(resolve=>relay.close(resolve));await new Promise(resolve=>server.close(resolve));await boundary.close();}
