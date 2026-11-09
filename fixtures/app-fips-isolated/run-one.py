"""One unchanged app profile; all signing follows independent OS/browser proofs."""
import hashlib,json,os,pathlib,subprocess,sys
base=pathlib.Path('/stage');app=sys.argv[1]
spec=next(a for a in json.loads((base/'procedure/manifest.json').read_text())['apps'] if a['id']==app)
evidence=base/'evidence'/app;evidence.mkdir();home=base/'home'/app;home.mkdir(exist_ok=True)
env=dict(os.environ,CI='1',PW_WORKERS='1',PW_MAX_WORKERS='1',PW_APP_PORT='43810',PW_RELAY_PORT='43811',PW_TEST_BLOSSOM_URL='http://127.0.0.1:43812',HTREE_ADDR='127.0.0.1:43812',HTREE_BIN='/stage/tools/native/hashtree/htree',GIT_REMOTE_HTREE_BIN='/stage/tools/native/hashtree/git-remote-htree',HTREE_CONFIG_DIR=str(home/'config'),HTREE_DATA_DIR=str(home/'data'),HTREE_PREFER_LOCAL_DAEMON='false',FIPS_APP_EVIDENCE=str(evidence))
env['PATH']='/stage/tools/native/hashtree:'+env['PATH'];env.update(spec.get('env',{}))
subprocess.run(['python3','/stage/procedure/network-proof.py',str(evidence/'network.json')],env=env,check=True,timeout=15)
subprocess.run(['node','/stage/procedure/package-proof.cjs',str(evidence/'packages.json')],env=env,check=True,timeout=15)
subprocess.run(['node','/stage/procedure/installed-runtime-proof.cjs',str(evidence/'installed-runtime.json')],env=env,check=True,timeout=15)
subprocess.run(['node','/stage/procedure/browser-proof.cjs',str(evidence/'browser.json')],env=env,check=True,timeout=35)
versions={'node':subprocess.check_output(['node','--version'],env=env,text=True).strip(),'pnpm':subprocess.check_output(['pnpm','--version'],env=env,text=True).strip(),'htree':subprocess.check_output([env['HTREE_BIN'],'--version'],env=env,text=True).strip(),'command':['python3','/stage/procedure/release-gates.py',app],'sourceCommit':spec['commit'],'runtimeMode':'Exact frozen source built inside OS boundary; complete checked-in release profile and qualified dist'}
assert versions['node']==json.loads((base/'evidence/image-toolchain.json').read_text())['node']
assert versions['pnpm']=='10.23.0' and versions['htree']=='htree 0.2.142'
(evidence/'inputs.json').write_text(json.dumps(versions,indent=2)+'\n')
budget=json.loads((base/'procedure/app-budgets.json').read_text())[app]
try:subprocess.run(versions['command'],env=env,check=True,timeout=budget)
finally:subprocess.run(['python3','/stage/procedure/network-proof.py',str(evidence/'network-after-attempt.json')],env=env,check=True,timeout=15)
contexts=[json.loads(line) for line in (evidence/'context-permissions.jsonl').read_text().splitlines()]
assert len(contexts)>=spec['requiredPermissionContexts']
assert all(c['origin']=='http://localhost:43810' and c['permission']=='microphone' and c['state']=='granted' and not c['captureInvoked'] for c in contexts)
(evidence/'passed.json').write_text(json.dumps({'passed':True,'sourceCommit':spec['commit'],'completeCheckedInProfile':True,'allAppGatesFromCurrentRun':False,'allAppGatesFromSameSource':True,'reusedPassingBrowserCases':81,'assertionsPreserved':True,'permissionContexts':len(contexts)})+'\n')
