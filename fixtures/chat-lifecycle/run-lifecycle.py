"""Focused Chat lifecycle unit proof; no vendor replacement or release claim."""
import hashlib,json,pathlib,subprocess,time
BASE=pathlib.Path('/stage');ROOT=BASE/'source';E=BASE/'evidence/chat-lifecycle';E.mkdir()
M=json.loads((BASE/'procedure/manifest.json').read_text());sha=lambda p:hashlib.sha256(p.read_bytes()).hexdigest()
def dump(name,value):(E/name).write_text(json.dumps(value,indent=2)+'\n')
def source_check():
    for file,digest in M['sourceFiles'].items():assert sha(ROOT/file)==digest,file
def network(name):subprocess.run(['python3','/stage/procedure/network-proof.py',str(E/name)],check=True,timeout=15)
source_check();network('network.json')
try:
    node=subprocess.check_output(['node','--version'],text=True,timeout=10).strip()
    pnpm=subprocess.check_output(['pnpm','--version'],text=True,timeout=10).strip()
    vitest=subprocess.check_output(['pnpm','exec','vitest','--version'],cwd=ROOT,text=True,timeout=20).strip()
    assert node==M['node'] and pnpm=='10.24.0' and vitest.startswith('vitest/'+M['vitest']+' ')
    dump('inputs.json',{'source':M['source'],'sourceArchiveSha256':M['sourceArchiveSha256'],'node':node,'pnpm':pnpm,'vitest':vitest,'sourceFiles':M['sourceFiles']})
    command=['pnpm','run','test','src/lib/deviceSyncLifecycle.test.ts','--maxWorkers=1','--reporter=json','--outputFile='+str(E/'units.json')]
    started=time.monotonic()
    with (E/'units.log').open('x') as log:result=subprocess.run(command,cwd=ROOT,stdout=log,stderr=subprocess.STDOUT,timeout=300)
    dump('command.json',{'command':command,'exitCode':result.returncode,'seconds':round(time.monotonic()-started,3)})
    assert result.returncode==0
    report=json.loads((E/'units.json').read_text());rows=[r for f in report['testResults'] for r in f['assertionResults']]
    expected=['starts shutdown synchronously and waits for it before one persisted pageshow restart','does not resurrect a logged-out or switched owner after deferred shutdown','holds asynchronous login readiness while hidden and ignores a stale restore after another hide','stops an already-started asynchronous runtime immediately and never resumes after disposal']
    assert report['success'] and report['numPassedTests']==report['numTotalTests']==4 and len(rows)==4 and len(report['testResults'])==1
    assert report['numFailedTests']==report['numPendingTests']==report['numTodoTests']==0
    assert sorted(r['title'] for r in rows)==sorted(expected) and all(r['status']=='passed' and not r['failureMessages'] for r in rows)
    source_check()
    dump('focused-passed.json',{'passed':True,'scope':M['scope'],'source':M['source'],'passedCases':4,'failedCases':0,'skippedCases':0,'originalVendorBytesUnchanged':True,'vendorSha256':sha(ROOT/'vendor/fips-file-transfer-0.1.0.tgz'),'fullAppQualified':False,'browserQualified':False,'nativeInteropQualified':False,'releaseAccepted':False})
finally:
    network('network-after.json');source_check()
    events=dict(row.split() for row in pathlib.Path('/sys/fs/cgroup/memory.events').read_text().splitlines());assert int(events['oom_kill'])==0
    dump('resources.json',{'events':events,'memoryPeak':int(pathlib.Path('/sys/fs/cgroup/memory.peak').read_text()),'noChildOOM':True})
