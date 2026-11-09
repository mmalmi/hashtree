"""Run the complete checked-in release profile inside the proven OS boundary."""
import hashlib,json,os,pathlib,selectors,shutil,subprocess,sys,tarfile,time,threading,signal,socket
app=sys.argv[1];base=pathlib.Path('/stage');evidence=pathlib.Path(os.environ['FIPS_APP_EVIDENCE']);root=pathlib.Path.cwd()
spec=next(a for a in json.loads((base/'procedure/manifest.json').read_text())['apps'] if a['id']==app)
env=dict(os.environ,PW_WORKERS='1',PW_MAX_WORKERS='1');sha=lambda p:hashlib.sha256(p.read_bytes()).hexdigest()
source_root=base/'apps'/app
assert sha(root/'package.json')==spec['packageSha256'] and sha(source_root/'pnpm-lock.yaml')==spec['lockSha256']
assert sha(source_root/spec['releaseProfilePath'])==spec['releaseProfileSha256']
code="const m=await import('./scripts/release-site.mjs');const p=m.releaseProfiles?.[process.argv[1]] ?? m.releaseProfile;if(!p)throw Error('No release profile');console.log(JSON.stringify(p));"
profile=json.loads(subprocess.check_output(['node','--input-type=module','-e',code,app],env=env,text=True,timeout=15));assert profile['distDir']==spec['distDir']
steps=[{'name':'build','command':profile['buildCommand']}]
for i,item in enumerate(profile['testCommands']):steps.append({'name':f'release-check-{i+1}',**({'command':item} if isinstance(item,list) else item)})
if app=='cc':
    steps.append({'name':'release-units','command':['pnpm','run','test:release']})
    portable=next(s for s in steps if s['command']==['node','./scripts/portable-smoke.mjs'])
    portable['originalCommand']=portable['command'];portable['command']=['node','/stage/procedure/cached-portable.mjs']
    peer=['pnpm','exec','playwright','test','e2e/p2p-connection.spec.ts','--workers=1','--retries=0']
else:
    peer=['pnpm','run','test:runtime','--workers=1','--retries=0']
steps.append({'name':'peer-recovery','command':peer})
steps.append({'name':'dry-run','command':['node','scripts/release-site.mjs']+(['contacts'] if app=='contacts' else [])+['--dry-run']})
if app=='sites':
    (evidence/'worker-optout').mkdir()
    steps.append({'name':'worker-optout','command':['node','/stage/procedure/worker-optout.mjs',str(root),str(evidence/'worker-optout')],'env':{'NODE_OPTIONS':''}})
result={'source':spec['commit'],'profileSha256':sha(source_root/spec['releaseProfilePath']),'steps':[],'passed':False};cache=None
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
def process_snapshot():
    found={}
    for directory in pathlib.Path('/proc').glob('[0-9]*'):
        try:
            fields=(directory/'stat').read_text().rsplit(')',1)[1].split()
            if fields[0]!='Z':found[int(directory.name)]=int(fields[19])
        except (FileNotFoundError,ProcessLookupError):pass
    return found
def clean_shard_children(before):
    def remaining():return {pid:born for pid,born in process_snapshot().items() if before.get(pid)!=born}
    observed=remaining()
    for sig in (signal.SIGTERM,signal.SIGKILL):
        for pid,born in remaining().items():
            if process_snapshot().get(pid)==born:
                try:os.kill(pid,sig)
                except ProcessLookupError:pass
        deadline=time.monotonic()+5
        while remaining() and time.monotonic()<deadline:time.sleep(.1)
        if not remaining():break
    assert not remaining(),'Task shard descendants remain'
    return list(observed)
def assert_git_ports_closed():
    for port in (43810,43811,43812):
        with socket.socket() as probe:
            probe.settimeout(.2);assert probe.connect_ex(('127.0.0.1',port))!=0,'Prior shard server remains'
def git_coverage():
    expected=json.loads((base/'procedure/git-case-identities.json').read_text())['cases'];actual=[]
    def visit(suite):
        for case in suite.get('specs',[]):
            for test in case.get('tests',[]):
                assert test['status']=='expected' and len(test['results'])==1 and test['results'][0]['retry']==0
                assert test['results'][0]['status']==test['expectedStatus']=='passed'
                actual.append({k:case[k] for k in ('id','file','line','column','title')}|{'projectName':test['projectName'],'expectedStatus':test['expectedStatus']})
        for child in suite.get('suites',[]):visit(child)
    for i in range(1,5):visit(json.loads((evidence/f'release-check-2-shard-{i}-playwright.json').read_text()))
    assert len(actual)==len(expected)==82 and sorted(actual,key=lambda x:x['id'])==sorted(expected,key=lambda x:x['id']),'Original Git case coverage differs'
    (evidence/'git-shard-coverage.json').write_text(json.dumps({'passed':True,'cases':actual,'count':len(actual),'retries':0,'skips':0,'originalCommand':['pnpm','run','test:e2e:git']},indent=2)+'\n')
