import hashlib,json,os,pathlib,subprocess,tarfile,time
root=pathlib.Path.cwd();stage=pathlib.Path('/stage');evidence=stage/'evidence/social';evidence.mkdir();spec=json.loads((stage/'procedure/manifest.json').read_text())['apps'][0];tmp=stage/'task-tmp';tmp.mkdir();env=dict(os.environ,CI='1',TMPDIR=str(tmp),**spec['env']);records=[]
sha=lambda b:hashlib.sha256(b).hexdigest()
def stats():
 return {p:pathlib.Path('/sys/fs/cgroup',p).read_text() for p in ['cpu.stat','cpu.max','cpuset.cpus.effective','memory.peak','memory.events'] if pathlib.Path('/sys/fs/cgroup',p).exists()}
def run(cmd,name,*,cwd=root,extra=None):
 start=time.monotonic();before=stats()
 with (evidence/(name+'.log')).open('wb') as out:r=subprocess.run(cmd,cwd=cwd,env=dict(env,QUALIFICATION_NAME=name,**(extra or {})),stdout=out,stderr=subprocess.STDOUT,timeout=800)
 records.append({'name':name,'exit':r.returncode,'seconds':time.monotonic()-start,'cgroupBefore':before,'cgroupAfter':stats()});(evidence/'checks.json').write_text(json.dumps({'source':spec['commit'],'baseline':spec['baseline'],'records':records},indent=2)+'\n');return r.returncode
baseline=stage/'apps/baseline'
launch="launchOptions:{channel:'chromium',args:['--disable-features=WebRtcHideLocalIpsWithMdns','--allow-loopback-in-peer-connection']}"
config=root/'playwright.qualification.config.ts';config.write_text("import {defineConfig,devices} from '@playwright/test';import original from './playwright.config';export default defineConfig({...original,workers:1,retries:0,projects:[{name:'chromium',use:{...devices['Desktop Chrome']}}],outputDir:'/stage/evidence/social/'+process.env.QUALIFICATION_NAME+'-output',reporter:[['list'],['json',{outputFile:process.env.QUALIFICATION_REPORT}]],use:{...original.use,trace:'retain-on-failure',"+launch+"}});")
# The prior proposal really painted a cold profile at the outgoing offset.
# Require the same first-presented-frame probe to reject that exact old build.
negative_spec=json.loads((stage/'procedure/manifest.json').read_text())['scrollNegative'];archive=stage/'scroll-negative.tar.gz';assert sha(archive.read_bytes())==negative_spec['sha256']
negative=stage/'apps/scroll-negative';negative.mkdir()
with tarfile.open(archive) as tar:tar.extractall(negative,filter='data')
assert sha((negative/'pnpm-lock.yaml').read_bytes())==spec['lockSha256']
(negative/'node_modules').symlink_to(baseline/'node_modules',target_is_directory=True)
assert run(['pnpm','run','build'],'scroll-negative-build',cwd=negative)==0
case=negative/'tests/page-scroll-shell.spec.ts';raw=case.read_bytes();case.write_bytes((root/'tests/page-scroll-shell.spec.ts').read_bytes())
(evidence/'scroll-negative-overlay.json').write_text(json.dumps({'source':negative_spec['source'],'archiveSha256':negative_spec['sha256'],'testBefore':sha(raw),'testAfter':sha(case.read_bytes())},indent=2)+'\n')
(negative/config.name).write_bytes(config.read_bytes())
status=run(['pnpm','exec','playwright','test','--config='+config.name,'tests/page-scroll-shell.spec.ts','--grep','cold and retained destinations'],'scroll-negative-browser',cwd=negative,extra={'QUALIFICATION_REPORT':str(evidence/'scroll-negative-browser.json')})
import base64
samples=[]
def collect(value):
 if isinstance(value,dict):
  for attachment in value.get('attachments',[]):
   if attachment.get('name')=='route-scroll-frames.json':samples.append(json.loads(base64.b64decode(attachment['body'])))
  for k,v in value.items():
   if k!='attachments':collect(v)
 elif isinstance(value,list):
  for v in value:collect(v)
collect(json.loads((evidence/'scroll-negative-browser.json').read_text()))
assert status!=0 and len(samples)==1 and samples[0][0]['kind']=='profile' and samples[0][0]['offset']>0, 'Negative control must catch the actual cold-profile offset regression'
(evidence/'scroll-negative-result.json').write_text(json.dumps({'source':negative_spec['source'],'expectedFailure':True,'samples':samples[0]},indent=2)+'\n')


assert run(['pnpm','exec','vite','build','--sourcemap','hidden'],'diagnostic-maps')==0
files={str(p.relative_to(root/'dist')):sha(p.read_bytes()) for p in (root/'dist').rglob('*') if p.is_file() and not p.name.endswith('.map')}
expected=json.loads((stage/'procedure/expected-artifact.json').read_text());assert expected['source']==spec['commit'] and expected['files']==files
with tarfile.open(evidence/'diagnostic-maps.tar.gz','w:gz') as tar:
 for p in (root/'dist').rglob('*.map'):tar.add(p,arcname=str(p.relative_to(root/'dist')))
case=root/'tests/navigation-performance.spec.ts';raw=case.read_bytes();text=raw.decode()
key="      inputAt = performance.now();";assert text.count(key)==1;text=text.replace(key,key+"\n      performance.mark('diagnostic-route-input');")
key="        const routeCommittedMs = performance.now() - inputAt;";assert text.count(key)==1;text=text.replace(key,"        performance.mark('diagnostic-route-commit');\n"+key)
key="  await visit('profile-first', 'desktop-nav-profile', 'profile-hero');";assert text.count(key)==1
before="""  await cdp.send('Profiler.enable'); await cdp.send('Profiler.start');
  await cdp.send('Tracing.start', {categories:'devtools.timeline,v8.execute,blink.user_timing',transferMode:'ReturnAsStream'});
"""
after="""
  await testInfo.attach('cpu-profile.json', {body:JSON.stringify(await cdp.send('Profiler.stop')), contentType:'application/json'});
  const traceComplete = new Promise<{stream:string}>(resolve => cdp.once('Tracing.tracingComplete', resolve));
  await cdp.send('Tracing.end');
  const {stream} = await traceComplete; let traceBody = '';
  for (;;) {const chunk = await cdp.send('IO.read', {handle:stream}); traceBody += chunk.data; if (chunk.eof) break;}
  await cdp.send('IO.close', {handle:stream});
  await testInfo.attach('cold-profile-trace.json', {body:traceBody, contentType:'application/json'});
"""
text=text.replace(key,before+key+after);case.write_text(text)
(evidence/'profile-overlay.json').write_text(json.dumps({'before':sha(raw),'after':sha(case.read_bytes()),'purpose':'diagnose first profile only; unchanged artifact bytes and unchanged timing gates'},indent=2)+'\n')
config.write_text(config.read_text().replace("trace:'retain-on-failure'","trace:'off'"))
run(['pnpm','exec','playwright','test','--config='+config.name,'tests/navigation-performance.spec.ts'],'candidate-profile',extra={'QUALIFICATION_REPORT':str(evidence/'candidate-profile.json')})
(evidence/'diagnostic-only.json').write_text(json.dumps({'source':spec['commit'],'qualificationPassed':False,'scope':'Measured first profile trace and verified expected scroll negative control; retain current candidate navigation failure separately.'},indent=2)+'\n')
