"""Bind retained RED to matching locked inputs, then run exact GREEN normal gates."""
from pathlib import Path
import collections,hashlib,json,os,shutil,socket,subprocess,tarfile,time
M=json.loads(Path('/stage/procedure/manifest.json').read_text());E=Path('/stage/evidence');ROOT=Path('/stage/workspace');WORK=ROOT/'ts';START=time.monotonic()
sha=lambda p:hashlib.sha256(Path(p).read_bytes()).hexdigest()
steps=[]
def write(name,value):(E/name).write_text(json.dumps(value,indent=2)+'\n')
def source(label):
 item=M['sources'][label]
 for name,row in item['files'].items():
  p=ROOT/name;assert p.is_file() and sha(p)==row['sha256'],f'{label} changed source: {name}'
  assert ('100755' if p.stat().st_mode&0o111 else '100644')==row['mode'],f'{label} mode: {name}'
 return {'commit':item['commit'],'archiveSha256':item['archiveSha256'],'files':len(item['files']),'lockSha256':sha(WORK/'pnpm-lock.yaml')}
def run(args,label,seconds):
 assert time.monotonic()-START<1400 and shutil.disk_usage(ROOT).free>M['resources']['reserveBytes']
 log=E/(label+'.log');assert not log.exists();started=time.monotonic()
 with log.open('wb') as f:result=subprocess.run(args,cwd=WORK,stdout=f,stderr=subprocess.STDOUT,timeout=seconds)
 row={'name':label,'args':args,'exitCode':result.returncode,'elapsedSeconds':time.monotonic()-started,'logSha256':sha(log)};steps.append(row);write('steps.json',steps);return result.returncode
def network_proof(label):
 # NetworkMode=none must still provide TCP loopback for the real relay fixture.
 server=socket.socket();server.bind(('127.0.0.1',0));server.listen(1);port=server.getsockname()[1]
 client=socket.create_connection(('127.0.0.1',port),timeout=1);remote,_=server.accept();client.sendall(b'control');assert remote.recv(7)==b'control';client.close();remote.close();server.close()
 outcomes=[]
 for family,target in [(socket.AF_INET,('192.0.2.1',443)),(socket.AF_INET6,('2001:db8::1',443))]:
  s=socket.socket(family,socket.SOCK_STREAM);s.settimeout(.5)
  try:
   s.connect(target);raise AssertionError('External namespace TCP unexpectedly reachable')
  except OSError as err:outcomes.append({'family':'ipv4' if family==socket.AF_INET else 'ipv6','tcpDenied':True,'errno':err.errno})
  finally:s.close()
 write('network-'+label+'.json',{'loopbackTcpPassed':True,'externalTcpDenied':outcomes})
def assertions(report):return [a for f in report['testResults'] for a in f['assertionResults']]
def counts(rows):return dict(collections.Counter(a['status'] for a in rows))
def report(path):
 r=json.loads(path.read_text());assert not r.get('testExecError') and not r.get('unhandledErrors');return r
installed={}
for name,version in M['installedVersions'].items():
 p=WORK/'packages/nostr-pubsub/node_modules'/name/'package.json';j=json.loads(p.read_text())
 assert j['name']==name and j['version']==version,{'package':name,'actual':j.get('version'),'expected':version}
 installed[name]={'version':version,'manifestSha256':sha(p)}
