import hashlib,json,os,pathlib,subprocess,tarfile,time
root=pathlib.Path.cwd();stage=pathlib.Path('/stage');evidence=stage/'evidence/social';evidence.mkdir();spec=json.loads((stage/'procedure/manifest.json').read_text())['apps'][0];tmp=stage/'task-tmp';tmp.mkdir();env=dict(os.environ,CI='1',TMPDIR=str(tmp),**spec['env']);records=[]
sha=lambda b:hashlib.sha256(b).hexdigest()
def stats():
 return {p:pathlib.Path('/sys/fs/cgroup',p).read_text() for p in ['cpu.stat','cpu.max','cpuset.cpus.effective','memory.peak','memory.events'] if pathlib.Path('/sys/fs/cgroup',p).exists()}
def run(cmd,name,*,cwd=root,extra=None):
 start=time.monotonic();before=stats()
 with (evidence/(name+'.log')).open('wb') as out:r=subprocess.run(cmd,cwd=cwd,env=dict(env,QUALIFICATION_NAME=name,**(extra or {})),stdout=out,stderr=subprocess.STDOUT,timeout=800)
 records.append({'name':name,'exit':r.returncode,'seconds':time.monotonic()-start,'cgroupBefore':before,'cgroupAfter':stats()});(evidence/'checks.json').write_text(json.dumps({'source':spec['commit'],'baseline':spec['baseline'],'records':records},indent=2)+'\n');return r.returncode
run(['pnpm','run','lint'],'lint');run(['pnpm','run','check'],'typecheck')
run(['pnpm','exec','playwright','test','--config=playwright.unit.config.ts','tests/profile-identity.spec.ts','tests/home-projection-commit.spec.ts','tests/progressive-scroll-restore.spec.ts','tests/social-repository.spec.ts','tests/social-data-plane.spec.ts','tests/social-data-plane-projection.spec.ts'],'focused-unit')
baseline=stage/'apps/baseline'
launch="launchOptions:{channel:'chromium',args:['--disable-features=WebRtcHideLocalIpsWithMdns','--allow-loopback-in-peer-connection']}"
for label,cwd in [('baseline',baseline),('candidate',root)]:
 assert run(['pnpm','run','build'],label+'-build',cwd=cwd)==0
 if label=='candidate':
  files={str(p.relative_to(root/'dist')):sha(p.read_bytes()) for p in (root/'dist').rglob('*') if p.is_file()};(evidence/'artifact.json').write_text(json.dumps({'source':spec['commit'],'archiveSha256':spec['archiveSha256'],'env':spec['env'],'files':files},indent=2)+'\n')
  with tarfile.open(evidence/'dist.tar.gz','w:gz') as tar:tar.add(root/'dist',arcname='dist')
 config=cwd/'playwright.matched.config.ts';config.write_text("import {defineConfig,devices} from '@playwright/test';import original from './playwright.config';export default defineConfig({...original,workers:1,retries:0,projects:[{name:'matched',use:{...devices['Desktop Chrome']}}],outputDir:'/stage/evidence/social/'+process.env.QUALIFICATION_NAME+'-output',reporter:[['list'],['json',{outputFile:process.env.QUALIFICATION_REPORT}]],use:{...original.use,trace:'off',"+launch+"}});")
 run(['pnpm','exec','playwright','test','--config='+config.name,'tests/navigation-performance.spec.ts'],label+'-navigation',cwd=cwd,extra={'QUALIFICATION_REPORT':str(evidence/(label+'-navigation.json'))})
config=root/'playwright.qualification.config.ts';config.write_text("import {defineConfig} from '@playwright/test';import original from './playwright.config';export default defineConfig({...original,workers:1,retries:0,outputDir:'/stage/evidence/social/'+process.env.QUALIFICATION_NAME+'-output',reporter:[['list'],['json',{outputFile:process.env.QUALIFICATION_REPORT}]],use:{...original.use,trace:'retain-on-failure',"+launch+"}});")
run(['pnpm','exec','playwright','test','--config='+config.name,'tests/page-scroll-shell.spec.ts','tests/home-feed-snapshot-ui.spec.ts','tests/for-you-shell.spec.ts','tests/profile-identity-ui.spec.ts','tests/navigation-shell.spec.ts'],'affected-browser',extra={'QUALIFICATION_REPORT':str(evidence/'affected-browser.json')})

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

if next(r for r in records if r['name']=='candidate-navigation')['exit']:
 assert run(['pnpm','exec','vite','build','--sourcemap','hidden'],'diagnostic-maps')==0
 observed={str(p.relative_to(root/'dist')):sha(p.read_bytes()) for p in (root/'dist').rglob('*') if p.is_file() and not p.name.endswith('.map')};assert observed==files
 with tarfile.open(evidence/'diagnostic-maps.tar.gz','w:gz') as tar:
  for p in (root/'dist').rglob('*.map'):tar.add(p,arcname=str(p.relative_to(root/'dist')))
 case=root/'tests/navigation-performance.spec.ts';raw=case.read_bytes();text=raw.decode();key="  await cdp.send('Performance.enable');";assert text.count(key)==1;text=text.replace(key,key+"\n  await cdp.send('Profiler.enable'); await cdp.send('Profiler.start');")
 key="  expect(p95DurationMs).toBeLessThan(300);";assert text.count(key)==1;text=text.replace(key,"  await testInfo.attach('cpu-profile.json', {body:JSON.stringify(await cdp.send('Profiler.stop')), contentType:'application/json'});\n"+key);case.write_text(text)
 (evidence/'profile-overlay.json').write_text(json.dumps({'before':sha(raw),'after':sha(case.read_bytes()),'purpose':'diagnostic only'},indent=2)+'\n')
 run(['pnpm','exec','playwright','test','--config=playwright.matched.config.ts','tests/navigation-performance.spec.ts'],'candidate-profile',extra={'QUALIFICATION_REPORT':str(evidence/'candidate-profile.json')})
assert all(r['exit']==0 for r in records if r['name'] not in ['baseline-navigation','scroll-negative-browser']),'Candidate checks incomplete; baseline failure retained separately.'

subprocess.run(['python3','/stage/procedure/run-final-closure.py'],cwd=root,env=env,check=True,timeout=1500)
