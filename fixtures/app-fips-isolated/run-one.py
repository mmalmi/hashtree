"""Exact Chat dist qualification; no build, downloads, native tools or public traffic."""
import hashlib,json,os,pathlib,shutil,signal,socket,subprocess,sys,time,urllib.request,urllib.error
base=pathlib.Path('/stage');app=sys.argv[1];assert app=='chat'
manifest=json.loads((base/'procedure/manifest.json').read_text());spec=manifest['apps'][0]
evidence=base/'evidence'/app;evidence.mkdir()
env=dict(os.environ,CI='1',FIPS_APP_EVIDENCE=str(evidence),**spec['env'])
assert env['HOME']=='/stage/home/chat'
assert env['npm_config_manage_package_manager_versions']=='false'
sha=lambda p:hashlib.sha256(pathlib.Path(p).read_bytes()).hexdigest()
def verify_inputs():
    assert sha('pnpm-lock.yaml')==spec['lockSha256']
    assert json.loads(pathlib.Path('package.json').read_text())['packageManager']==spec['packageManager']
    for file,digest in spec['testFiles'].items():assert sha(file)==digest
    expected={file['path']:file['sha256'] for file in spec['distFiles']}
    actual={str(p.relative_to(spec['distDir'])):sha(p) for p in pathlib.Path(spec['distDir']).rglob('*') if p.is_file()}
    assert actual==expected
verify_inputs()
versions={'node':subprocess.check_output(['node','--version'],env=env,text=True).strip(),
          'pnpm':subprocess.check_output(['pnpm','--version'],env=env,text=True,timeout=10).strip(),
          'pnpmEntrySha256':sha('/stage/tools/pnpm/package/bin/pnpm.cjs')}
assert versions['node']==json.loads((base/'evidence/image-toolchain.json').read_text())['node']=='v26.13.0'
assert versions['pnpm']=='10.24.0'
subprocess.run(['python3','/stage/procedure/network-proof.py',str(evidence/'network.json')],env=env,check=True,timeout=15)
subprocess.run(['node','/stage/procedure/browser-proof.cjs',str(evidence/'browser.json')],env=env,check=True,timeout=90)
# These proofs precede loading test modules that can generate fixture identities.
if '--unsigned-only' in sys.argv:
    (evidence/'unsigned-passed.json').write_text(json.dumps({'passed':True,'unsignedOnly':True})+'\n');sys.exit(0)
command=['pnpm','exec','playwright','test',*spec['specs'],'--config=isolated.playwright.config.ts','--workers=2','--retries=0']
(evidence/'inputs.json').write_text(json.dumps({**versions,'command':command,'sourceCommit':spec['commit'],
    'runtimeMode':'Exact hash-verified dist preview; no rebuild','image':manifest['image']},indent=2)+'\n')
budget=json.loads((base/'procedure/app-budgets.json').read_text())[app]
started=time.monotonic();processes=[]
def stop(process):
    # Reap the owned group even if its immediate parent has exited.
    try:os.killpg(process.pid,signal.SIGTERM)
    except ProcessLookupError:pass
    try:process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid,signal.SIGKILL);process.wait(timeout=5)
def test_selection(report):
    selected=[]
    def walk(suites):
        for suite in suites:
            for item in suite.get('specs',[]):
                for test in item.get('tests',[]):selected.append((test['projectName'],item['file'],item['title']))
            walk(suite.get('suites',[]))
    walk(report['suites'])
    assert set(p for p,_,_ in selected)==set(spec['expectedProjects']),selected
    for name in spec['specs']:
        assert any(p=='chromium' and f in (name,name.removeprefix('e2e/')) for p,f,_ in selected),name
    return sorted(selected)
