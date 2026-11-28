import hashlib,json,os,pathlib,subprocess,tarfile,time
root=pathlib.Path.cwd();stage=pathlib.Path('/stage');evidence=stage/'evidence/social';evidence.mkdir();spec=json.loads((stage/'procedure/manifest.json').read_text())['apps'][0];tmp=stage/'task-tmp';tmp.mkdir();env=dict(os.environ,CI='1',TMPDIR=str(tmp),**spec['env']);records=[]
def run(cmd,name,*,cwd=root,extra=None):
 start=time.monotonic()
 with (evidence/(name+'.log')).open('wb') as out:
  r=subprocess.run(cmd,cwd=cwd,env=dict(env,QUALIFICATION_NAME=name,**(extra or {})),stdout=out,stderr=subprocess.STDOUT,timeout=800)
 records.append({'name':name,'exit':r.returncode,'seconds':time.monotonic()-start});(evidence/'checks.json').write_text(json.dumps({'source':spec['commit'],'baseline':spec['baseline'],'records':records},indent=2)+'\n');return r.returncode
run(['node','scripts/verify-dependency-lock.mjs'],'verify-pins');run(['pnpm','run','lint'],'lint');run(['pnpm','run','check'],'typecheck');assert run(['pnpm','run','build'],'build')==0
files={str(p.relative_to(root/'dist')):hashlib.sha256(p.read_bytes()).hexdigest() for p in (root/'dist').rglob('*') if p.is_file()};prior=json.loads((stage/'procedure/prior-artifact.json').read_text());assert files==prior['files'],'Removing an unused import unexpectedly changed runtime artifact bytes'
(evidence/'artifact.json').write_text(json.dumps({'source':spec['commit'],'archiveSha256':spec['archiveSha256'],'env':spec['env'],'files':files,'byteIdenticalTo':prior['source']},indent=2)+'\n')
with tarfile.open(evidence/'dist.tar.gz','w:gz') as tar:tar.add(root/'dist',arcname='dist')
# Reuse the prior accepted Social lane's full Chromium and isolated RTC flags.
# The namespace remains unconnected; no host route, devices or privileges added.
launch="launchOptions:{channel:'chromium',args:['--disable-features=WebRtcHideLocalIpsWithMdns','--allow-loopback-in-peer-connection']}"
config=root/'playwright.qualification.config.ts';config.write_text("import { defineConfig } from '@playwright/test'; import original from './playwright.config'; export default defineConfig({...original,workers:1,retries:0,outputDir:'/stage/evidence/social/'+process.env.QUALIFICATION_NAME+'-output',reporter:[['list'],['json',{outputFile:process.env.QUALIFICATION_REPORT}]],use:{...original.use,trace:'retain-on-failure',"+launch+"}});")
(run_browser:=lambda cmd,name,cwd=root,extra={}:run(['pnpm','exec','playwright','test','--config='+config.name]+cmd,name,cwd=cwd,extra={'QUALIFICATION_REPORT':str(evidence/(name+'.json')),**extra}))(['tests/for-you-shell.spec.ts','tests/initial-paint.spec.ts','tests/post-search-consent-ui.spec.ts'],'feed-browser')
run_browser(['tests/fips-p2p.spec.ts','tests/home-feed-stability-ui.spec.ts'],'candidate-fips',extra={'PLAYWRIGHT_USE_PREVIEW':'0','PLAYWRIGHT_PORT':'4182'})
baseline=stage/'apps/baseline';overlay={}
for name in ['tests/fips-p2p.spec.ts','tests/helpers/peerNetworkSettings.ts']:
 data=(root/name).read_bytes();overlay[name]={'before':hashlib.sha256((baseline/name).read_bytes()).hexdigest(),'after':hashlib.sha256(data).hexdigest()};(baseline/name).write_bytes(data)
(evidence/'baseline-test-overlay.json').write_text(json.dumps(overlay,indent=2)+'\n');(baseline/config.name).write_text(config.read_text());assert run(['pnpm','run','build'],'baseline-build',cwd=baseline)==0
run_browser(['tests/fips-p2p.spec.ts','--grep','automatically exchanges'],'baseline-fips',cwd=baseline,extra={'PLAYWRIGHT_USE_PREVIEW':'0','PLAYWRIGHT_PORT':'4182'})
for label,cwd in [('baseline',baseline),('candidate',root)]:
 perf=cwd/'playwright.matched.config.ts';perf.write_text("import { defineConfig,devices } from '@playwright/test'; import original from './playwright.config'; export default defineConfig({...original,workers:1,retries:0,projects:[{name:'matched',use:{...devices['Desktop Chrome']}}],outputDir:'/stage/evidence/social/performance-"+label+"',reporter:[['list'],['json',{outputFile:'/stage/evidence/social/performance-"+label+".json'}]],use:{...original.use,trace:'off',"+launch+"}});")
 run(['pnpm','exec','playwright','test','--config='+perf.name,'tests/navigation-performance.spec.ts','tests/idle-cpu.spec.ts','tests/post-search-ai-ui.spec.ts'],label+'-performance',cwd=cwd)
(evidence/'browser-binaries.json').write_text(json.dumps({str(p):hashlib.sha256(p.read_bytes()).hexdigest() for p in pathlib.Path('/ms-playwright').glob('chromium-*/chrome-linux*/chrome')},indent=2)+'\n')
assert all(r['exit']==0 for r in records),'See retained failures; no publication.'
