"""Qualify unexecuted gates, reusing exact-source checks and unit results."""
import hashlib,json,os,pathlib,subprocess,tarfile,time
root=pathlib.Path.cwd();procedure=pathlib.Path('/stage/procedure');evidence=pathlib.Path('/stage/evidence/social')
env=dict(os.environ);sha=lambda p:hashlib.sha256(p.read_bytes()).hexdigest();codes={}
spec=json.loads((procedure/'manifest.json').read_text())['apps'][0]
def run(args,name,seconds):
 with (evidence/(name+'.log')).open('wb') as log:
  result=subprocess.run(args,env=env,stdout=log,stderr=subprocess.STDOUT,timeout=seconds)
 codes[name]=result.returncode
def report(name):return json.loads((evidence/(name+'.json')).read_text())['stats']
run(['pnpm','run','check'],'full-check',180)
temporary=evidence/'native-fixture-tmp';temporary.mkdir();env['TMPDIR']=str(temporary)
run(['pnpm','exec','playwright','test','--config=playwright.unit.config.ts','--workers=1','--reporter=json','--output=/stage/evidence/social/full-unit'],'full-unit',300)
unit=json.loads((evidence/'full-unit.log').read_text());env.pop('TMPDIR')
def specs(node):
 yield from node.get('specs',[])
 for child in node.get('suites',[]):yield from specs(child)
records=[(s,t) for s in specs(unit) for t in s['tests']]
assert len(records)==1345 and unit['stats']['expected']==1344 and unit['stats']['skipped']==1
skipped=[(s,t) for s,t in records if t['expectedStatus']=='skipped']
assert len(skipped)==1 and skipped[0][0]['file']=='private-ratchet-controls.spec.ts'
assert skipped[0][0]['title']=='dispatches actual native-produced current and legacy sibling controls'
skipReason='Native core fixture is required for the dedicated interoperability gate.'
assert any(a['type']=='skip' and a.get('description')==skipReason for a in skipped[0][1]['annotations'])
for spec_record,test_record in records:
 assert len(test_record['results'])==1 and test_record['results'][0]['retry']==0
 result=test_record['results'][0]
 if test_record['expectedStatus']=='skipped':
  assert test_record['status']==result['status']=='skipped'
 else:
  assert spec_record['ok'] and test_record['expectedStatus']=='passed' and test_record['status']=='expected'
  assert result['status']=='passed' and not result.get('errors')
config=root/'playwright.graph-qual.config.ts'
for name,files,seconds in [
 ('full-graph',['tests/public-graph-bootstrap.spec.ts','tests/social-worker-profile-snapshot-browser.spec.ts'],300),
 ('full-mobile',['tests/mobile-navigation.spec.ts'],240),
]:
 env['GRAPH_REPORT']=str(evidence/(name+'.json'));env['GRAPH_OUTPUT']=str(evidence/name)
 run(['pnpm','exec','playwright','test','--config',config.name,*files],name,seconds)
manifest=json.loads((procedure/'public-read-manifest.json').read_text());fixture=procedure/'public-read'
archive=procedure/'public-read-fixture.tar.gz';assert sha(archive)==manifest['sha256']
with tarfile.open(archive) as tar:
 members=tar.getmembers();assert set(m.name for m in members)==set(manifest['files'])
 for m in members:
  assert m.isfile() and not m.name.startswith('/') and '..' not in pathlib.PurePosixPath(m.name).parts
  raw=tar.extractfile(m).read();expected=manifest['files'][m.name]
  assert len(raw)==expected['bytes'] and hashlib.sha256(raw).hexdigest()==expected['sha256']
 tar.extractall(fixture,filter='data')
assert 'await fetch(' not in (fixture/'bridge.mjs').read_text()
bridge=None
try:
 with (evidence/'public-read.log').open('wb') as log:
  bridge=subprocess.Popen(['node',str(fixture/'bridge.mjs')],env=env,stdout=log,stderr=subprocess.STDOUT)
  ready=fixture/'bridge-ready.json'
  for _ in range(100):
   if ready.exists():
    value=json.loads(ready.read_text());assert value['pid']==bridge.pid;break
   assert bridge.poll() is None;time.sleep(.1)
  else:raise RuntimeError('Static relay readiness deadline')
  env['IRIS_PUBLIC_READ_BRIDGE']=value['origin'];env['GRAPH_REPORT']=str(evidence/'full-smoke.json')
  env['GRAPH_OUTPUT']=str(evidence/'full-smoke')
  smoke=root/'playwright.full-smoke.config.ts'
  smoke.write_text("""import {defineConfig} from '@playwright/test';
import original from './playwright.graph-qual.config';
const baseURL='http://127.0.0.1:41967';
const project=original.projects!.find(p=>p.name==='chromium')!;
export default defineConfig({...original,use:{...original.use,baseURL},
 projects:[{...project,use:{...project.use,baseURL}}],
 webServer:{command:'node /stage/procedure/public-read/preview.mjs 41967',url:baseURL,reuseExistingServer:false,timeout:30000}});
""")
  run(['pnpm','exec','playwright','test','tests/smoke.spec.ts','--config',smoke.name],'full-smoke',600)
finally:
 if bridge:
  bridge.terminate()
  try:bridge.wait(10)
  except subprocess.TimeoutExpired:bridge.kill();bridge.wait()
  for p in fixture.glob('receipt-*.json'):(evidence/('public-read-'+p.name)).write_bytes(p.read_bytes())
stats={name:report(name) for name in ['full-graph','full-mobile','full-smoke']}
expected={'full-graph':6,'full-mobile':6,'full-smoke':49}
passed=all(code==0 for code in codes.values()) and unit['stats']['unexpected']==unit['stats']['flaky']==0 and unit['stats']['skipped']==1 and not unit['errors'] and all(s['expected']==expected[name] and s['unexpected']==s['flaky']==s['skipped']==0 for name,s in stats.items())
(evidence/'full-profile.json').write_text(json.dumps({'passed':passed,'source':spec['commit'],'codes':codes,'unit':unit['stats'],'declaredNativeInteropSkip':{'file':skipped[0][0]['file'],'title':skipped[0][0]['title'],'reason':skipReason},'stats':stats,'fixtureSha256':manifest['sha256']},indent=2)+'\n')
assert passed,'One or more remaining gates failed; inspect full-profile.json'
