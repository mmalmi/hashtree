import hashlib,json,os,pathlib,subprocess,tarfile,time
root=pathlib.Path.cwd();stage=pathlib.Path('/stage');evidence=stage/'evidence/social';evidence.mkdir();spec=json.loads((stage/'procedure/manifest.json').read_text())['apps'][0];tmp=stage/'task-tmp';tmp.mkdir();env=dict(os.environ,CI='1',TMPDIR=str(tmp),**spec['env']);records=[]
sha=lambda b:hashlib.sha256(b).hexdigest()
(evidence/'cpu-provenance.json').write_text(json.dumps({'models':sorted(set(line.split(':',1)[1].strip() for line in pathlib.Path('/proc/cpuinfo').read_text().splitlines() if line.startswith('model name'))),'affinity':sorted(os.sched_getaffinity(0)),'cpuMax':pathlib.Path('/sys/fs/cgroup/cpu.max').read_text(),'memoryMax':pathlib.Path('/sys/fs/cgroup/memory.max').read_text()},indent=2)+'\n')
def run(cmd,name,cwd=root,extra=None):
 start=time.monotonic()
 with (evidence/(name+'.log')).open('wb') as out:r=subprocess.run(cmd,cwd=cwd,env=dict(env,QUALIFICATION_NAME=name,QUALIFICATION_REPORT=str(evidence/(name+'.json')),**(extra or {})),stdout=out,stderr=subprocess.STDOUT,timeout=300)
 records.append({'name':name,'exit':r.returncode,'seconds':time.monotonic()-start});(evidence/'checks.json').write_text(json.dumps({'source':spec['commit'],'records':records},indent=2)+'\n');return r.returncode
assert run(['pnpm','exec','playwright','test','--config=playwright.unit.config.ts','--workers=1','tests/app-route-state.spec.ts','tests/account-signer.spec.ts','tests/app-history-navigation.spec.ts','tests/profile-content.spec.ts','tests/embeds-repost.spec.ts'],'focused-unit')==0
assert run(['pnpm','run','lint'],'early-lint')==0
assert run(['pnpm','run','check'],'early-typecheck')==0
assert run(['pnpm','run','build'],'candidate-build')==0
files={str(p.relative_to(root/'dist')):sha(p.read_bytes()) for p in (root/'dist').rglob('*') if p.is_file()};(evidence/'artifact.json').write_text(json.dumps({'source':spec['commit'],'archiveSha256':spec['archiveSha256'],'env':spec['env'],'files':files},indent=2)+'\n')
with tarfile.open(evidence/'dist.tar.gz','w:gz') as tar:tar.add(root/'dist',arcname='dist')
launch="launchOptions:{channel:'chromium',args:['--disable-features=WebRtcHideLocalIpsWithMdns','--allow-loopback-in-peer-connection']}"
config=root/'playwright.qualification.config.ts';config.write_text("import {defineConfig} from '@playwright/test';import original from './playwright.config';export default defineConfig({...original,workers:1,retries:0,outputDir:'/stage/evidence/social/'+process.env.QUALIFICATION_NAME+'-output',reporter:[['list'],['json',{outputFile:process.env.QUALIFICATION_REPORT}]],use:{...original.use,trace:'retain-on-failure',"+launch+"}});")
negative=stage/'apps/retained-scroll-negative';(negative/'node_modules').symlink_to(root/'node_modules',target_is_directory=True)
case='tests/home-feed-stability-ui.spec.ts';(negative/case).write_bytes((root/case).read_bytes());(negative/config.name).write_text(config.read_text().replace('...original,workers:1',"...original,projects:[{name:'negative',use:original.projects[0].use}],testMatch:'**/home-feed-stability-ui.spec.ts',workers:1"))
assert run(['pnpm','exec','playwright','test','--config='+config.name,case,'--grep','scrolling during a retained profile'],'retained-scroll-negative',negative,extra={'PLAYWRIGHT_USE_PREVIEW':'0','PLAYWRIGHT_PORT':'4183'})==1
report=json.loads((evidence/'retained-scroll-negative.json').read_text());assert report['stats']['unexpected']==1 and not report['errors']
def messages(v):
 if isinstance(v,dict):return ([v['message']] if 'message' in v else [])+sum((messages(x) for x in v.values()),[])
 if isinstance(v,list):return sum((messages(x) for x in v),[])
 return []
assert any('toBeVisible' in s and 'data-note-id' in s for s in messages(report)), 'Negative control failed for an unrelated reason'
(evidence/'negative-control.json').write_text(json.dumps({'expectedFailure':True,**json.loads((stage/'procedure/manifest.json').read_text())['retainedScrollNegative']},indent=2)+'\n')
assert run(['pnpm','exec','playwright','test','--config='+config.name,case,'--grep','scrolling during a retained profile'],'retained-scroll-regression')==0
assert run(['pnpm','exec','playwright','test','--config='+config.name,'tests/note-mentions-ui.spec.ts','tests/profile-sections.spec.ts','tests/navigation-shell.spec.ts','tests/page-scroll-shell.spec.ts'],'mention-and-profile-regression')==0
for label,cwd in [('baseline',stage/'apps/baseline'),('candidate',root)]:
 if label=='baseline':assert run(['pnpm','run','build'],label+'-build',cwd)==0
 config=cwd/'playwright.matched.config.ts';config.write_text("import {defineConfig,devices} from '@playwright/test';import original from './playwright.config';export default defineConfig({...original,workers:1,retries:0,projects:[{name:'matched',use:{...devices['Desktop Chrome']}}],outputDir:'/stage/evidence/social/'+process.env.QUALIFICATION_NAME+'-output',reporter:[['list'],['json',{outputFile:process.env.QUALIFICATION_REPORT}]],use:{...original.use,trace:'off',"+launch+"}});")
 status=run(['pnpm','exec','playwright','test','--config='+config.name,'tests/navigation-performance.spec.ts'],label+'-navigation',cwd)
 if label=='candidate' and status:
  diagnostic_case=root/'tests/navigation-performance.spec.ts';original=diagnostic_case.read_bytes()
  try:subprocess.run(['python3','/stage/procedure/run-diagnostic.py'],cwd=root,env=env,check=True,timeout=300)
  finally:diagnostic_case.write_bytes(original)
  raise AssertionError('Unchanged navigation gate failed; diagnostics retained and source test restored')
subprocess.run(['python3','/stage/procedure/run-final-closure.py'],cwd=root,env=env,check=True,timeout=1800)

assert next(r for r in records if r['name']=='candidate-navigation')['exit']==0, 'Unchanged navigation gate failed; full evidence retained, no publication'
