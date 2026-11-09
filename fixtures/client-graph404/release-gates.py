"""Fresh Client release plan, preserving both source checks and all original skips."""
import hashlib,json,os,pathlib,runpy,shutil,subprocess,tarfile,time
R=pathlib.Path.cwd();E=pathlib.Path(os.environ['FIPS_APP_EVIDENCE']);M=json.loads(pathlib.Path('/stage/procedure/manifest.json').read_text());S=M['apps'][0];env=dict(os.environ);sha=lambda p:hashlib.sha256(p.read_bytes()).hexdigest()
assert sha(R/'package.json')==S['packageSha256'] and sha(R/'pnpm-lock.yaml')==S['lockSha256']
assert sha(R/'scripts/release-site.mjs')==S['releaseProfileSha256'] and sha(R/'scripts/check-release-source.mjs')==S['sourceCheckerSha256']
code="const m=await import('./scripts/release-site.mjs');console.log(JSON.stringify(m.createReleasePlan(m.parseArgs([]))));"
plan=json.loads(subprocess.check_output(['node','--input-type=module','-e',code],env=env,text=True,timeout=20));(E/'original-release-plan.json').write_text(json.dumps(plan,indent=2)+'\n')
steps=[]
for step in plan['steps']:
 if step['id'] in ['publish','deploy']:break
 steps.append(step)
assert [s['id'] for s in steps]==['check-source-start','build','test-1','test-2','test-3','test-4','test-5','test-6','check-source-final']
steps.insert(-1,{'id':'peer-recovery','command':['pnpm','exec','playwright','test','tests/peer-files.spec.ts','--grep','cached encrypted attachment is served across real browser peers without file servers','--project=chromium','--workers=1','--retries=0','--reporter=list,json'],
                 'env':{'IRIS_E2E_BUILT_DIST':'false','IRIS_E2E_BUILT':'false'}})
original=(R/'playwright.config.ts').read_bytes();overlaid=False;result={'passed':False,'source':S['commit'],'steps':[],'publicationStepsNotExecuted':True,'allAppGatesFromCurrentRun':True,'reusedAppScopes':[]}
checks=runpy.run_path('/stage/procedure/browser-results.py')
peer_path=R/'tests/peer-files.spec.ts';peer_original=peer_path.read_bytes();peer_helper=R/'tests/isolated-peer-permission.ts';peer_overlaid=False
assert sha(peer_path)==S['testFiles']['tests/peer-files.spec.ts'] and not peer_helper.exists()
def restore_peer():
 global peer_overlaid
 if peer_overlaid:
  peer_path.write_bytes(peer_original);peer_helper.unlink();peer_overlaid=False
def overlay_peer():
 global peer_overlaid
 text=peer_original.decode();marker='  const context = await browser.newContext()';assert text.count(marker)==1
 text='import {grantIsolatedPermission} from "./isolated-peer-permission"\n'+text.replace(marker,marker+'\n  await grantIsolatedPermission(context)',1)
 peer_helper.write_bytes(pathlib.Path('/stage/procedure/permission-context.ts').read_bytes());peer_path.write_text(text);peer_overlaid=True
 (E/'peer-permission-overlay.json').write_text(json.dumps({'testBeforeSha256':hashlib.sha256(peer_original).hexdigest(),'testAfterSha256':sha(peer_path),'helperSha256':sha(peer_helper),
   'scope':'Only manual peer-test contexts, exact owned origin microphone grant; original assertions and skip unchanged; restored before final source check'})+'\n')
def restore():
 global overlaid
 if overlaid:(R/'playwright.config.ts').write_bytes(original);overlaid=False
def artifacts(name):
 out=[];total=0
 for relative in ['test-results','playwright-report']:
  d=R/relative
  if not d.exists():continue
  assert not d.is_symlink()
  for p in sorted(d.rglob('*')):
   assert not p.is_symlink()
   if not p.is_file():continue
   total+=p.stat().st_size;assert total<128*1024**2
   dest=E/'artifacts'/name/p.relative_to(R);dest.parent.mkdir(parents=True,exist_ok=True);shutil.copyfile(p,dest);out.append({'path':dest.relative_to(E).as_posix(),'bytes':dest.stat().st_size,'sha256':sha(dest)})
 return out
