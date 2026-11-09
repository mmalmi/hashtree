"""Run only mixed file transfers in a nonroot, capability-free, isolated namespace."""
from pathlib import Path
import hashlib,json,os,socket,subprocess,sys
E=Path('/stage/evidence');sha=lambda p:hashlib.sha256(p.read_bytes()).hexdigest()
def network():
 interfaces=sorted(line.split(':')[0].strip() for line in Path('/proc/net/dev').read_text().splitlines()[2:]);assert interfaces==['lo']
 status=dict(line.split(':',1) for line in Path('/proc/self/status').read_text().splitlines() if ':' in line)
 assert os.getuid()!=0 and int(status['CapEff'].strip(),16)==int(status['CapBnd'].strip(),16)==0
 addresses=['1.1.1.1','8.8.8.8','198.51.100.1','203.0.113.1'];denied=[]
 for address in addresses:
  with socket.socket() as sock:sock.settimeout(1);code=sock.connect_ex((address,443));assert code==101;denied.append({'address':address,'errno':code})
 child=json.loads(subprocess.check_output([sys.executable,'-c','import json,socket; rows=[]\nfor address in '+repr(addresses)+':\n s=socket.socket();s.settimeout(1);rows.append(s.connect_ex((address,443)));s.close()\nprint(json.dumps(rows))'],text=True));assert child==[101]*4
 return {'interfaces':interfaces,'uid':os.getuid(),'capabilities':0,'externalDenied':denied,'childDenied':child}
(E/'network-before.json').write_text(json.dumps(network(),indent=2)+'\n')
inputs=E/'run-inputs.json';proof=json.loads((E/'installed-inputs.json').read_text());assert proof['passed'] and sha(inputs)==proof['runInputSha256']
env=dict(os.environ,FIPS_FILE_INTEROP_INPUTS=str(inputs),FIPS_FILE_INTEROP_INPUTS_SHA256=sha(inputs),NODE_OPTIONS='')
code=None
try:code=subprocess.run(['/stage/tools/bin/node','/stage/fixture/node/run.mjs'],env=env,timeout=570).returncode
finally:
 (E/'network-after.json').write_text(json.dumps(network(),indent=2)+'\n')
 (E/'runtime-exit.json').write_text(json.dumps({'exitCode':code,'inputSha256':sha(inputs)})+'\n')
assert code==0
receipt=json.loads((E/'interop/interop-receipt.json').read_text());assert receipt['passed'] and len(receipt['scenarios'])==4
assert [r['name'] for r in receipt['scenarios']]==['rust-to-ts-complete','rust-to-ts-restart','ts-to-rust-complete','ts-to-rust-restart']
for row in receipt['scenarios']:
 assert row['bothTerminalCompleted'] and row['identitiesStable'] and row['completedStateSurvivesRestart'] and row['outputUnchangedAfterRestart']
 assert (row['durablePrefixBytes']>0)==row['resumed'] and len(row['files'])==3
 assert row['network']['ingressBytes']>0 and row['network']['ingressBytes']<=row['network']['ingressLimitBytes']==128*1024**2
 assert row['network']['egressBytes']>0 and row['network']['maxConcurrent']<=8
print(json.dumps({'passed':True,'scenarios':4,'freshDirections':2,'interruptedRestartDirections':2}))
