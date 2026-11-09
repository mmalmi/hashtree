import hashlib,json,os,pathlib,subprocess
root=pathlib.Path.cwd();procedure=pathlib.Path('/stage/procedure');evidence=pathlib.Path('/stage/evidence/social')
reader=root/'src/lib/recentAuthorFeedReader.ts';current=reader.read_bytes();before=(procedure/'reader-before-batching.ts').read_bytes()
sha=lambda raw:hashlib.sha256(raw).hexdigest()
command=['pnpm','exec','playwright','test','--config=playwright.unit.config.ts','tests/recent-author-feed-reader.spec.ts',
 '--grep','paints a second verified post before the next Home batch','--workers=1','--reporter=json']
def run(name):
 with (evidence/(name+'.log')).open('wb') as log:
  result=subprocess.run(command+['--output='+str(evidence/name)],env=os.environ,stdout=log,stderr=subprocess.STDOUT,timeout=40)
 report=json.loads((evidence/(name+'.log')).read_text());return result.returncode,report
try:
 reader.write_bytes(before)
 red_code,red=run('batching-red')
finally:reader.write_bytes(current)
assert reader.read_bytes()==current
assert red_code!=0 and red['stats']['unexpected']==2 and red['stats']['expected']==red['stats']['skipped']==red['stats']['flaky']==0
green_code,green=run('batching-green')
assert green_code==0 and green['stats']['expected']==2 and green['stats']['unexpected']==green['stats']['skipped']==green['stats']['flaky']==0
(evidence/'batching-regression.json').write_text(json.dumps(dict(source=json.loads((procedure/'manifest.json').read_text())['apps'][0]['commit'],
 beforeReaderSha256=sha(before),afterReaderSha256=sha(current),restored=True,
 testSha256=sha((root/'tests/recent-author-feed-reader.spec.ts').read_bytes()),
 red=dict(exit=red_code,stats=red['stats']),green=dict(exit=green_code,stats=green['stats'])))+'\n')
