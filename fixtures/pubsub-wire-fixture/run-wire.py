from pathlib import Path
import collections,hashlib,json,subprocess,time
M=json.loads(Path('/stage/procedure/manifest.json').read_text());E=Path('/stage/evidence');ROOT=Path('/stage/workspace');WORK=ROOT/'ts';sha=lambda p:hashlib.sha256(Path(p).read_bytes()).hexdigest()
def write(name,value):(E/name).write_text(json.dumps(value,indent=2)+'\n')
def source():
 for name,row in M['greenSource']['files'].items():assert sha(ROOT/name)==row['sha256']
source();prior=M['priorGreen'];installed={}
for name,wanted in prior['installed']['packages'].items():
 p=WORK/'packages/nostr-pubsub/node_modules'/name/'package.json';j=json.loads(p.read_text());row={'version':j['version'],'manifestSha256':sha(p)};assert j['name']==name and row==wanted;installed[name]=row
assert sha(WORK/'pnpm-lock.yaml')==prior['installed']['sourceLockSha256'];assert sha(WORK/'node_modules/.pnpm/lock.yaml')==prior['installed']['installedLockSha256'];assert json.loads((E/'toolchain.json').read_text())==prior['toolchain']
write('locked-input-comparison.json',{'allEightInstalledManifestsMatch':True,'sourceLockSha256':sha(WORK/'pnpm-lock.yaml'),'installedLockSha256':sha(WORK/'node_modules/.pnpm/lock.yaml'),'sameLockedInputs':True,'sameInstallation':False,'originalRun':prior['run']})
assert sha('/stage/procedure/prior-green-tests.json')==prior['reportSha256'];old=json.loads(Path('/stage/procedure/prior-green-tests.json').read_text());oldRows=[a for f in old['testResults'] for a in f['assertionResults']]
assert collections.Counter(a['status'] for a in oldRows)=={'passed':172,'failed':1,'skipped':1}
case=M['case'];assert [a['fullName'] for a in oldRows if a['status']=='failed']==[case['fullName']];assert [a['fullName'] for a in oldRows if a['status']=='skipped']==[M['legacyConditionalSkip']['fullName']]
assert {a['fullName'] for a in oldRows if a['status']=='passed' and a['fullName'] in M['newRegressions']}==set(M['newRegressions'])
originalFile=next(f for f in old['testResults'] if f['name'].endswith('/tests/fips-pubsub-client.test.ts'));allNames={a['fullName'] for a in originalFile['assertionResults']}
# A general-purpose cargo command and the correct command from a wrong cwd must be refused.
negative=[]
for args,cwd in [(['build'],ROOT),(M['originalCargoArgv'],WORK)]:
 p=subprocess.run(['cargo',*args],cwd=cwd,capture_output=True,text=True,timeout=5);assert p.returncode==64 and not p.stdout and p.stderr=='Unsupported fixture invocation\n';negative.append({'kind':'wrong-args' if args==['build'] else 'wrong-cwd','exitCode':p.returncode})
assert not (E/'fixture-invocations.jsonl').exists();write('launcher-negative-controls.json',negative)
b=json.loads((E/'wire-binary.json').read_text());assert sha(b['path'])==b['sha256'] and b['source']==M['nativeSource']['commit'];linked=subprocess.run(['ldd',b['path']],capture_output=True,text=True,timeout=5);(E/'binary-linkage.txt').write_text(linked.stdout+linked.stderr);assert linked.returncode==0 and 'not found' not in linked.stdout+linked.stderr
args=['pnpm','--filter','nostr-pubsub','test','tests/fips-pubsub-client.test.ts','--testNamePattern','^'+case['fullName']+'$','--reporter=json','--outputFile=/stage/evidence/wire-tests.json']
start=time.monotonic()
with (E/'wire-tests.log').open('wb') as f:p=subprocess.run(args,cwd=WORK,stdout=f,stderr=subprocess.STDOUT,timeout=90)
write('wire-command.json',{'args':args,'exitCode':p.returncode,'seconds':time.monotonic()-start});assert p.returncode==0
r=json.loads((E/'wire-tests.json').read_text());rows=[a for f in r['testResults'] for a in f['assertionResults']];assert r['success'] and r['numPassedTests']==1 and r['numFailedTests']==0 and r['numPendingTests']==len(allNames)-1
assert len(r['testResults'])==1 and {a['fullName'] for a in rows}==allNames
assert [a['fullName'] for a in rows if a['status']=='passed']==[case['fullName']] and all(a['status']=='skipped' for a in rows if a['fullName']!=case['fullName'])
invocations=[json.loads(line) for line in (E/'fixture-invocations.jsonl').read_text().splitlines()];assert len(invocations)==1 and invocations[0]['argv']==M['originalCargoArgv'] and invocations[0]['cwd']=='/stage/workspace' and invocations[0]['binarySha256']==b['sha256']
source();write('wire-case.json',{'passed':True,'case':case,'runSource':M['greenSource']['commit'],'testReportSha256':sha(E/'wire-tests.json'),'binarySha256':b['sha256'],'sourceUnchanged':True,'realRustProcess':True,'launcherSubstitution':invocations[0],'negativeControlsPassed':True,'unselectedAssertions':len(allNames)-1,'priorGreenRun':prior['run'],'sameLockedInputs':True,'sameInstallation':False,'combinedGreen':{'passed':173,'skipped':1,'failed':0},'combinedLegacySkip':M['legacyConditionalSkip'],'releaseAccepted':False})
