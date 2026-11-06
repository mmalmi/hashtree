import hashlib,json,os,pathlib,subprocess
B=pathlib.Path('/stage');M=json.loads((B/'procedure/manifest.json').read_text());S=M['apps'][0];E=B/'evidence/client';E.mkdir();env=dict(os.environ,FIPS_APP_EVIDENCE=str(E),CI='1',IRIS_E2E_LOCAL_RELAY='true',IRIS_E2E_PORT='5173')
subprocess.run(['python3','/stage/procedure/network-proof.py',str(E/'network.json')],env=env,check=True,timeout=15)
subprocess.run(['node','/stage/procedure/browser-proof.cjs',str(E/'browser.json')],env=env,check=True,timeout=35)
image=json.loads((B/'evidence/image-toolchain.json').read_text());assert image['node']==M['node'] and image['nodeSha256']==M['nodeSha256']
assert subprocess.check_output(['pnpm','--version'],env=env,text=True).strip()=='9.15.0'
try:subprocess.run(['python3','/stage/procedure/release-gates.py'],env=env,check=True,timeout=3540)
finally:subprocess.run(['python3','/stage/procedure/network-proof.py',str(E/'network-after.json')],env=env,check=True,timeout=15)
(E/'passed.json').write_text(json.dumps({'passed':True,'sourceCommit':S['commit'],'completeOriginalReleaseProfile':True,'bothOriginalSourceChecks':True,'productionPublished':False})+'\n')
