import hashlib,http.server,json,os,pathlib,subprocess,tarfile,threading,time
root=pathlib.Path.cwd();stage=pathlib.Path('/stage');evidence=stage/'evidence/social';evidence.mkdir();spec=json.loads((stage/'procedure/manifest.json').read_text())['apps'][0];tmp=stage/'task-tmp';tmp.mkdir();env=dict(os.environ,CI='1',TMPDIR=str(tmp),**spec['env']);records=[]
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
baseline=stage/'apps/baseline';overlay={}
for name in ['tests/fips-p2p.spec.ts','tests/helpers/peerNetworkSettings.ts']:
 data=(root/name).read_bytes();overlay[name]={'before':sha((baseline/name).read_bytes()),'after':sha(data)};(baseline/name).write_bytes(data)
(evidence/'baseline-test-overlay.json').write_text(json.dumps(overlay,indent=2)+'\n')
launch="launchOptions:{channel:'chromium',args:['--disable-features=WebRtcHideLocalIpsWithMdns','--allow-loopback-in-peer-connection','--proxy-server='+process.env.REJECT_PROXY,'--proxy-bypass-list=localhost;127.0.0.1;[::1]']}"
for label,cwd in [('baseline',baseline),('candidate',root)]:
 assert run(['pnpm','run','build'],label+'-build',cwd=cwd)==0
 if label=='candidate':
  files={str(p.relative_to(root/'dist')):sha(p.read_bytes()) for p in (root/'dist').rglob('*') if p.is_file()};prior=json.loads((stage/'procedure/prior-artifact.json').read_text());assert files==prior['files']
  (evidence/'artifact.json').write_text(json.dumps({'source':spec['commit'],'archiveSha256':spec['archiveSha256'],'files':files,'byteIdenticalTo':prior['source']},indent=2)+'\n')
 # Exact previously accepted two-context origin-only isolated ICE fixture.
 case=cwd/'tests/fips-p2p.spec.ts';raw=case.read_bytes();text=raw.decode();split=text.index("test('automatically exchanges live events");head,body=text[:split],text[split:];origin='http://127.0.0.1:4182'
 for expected,ctx in [("  await page.goto('/');",'page.context()'),("  const secondPage = await secondContext.newPage();",'secondContext')]:
  assert body.count(expected)==1;body=body.replace(expected,f"  await {ctx}.grantPermissions(['microphone'], {{ origin: '{origin}' }});\n"+expected)
 case.write_text(head+body);assert (head+body).count("grantPermissions(['microphone']")==2
 (evidence/(label+'-fixture-origin.json')).write_text(json.dumps({'source':spec['commit'] if label=='candidate' else spec['baseline'],'before':sha(raw),'after':sha(case.read_bytes()),'origin':origin,'contexts':2,'permission':'microphone','captureInvoked':False},indent=2)+'\n')
 config=cwd/'playwright.qualification.config.ts';config.write_text("import {defineConfig} from '@playwright/test';import original from './playwright.config';export default defineConfig({...original,workers:1,retries:0,outputDir:'/stage/evidence/social/'+process.env.QUALIFICATION_NAME+'-output',reporter:[['list'],['json',{outputFile:process.env.QUALIFICATION_REPORT}]],use:{...original.use,trace:'retain-on-failure',"+launch+"}});")
 run(['pnpm','exec','playwright','test','--config='+config.name,'tests/fips-p2p.spec.ts','--grep','automatically exchanges'],label+'-fips',cwd=cwd,extra={'QUALIFICATION_REPORT':str(evidence/(label+'-fips.json')),'PLAYWRIGHT_USE_PREVIEW':'0','PLAYWRIGHT_PORT':'4182'})
 perf=cwd/'playwright.matched.config.ts';perf.write_text("import {defineConfig,devices} from '@playwright/test';import original from './playwright.config';export default defineConfig({...original,workers:1,retries:0,projects:[{name:'matched',use:{...devices['Desktop Chrome']}}],outputDir:'/stage/evidence/social/'+process.env.QUALIFICATION_NAME+'-output',reporter:[['list'],['json',{outputFile:process.env.QUALIFICATION_REPORT}]],use:{...original.use,trace:'off',"+launch+"}});")
 run(['pnpm','exec','playwright','test','--config='+perf.name,'tests/navigation-performance.spec.ts'],label+'-navigation',cwd=cwd,extra={'QUALIFICATION_REPORT':str(evidence/(label+'-navigation.json'))})
# Capture a separate diagnostic profile; do not mix it into timed acceptance.
case=root/'tests/navigation-performance.spec.ts';raw=case.read_bytes();text=raw.decode();key="  await cdp.send('Performance.enable');";assert text.count(key)==1;text=text.replace(key,key+"\n  await cdp.send('Profiler.enable'); await cdp.send('Profiler.start');")
key="  expect(p95DurationMs).toBeLessThan(300);";assert text.count(key)==1;text=text.replace(key,"  await testInfo.attach('cpu-profile.json', {body:JSON.stringify(await cdp.send('Profiler.stop')), contentType:'application/json'});\n"+key);case.write_text(text)
(evidence/'profile-overlay.json').write_text(json.dumps({'before':sha(raw),'after':sha(case.read_bytes()),'purpose':'diagnostic only; timed acceptance is uninstrumented'},indent=2)+'\n')
run(['pnpm','exec','playwright','test','--config=playwright.matched.config.ts','tests/navigation-performance.spec.ts'],'candidate-profile',extra={'QUALIFICATION_REPORT':str(evidence/'candidate-profile.json')})
assert run(['python3','/stage/procedure/network-proof.py',str(evidence/'network-after.json')],'network-after')==0
proxy.shutdown();proxy.server_close()
assert all(r['exit']==0 for r in records),'See retained failures; no publication.'
