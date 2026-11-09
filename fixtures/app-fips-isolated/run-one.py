"""Fresh complete checked-in Chat release profile, then actual peer recovery."""
import hashlib,json,os,pathlib,runpy,shutil,signal,socket,subprocess,sys,tarfile,time,urllib.request,urllib.error
base=pathlib.Path('/stage');app=sys.argv[1];assert app=='chat'
manifest=json.loads((base/'procedure/manifest.json').read_text());spec=manifest['apps'][0]
root=pathlib.Path.cwd();evidence=base/'evidence'/app;evidence.mkdir()
env=dict(os.environ,CI='1',FIPS_APP_EVIDENCE=str(evidence),**spec['env'])
assert env['HOME']=='/stage/home/chat' and env['npm_config_manage_package_manager_versions']=='false'
sha=lambda p:hashlib.sha256(pathlib.Path(p).read_bytes()).hexdigest()
def verify_inputs():
    assert sha('package.json')==spec['packageSha256'] and sha('pnpm-lock.yaml')==spec['lockSha256']
    assert sha(spec['releaseProfilePath'])==spec['releaseProfileSha256']
    for file,digest in spec['testFiles'].items():assert sha(file)==digest
verify_inputs()
versions={'node':subprocess.check_output(['node','--version'],env=env,text=True).strip(),
          'pnpm':subprocess.check_output(['pnpm','--version'],env=env,text=True,timeout=10).strip(),
          'pnpmEntrySha256':sha('/stage/tools/pnpm/package/bin/pnpm.cjs')}
assert versions['node']==json.loads((base/'evidence/image-toolchain.json').read_text())['node']=='v26.13.0'
assert versions['pnpm']=='10.24.0'
for command,timeout in [(['python3','/stage/procedure/network-proof.py',str(evidence/'network.json')],15),
                        (['node','/stage/procedure/installed-runtime-proof.cjs',str(evidence/'installed-runtime.json')],15),
                        (['node','/stage/procedure/browser-proof.cjs',str(evidence/'browser.json')],90)]:
    subprocess.run(command,env=env,check=True,timeout=timeout)
# No fixture identities are loaded before the independent boundary controls.
code="const m=await import('./scripts/release-site.mjs');console.log(JSON.stringify(m.createReleasePlan(m.parseArgs(['--dry-run']))));"
plan=json.loads(subprocess.check_output(['node','--input-type=module','-e',code],env=env,text=True,timeout=20))
assert [s['id'] for s in plan['steps']]==['install','build','test-portable','test-smoke','test-graph','test-relay-policy','test-e2e-nip07','publish','deploy']
steps=[s for s in plan['steps'] if s['id'] not in ('install','publish','deploy')]
(evidence/'original-release-plan.json').write_text(json.dumps(plan,indent=2)+'\n')
(evidence/'inputs.json').write_text(json.dumps({**versions,'sourceCommit':spec['commit'],'image':manifest['image'],
    'runtimeMode':'Fresh source build and complete original profile plus one additional actual-peer browser case'},indent=2)+'\n')
budget=json.loads((base/'procedure/app-budgets.json').read_text())[app]
started=time.monotonic();processes=[];dist_files=None
result={'passed':False,'source':spec['commit'],'steps':[],'allAppGatesFromCurrentRun':True,'reusedAppScopes':[]}
checks=runpy.run_path('/stage/procedure/browser-results.py')
def stop(process):
    try:os.killpg(process.pid,signal.SIGTERM)
    except ProcessLookupError:pass
    try:process.wait(timeout=5)
    except subprocess.TimeoutExpired:os.killpg(process.pid,signal.SIGKILL);process.wait(timeout=5)
def artifacts(name):
    records=[];total=0
    for relative in ['test-results','tmp/screens','playwright-report']:
        directory=root/relative
        if not directory.exists():continue
        assert not directory.is_symlink()
        for p in sorted(directory.rglob('*')):
            assert not p.is_symlink()
            if not p.is_file():continue
            total+=p.stat().st_size;assert total<256*1024**2
            dest=evidence/'artifacts'/name/p.relative_to(root);dest.parent.mkdir(parents=True,exist_ok=True);shutil.copyfile(p,dest)
            records.append({'path':dest.relative_to(evidence).as_posix(),'bytes':dest.stat().st_size,'sha256':sha(dest)})
    return records
def execute(name,command,seconds,extra=None):
    row={'id':name,'command':command,'exitCode':None};result['steps'].append(row);begin=time.monotonic()
    try:
        with (evidence/(name+'.log')).open('wb') as log:
            process=subprocess.Popen(command,env=env|(extra or {}),stdout=log,stderr=subprocess.STDOUT,start_new_session=True);processes.append(process)
            row['exitCode']=process.wait(timeout=min(seconds,max(1,budget-(time.monotonic()-started))))
        stop(process);assert row['exitCode']==0,name+' failed'
    finally:row.update(seconds=round(time.monotonic()-begin,3),artifacts=artifacts(name))