try:
 for step in steps:
  if step['id']=='check-source-final':restore()
  command=step['command'];name=step['id']
  if name=='peer-recovery':overlay_peer()
  report=E/(name+'-playwright.json')
  if name in ('test-3','test-4','test-5','test-6'):command=[*command,'--reporter=list,json','--workers=1','--retries=0']
  if command[:4]==['pnpm','exec','vitest','run']:command=[*command,'--maxWorkers=2','--reporter=verbose','--reporter=json','--outputFile='+str(E/'units.json')]
  row={'id':name,'command':command,'exitCode':None};result['steps'].append(row);started=time.monotonic()
  try:
   with (E/(name+'.log')).open('wb') as log:p=subprocess.run(command,cwd=R,env=env|step.get('env',{})|{'PLAYWRIGHT_JSON_OUTPUT_FILE':str(report)},stdout=log,stderr=subprocess.STDOUT,timeout=300 if name=='peer-recovery' else 2700)
   row['exitCode']=p.returncode;assert p.returncode==0,name+' failed'
   if name=='test-3':checks['verify'](report,193,61,'/stage/procedure/original-browser-cases.json')
   elif name in ('test-4','test-5','test-6'):checks['verify'](report,{'test-4':9,'test-5':12,'test-6':7}[name],0)
   elif name=='peer-recovery':
    rows=checks['verify'](report,1,0)
    (E/'peer-recovery.json').write_text(json.dumps(checks['recovery'](rows,'client'),indent=2)+'\n')
    contexts=[json.loads(line) for line in (E/'context-permissions.jsonl').read_text().splitlines()]
    assert len(contexts)==3 and all(c['origin']=='http://127.0.0.1:5173' and c['permission']=='microphone' and c['state']=='granted' and not c['captureInvoked'] for c in contexts)
   elif name=='test-1':
    units=json.loads((E/'units.json').read_text());assert units['success'] and all(a['status']=='passed' for f in units['testResults'] for a in f['assertionResults'])
  finally:
   if name=='peer-recovery':restore_peer()
   row.update(seconds=round(time.monotonic()-started,3),artifacts=artifacts(name))
  if name=='build':
   dist=R/'dist';files=[p for p in sorted(dist.rglob('*')) if p.is_file()];assert files and not any(p.is_symlink() for p in dist.rglob('*'));assert sum(p.stat().st_size for p in files)<128*1024**2
   entries=[{'path':p.relative_to(dist).as_posix(),'bytes':p.stat().st_size,'sha256':sha(p)} for p in files]
   with tarfile.open(E/'built-dist.tar.gz','w:gz') as tar:
    for p in files:tar.add(p,arcname=p.relative_to(dist).as_posix(),recursive=False)
   (E/'dist-manifest.json').write_text(json.dumps({'source':S['commit'],'files':entries},indent=2)+'\n');built_hash=sha(E/'built-dist.tar.gz')
   raw=original.decode();assert raw.count('export default ')==1
   patched=raw.replace('export default ','const isolatedOriginalConfig = ',1)+"\nexport default {...isolatedOriginalConfig,workers:1,retries:0,use:{...isolatedOriginalConfig.use,trace:'retain-on-failure',screenshot:'only-on-failure',launchOptions:{...isolatedOriginalConfig.use?.launchOptions,args:[...(isolatedOriginalConfig.use?.launchOptions?.args??[]),'--disable-features=WebRtcHideLocalIpsWithMdns','--allow-loopback-in-peer-connection']}}};\n"
   (R/'playwright.config.ts').write_text(patched);overlaid=True;(E/'config-overlay.json').write_text(json.dumps({'beforeSha256':hashlib.sha256(original).hexdigest(),'afterSha256':sha(R/'playwright.config.ts'),'scope':'Task-only concurrency/trace/RTC flags; original projects/selectors/assertions; restored before final source check'})+'\n')
 with (E/'dry-run.log').open('wb') as log:subprocess.run(['pnpm','release:iris','--dry-run'],cwd=R,env=env,stdout=log,stderr=subprocess.STDOUT,check=True,timeout=30)
 assert sha(E/'built-dist.tar.gz')==built_hash
 assert [{'path':p.relative_to(dist).as_posix(),'bytes':p.stat().st_size,'sha256':sha(p)} for p in sorted(dist.rglob('*')) if p.is_file()]==entries
 result.update(passed=True,distFiles=len(files),distArchiveSha256=built_hash,originalBrowserPassed=193,originalConditionalSkips=61,additionalPeerCases=1)
finally:
 restore_peer();restore();(E/'release-gates.json').write_text(json.dumps(result,indent=2)+'\n')
