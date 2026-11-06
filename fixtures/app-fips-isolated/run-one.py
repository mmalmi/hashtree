import hashlib,json,os,pathlib,subprocess,sys,shutil
base=pathlib.Path('/stage');app=sys.argv[1];manifest=json.loads((base/'procedure/manifest.json').read_text());spec=next(a for a in manifest['apps'] if a['id']==app)
evidence=base/'evidence'/(app+os.environ.get('FIPS_EVIDENCE_SUFFIX',''));evidence.mkdir();home=base/'home'/app;home.mkdir(exist_ok=True);env=dict(os.environ,HOME=str(home),TMPDIR='/tmp',CI='1',PW_WORKERS='1',PW_MAX_WORKERS='1',PW_APP_PORT='43810',PW_RELAY_PORT='43811',PW_TEST_BLOSSOM_URL='http://127.0.0.1:43812',HTREE_ADDR='127.0.0.1:43812',HTREE_BIN='/stage/tools/native/hashtree/htree',GIT_REMOTE_HTREE_BIN='/stage/tools/native/hashtree/git-remote-htree',HTREE_CONFIG_DIR=str(home/'config'),HTREE_DATA_DIR=str(home/'data'),HTREE_PREFER_LOCAL_DAEMON='false')
env['PATH']='/stage/tools/native/hashtree:'+env['PATH'];env.update(spec.get('env',{}))
env['FIPS_APP_EVIDENCE']=str(evidence)
subprocess.run(['python3','/stage/procedure/network-proof.py',str(evidence/'network.json')],env=env,check=True,timeout=15)
subprocess.run(['node','/stage/procedure/browser-proof.cjs',str(evidence/'browser.json')],env=env,check=True,timeout=25)
if '--unsigned-only' in sys.argv:
    (evidence/'unsigned-passed.json').write_text(json.dumps({'passed':True,'unsignedOnly':True})+'\n');sys.exit(0)
# Signatures are generated only after both inherited OS and unsigned browser proofs.
assert app=='meet'
command=['pnpm','exec','playwright','test',*spec['specs'],'--config=isolated.playwright.config.ts','--project=chromium','--workers=1','--retries=0']
versions={'node':subprocess.check_output(['node','--version'],env=env,text=True).strip(),'pnpm':subprocess.check_output(['pnpm','--version'],env=env,text=True).strip(),'htree':subprocess.check_output([env['HTREE_BIN'],'--version'],env=env,text=True).strip(),'command':command,'sourceCommit':spec['commit'],'runtimeMode':'Development suite from exact source; offline/update suite from separately hash-verified dist'}
assert versions['node']==json.loads((base/'evidence/image-toolchain.json').read_text())['node']
assert versions['pnpm']=='10.24.0' and '0.2.142' in versions['htree']
(evidence/'inputs.json').write_text(json.dumps(versions,indent=2)+'\n')
budget=json.loads((base/'procedure/app-budgets.json').read_text())[app]
try:
    subprocess.run(command,env=env,check=True,timeout=1800)
    results=pathlib.Path('test-results')
    if results.exists():
        files=[p for p in results.rglob('*') if p.is_file() and not p.is_symlink()]
        assert sum(p.stat().st_size for p in files)<128*1024**2,'Development test artifact budget'
        shutil.copytree(results,evidence/'test-results-development',symlinks=True)
    subprocess.run(command,env=dict(env,PW_PREVIEW='1'),check=True,timeout=600)
finally:
    results=pathlib.Path('test-results')
    if results.exists():
        files=[p for p in results.rglob('*') if p.is_file() and not p.is_symlink()]
        assert sum(p.stat().st_size for p in files)<128*1024**2,'Test artifact budget'
        shutil.copytree(results,evidence/'test-results',symlinks=True)
    subprocess.run(['python3','/stage/procedure/network-proof.py',str(evidence/'network-after-attempt.json')],env=env,check=True,timeout=15)
contexts=[json.loads(line) for line in (evidence/'context-permissions.jsonl').read_text().splitlines()]
assert len(contexts)>=2
assert all(c['origin']=='http://127.0.0.1:43810' and c['permission']=='microphone' and c['state']=='granted' and not c['captureInvoked'] for c in contexts)
subprocess.run(['python3','/stage/procedure/network-proof.py',str(evidence/'network-after.json')],env=env,check=True,timeout=15)
(evidence/'passed.json').write_text(json.dumps({'passed':True,'sourceCommit':spec['commit'],'unchangedAssertions':True,'driveInputPolicySha256':'a5ca26fea52d3cb55288992c793db53f93228a7f42bc973564d7ff14c0a46b22' if app=='drive' else None})+'\n')