try:
    if spec.get('requiredUnitFiles'):
        unit_command=['pnpm','test',*spec['requiredUnitFiles'],'--maxWorkers=1','--reporter=verbose','--reporter=json','--outputFile='+str(evidence/'units.json')]
        with (evidence/'units.log').open('wb') as unit_log:
            unit=subprocess.Popen(unit_command,env=env,stdout=unit_log,stderr=subprocess.STDOUT,start_new_session=True);processes.append(unit)
            unit_code=unit.wait(timeout=300)
        stop(unit)
        assert unit_code==0,f'Uncovered exact-tuple release units exited {unit_code}'
        unit_report=json.loads((evidence/'units.json').read_text())
        assert unit_report['success'] and unit_report['numFailedTests']==0 and unit_report['numFailedTestSuites']==0
    with socket.socket() as probe:probe.bind(('127.0.0.1',5721))
    output=(evidence/'preview.log').open('wb')
    preview=subprocess.Popen(spec['previewCommand'],env=env,stdout=output,stderr=subprocess.STDOUT,start_new_session=True);processes.append(preview)
    try:
        deadline=time.monotonic()+30
        while True:
            assert preview.poll() is None,'Exact preview exited before readiness'
            try:
                with urllib.request.urlopen('http://127.0.0.1:5721/',timeout=1) as response:body=response.read()
                assert hashlib.sha256(body).hexdigest()==next(f['sha256'] for f in spec['distFiles'] if f['path']=='index.html')
                break
            except (urllib.error.URLError,TimeoutError):
                assert time.monotonic()<deadline,'Exact preview readiness deadline';time.sleep(.1)
        readback=[]
        for file in spec['distFiles']:
            with urllib.request.urlopen('http://127.0.0.1:5721/'+file['path'],timeout=2) as response:body=response.read()
            assert len(body)==file['bytes'] and hashlib.sha256(body).hexdigest()==file['sha256'],file['path']
            readback.append(file)
        (evidence/'preview-readback.json').write_text(json.dumps({'passed':True,'files':readback},indent=2)+'\n')
        listed=subprocess.run([*command,'--list','--reporter=json'],env=env,check=True,capture_output=True,text=True,timeout=30)
        (evidence/'selection.json').write_text(listed.stdout)
        selection=test_selection(json.loads(listed.stdout))
        with (evidence/'tests.log').open('wb') as log:
            test_env=dict(env,PLAYWRIGHT_JSON_OUTPUT_FILE=str(evidence/'results.json'))
            process=subprocess.Popen([*command,'--reporter=line,json'],env=test_env,stdout=log,stderr=subprocess.STDOUT,start_new_session=True);processes.append(process)
            code=process.wait(timeout=max(1,budget-(time.monotonic()-started)))
        assert code==0,f'Unchanged Chat selected suites exited {code}'
        report=json.loads((evidence/'results.json').read_text())
        assert test_selection(report)==selection,'Executed selection differs from the admitted projects/specs'
        assert report['stats']['unexpected']==0 and report['stats']['flaky']==0
    finally:output.close()
finally:
    for process in reversed(processes):stop(process)
    results=pathlib.Path('test-results')
    if results.exists():
        files=[p for p in results.rglob('*') if p.is_file() and not p.is_symlink()]
        assert sum(p.stat().st_size for p in files)<256*1024**2,'Test artifact budget exceeded; originals retained in owned stage'
        shutil.copytree(results,evidence/'test-results',symlinks=True)
    verify_inputs()
    subprocess.run(['python3','/stage/procedure/network-proof.py',str(evidence/'network-after-attempt.json')],env=env,check=True,timeout=15)
contexts=[json.loads(line) for line in (evidence/'context-permissions.jsonl').read_text().splitlines()]
expected_engines={'chromium' if name=='chromium' else name.split('-')[0] for name in spec['expectedProjects']}
assert set(c['engine'] for c in contexts)==expected_engines
assert all(c['origin']=='http://127.0.0.1:5721' and not c['captureInvoked'] for c in contexts)
assert all(c['state']=='granted' and c['permission']=='microphone' for c in contexts if c['engine']=='chromium')
assert all(c['permission'] is None for c in contexts if c['engine']!='chromium')
(evidence/'passed.json').write_text(json.dumps({'passed':True,'sourceCommit':spec['commit'],
    'unchangedAssertions':True,'testFilesByteIdentical':True,'distBytesUnchanged':True,
    'projects':spec['expectedProjects'],'stats':report['stats']},indent=2)+'\n')