# Capture the whole container's memory pressure, including child-only OOMs.
# No host process names, command lines, event bodies or application keys are read.
resource_stop=threading.Event();resource_errors=[]
def resource_sample():
    cg=pathlib.Path('/sys/fs/cgroup')
    row={'monotonic':time.monotonic(),'current':int((cg/'memory.current').read_text()),'peak':int((cg/'memory.peak').read_text()),'events':dict(line.split() for line in (cg/'memory.events').read_text().splitlines())}
    row['stat']={k:int(v) for k,v in (line.split() for line in (cg/'memory.stat').read_text().splitlines()) if k in ('anon','file','kernel','sock','shmem')}
    processes=[]
    for directory in pathlib.Path('/proc').glob('[0-9]*'):
        try:
            status=dict(line.split(':',1) for line in (directory/'status').read_text().splitlines() if ':' in line)
            processes.append({'pid':int(directory.name),'ppid':int(status['PPid']),'name':status['Name'].strip(),'rssKiB':int(status.get('VmRSS','0 kB').split()[0])})
        except (FileNotFoundError,ProcessLookupError):pass
    row['processCount']=len(processes);row['largestProcesses']=sorted(processes,key=lambda p:p['rssKiB'],reverse=True)[:12]
    return row
def resource_watch():
    with (evidence/'resources.jsonl').open('x') as output:
        while True:
            try:output.write(json.dumps(resource_sample())+'\n');output.flush()
            except Exception as error:resource_errors.append(str(error));return
            if resource_stop.wait(2):return
resource_thread=threading.Thread(target=resource_watch,daemon=True);resource_thread.start()
try:
    if app=='maps':
        cache=subprocess.Popen(['node','/stage/procedure/map-cache.mjs',str(root),str(base/'map-cache')],env=env,stdout=subprocess.PIPE,stderr=(evidence/'map-cache.log').open('wb'),text=True)
        ready=selectors.DefaultSelector();ready.register(cache.stdout,selectors.EVENT_READ)
        assert ready.select(15),'Map cache startup deadline';env['MAP_FIXTURE_CACHE_URL']=json.loads(cache.stdout.readline())['url'];ready.close()
    for step in steps:
        command=step['command'];assert isinstance(command,list) and command and all(isinstance(x,str) for x in command)
        step_env=env|step.get('env',{})|{'BROWSER_BOUNDARY_RECEIPT':str(evidence/(step['name']+'-browser-boundary.jsonl'))}|{'PW_WORKERS':'1','PW_MAX_WORKERS':'1','PLAYWRIGHT_JSON_OUTPUT_FILE':str(evidence/(step['name']+'-playwright.json'))}
        if command[:3] in (['pnpm','run','test:unit'],['pnpm','run','test:model']) or command[:4]==['pnpm','exec','vitest','run']:command=[*command,'--maxWorkers=2']
        row={'name':step['name'],'command':command,'exitCode':None};result['steps'].append(row);started=time.monotonic()
        is_shard='gitShard' in step
        if is_shard:assert_git_ports_closed()
        before_processes=process_snapshot();row['resourcesBefore']=resource_sample()
        try:
            with (evidence/(step['name']+'.log')).open('wb') as log:done=subprocess.run(command,env=step_env,stdout=log,stderr=subprocess.STDOUT,timeout=1800,start_new_session=True)
            row['exitCode']=done.returncode
            if not is_shard:assert done.returncode==0,f"{step['name']} failed; retained complete log"
        finally:
            if is_shard:
                row['reapedTaskDescendants']=clean_shard_children(before_processes);assert_git_ports_closed()
            row.update(seconds=round(time.monotonic()-started,3),resourcesAfter=resource_sample(),artifacts=retain_artifacts(step['name']))
        if is_shard and step['gitShard']==4:
            assert all(r['exitCode']==0 for r in result['steps'] if '-shard-' in r['name']),'Original Git shard failed'
            git_coverage()
    if app=='sites':
        assert json.loads((evidence/'worker-optout/worker-optout.json').read_text())['status']=='passed'
    if app=='cc':
        fixture=json.loads((evidence/'cached-portable-reads.json').read_text())
        assert fixture['exitCode']==0 and fixture['signal'] is None and fixture['uniqueBlocks']>0 and fixture['noForwarding'] is True
    assert not resource_errors,'Resource sampler failed'
    dist=root/spec['distDir'];files=[p for p in sorted(dist.rglob('*')) if p.is_file()]
    assert files and not any(p.is_symlink() for p in dist.rglob('*'));assert sum(p.stat().st_size for p in files)<128*1024**2
    manifest=[{'path':p.relative_to(dist).as_posix(),'bytes':p.stat().st_size,'sha256':sha(p)} for p in files]
    with tarfile.open(evidence/'qualified-dist.tar.gz','w:gz') as archive:
        for p in files:archive.add(p,arcname=p.relative_to(dist).as_posix(),recursive=False)
    (evidence/'dist-manifest.json').write_text(json.dumps({'source':spec['commit'],'files':manifest},indent=2)+'\n')
    assert sha(source_root/'pnpm-lock.yaml')==spec['lockSha256'],'Frozen lock mutated'
    result.update(passed=True,distFiles=len(files),distBytes=sum(x['bytes'] for x in manifest),distArchiveSha256=sha(evidence/'qualified-dist.tar.gz'))
finally:
    resource_stop.set();resource_thread.join(timeout=5)
    (evidence/'resource-final.json').write_text(json.dumps({'sample':resource_sample(),'samplerErrors':resource_errors},indent=2)+'\n')
    if cache is not None:
        cache.terminate()
        try:cache.wait(timeout=10)
        except subprocess.TimeoutExpired:cache.kill();cache.wait(timeout=5)
    (evidence/'release-gates.json').write_text(json.dumps(result,indent=2)+'\n')
