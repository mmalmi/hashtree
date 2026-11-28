import hashlib,json,os,pathlib,subprocess,tarfile,time
root=pathlib.Path.cwd();stage=pathlib.Path('/stage');evidence=stage/'evidence/social';evidence.mkdir();spec=json.loads((stage/'procedure/manifest.json').read_text())['apps'][0];tmp=stage/'task-tmp';tmp.mkdir();env=dict(os.environ,CI='1',TMPDIR=str(tmp),**spec['env']);records=[]
def run(cmd,name,*,cwd=root,extra=None):
 start=time.monotonic()
 with (evidence/(name+'.log')).open('wb') as out:
  r=subprocess.run(cmd,cwd=cwd,env=dict(env,QUALIFICATION_NAME=name,**(extra or {})),stdout=out,stderr=subprocess.STDOUT,timeout=800)
 records.append({'name':name,'exit':r.returncode,'seconds':time.monotonic()-start})
 (evidence/'checks.json').write_text(json.dumps({'source':spec['commit'],'baseline':spec['baseline'],'records':records},indent=2)+'\n')
 return r.returncode
run(['node','scripts/verify-dependency-lock.mjs'],'verify-pins')
run(['pnpm','run','lint'],'lint')
assert run(['pnpm','run','build'],'build')==0
files={str(p.relative_to(root/'dist')):hashlib.sha256(p.read_bytes()).hexdigest() for p in (root/'dist').rglob('*') if p.is_file()}
(evidence/'artifact.json').write_text(json.dumps({'source':spec['commit'],'archiveSha256':spec['archiveSha256'],'env':spec['env'],'files':files},indent=2)+'\n')
with tarfile.open(evidence/'dist.tar.gz','w:gz') as tar:tar.add(root/'dist',arcname='dist')
run(['pnpm','run','check'],'typecheck')
run(['pnpm','exec','playwright','test','--config=playwright.unit.config.ts','--workers=1','--reporter=list'],'full-unit')
config=root/'playwright.qualification.config.ts';config.write_text("import { defineConfig } from '@playwright/test'; import original from './playwright.config'; export default defineConfig({...original,workers:1,retries:0,outputDir:'/stage/evidence/social/'+process.env.QUALIFICATION_NAME+'-output',reporter:[['list'],['json',{outputFile:process.env.QUALIFICATION_REPORT}]],use:{...original.use,trace:'retain-on-failure'}});")
# Unchanged full-gate results are retained from 46089680516. These paths cover
# corrected fixture, metadata formatting and extracted graph validators.
run(['pnpm','exec','playwright','test','--config='+config.name,'tests/anonymous-frontier-first.spec.ts','tests/profile-identity-ui.spec.ts','tests/social-worker-profile-snapshot-browser.spec.ts','tests/public-graph-bootstrap.spec.ts'], 'affected-browser',extra={'QUALIFICATION_REPORT':str(evidence/'affected-browser.json')})
run(['pnpm','exec','playwright','test','--config='+config.name,'tests/fips-p2p.spec.ts'], 'candidate-fips',extra={'QUALIFICATION_REPORT':str(evidence/'candidate-fips.json'),'PLAYWRIGHT_USE_PREVIEW':'0','PLAYWRIGHT_PORT':'4182'})
# Same real peer flow on the prior libraries. Copy only the diagnostic/fixture
# test changes, with explicit hashes; no baseline application code is changed.
baseline=stage/'apps/baseline';test_overlay={}
for name in ['tests/fips-p2p.spec.ts','tests/helpers/peerNetworkSettings.ts']:
 data=(root/name).read_bytes();test_overlay[name]={'before':hashlib.sha256((baseline/name).read_bytes()).hexdigest(),'after':hashlib.sha256(data).hexdigest()};(baseline/name).write_bytes(data)
(evidence/'baseline-test-overlay.json').write_text(json.dumps(test_overlay,indent=2)+'\n');(baseline/config.name).write_text(config.read_text())
run(['pnpm','exec','playwright','test','--config='+config.name,'tests/fips-p2p.spec.ts','--grep','automatically exchanges'], 'baseline-fips',cwd=baseline,extra={'QUALIFICATION_REPORT':str(evidence/'baseline-fips.json'),'PLAYWRIGHT_USE_PREVIEW':'0','PLAYWRIGHT_PORT':'4182'})
assert run(['pnpm','run','build'],'baseline-build',cwd=baseline)==0
for label,cwd in [('baseline',baseline),('candidate',root)]:
 # DOM tracing itself adds work to every navigation. Keep the existing
 # first-attempt performance conditions (trace off), identical for both inputs.
 perf=cwd/'playwright.matched.config.ts';perf.write_text("import { defineConfig,devices } from '@playwright/test'; import original from './playwright.config'; export default defineConfig({...original,workers:1,retries:0,projects:[{name:'matched',use:{...devices['Desktop Chrome']}}],outputDir:'/stage/evidence/social/performance-"+label+"',reporter:[['list'],['json',{outputFile:'/stage/evidence/social/performance-"+label+".json'}]],use:{...original.use,trace:'off'}});")
 run(['pnpm','exec','playwright','test','--config='+perf.name,'tests/navigation-performance.spec.ts','tests/idle-cpu.spec.ts','tests/post-search-ai-ui.spec.ts'],label+'-performance',cwd=cwd)
assert all(r['exit']==0 for r in records),'See exact retained failures; no publication.'
