import fs from 'node:fs';
import {spawnSync} from 'node:child_process';
import {pathToFileURL} from 'node:url';
const app=process.env.APP, out='/stage/evidence', wd='/stage/app';
const {createReleasePlan,parseArgs}=await import(pathToFileURL(wd+'/scripts/release-site.mjs'));
const args=app==='contacts'?['contacts','--dry-run','--skip-cloudflare']:['--dry-run','--skip-cloudflare'];
const {steps}=createReleasePlan(parseArgs(args));
const allowed=steps.filter(s=>s.id==='build'||s.id.startsWith('test-'));
if(!allowed.length||!allowed.some(s=>s.id.startsWith('test-')))throw new Error('Missing original release profile');
const [build,...tests]=allowed;
const focused=app==='social'?['pnpm','exec','playwright','test','tests/public-graph-bootstrap.spec.ts','--project=chromium','--workers=1','--retries=0']:['node','./scripts/smoke-negentropy-graph.mjs',app];
const actual=[build,{id:'test-signed-graph',command:focused},...tests.filter(s=>s.command.join(' ')!==focused.join(' '))];
if(app==='contacts')actual.push({id:'test-runtime',command:['pnpm','run','test:runtime']});
let failed=false;
for(const s of actual){
 const [command,...argv]=s.command;
 const fd=fs.openSync(`${out}/${s.id}.log`,'w');
 const environment={...process.env,...s.env};
 if(app==='contacts' && s.id==='test-runtime') {
   // Chromium reveals a usable WebRTC host candidate on the synthetic isolated
   // interface after the previously qualified, origin-scoped permission grant.
   const config=wd+'/playwright.runtime.config.ts';
   const original=fs.readFileSync(config,'utf8');
   if(!original.includes('export default defineConfig(')) throw new Error('Unknown Playwright runtime configuration');
   fs.writeFileSync(config,original.replace('export default defineConfig(', 'const originalRuntimeConfig=defineConfig(')+`
export default {...originalRuntimeConfig, use:{...originalRuntimeConfig.use, launchOptions:{args:['--disable-features=WebRtcHideLocalIpsWithMdns','--allow-loopback-in-peer-connection']}}};
`);
   Object.assign(environment,{NODE_OPTIONS:'--import=/stage/procedure/deny-browser-preload.mts',FIPS_APP_EVIDENCE:out,FIPS_PERMISSION_ORIGIN:'http://127.0.0.1:49314',BROWSER_BOUNDARY_RECEIPT:out+'/peer-browser.jsonl',PLAYWRIGHT_DISABLE_FORCED_CHROMIUM_PROXIED_LOOPBACK:'1'});
 }
 const r=spawnSync(command,argv,{cwd:wd,env:environment,stdio:['ignore',fd,fd]});fs.closeSync(fd);
 const report={id:s.id,command:s.command,status:r.status,signal:r.signal,error:r.error?.message};
 console.log(JSON.stringify(report));fs.appendFileSync(out+'/gates.jsonl',JSON.stringify(report)+'\n');
 if(r.status!==0){const log=fs.readFileSync(`${out}/${s.id}.log`,'utf8');console.error(log.slice(-3200));failed=true;if(s.id==='build')break;}
}
if(failed)process.exit(1);
