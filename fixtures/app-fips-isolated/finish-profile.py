"""Finish the declared Social release checks inside the existing isolated runner."""
import hashlib,json,os,pathlib,signal,subprocess,tarfile,time
root=pathlib.Path.cwd();procedure=pathlib.Path('/stage/procedure');evidence=pathlib.Path(os.environ['FIPS_APP_EVIDENCE'])
env=dict(os.environ);sha=lambda p:hashlib.sha256(p.read_bytes()).hexdigest()
def run(args,name,seconds):
 with (evidence/(name+'.log')).open('wb') as log:
  subprocess.run(args,env=env,stdout=log,stderr=subprocess.STDOUT,timeout=seconds,check=True)
run(['pnpm','run','check'],'full-check',180)
run(['pnpm','exec','playwright','test','--config=playwright.unit.config.ts','--workers=1','--reporter=json'],'full-unit',300)
unit=json.loads((evidence/'full-unit.log').read_text())
assert unit['stats']['unexpected']==unit['stats']['flaky']==unit['stats']['skipped']==0
config=root/'playwright.graph-qual.config.ts'
env['GRAPH_REPORT']=str(evidence/'full-graph.json')
run(['pnpm','exec','playwright','test','--config',config.name,'tests/public-graph-bootstrap.spec.ts','tests/social-worker-profile-snapshot-browser.spec.ts'],'full-graph',300)
graph=json.loads((evidence/'full-graph.json').read_text())
assert graph['stats']['unexpected']==graph['stats']['flaky']==graph['stats']['skipped']==0
manifest=json.loads((procedure/'public-read-manifest.json').read_text())
fixture=procedure/'public-read'
expected=manifest['files'];archive=procedure/'public-read-fixture.tar.gz'
assert sha(archive)==manifest['sha256']
with tarfile.open(archive) as tar:
 members=tar.getmembers()
 assert set(m.name for m in members)==set(expected)
 for m in members:
  assert m.isfile() and not m.name.startswith('/') and '..' not in pathlib.PurePosixPath(m.name).parts
  raw=tar.extractfile(m).read()
  assert len(raw)==expected[m.name]['bytes'] and hashlib.sha256(raw).hexdigest()==expected[m.name]['sha256']
 tar.extractall(fixture,filter='data')
assert 'await fetch(' not in (fixture/'bridge.mjs').read_text()
env['IRIS_PUBLIC_READ_REPLAY']='1'
bridge=None
try:
 with (evidence/'public-read.log').open('wb') as log:
  bridge=subprocess.Popen(['node',str(fixture/'bridge.mjs')],env=env,stdout=log,stderr=subprocess.STDOUT)
  ready=fixture/'bridge-ready.json'
  for _ in range(100):
   if ready.exists():
    value=json.loads(ready.read_text());assert value['pid']==bridge.pid;break
   assert bridge.poll() is None
   time.sleep(.1)
  else:raise RuntimeError('Static relay readiness deadline')
  env['IRIS_PUBLIC_READ_BRIDGE']=value['origin']
  env['GRAPH_REPORT']=str(evidence/'full-smoke.json')
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
  smoke_report=json.loads((evidence/'full-smoke.json').read_text())
  assert smoke_report['stats']['expected']==49
  assert smoke_report['stats']['unexpected']==smoke_report['stats']['flaky']==smoke_report['stats']['skipped']==0
finally:
 if bridge:
  bridge.terminate()
  try:bridge.wait(10)
  except subprocess.TimeoutExpired:bridge.kill();bridge.wait()
  for p in fixture.glob('receipt-*.json'):
   (evidence/('public-read-'+p.name)).write_bytes(p.read_bytes())
(evidence/'full-profile.json').write_text(json.dumps({'passed':True,'unit':unit['stats'],'graph':graph['stats'],'smoke':smoke_report['stats'],'fixtureSha256':manifest['sha256']},indent=2)+'\n')
