"""Qualify unexecuted gates, reusing exact-source checks and unit results."""
import hashlib,json,os,pathlib,subprocess,tarfile,time
root=pathlib.Path.cwd();procedure=pathlib.Path('/stage/procedure');evidence=pathlib.Path('/stage/evidence/social')
env=dict(os.environ);sha=lambda p:hashlib.sha256(p.read_bytes()).hexdigest();codes={}
codes={}
def run(args,name,seconds):
 with (evidence/(name+'.log')).open('wb') as log:
  result=subprocess.run(args,env=env,stdout=log,stderr=subprocess.STDOUT,timeout=seconds)
 codes[name]=result.returncode
reader=root/'src/lib/recentAuthorFeedReader.ts';current=reader.read_bytes()
command=['pnpm','exec','playwright','test','--config=playwright.unit.config.ts','--workers=1',
 'tests/recent-author-feed-reader.spec.ts','--reporter=json']
try:
 reader.write_bytes((procedure/'reader-before.ts').read_bytes())
 run(command+['--grep','paints a bounded followed-author window before sparse kind cursors settle'],'reader-red',40)
finally:reader.write_bytes(current)
red=json.loads((evidence/'reader-red.log').read_text())['stats']
assert codes['reader-red']!=0 and red['unexpected']==1 and red['expected']==red['skipped']==red['flaky']==0
run(command,'reader-green',60)
green=json.loads((evidence/'reader-green.log').read_text())['stats']
assert codes['reader-green']==0 and green['expected']==8 and green['unexpected']==green['skipped']==green['flaky']==0
run(['pnpm','run','check'],'check',180)
assert codes['check']==0
config=root/'playwright.graph-qual.config.ts'
case=root/'tests/smoke/home.ts';before=case.read_bytes();needle='  expect(Date.now() - startedAt).toBeLessThan(6_000);'
assert before.decode().count(needle)==1
case.write_text(before.decode().replace(needle,"  console.log('viewer-followed-feed-ready-ms', Date.now() - startedAt);\n"+needle))
(evidence/'latency-observer.json').write_text(json.dumps(dict(before=sha(pathlib.Path('/stage/procedure/reader-before.ts')),
 readerRestored=sha(reader),testBefore=hashlib.sha256(before).hexdigest(),testAfter=sha(case),assertion=needle.strip()))+'\n')
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
  run(['pnpm','exec','playwright','test','tests/smoke.spec.ts','--config',smoke.name,'--grep','viewer-rooted Latest replaces the viewer-only fallback and keeps followed authors after profile navigation'],'full-smoke',600)
finally:
 if bridge:
  bridge.terminate()
  try:bridge.wait(10)
  except subprocess.TimeoutExpired:bridge.kill();bridge.wait()
  for p in fixture.glob('receipt-*.json'):(evidence/('public-read-'+p.name)).write_bytes(p.read_bytes())
smoke=json.loads((evidence/'full-smoke.json').read_text())['stats']
passed=codes['full-smoke']==0 and smoke['expected']==1 and smoke['unexpected']==smoke['skipped']==smoke['flaky']==0
(evidence/'home-preview.json').write_text(json.dumps(dict(passed=passed,red=red,green=green,smoke=smoke,codes=codes),indent=2)+'\n')
assert passed,'Cold feed latency gate failed'
