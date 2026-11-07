"""Run the complete checked-in release profile inside the proven OS boundary."""
import hashlib,json,os,pathlib,selectors,shutil,subprocess,sys,tarfile,time,threading,signal,socket
app=sys.argv[1];base=pathlib.Path('/stage');evidence=pathlib.Path(os.environ['FIPS_APP_EVIDENCE']);root=pathlib.Path.cwd()
spec=next(a for a in json.loads((base/'procedure/manifest.json').read_text())['apps'] if a['id']==app)
env=dict(os.environ,PW_WORKERS='1',PW_MAX_WORKERS='1');sha=lambda p:hashlib.sha256(p.read_bytes()).hexdigest()
assert sha(root/'package.json')==spec['packageSha256'] and sha(root/'pnpm-lock.yaml')==spec['lockSha256']
assert sha(root/spec['releaseProfilePath'])==spec['releaseProfileSha256']
code="const m=await import('./scripts/release-site.mjs');const p=m.releaseProfiles[process.argv[1]];if(!p)throw Error('No release profile');console.log(JSON.stringify(p));"
profile=json.loads(subprocess.check_output(['node','--input-type=module','-e',code,app],env=env,text=True,timeout=15));assert profile['distDir']==spec['distDir']
steps=[{'name':'build','command':profile['buildCommand']}]
for i,item in enumerate(profile['testCommands']):steps.append({'name':f'release-check-{i+1}',**({'command':item} if isinstance(item,list) else item)})
if ['pnpm','run','test:unit'] not in [s['command'] for s in steps]:steps.append({'name':'unit','command':['pnpm','run','test:unit']})
steps.append({'name':'dry-run','command':['node','scripts/release-site.mjs',app,'--dry-run']})
if app=='git':
    original=next(s for s in steps if s['command']==['pnpm','run','test:e2e:git'])
    position=steps.index(original)
    steps[position:position+1]=[dict(original,name=original['name']+f'-shard-{i}',command=original['command']+[f'--shard={i}/12'],originalCommand=original['command'],gitShard=i) for i in range(1,13)]
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
    for i in range(1,13):visit(json.loads((evidence/f'release-check-2-shard-{i}-playwright.json').read_text()))
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
    # Resume exact already executed browser cases from the checksum-pinned Actions artifact.
    # This release uses the same app archive, release profile, image, lock and test fixtures.
    previous=base/'earlier/git'
    load=lambda directory,name:json.loads((directory/name).read_bytes())
    prev=load(previous,'release-gates.json')
    eq=load(evidence.parent,'git-source-equivalence.json')
    assert prev['source']==eq['earlierSource'] and spec['commit']==eq['currentSource'] and eq['changed']==['tests/releaseSite.test.ts'] and eq['otherwiseIdentical']>500
    assert prev['profileSha256']==sha(root/spec['releaseProfilePath'])
    assert load(previous,'inputs.json')|{'sourceCommit':spec['commit']}==load(evidence,'inputs.json')
    assert load(previous,'packages.json')==load(evidence,'packages.json')
    assert load(previous,'release-plan.json')==load(evidence,'release-plan.json')
    old,new=load(previous,'browser.json'),load(evidence,'browser.json')
    for field in ['node','playwright','browser']:assert old[field]==new[field]
    image_proof=lambda report:{(p['path'],p['sha256']) for p in report['actualBrowserExecutables']}
    assert image_proof(old)==image_proof(new) and old['passed'] and old['rtc']['echoed']=='loopback-only' and old['candidateAddressesAllowed'] and len(old['rawWorkerDenial'])==4
    for name in ['network.json','network-after-attempt.json']:
        net=load(previous,name)
        assert net['interfaces']==['fixture0','lo'] and net['uid']!=0 and net['effectiveAndBoundingCapabilities']==0 and len(net['externalDenied'])==len(net['childDenied'])==4
    assert not load(previous,'resource-final.json')['samplerErrors'] and int(load(previous,'resource-final.json')['sample']['events']['oom_kill'])==0
    shutil.copyfile(previous/'context-permissions.jsonl',evidence/'context-permissions.jsonl')
    (evidence/'browser-reuse.json').write_text(json.dumps({'run':42352553046,'artifact':13566378424,'sha256':'e1c1dad63a3c49b4474d7193a0a68931aceafccaa1cb81ad7f0b4a555197e05d','tests':82,'correctedSourceLineCount':3}))
    for step in steps:
        command=step['command'];assert isinstance(command,list) and command and all(isinstance(x,str) for x in command)
        step_env=env|step.get('env',{})|{'PW_WORKERS':'1','PW_MAX_WORKERS':'1','PLAYWRIGHT_JSON_OUTPUT_FILE':str(evidence/(step['name']+'-playwright.json'))}
        if command==['pnpm','run','test:unit']:
            # The package unit suite checks executable discovery and the default
            # command name with its own tiny shell fixtures. Product/native gates
            # continue using the digest-pinned absolute executable.
            tmp=base/'git-unit-temp';tmp.mkdir(mode=0o700)
            step_env['TMPDIR']=str(tmp)
            step_env.pop('HTREE_BIN',None)
            step_env.pop('GIT_REMOTE_HTREE_BIN',None)
        if command[:3]==['pnpm','run','test:unit'] or command[:4]==['pnpm','exec','vitest','run']:command=[*command,'--maxWorkers=2']
        row={'name':step['name'],'command':command,'exitCode':None};result['steps'].append(row);started=time.monotonic()
        is_shard='gitShard' in step
        if is_shard:
            oldrow=next(v for v in prev['steps'] if v['name']==step['name'])
            assert oldrow['command']==command and oldrow['exitCode']==0
            row.update(oldrow,reusedFromRun=42352553046)
            for f in oldrow['artifacts']:
                rel=pathlib.PurePosixPath(f['path']);assert not rel.is_absolute() and '..' not in rel.parts
                target=evidence/rel;target.parent.mkdir(parents=True,exist_ok=True)
                assert sha(previous/rel)==f['sha256'] and (previous/rel).stat().st_size==f['bytes']
                shutil.copyfile(previous/rel,target)
            for suffix in ['.log','-playwright.json']:
                name=step['name']+suffix;shutil.copyfile(previous/name,evidence/name)
            if step['gitShard']==12:git_coverage()
            continue
        if is_shard:
            assert_git_ports_closed()
            step_env['NODE_OPTIONS']=(step_env.get('NODE_OPTIONS','')+' --max-old-space-size=1280').strip()
        before_processes=process_snapshot();row['resourcesBefore']=resource_sample()
        try:
            with (evidence/(step['name']+'.log')).open('wb') as log:done=subprocess.run(command,env=step_env,stdout=log,stderr=subprocess.STDOUT,timeout=1800,start_new_session=True)
            row['exitCode']=done.returncode
            if not is_shard:assert done.returncode==0,f"{step['name']} failed; retained complete log"
        finally:
            if is_shard:
                row['reapedTaskDescendants']=clean_shard_children(before_processes);assert_git_ports_closed()
            row.update(seconds=round(time.monotonic()-started,3),resourcesAfter=resource_sample(),artifacts=retain_artifacts(step['name']))
        if is_shard and step['gitShard']==12:
            assert all(r['exitCode']==0 for r in result['steps'] if '-shard-' in r['name']),'Original Git shard failed'
            git_coverage()
    assert not resource_errors,'Resource sampler failed'
    dist=root/spec['distDir'];files=[p for p in sorted(dist.rglob('*')) if p.is_file()]
    assert files and not any(p.is_symlink() for p in dist.rglob('*'));assert sum(p.stat().st_size for p in files)<128*1024**2
    manifest=[{'path':p.relative_to(dist).as_posix(),'bytes':p.stat().st_size,'sha256':sha(p)} for p in files]
    with tarfile.open(evidence/'qualified-dist.tar.gz','w:gz') as archive:
        for p in files:archive.add(p,arcname=p.relative_to(dist).as_posix(),recursive=False)
    (evidence/'dist-manifest.json').write_text(json.dumps({'source':spec['commit'],'files':manifest},indent=2)+'\n')
    assert sha(root/'pnpm-lock.yaml')==spec['lockSha256'],'Frozen lock mutated'
    result.update(passed=True,distFiles=len(files),distBytes=sum(x['bytes'] for x in manifest),distArchiveSha256=sha(evidence/'qualified-dist.tar.gz'))
finally:
    resource_stop.set();resource_thread.join(timeout=5)
    (evidence/'resource-final.json').write_text(json.dumps({'sample':resource_sample(),'samplerErrors':resource_errors},indent=2)+'\n')
    if cache is not None:
        cache.terminate()
        try:cache.wait(timeout=10)
        except subprocess.TimeoutExpired:cache.kill();cache.wait(timeout=5)
    (evidence/'release-gates.json').write_text(json.dumps(result,indent=2)+'\n')