def snapshot_dist():
    dist=root/spec['distDir'];paths=[p for p in sorted(dist.rglob('*')) if p.is_file()]
    assert paths and not any(p.is_symlink() for p in dist.rglob('*')) and sum(p.stat().st_size for p in paths)<128*1024**2
    return [{'path':p.relative_to(dist).as_posix(),'bytes':p.stat().st_size,'sha256':sha(p)} for p in paths]
try:
    for step in steps:
        command=step['command'];name=step['id']
        if name=='test-e2e-nip07':
            assert command[:4]==['pnpm','exec','playwright','test']
            assert [x for x in command[4:] if x.endswith('.spec.ts')]==spec['specs']
            with socket.socket() as probe:probe.bind(('127.0.0.1',5721))
            output=(evidence/'preview.log').open('wb')
            preview=subprocess.Popen(spec['previewCommand'],env=env,stdout=output,stderr=subprocess.STDOUT,start_new_session=True);processes.append(preview)
            deadline=time.monotonic()+30
            while True:
                assert preview.poll() is None,'Preview exited before readiness'
                try:
                    with urllib.request.urlopen('http://127.0.0.1:5721/',timeout=1) as response:body=response.read()
                    assert hashlib.sha256(body).hexdigest()==next(f['sha256'] for f in dist_files if f['path']=='index.html');break
                except (urllib.error.URLError,TimeoutError):assert time.monotonic()<deadline;time.sleep(.1)
            for file in dist_files:
                with urllib.request.urlopen('http://127.0.0.1:5721/'+file['path'],timeout=2) as response:body=response.read()
                assert len(body)==file['bytes'] and hashlib.sha256(body).hexdigest()==file['sha256']
            (evidence/'preview-readback.json').write_text(json.dumps({'passed':True,'files':dist_files},indent=2)+'\n')
            command=[*command,'--config=isolated.playwright.config.ts','--reporter=line,json']
            execute(name,command,2400,{'PLAYWRIGHT_JSON_OUTPUT_FILE':str(evidence/'results.json')})
            checks['verify'](evidence/'results.json',62,3,base/'procedure/original-browser-cases.json')
            peer=['pnpm','exec','playwright','test','e2e/peer-attachments.spec.ts','--config=isolated.playwright.config.ts','--project=chromium','--workers=1','--retries=0','--reporter=line,json']
            execute('peer-recovery',peer,360,{'PLAYWRIGHT_JSON_OUTPUT_FILE':str(evidence/'peer-results.json')})
            rows=checks['verify'](evidence/'peer-results.json',1,0)
            (evidence/'peer-recovery.json').write_text(json.dumps(checks['recovery'](rows,'chat'),indent=2)+'\n')
            stop(preview);output.close()
        else:
            if name=='test-portable':
                assert command[:2]==['pnpm','test']
                command=[*command,'--maxWorkers=1','--reporter=verbose','--reporter=json','--outputFile='+str(evidence/'units.json')]
            execute(name,command,600 if name=='build' else 300)
            if name=='build':
                dist_files=snapshot_dist()
                with tarfile.open(evidence/'qualified-dist.tar.gz','w:gz') as archive:
                    for item in dist_files:archive.add(root/spec['distDir']/item['path'],arcname=item['path'],recursive=False)
                (evidence/'dist-manifest.json').write_text(json.dumps({'source':spec['commit'],'files':dist_files},indent=2)+'\n')
            if name=='test-portable':
                units=json.loads((evidence/'units.json').read_text())
                actual=[a for file in units['testResults'] for a in file['assertionResults']]
                assert units['success'] and len(units['testResults'])==34 and len(actual)==258 and all(a['status']=='passed' for a in actual)
    execute('dry-run',['node','scripts/release-site.mjs','--dry-run'],30)
    assert snapshot_dist()==dist_files
    result.update(passed=True,originalBrowserPassed=62,originalPlatformSkips=3,additionalPeerCases=1,distArchiveSha256=sha(evidence/'qualified-dist.tar.gz'))
finally:
    for process in reversed(processes):stop(process)
    verify_inputs()
    subprocess.run(['python3','/stage/procedure/network-proof.py',str(evidence/'network-after-attempt.json')],env=env,check=True,timeout=15)
    (evidence/'release-gates.json').write_text(json.dumps(result,indent=2)+'\n')
contexts=[json.loads(line) for line in (evidence/'context-permissions.jsonl').read_text().splitlines()]
assert set(c['engine'] for c in contexts)=={'chromium','firefox','webkit'}
assert all(c['origin']=='http://127.0.0.1:5721' and not c['captureInvoked'] for c in contexts)
assert all(c['state']=='granted' and c['permission']=='microphone' for c in contexts if c['engine']=='chromium')
assert all(c['permission'] is None for c in contexts if c['engine']!='chromium')
(evidence/'passed.json').write_text(json.dumps({'passed':True,'sourceCommit':spec['commit'],'completeOriginalReleaseProfile':True,'additionalActualPeerRecovery':True,'allAppGatesFromCurrentRun':True})+'\n')
