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
run(['pnpm','exec','playwright','test','--config=playwright.unit.config.ts','tests/profile-identity.spec.ts','tests/home-projection-commit.spec.ts','tests/progressive-scroll-restore.spec.ts'],'focused-unit')
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
if next(r for r in records if r['name']=='candidate-navigation')['exit']:
 assert run(['pnpm','exec','vite','build','--sourcemap','hidden'],'diagnostic-maps')==0
 observed={str(p.relative_to(root/'dist')):sha(p.read_bytes()) for p in (root/'dist').rglob('*') if p.is_file() and not p.name.endswith('.map')};assert observed==files
 with tarfile.open(evidence/'diagnostic-maps.tar.gz','w:gz') as tar:
  for p in (root/'dist').rglob('*.map'):tar.add(p,arcname=str(p.relative_to(root/'dist')))
 case=root/'tests/navigation-performance.spec.ts';raw=case.read_bytes();text=raw.decode();key="  await cdp.send('Performance.enable');";assert text.count(key)==1;text=text.replace(key,key+"\n  await cdp.send('Profiler.enable'); await cdp.send('Profiler.start');")
 key="  expect(p95DurationMs).toBeLessThan(300);";assert text.count(key)==1;text=text.replace(key,"  await testInfo.attach('cpu-profile.json', {body:JSON.stringify(await cdp.send('Profiler.stop')), contentType:'application/json'});\n"+key);case.write_text(text)
 (evidence/'profile-overlay.json').write_text(json.dumps({'before':sha(raw),'after':sha(case.read_bytes()),'purpose':'diagnostic only'},indent=2)+'\n')
 run(['pnpm','exec','playwright','test','--config=playwright.matched.config.ts','tests/navigation-performance.spec.ts'],'candidate-profile',extra={'QUALIFICATION_REPORT':str(evidence/'candidate-profile.json')})
assert all(r['exit']==0 for r in records if r['name']!='baseline-navigation'),'Candidate checks incomplete; baseline failure retained separately.'
