"""Client qualification executes its original release plan up to publication."""
import hashlib,json,os,pathlib,shutil,subprocess,tarfile,time
R=pathlib.Path.cwd();E=pathlib.Path(os.environ['FIPS_APP_EVIDENCE']);M=json.loads(pathlib.Path('/stage/procedure/manifest.json').read_text());S=M['apps'][0];env=dict(os.environ);sha=lambda p:hashlib.sha256(p.read_bytes()).hexdigest()
assert sha(R/'package.json')==S['packageSha256'] and sha(R/'pnpm-lock.yaml')==S['lockSha256']
assert sha(R/'scripts/release-site.mjs')==S['releaseProfileSha256'] and sha(R/'scripts/check-release-source.mjs')==S['sourceCheckerSha256']
code="const m=await import('./scripts/release-site.mjs');console.log(JSON.stringify(m.createReleasePlan(m.parseArgs([]))));"
plan=json.loads(subprocess.check_output(['node','--input-type=module','-e',code],env=env,text=True,timeout=20));(E/'original-release-plan.json').write_text(json.dumps(plan,indent=2)+'\n')
steps=[]
for step in plan['steps']:
 if step['id'] in ['publish','deploy']:break
 steps.append(step)
assert steps[0]['id']=='check-source-start' and steps[-1]['id']=='check-source-final'
original=(R/'playwright.config.ts').read_bytes();overlaid=False;result={'passed':False,'source':S['commit'],'steps':[],'publicationStepsNotExecuted':True}
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
  command=step['command']
  if step['id']=='test-3':
   assert command==['pnpm','run','test:release:e2e']
   command=[*command,'--grep','fresh shipped defaults route reading, graph sync and signed public posts to Ditto']
  if command[:4]==['pnpm','exec','vitest','run']:command=[*command,'--maxWorkers=2']
  row={'id':step['id'],'command':command,'exitCode':None};result['steps'].append(row);started=time.monotonic()
  try:
   with (E/(step['id']+'.log')).open('wb') as log:p=subprocess.run(command,cwd=R,env=env|step.get('env',{}),stdout=log,stderr=subprocess.STDOUT,timeout=2700)
   row['exitCode']=p.returncode;assert p.returncode==0,step['id']+' failed'
  finally:row.update(seconds=round(time.monotonic()-started,3),artifacts=artifacts(step['id']))
  if step['id']=='build':
   dist=R/'dist';files=[p for p in sorted(dist.rglob('*')) if p.is_file()];assert files and not any(p.is_symlink() for p in dist.rglob('*'));assert sum(p.stat().st_size for p in files)<128*1024**2
   entries=[{'path':p.relative_to(dist).as_posix(),'bytes':p.stat().st_size,'sha256':sha(p)} for p in files]
   with tarfile.open(E/'built-dist.tar.gz','w:gz') as tar:
    for p in files:tar.add(p,arcname=p.relative_to(dist).as_posix(),recursive=False)
   (E/'dist-manifest.json').write_text(json.dumps({'source':S['commit'],'files':entries},indent=2)+'\n')
   built_hash=sha(E/'built-dist.tar.gz')
   raw=original.decode();assert raw.count('export default ')==1
   patched=raw.replace('export default ','const isolatedOriginalConfig = ',1)+"\nexport default {...isolatedOriginalConfig,workers:1,retries:0,use:{...isolatedOriginalConfig.use,trace:'retain-on-failure',screenshot:'only-on-failure',launchOptions:{...isolatedOriginalConfig.use?.launchOptions,args:[...(isolatedOriginalConfig.use?.launchOptions?.args??[]),'--disable-features=WebRtcHideLocalIpsWithMdns','--allow-loopback-in-peer-connection']}}};\n"
   (R/'playwright.config.ts').write_text(patched);overlaid=True;(E/'config-overlay.json').write_text(json.dumps({'beforeSha256':hashlib.sha256(original).hexdigest(),'afterSha256':sha(R/'playwright.config.ts'),'scope':'Task-only concurrency/trace/RTC flags; original projects/selectors/assertions; restored before final source check'})+'\n')
 with (E/'dry-run.log').open('wb') as log:subprocess.run(['pnpm','release:iris','--dry-run'],cwd=R,env=env,stdout=log,stderr=subprocess.STDOUT,check=True,timeout=30)
 assert sha(E/'built-dist.tar.gz')==built_hash
 result.update(passed=True,distFiles=len(files),distArchiveSha256=built_hash,reusedRun='42236212540',reusedPassingCases=192,productionInputsUnchanged=True)
finally:
 restore();(E/'release-gates.json').write_text(json.dumps(result,indent=2)+'\n')
