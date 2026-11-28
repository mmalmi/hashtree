import hashlib,json,os,pathlib,subprocess,tarfile,time
root=pathlib.Path.cwd();stage=pathlib.Path('/stage');evidence=stage/'evidence/social';evidence.mkdir();spec=json.loads((stage/'procedure/manifest.json').read_text())['apps'][0];env=dict(os.environ,CI='1',**spec['env']);records=[]
def run(cmd,name,*,cwd=root,extra=None,required=True):
 start=time.monotonic()
 with (evidence/(name+'.log')).open('wb') as out:
  r=subprocess.run(cmd,cwd=cwd,env=dict(env,**(extra or {})),stdout=out,stderr=subprocess.STDOUT,timeout=800)
 records.append({'name':name,'exit':r.returncode,'seconds':time.monotonic()-start})
 (evidence/'checks.json').write_text(json.dumps({'source':spec['commit'],'baseline':spec['baseline'],'records':records},indent=2)+'\n')
 if required:assert r.returncode==0,name
 return r.returncode
run(['node','scripts/verify-dependency-lock.mjs'],'verify-pins')
run(['pnpm','run','lint'],'lint',required=False)
run(['pnpm','run','build'],'build')
files={str(p.relative_to(root/'dist')):hashlib.sha256(p.read_bytes()).hexdigest() for p in (root/'dist').rglob('*') if p.is_file()}
(evidence/'artifact.json').write_text(json.dumps({'source':spec['commit'],'archiveSha256':spec['archiveSha256'],'env':spec['env'],'files':files},indent=2)+'\n')
with tarfile.open(evidence/'dist.tar.gz','w:gz') as tar:tar.add(root/'dist',arcname='dist')
profile=json.loads(subprocess.check_output(['node','--input-type=module','-e',"import { releaseProfile } from './scripts/release-site.mjs'; console.log(JSON.stringify(releaseProfile));"],cwd=root,env=env,text=True));(evidence/'release-gates.json').write_text(json.dumps(profile,indent=2)+'\n')
config=root/'playwright.qualification.config.ts';config.write_text("import { defineConfig } from '@playwright/test'; import original from './playwright.config'; export default defineConfig({...original,workers:1,retries:0,outputDir:'/stage/evidence/social/browser-output',reporter:[['list'],['json',{outputFile:process.env.QUALIFICATION_REPORT}]],use:{...original.use,trace:'retain-on-failure'}});")
for i,cmd in enumerate(profile['testCommands']):
 extra={}
 if 'test:unit' in cmd:cmd=['pnpm','exec','playwright','test','--config=playwright.unit.config.ts','--workers=1','--reporter=list']
 elif 'playwright' in cmd:
  cmd=[x for x in cmd if not x.startswith('--workers')]+['--config='+config.name,'--workers=1']
  extra={'QUALIFICATION_REPORT':str(evidence/f'gate-{i+1}.json')}
  if 'tests/personal-event-index-publication.spec.ts' in cmd:extra.update(PLAYWRIGHT_USE_PREVIEW='0',PLAYWRIGHT_PORT='4182')
 run(cmd,f'gate-{i+1}',extra=extra,required=False)
run(['pnpm','exec','playwright','test','--config='+config.name,'tests/profile-identity-ui.spec.ts','tests/notifications-pagination-ui.spec.ts'], 'review-browser',extra={'QUALIFICATION_REPORT':str(evidence/'review-browser.json')},required=False)
# Compare the accepted-dependency review baseline and the combined candidate on
# this same runner, same private network namespace and browser/toolchain.
for label,cwd in [('baseline',stage/'apps/baseline'),('candidate',root)]:
 if label=='baseline':run(['pnpm','run','build'],'baseline-build',cwd=cwd)
 perf=cwd/'playwright.matched.config.ts';perf.write_text("import { defineConfig,devices } from '@playwright/test'; import original from './playwright.config'; export default defineConfig({...original,workers:1,retries:0,projects:[{name:'matched',use:{...devices['Desktop Chrome']}}],outputDir:'/stage/evidence/social/performance-"+label+"',reporter:[['list'],['json',{outputFile:'/stage/evidence/social/performance-"+label+".json'}]],use:{...original.use,trace:'retain-on-failure'}});")
 run(['pnpm','exec','playwright','test','--config='+perf.name,'tests/navigation-performance.spec.ts','tests/idle-cpu.spec.ts','tests/post-search-ai-ui.spec.ts'],label+'-performance',cwd=cwd,required=False)
assert all(r['exit']==0 for r in records),'One or more full gates failed; retain exact evidence.'