write('installed-inputs.json',{'packages':installed,'sourceLockSha256':sha(WORK/'pnpm-lock.yaml'),'installedLockSha256':sha(WORK/'node_modules/.pnpm/lock.yaml'),'osRelease':Path('/etc/os-release').read_text()})
network_proof('before');redProof=source('red')
assert 'NOSTR_PUBSUB_LEGACY_MODULE' not in os.environ
assert sha('/stage/procedure/red-reuse.json')==M['redReuse']['provenanceSha256']
prior=json.loads(Path('/stage/procedure/red-reuse.json').read_text())
assert prior['run']==M['redReuse']['run'] and prior['source']==M['sources']['red']['commit'] and prior['archiveSha256']==M['sources']['red']['archiveSha256']
assert prior['testSha256']==M['sources']['red']['files']['ts/packages/nostr-pubsub/tests/private-relay-publisher.test.ts']['sha256']
assert prior['installed']['packages']==installed
assert prior['installed']['sourceLockSha256']==sha(WORK/'pnpm-lock.yaml')
assert prior['installed']['installedLockSha256']==sha(WORK/'node_modules/.pnpm/lock.yaml')
assert prior['toolchain']==json.loads((E/'toolchain.json').read_text())
assert sha('/stage/procedure/red-tests-prior.json')==M['redReuse']['reportSha256']==prior['reportSha256']
shutil.copyfile('/stage/procedure/red-tests-prior.json',E/'red-tests.json')
shutil.copyfile('/stage/procedure/red-reuse.json',E/'red-reuse.json')
r=report(E/'red-tests.json');rows=assertions(r);selected=[a for a in rows if a['status']=='failed']
expected=set(M['regressions']['fullNames']);assert len(expected)==7 and expected==set(prior['expectedFullNames'])
assert not r['success'];assert len(selected)==7 and {a['fullName'] for a in selected}==expected
assert len(rows)==19 and sum(a['status']=='skipped' for a in rows)==r['numPendingTests']==12
assert all('not to be' in '\n'.join(a.get('failureMessages',[])) for a in selected),'RED failed for an unexpected reason'
assert r['numFailedTests']==7 and r['numPassedTests']==0
write('red-proof.json',{'source':redProof,'originalRun':prior['run'],'originalCarrier':prior['carrier'],'counts':counts(rows),'expectedFailures':[{'fullName':a['fullName'],'status':a['status']} for a in selected],'samePromiseAssertionFailedInAllSeven':True,'testReportSha256':sha(E/'red-tests.json'),'sameLockedInputs':True,'sameInstallation':False,'allEightInstalledManifestsMatch':True,'redTestsNotRepeated':True})
source('red')
file=M['patchFile'];target=ROOT/file
with tarfile.open('/stage/green-source.tar.gz') as archive:raw=archive.extractfile(file).read()
assert hashlib.sha256(raw).hexdigest()==M['sources']['green']['files'][file]['sha256'];target.write_bytes(raw)
greenProof=source('green')
assert run(['pnpm','check'],'green-check',180)==0
assert run(['pnpm','build'],'green-build',180)==0
assert run(['pnpm','--filter','nostr-pubsub','test','--reporter=json','--outputFile=/stage/evidence/green-tests.json'],'green-tests',600)==0
r=report(E/'green-tests.json');rows=assertions(r);byName={a['fullName']:a for a in rows};assert len(byName)==len(rows)
assert r['success'] and r['numFailedTests']==0 and not any(a['status']=='failed' for a in rows)
assert all(byName[name]['status']=='passed' for name in expected)
files={Path(f['name']).relative_to(ROOT).as_posix() for f in r['testResults']};assert files==set(M['testFiles']),{'missing':sorted(set(M['testFiles'])-files),'extra':sorted(files-set(M['testFiles']))}
skips=[a['fullName'] for a in rows if a['status']!='passed'];assert skips==[M['legacyConditionalSkip']['fullName']],skips
assert r['numPendingTests']==1 and r['numPassedTests']==len(rows)-1 and all(a['status'] in ('passed','skipped') for a in rows)
source('green');network_proof('after')
dist=WORK/'packages/nostr-pubsub/dist';built={p.relative_to(dist).as_posix():{'bytes':p.stat().st_size,'sha256':sha(p)} for p in sorted(dist.rglob('*')) if p.is_file()};assert built and 'private-relay-publisher.js' in built
write('green-build-files.json',built)
write('qualification.json',{'passed':True,'releaseAccepted':False,'red':redProof,'green':greenProof,'greenCounts':counts(rows),'greenTestFiles':len(files),'newRegressionCasesPassed':7,'legacyConditionalSkip':M['legacyConditionalSkip'],'sourceUnchangedAfterGates':True,'sameLockedInputsForRedAndGreen':True,'sameInstallationForRedAndGreen':False,'redEvidenceRun':prior['run'],'installedLockSha256':sha(WORK/'node_modules/.pnpm/lock.yaml'),'greenReportSha256':sha(E/'green-tests.json'),'buildFilesSha256':sha(E/'green-build-files.json'),'steps':steps})
