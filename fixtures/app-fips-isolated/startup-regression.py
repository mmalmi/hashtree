import hashlib,json,os,pathlib,subprocess
root=pathlib.Path.cwd();procedure=pathlib.Path('/stage/procedure');evidence=pathlib.Path('/stage/evidence/social')
source=root/'src/lib/privateContactSync.ts';current=source.read_bytes();before=(procedure/'private-startup-before.ts').read_bytes()
sha=lambda raw:hashlib.sha256(raw).hexdigest()
command=['pnpm','exec','playwright','test','--config=playwright.unit.config.ts','tests/private-contact-sync-startup.spec.ts',
 '--workers=1','--reporter=json']
def run(name,extra):
 with (evidence/(name+'.log')).open('wb')as log:
  result=subprocess.run(command+extra+['--output='+str(evidence/name)],env=os.environ,stdout=log,stderr=subprocess.STDOUT,timeout=40)
 report=json.loads((evidence/(name+'.log')).read_text());return result.returncode,report
try:
 source.write_bytes(before)
 red_code,red=run('startup-red',['--grep','startup and an online wake share one pending private ratchet writer'])
finally:source.write_bytes(current)
assert source.read_bytes()==current
assert red_code!=0 and red['stats']['unexpected']==1 and red['stats']['expected']==red['stats']['skipped']==red['stats']['flaky']==0
green_code,green=run('startup-green',[])
assert green_code==0 and green['stats']['expected']==3 and green['stats']['unexpected']==green['stats']['skipped']==green['stats']['flaky']==0
(evidence/'startup-regression.json').write_text(json.dumps(dict(source=json.loads((procedure/'manifest.json').read_text())['apps'][0]['commit'],
 beforeSourceSha256=sha(before),afterSourceSha256=sha(current),restored=True,
 testSha256=sha((root/'tests/private-contact-sync-startup.spec.ts').read_bytes()),
 red=dict(exit=red_code,stats=red['stats']),green=dict(exit=green_code,stats=green['stats'])))+'\n')
