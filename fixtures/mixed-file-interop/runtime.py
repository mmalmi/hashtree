"""Run only mixed file transfers in a nonroot, capability-free, isolated namespace."""
from pathlib import Path
import hashlib,json,os,subprocess,sys
E=Path('/stage/evidence');sha=lambda p:hashlib.sha256(p.read_bytes()).hexdigest()
def network(name):
 script=Path('/stage/procedure/network-proof.py')
 assert sha(script)=='ad53bbfab4f7089b6981ed0e30b7544437bff6ba94dd04245d74b2acc2f7612f'
 subprocess.run([sys.executable,str(script),str(E/name)],env=dict(os.environ,FIPS_DUMMY='0'),check=True,timeout=20)
network('network-before.json')
inputs=E/'run-inputs.json';proof=json.loads((E/'installed-inputs.json').read_text());assert proof['passed'] and sha(inputs)==proof['runInputSha256']
env=dict(os.environ,FIPS_FILE_INTEROP_INPUTS=str(inputs),FIPS_FILE_INTEROP_INPUTS_SHA256=sha(inputs),NODE_OPTIONS='')
code=None
try:code=subprocess.run(['/stage/tools/bin/node','/stage/fixture/node/run.mjs'],env=env,timeout=570).returncode
finally:
 network('network-after.json')
 (E/'runtime-exit.json').write_text(json.dumps({'exitCode':code,'inputSha256':sha(inputs)})+'\n')
assert code==0
receipt=json.loads((E/'interop/interop-receipt.json').read_text());assert receipt['passed'] and len(receipt['scenarios'])==4
assert [r['name'] for r in receipt['scenarios']]==['rust-to-ts-complete','rust-to-ts-restart','ts-to-rust-complete','ts-to-rust-restart']
for row in receipt['scenarios']:
 assert row['bothTerminalCompleted'] and row['identitiesStable'] and row['completedStateSurvivesRestart'] and row['outputUnchangedAfterRestart']
 assert (row['durablePrefixBytes']>0)==row['resumed'] and len(row['files'])==3
 if row['resumed']:
  proof=row['resumeProof'];sender='rust' if row['name'].startswith('rust-to-ts') else 'ts'
  assert proof['sender']==sender and proof['accepted'] and proof['acceptedOffset']==proof['receiverSavedOffset']==row['durablePrefixBytes']
  assert proof['senderEvent']['event']=='accepted' and proof['senderEvent']['id']==row['name']
  if sender=='rust':assert proof['method']=='sender-event-and-authenticated-ready' and proof['readyEvent']['event']=='sender-ready' and proof['readyEvent']['id']==row['name'] and proof['readyEvent']['bytes']==row['durablePrefixBytes']
  else:assert proof['method']=='sender-event-offset' and proof['senderEvent']['bytes']==row['durablePrefixBytes']
 assert row['network']['ingressBytes']>0 and row['network']['ingressBytes']<=row['network']['ingressLimitBytes']==128*1024**2
 assert row['network']['egressBytes']>0 and row['network']['maxConcurrent']<=8
print(json.dumps({'passed':True,'scenarios':4,'freshDirections':2,'interruptedRestartDirections':2}))
