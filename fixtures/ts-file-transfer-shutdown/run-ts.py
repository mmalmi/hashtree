"""Isolated TS phase. Live native interop and public-source acceptance remain open."""
import base64,hashlib,json,os,pathlib,re,subprocess,tarfile,time
BASE=pathlib.Path('/stage');ROOT=BASE/'source';WORK=ROOT/'file-transfer-ts';E=BASE/'evidence/file-transfer';E.mkdir()
M=json.loads((BASE/'procedure/manifest.json').read_text());sha=lambda p:hashlib.sha256(p.read_bytes()).hexdigest()
def dump(name,value):(E/name).write_text(json.dumps(value,indent=2)+'\n')
def command(name,args,seconds=180,expected=0):
    started=time.monotonic()
    with (E/(name+'.log')).open('x') as log:
        p=subprocess.run(args,cwd=WORK,stdout=log,stderr=subprocess.STDOUT,timeout=seconds)
    steps.append({'name':name,'command':args,'exitCode':p.returncode,'seconds':round(time.monotonic()-started,3)})
    dump('steps.json',steps);assert p.returncode==expected,(name,p.returncode)
def source_check():
    for path,digest in M['sourceFiles'].items():assert sha(ROOT/path)==digest,path
def network(name):subprocess.run(['python3','/stage/procedure/network-proof.py',str(E/name)],check=True,timeout=15)
def assertions(name):
    report=json.loads((E/name).read_text());rows=[a for f in report['testResults'] for a in f['assertionResults']]
    assert len(rows)==report['numTotalTests'] and len({r['fullName'] for r in rows})==len(rows)
    return report,rows
steps=[];source_check();network('network.json')
try:
    versions={}
    for name,args in [('node',['node','--version']),('pnpm',['pnpm','--version']),('typescript',['pnpm','exec','tsc','--version']),('vitest',['pnpm','exec','vitest','--version'])]:
        versions[name]=subprocess.check_output(args,cwd=WORK,text=True,timeout=20).strip()
    assert versions['node']==M['node'] and versions['pnpm']=='10.24.0' and versions['typescript']=='Version '+M['typescript'] and versions['vitest'].startswith('vitest/'+M['vitest']+' ')
    dump('inputs.json',{'source':M['source'],'versions':versions,'sourceFiles':M['sourceFiles'],'sourceArchiveSha256':M['sourceArchiveSha256']})
    index=WORK/'src/index.ts';green=index.read_bytes();red=BASE/'procedure/red-index.ts';assert sha(red)==M['redIndexSha256']
    restored=False
    try:
        index.write_bytes(red.read_bytes())
        command('red',['pnpm','exec','vitest','run','test/resume.test.ts','-t','preserves shutdown intent when a sending Blob read rejects','--maxWorkers=1','--reporter=json','--outputFile='+str(E/'red.json')],expected=1)
        report,rows=assertions('red.json')
        selected=[r for r in rows if r['title'].startswith('preserves shutdown intent when a sending Blob read rejects: ')]
        assert len(rows)==15 and len(selected)==3 and report['numPassedTests']==2 and report['numFailedTests']==1 and report['numPendingTests']==12
        for row in selected:
            intent=row['title'].rsplit(': ',1)[1];assert intent in ('dispose','cancel','revoke')
            assert row['status']==('failed' if intent=='dispose' else 'passed')
            if intent=='dispose':
                failure='\n'.join(row['failureMessages']);assert 'claimed' in failure and 'cancelled' in failure
        assert all(r['status']=='pending' for r in rows if r not in selected)
    finally:
        index.write_bytes(green);source_check();restored=True
        dump('source-restored.json',{'restored':True,'source':M['source'],'indexSha256':sha(index),'redIndexSha256':sha(red),'sourceFiles':M['sourceFiles']})
    command('check',['pnpm','run','check'])
    command('green',['pnpm','exec','vitest','run','--maxWorkers=1','--reporter=json','--outputFile='+str(E/'green.json')],seconds=300)
    report,rows=assertions('green.json');assert report['success'] and len(report['testResults'])==2 and len(rows)==27
    assert report['numPassedTests']==report['numTotalTests']==27 and report['numFailedTests']==report['numPendingTests']==report['numTodoTests']==0 and all(r['status']=='passed' for r in rows)
    assert sum(r['title']=='matches canonical Rust/TypeScript manifest and control vectors' for r in rows)==1
    command('build',['pnpm','run','build'])
    command('pack',['pnpm','pack','--pack-destination',str(E)])
    source_check()
    archive=E/'fips-file-transfer-0.1.1.tgz';expected={'package/package.json','package/README.md','package/LICENSE'}
    for directory in ['src','dist']:
        paths=[p for p in (WORK/directory).rglob('*') if p.is_file()];assert paths
        assert not any(p.is_symlink() for p in (WORK/directory).rglob('*'))
        expected.update('package/'+p.relative_to(WORK).as_posix() for p in paths)
    members=[]
    with tarfile.open(archive) as t:
        files=t.getmembers();assert all(p.isfile() and not pathlib.PurePosixPath(p.name).is_absolute() and '..' not in pathlib.PurePosixPath(p.name).parts for p in files)
        assert len(files)==len(expected) and {p.name for p in files}==expected
        for p in files:
            raw=t.extractfile(p).read();name=p.name.removeprefix('package/')
            if name=='package.json':
                package=json.loads(raw);source=json.loads((WORK/name).read_bytes())
                for key in ['name','version','main','types','exports','peerDependencies','dependencies','repository']:assert package[key]==source[key]
                assert package['name']==M['packageName'] and package['version']==M['packageVersion']
            else:assert raw==(WORK/name).read_bytes(),name
            members.append({'path':p.name,'bytes':len(raw),'sha256':hashlib.sha256(raw).hexdigest()})
    dump('package-provenance.json',{'source':M['source'],'sourceArchiveSha256':M['sourceArchiveSha256'],'lockSha256':sha(WORK/'pnpm-lock.yaml'),'packageSha256':sha(WORK/'package.json'),'versions':versions,'package':M['packageName'],'version':M['packageVersion'],'archive':archive.name,'archiveSha256':sha(archive),'archiveIntegrity':'sha512-'+base64.b64encode(hashlib.sha512(archive.read_bytes()).digest()).decode(),'members':members,'typescriptGatesPassed':True,'sharedVectorsPassed':True,'liveInteropPassed':False,'publicSourceClosurePassed':False,'releaseAccepted':False})
    dump('typescript-passed.json',{'passed':True,'scope':M['scope'],'source':M['source'],'redPassed':2,'redExpectedFailed':1,'greenPassed':27,'sharedVectorsPassed':True,'sourceRestored':restored,'requiredLiveInterop':M['requiredLiveInterop'],'allReleaseGatesPassed':False})

finally:
    network('network-after.json');source_check()
    events=dict(row.split() for row in pathlib.Path('/sys/fs/cgroup/memory.events').read_text().splitlines());assert int(events['oom_kill'])==0
    dump('resources.json',{'events':events,'memoryPeak':int(pathlib.Path('/sys/fs/cgroup/memory.peak').read_text()),'noChildOOM':True})
