// Prepared for a root-reviewed isolated Linux browser launcher; never run locally.
import assert from 'node:assert/strict';
import {createRequire} from 'node:module';
import {readFile, writeFile, readlink, readdir} from 'node:fs/promises';
import {createServer} from 'node:http';
import {execFileSync} from 'node:child_process';
import path from 'node:path';
import {loadPayloads, sha} from './worker-response.mjs';
const [planPath, planSha, networkProof, outputDir] = process.argv.slice(2);
assert.equal(process.platform, 'linux');
assert.deepEqual(await readdir(outputDir), [], 'Preserve prior mobile evidence; use a fresh output directory');
const planBytes = await readFile(planPath); assert.equal(sha(planBytes), planSha);
const plan = JSON.parse(planBytes);
assert.equal(plan.productionVerified, false);
assert.equal(sha(await readFile(networkProof)), 'ad53bbfab4f7089b6981ed0e30b7544437bff6ba94dd04245d74b2acc2f7612f');
async function boundary(label) {
  const filename = path.join(outputDir, label + '.json');
  execFileSync('python3', [networkProof, filename], {env:{...process.env, FIPS_DUMMY:'0'}, timeout:15000});
  const result = JSON.parse(await readFile(filename));
  assert.deepEqual(result.interfaces, ['lo']);
  assert.equal(result.netNamespace, await readlink('/proc/self/ns/net'));
  assert.equal(result.effectiveAndBoundingCapabilities, 0);
  return result;
}
const before = await boundary('network-before');
const {files, expected} = await loadPayloads(plan);
const generatedResponses = [];
const payloads = new Map(), types = new Map();
for (const [name, item] of files) {
  const bytes = await expected(plan.site + '/' + name, item);
  const extension = name.split('.').at(-1);
  const contentType = name.includes('apple-app-site-association') ? 'application/json' : ({html:'text/html; charset=utf-8',js:'text/javascript',css:'text/css',json:'application/json',webmanifest:'application/manifest+json',wasm:'application/wasm',svg:'image/svg+xml',png:'image/png',jpg:'image/jpeg',jpeg:'image/jpeg',ico:'image/x-icon'}[extension] ?? 'application/octet-stream');
  payloads.set(name, bytes); types.set(name, contentType);
  generatedResponses.push({...item,servedBytes:bytes.length,servedSha256:sha(bytes),contentType});
}
const errors = [], requests = [];
const server = createServer((request, response) => {
  try {
    const url = new URL(request.url, 'http://local');
    if (url.pathname === '/__boundary') return response.writeHead(200, {'content-type':'text/html'}).end('<!doctype html><title>Boundary</title>');
    let name = decodeURIComponent(url.pathname).slice(1) || 'index.html';
    if (!payloads.has(name)) name = 'index.html';
    const bytes = payloads.get(name); requests.push({path:url.pathname, asset:name, sha256:sha(bytes)});
    response.writeHead(200, {'content-type':types.get(name), 'service-worker-allowed':'/'}).end(bytes);
  } catch { errors.push('fixture-response'); response.writeHead(500).end(); }
});
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
let browser;
try {
  const req = createRequire(path.join(plan.sourceRoot, 'package.json'));
  assert.equal(req('@playwright/test/package.json').version, '1.64.1');
  browser = await req('@playwright/test').chromium.launch({channel:'chromium'});
  const context = await browser.newContext({viewport:{width:390,height:844}, serviceWorkers:'allow'});
  const page = await context.newPage(), origin = 'http://127.0.0.1:' + server.address().port;
  await page.goto(origin + '/__boundary');
  const rawWorkerDenial = await page.evaluate(async () => {
    const code = `(async()=>{const out=[];for(const host of ['203.0.113.1','[2001:db8::1]']){try{await fetch('https://'+host+'/unsigned-boundary');out.push(false)}catch{out.push(true)}out.push(await new Promise(resolve=>{const s=new WebSocket('wss://'+host+'/unsigned-boundary');let error=false;s.onerror=()=>error=true;s.onopen=()=>{s.close();resolve(false)};s.onclose=()=>resolve(error)}))}postMessage(out)})()`;
    return await new Promise((resolve,reject) => {
      const url = URL.createObjectURL(new Blob([code], {type:'application/javascript'})), worker = new Worker(url);
      const close = () => {worker.terminate();URL.revokeObjectURL(url)};
      const timer = setTimeout(() => {close();reject(Error('Unsigned worker deadline'))},8000);
      worker.onmessage = event => {clearTimeout(timer);close();resolve(event.data)};
      worker.onerror = () => {clearTimeout(timer);close();reject(Error('Unsigned worker failure'))};
    });
  });
  assert.deepEqual(rawWorkerDenial, [true,true,true,true]);
  page.on('pageerror', () => errors.push('page-error'));
  await page.goto(origin, {waitUntil:'domcontentloaded'});
  await page.getByText('Secure, private messaging').waitFor({state:'visible',timeout:15000});
  const registration = await page.evaluate(() => Promise.race([navigator.serviceWorker.ready.then(r => ({scope:r.scope,active:r.active?.state})), new Promise((_,reject) => setTimeout(() => reject(Error('Service worker deadline')),20000))]));
  await page.reload({waitUntil:'domcontentloaded'});
  await page.getByText('Secure, private messaging').waitFor({state:'visible',timeout:15000});
  await page.waitForFunction(() => !!navigator.serviceWorker.controller, undefined, {timeout:10000});
  const geometry = await page.evaluate(() => ({width:innerWidth,documentWidth:document.documentElement.scrollWidth}));
  assert.equal(geometry.width, 390); assert(geometry.documentWidth <= geometry.width);
  await page.screenshot({path:path.join(outputDir,'mobile-online.png'),fullPage:true});
  await context.setOffline(true); await page.reload({waitUntil:'domcontentloaded'});
  await page.getByText('Secure, private messaging').waitFor({state:'visible',timeout:15000});
  await page.screenshot({path:path.join(outputDir,'mobile-offline.png'),fullPage:true});
  assert.deepEqual(errors, []); await context.close();
  const after = await boundary('network-after'); assert.equal(after.netNamespace, before.netNamespace);
  const screenshots = [];
  for (const file of ['mobile-online.png','mobile-offline.png']) {
    const bytes = await readFile(path.join(outputDir,file)); screenshots.push({file,bytes:bytes.length,sha256:sha(bytes)});
  }
  await writeFile(path.join(outputDir,'mobile-offline.json'), JSON.stringify({passed:true,source:plan.source,
    sourcePlanSha256:planSha,productionVerified:false,sourceWorkerResponseFiles:files.size,sourceWorkerResponses:generatedResponses,geometry,
    viewport:{width:390,height:844},browser:'Chromium desktop user agent at mobile viewport',
    offlineReload:true,serviceWorkerControlled:true,registration,anonymous:true,accountCreated:false,syntheticPosts:false,
    rawWorkerDenial,permissionsGranted:[],captureInvoked:false,pageErrors:errors,requests,screenshots,
    scope:'Exact retained dist rendered through the unchanged source Worker on isolated loopback before deployment; no live public-byte or NEG claim'},null,2)+'\n',{flag:'wx'});
} finally { await browser?.close(); await new Promise(resolve => server.close(resolve)); }
