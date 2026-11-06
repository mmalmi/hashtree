"""Run the complete checked-in release profile inside the proven OS boundary."""
import hashlib,json,os,pathlib,selectors,shutil,subprocess,sys,tarfile,time
app=sys.argv[1];base=pathlib.Path('/stage');evidence=pathlib.Path(os.environ['FIPS_APP_EVIDENCE']);root=pathlib.Path.cwd()
spec=next(a for a in json.loads((base/'procedure/manifest.json').read_text())['apps'] if a['id']==app)
env=dict(os.environ,PW_WORKERS='1',PW_MAX_WORKERS='1');sha=lambda p:hashlib.sha256(p.read_bytes()).hexdigest()
assert sha(root/'package.json')==spec['packageSha256'] and sha(root/'pnpm-lock.yaml')==spec['lockSha256']
assert sha(root/spec['releaseProfilePath'])==spec['releaseProfileSha256']
if app=='meet':
    code="const m=await import('./scripts/release-site.mjs');console.log(JSON.stringify(m.createReleasePlan(m.parseArgs(['--dry-run']))));"
    profile=json.loads(subprocess.check_output(['node','--input-type=module','-e',code],env=env,text=True,timeout=15))
    # The frozen installation is completed before isolation; only publication/deployment are excluded.
    assert {s['id'] for s in profile['steps']}=={'install','build','test-unit','test-smoke','test-check','test-calls','test-offline','publish','deploy'}
    steps=[{'name':s['id'],'command':s['command']} for s in profile['steps'] if s['id'] not in {'install','publish','deploy'}]
    steps.append({'name':'dry-run','command':['node','scripts/release-site.mjs','--dry-run']})
else:
    code="const m=await import('./scripts/release-site.mjs');const p=m.releaseProfiles[process.argv[1]];if(!p)throw Error('No release profile');console.log(JSON.stringify(p));"
    profile=json.loads(subprocess.check_output(['node','--input-type=module','-e',code,app],env=env,text=True,timeout=15));assert profile['distDir']==spec['distDir']
    steps=[{'name':'build','command':profile['buildCommand']}]
    for i,item in enumerate(profile['testCommands']):steps.append({'name':f'release-check-{i+1}',**({'command':item} if isinstance(item,list) else item)})
    steps.append({'name':'dry-run','command':['node','scripts/release-site.mjs',app,'--dry-run']})
result={'source':spec['commit'],'profileSha256':sha(root/spec['releaseProfilePath']),'steps':[],'passed':False};cache=None
(evidence/'release-plan.json').write_text(json.dumps({'profile':profile,'steps':steps},indent=2)+'\n')
def retain_artifacts(name):
    records=[];total=0
    for relative in ['test-results','tmp/screens','playwright-report']:
        directory=root/relative
        if not directory.exists():continue
        assert not directory.is_symlink()
        for p in sorted(directory.rglob('*')):
            assert not p.is_symlink(),'Artifact symlink is not permitted'
            if not p.is_file():continue
            total+=p.stat().st_size;assert total<128*1024**2,'Step artifact budget'
            dest=evidence/'artifacts'/name/p.relative_to(root);dest.parent.mkdir(parents=True,exist_ok=True);shutil.copyfile(p,dest)
            records.append({'path':dest.relative_to(evidence).as_posix(),'sha256':sha(dest),'bytes':dest.stat().st_size})
    return records
try:
    for step in steps:
        command=step['command'];assert isinstance(command,list) and command and all(isinstance(x,str) for x in command)
        step_env=env|step.get('env',{})|{'PW_WORKERS':'1','PW_MAX_WORKERS':'1','PLAYWRIGHT_JSON_OUTPUT_FILE':str(evidence/(step['name']+'-playwright.json'))}
        if command[:3] in (['pnpm','run','test:unit'],['pnpm','run','test']) or command[:4]==['pnpm','exec','vitest','run']:command=[*command,'--maxWorkers=2']
        row={'name':step['name'],'command':command,'exitCode':None};result['steps'].append(row);started=time.monotonic()
        try:
            with (evidence/(step['name']+'.log')).open('wb') as log:done=subprocess.run(command,env=step_env,stdout=log,stderr=subprocess.STDOUT,timeout=1800)
            row['exitCode']=done.returncode;assert done.returncode==0,f"{step['name']} failed; retained complete log"
        finally:row.update(seconds=round(time.monotonic()-started,3),artifacts=retain_artifacts(step['name']))
    dist=root/spec['distDir'];files=[p for p in sorted(dist.rglob('*')) if p.is_file()]
    assert files and not any(p.is_symlink() for p in dist.rglob('*'));assert sum(p.stat().st_size for p in files)<128*1024**2
    manifest=[{'path':p.relative_to(dist).as_posix(),'bytes':p.stat().st_size,'sha256':sha(p)} for p in files]
    with tarfile.open(evidence/'qualified-dist.tar.gz','w:gz') as archive:
        for p in files:archive.add(p,arcname=p.relative_to(dist).as_posix(),recursive=False)
    (evidence/'dist-manifest.json').write_text(json.dumps({'source':spec['commit'],'files':manifest},indent=2)+'\n')
    assert sha(root/'pnpm-lock.yaml')==spec['lockSha256'],'Frozen lock mutated'
    result.update(passed=True,distFiles=len(files),distBytes=sum(x['bytes'] for x in manifest),distArchiveSha256=sha(evidence/'qualified-dist.tar.gz'))
finally:
    if cache is not None:
        cache.terminate()
        try:cache.wait(timeout=10)
        except subprocess.TimeoutExpired:cache.kill();cache.wait(timeout=5)
    (evidence/'release-gates.json').write_text(json.dumps(result,indent=2)+'\n')
