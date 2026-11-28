import hashlib,http.server,json,os,pathlib,subprocess,tarfile,threading,time
root=pathlib.Path.cwd();stage=pathlib.Path('/stage');evidence=stage/'evidence/social-final';evidence.mkdir();spec=json.loads((stage/'procedure/manifest.json').read_text())['apps'][0];tmp=stage/'task-tmp';tmp.mkdir(exist_ok=True);env=dict(os.environ,CI='1',TMPDIR=str(tmp),**spec['env']);records=[]
sha=lambda b:hashlib.sha256(b).hexdigest()
def stats():
 return {p:pathlib.Path('/sys/fs/cgroup',p).read_text() for p in ['cpu.stat','cpu.max','cpuset.cpus.effective','memory.peak','memory.events'] if pathlib.Path('/sys/fs/cgroup',p).exists()}
def run(cmd,name,*,cwd=root,extra=None):
 start=time.monotonic();before=stats()
 with (evidence/(name+'.log')).open('wb') as out:
  r=subprocess.run(cmd,cwd=cwd,env=dict(env,QUALIFICATION_NAME=name,**(extra or {})),stdout=out,stderr=subprocess.STDOUT,timeout=800)
 records.append({'name':name,'exit':r.returncode,'seconds':time.monotonic()-start,'cgroupBefore':before,'cgroupAfter':stats()});(evidence/'checks.json').write_text(json.dumps({'source':spec['commit'],'baseline':spec['baseline'],'records':records},indent=2)+'\n');return r.returncode
class Reject(http.server.BaseHTTPRequestHandler):
 def reject(self): self.send_error(403)
 do_CONNECT=do_GET=do_POST=reject
 def log_message(self,*args):pass
proxy=http.server.ThreadingHTTPServer(('127.0.0.1',0),Reject);threading.Thread(target=proxy.serve_forever,daemon=True).start();env['REJECT_PROXY']='http://127.0.0.1:'+str(proxy.server_port)
assert run(['python3','/stage/procedure/network-proof.py',str(evidence/'network.json')],'network')==0
assert run(['node','/stage/procedure/browser-proof.cjs',str(evidence/'browser.json')],'browser')==0

run(['node','scripts/verify-dependency-lock.mjs'],'verify-pins')
run(['pnpm','run','lint'],'lint')
assert run(['pnpm','run','build'],'build')==0
files={str(p.relative_to(root/'dist')):sha(p.read_bytes()) for p in (root/'dist').rglob('*') if p.is_file()}
prior=json.loads((stage/'evidence/social/artifact.json').read_text());assert prior['source']==spec['commit'] and files==prior['files']
(evidence/'artifact.json').write_text(json.dumps({'source':spec['commit'],'archiveSha256':spec['archiveSha256'],'env':spec['env'],'files':files,'byteIdenticalTo':prior['source']},indent=2)+'\n')
with tarfile.open(evidence/'dist.tar.gz','w:gz') as tar:tar.add(root/'dist',arcname='dist')
profile=json.loads(subprocess.check_output(['node','--input-type=module','-e',"import { releaseProfile } from './scripts/release-site.mjs'; console.log(JSON.stringify(releaseProfile));"],cwd=root,env=env,text=True));(evidence/'release-gates.json').write_text(json.dumps(profile,indent=2)+'\n')
launch="launchOptions:{channel:'chromium',args:['--disable-features=WebRtcHideLocalIpsWithMdns','--allow-loopback-in-peer-connection','--proxy-server='+process.env.REJECT_PROXY,'--proxy-bypass-list=localhost;127.0.0.1;[::1]']}"
# Match the previously qualified isolated two-context ICE fixture, without
# invoking capture or changing application permissions outside these contexts.
case=root/'tests/fips-p2p.spec.ts';raw=case.read_bytes();text=raw.decode();split=text.index("test('automatically exchanges live events");head_case,body_case=text[:split],text[split:];origin='http://127.0.0.1:4182'
for expected,ctx in [("  await page.goto('/');",'page.context()'),("  const secondPage = await secondContext.newPage();",'secondContext')]:
 assert body_case.count(expected)==1;body_case=body_case.replace(expected,f"  await {ctx}.grantPermissions(['microphone'], {{ origin: '{origin}' }});\n"+expected)
case.write_text(head_case+body_case)
(evidence/'fixture-origin.json').write_text(json.dumps({'before':sha(raw),'after':sha(case.read_bytes()),'origin':origin,'contexts':2,'permission':'microphone','captureInvoked':False},indent=2)+'\n')
config=root/'playwright.qualification.config.ts';config.write_text("import {defineConfig} from '@playwright/test';import original from './playwright.config';export default defineConfig({...original,workers:1,retries:0,outputDir:'/stage/evidence/social-final/'+process.env.QUALIFICATION_NAME+'-output',reporter:[['list'],['json',{outputFile:process.env.QUALIFICATION_REPORT}]],use:{...original.use,trace:'retain-on-failure',"+launch+"}});")
for i,cmd in enumerate(profile['testCommands']):
 extra={}
 if 'test:unit' in cmd:cmd=['pnpm','exec','playwright','test','--config=playwright.unit.config.ts','--workers=1','--reporter=list']
 elif 'playwright' in cmd:
  cmd=[x for x in cmd if not x.startswith('--workers')]+['--config='+config.name,'--workers=1']
  extra={'QUALIFICATION_REPORT':str(evidence/f'gate-{i+1}.json')}
  if 'tests/personal-event-index-publication.spec.ts' in cmd:extra.update(PLAYWRIGHT_USE_PREVIEW='0',PLAYWRIGHT_PORT='4182')
 run(cmd,f'gate-{i+1}',extra=extra)
run(['pnpm','exec','playwright','test','--config='+config.name,'tests/profile-identity-ui.spec.ts','tests/notifications-pagination-ui.spec.ts','tests/home-feed-snapshot-ui.spec.ts','tests/page-scroll-shell.spec.ts'], 'review-browser',extra={'QUALIFICATION_REPORT':str(evidence/'review-browser.json')})
# Navigation comparison is reused from the exact artifact qualified in the
# preceding run. Compare changed-flow idle/search costs on this same runner.
for label,cwd in [('baseline',stage/'apps/baseline'),('candidate',root)]:
 if label=='baseline':assert run(['pnpm','run','build'],'baseline-build',cwd=cwd)==0
 perf=cwd/'playwright.matched.config.ts';perf.write_text("import {defineConfig,devices} from '@playwright/test';import original from './playwright.config';export default defineConfig({...original,workers:1,retries:0,projects:[{name:'matched',use:{...devices['Desktop Chrome']}}],outputDir:'/stage/evidence/social-final/'+process.env.QUALIFICATION_NAME+'-output',reporter:[['list'],['json',{outputFile:process.env.QUALIFICATION_REPORT}]],use:{...original.use,trace:'off',"+launch+"}});")
 run(['pnpm','exec','playwright','test','--config='+perf.name,'tests/idle-cpu.spec.ts','tests/post-search-ai-ui.spec.ts'],label+'-idle-search',cwd=cwd,extra={'QUALIFICATION_REPORT':str(evidence/(label+'-idle-search.json'))})
assert run(['python3','/stage/procedure/network-proof.py',str(evidence/'network-after.json')],'network-after')==0
proxy.shutdown();proxy.server_close()
assert all(r['exit']==0 for r in records),'See retained failures; no publication.'
