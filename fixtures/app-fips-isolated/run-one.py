import json,os,pathlib,re,subprocess,sys
base=pathlib.Path('/stage');app=sys.argv[1]
spec=next(a for a in json.loads((base/'procedure/manifest.json').read_text())['apps'] if a['id']==app)
evidence=base/'evidence'/app;evidence.mkdir();home=base/'home'/app;home.mkdir(exist_ok=True)
env=dict(os.environ,CI='1',PW_WORKERS='1',PW_MAX_WORKERS='1',HTREE_BIN='/stage/tools/native/hashtree/htree',GIT_REMOTE_HTREE_BIN='/stage/tools/native/hashtree/git-remote-htree',HTREE_CONFIG_DIR=str(home/'config'),HTREE_DATA_DIR=str(home/'data'),HTREE_PREFER_LOCAL_DAEMON='false',FIPS_APP_EVIDENCE=str(evidence),FIPS_PERMISSION_ORIGIN=spec['runtimeOrigin'])
env['PATH']='/stage/tools/native/hashtree:'+env['PATH']
for name,command,seconds in [('network',['python3','/stage/procedure/network-proof.py'],15),('packages',['node','/stage/procedure/package-proof.cjs'],15),('browser',['node','/stage/procedure/browser-proof.cjs'],35)]:
    subprocess.run(command+[str(evidence/(name+'.json'))],env=env,check=True,timeout=seconds)
versions={'node':subprocess.check_output(['node','--version'],env=env,text=True).strip(),'pnpm':subprocess.check_output(['pnpm','--version'],env=env,text=True).strip(),'htree':subprocess.check_output([env['HTREE_BIN'],'--version'],env=env,text=True).strip(),'sourceCommit':spec['commit']}
assert versions['node']==json.loads((base/'evidence/image-toolchain.json').read_text())['node']
assert versions['pnpm']=='10.23.0' and versions['htree']=='htree 0.2.142'
(evidence/'inputs.json').write_text(json.dumps(versions,indent=2)+'\n')
env['NODE_OPTIONS']='--import=/stage/procedure/deny-browser-preload.mts'
budget=json.loads((base/'procedure/app-budgets.json').read_text())[app]
try:subprocess.run(['python3','/stage/procedure/release-gates.py',app],env=env,check=True,timeout=budget)
finally:subprocess.run(['python3','/stage/procedure/network-proof.py',str(evidence/'network-after-attempt.json')],env=env,check=True,timeout=15)
contexts=[json.loads(line) for line in (evidence/'context-permissions.jsonl').read_text().splitlines()]
assert contexts and all(c['origin']==spec['runtimeOrigin'] and c['permission']=='microphone' and c['state']=='granted' and not c['captureInvoked'] for c in contexts)
reports=[]
for p in evidence.glob('*playwright.json'):
    x=json.loads(p.read_bytes());s=x['stats'];assert s['unexpected']==s['flaky']==s['skipped']==0
    def visit(v):
        for case in v.get('specs',[]):
            for test in case.get('tests',[]):
                assert test['status']=='expected' and test['expectedStatus']=='passed'
                assert len(test['results'])==1 and test['results'][0]['retry']==0 and test['results'][0]['status']=='passed'
        for child in v.get('suites',[]):visit(child)
    visit(x);reports.append({'file':p.name,'cases':s['expected'],'skips':0,'retries':0})
assert reports
runtime=re.sub(r'\x1b\[[0-?]*[ -/]*[@-~]','',(evidence/'peer-recovery.log').read_text())
proofs=[json.loads(line.strip().split('BROWSER_PEER_RELOAD ',1)[1]) for line in runtime.splitlines() if line.strip().startswith('BROWSER_PEER_RELOAD ')]
assert len(proofs)==2 and {p['endpoint'] for p in proofs}=={'first','second'}
assert all(p['identityStable'] and p['bothPeersConnected'] and p['freshTransfer'] for p in proofs)
(evidence/'passed.json').write_text(json.dumps({'passed':True,'sourceCommit':spec['commit'],'completeCheckedInProfile':True,'assertionsPreserved':True,'permissionContexts':len(contexts),'browserReports':reports,'peerReloads':proofs},indent=2)+'\n')
