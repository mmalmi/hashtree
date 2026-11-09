"""Validate exact failed-run bytes, reusing only its successful unchanged gates."""
import hashlib,json,pathlib,runpy,tarfile
sha=lambda p:hashlib.sha256(p.read_bytes()).hexdigest()
load=lambda p:json.loads(p.read_text())
def verify(base,evidence,spec):
    prior=base/'earlier';old=prior/'chat';binding=spec['priorEvidence']
    assert sha(base/'evidence/prior-artifact.zip')==binding['artifactSha256']
    members=load(base/'procedure/prior-members.json')
    assert sha(base/'procedure/prior-members.json')==binding['membersSha256']
    assert {p.relative_to(prior).as_posix() for p in prior.rglob('*') if p.is_file()}=={r['path'] for r in members}
    for row in members:
        p=prior/row['path'];assert not p.is_symlink() and p.stat().st_size==row['bytes'] and sha(p)==row['sha256']
    oldManifest=load(base/'procedure/prior-manifest.json');oldSpec=oldManifest['apps'][0]
    assert sha(base/'procedure/prior-manifest.json')==binding['manifestSha256']==load(prior/'prepared.json')['manifestSha256']
    for key in ['commit','archiveSha256','lockSha256','packageSha256','releaseProfileSha256','testFiles','sourceInputs','packageManager','playwright','installedRuntimeExpectations','acceptedDependencyRelease','fileTransferRelease']:
        assert oldSpec[key]==spec[key],key
    assert oldSpec['commit']==binding['source']
    currentInputs=load(evidence/'inputs.json');oldInputs=load(old/'inputs.json')
    for key in ('node','pnpm','pnpmEntrySha256','sourceCommit','image'):assert currentInputs[key]==oldInputs[key]
    assert load(old/'installed-runtime.json')==load(evidence/'installed-runtime.json')
    assert load(prior/'image-toolchain.json')==load(base/'evidence/image-toolchain.json')
    for prefix in ['install','test']:
        container=load(prior/('iris-fips-'+prefix+'-chat-container.json'));h=container['hostConfig']
        assert h['NetworkMode']==('bridge' if prefix=='install' else 'none')
        assert h['Memory']==h['MemorySwap']==4294967296 and h['NanoCpus']==2000000000 and h['PidsLimit']==512
        assert h['CapDrop']==['ALL'] and h['ReadonlyRootfs'] and h['PidMode']=='' and 'no-new-privileges' in h['SecurityOpt']
        assert container['config']['Image']==oldManifest['image'] and container['imageId']==load(base/'evidence/iris-fips-image-proof-container.json')['imageId']
        terminal=load(prior/('iris-fips-'+prefix+'-chat-terminal.json'))
        assert terminal['Status']=='exited' and terminal['ExitCode']==(0 if prefix=='install' else 1)
        assert not terminal['Running'] and not terminal['OOMKilled']
    resource=load(old/'resource-final.json');assert resource['noChildOOM'] and int(resource['events']['oom_kill'])==0
    namespaces=[]
    for name in ['network.json','network-after-attempt.json']:
        net=load(old/name);assert net['interfaces']==['fixture0','lo'] and net['uid']!=0 and net['effectiveAndBoundingCapabilities']==0
        for rows in [net['externalDenied'],net['childDenied']]:
            assert {(r['family'],r['kind']) for r in rows}=={(2,1),(2,2),(10,1),(10,2)}
            assert all(r['errno'] in (101,113,99,97) for r in rows)
        assert net['loopbackTcpUdp']==[1,2] and not net['route4'].splitlines()[1:]
        assert all(line.split()[-1]=='lo' for line in net['route6'].splitlines());namespaces.append(net['netNamespace'])
    assert namespaces[0]==namespaces[1]
    oldBrowser=load(old/'browser.json');currentBrowser=load(evidence/'browser.json')
    assert oldBrowser['passed'] and currentBrowser['passed']
    for key in ('node','playwright','image','browserMetadata','packageFiles'):assert oldBrowser[key]==currentBrowser[key]
    for engine in ['chromium','firefox','webkit']:
        a=oldBrowser['engines'][engine];b=currentBrowser['engines'][engine]
        assert a['passed'] and a['browser']==b['browser']
        assert sorted(a['executables'],key=lambda r:r['path'])==sorted(b['executables'],key=lambda r:r['path'])
        assert len(a['rawWorkerDenial'])==4 and all(r.get('https')=='rejected' or r.get('wss')=='rejected' for r in a['rawWorkerDenial'])
        if engine=='chromium':assert a['rtc']['echoed']=='loopback-only' and a['candidateAddressesAllowed']
        else:assert a['loopbackWebSocket']=='unsigned-local-echo' and not a['mediaPermission']['requested']
        assert not a['mediaPermission']['captureInvoked']
    contexts=[json.loads(line) for line in (old/'context-permissions.jsonl').read_text().splitlines()]
    assert contexts and {r['engine'] for r in contexts}=={'chromium','firefox','webkit'}
    for row in contexts:
        assert row['origin']=='http://127.0.0.1:5721' and not row['captureInvoked']
        if row['engine']=='chromium':assert row['permission']=='microphone' and row['state']=='granted'
        else:assert row['permission'] is None
    units=load(old/'units.json');actual=[a for f in units['testResults'] for a in f['assertionResults']]
    assert units['success'] and len(units['testResults'])==35 and len(actual)==262 and all(a['status']=='passed' for a in actual)
    checks=runpy.run_path(str(base/'procedure/browser-results.py'))
    checks['verify'](old/'results.json',62,3,base/'procedure/original-browser-cases.json')
    gates=load(old/'release-gates.json');assert not gates['passed'] and gates['source']==spec['commit'] and gates['allAppGatesFromCurrentRun']
    assert [(s['id'],s['exitCode']) for s in gates['steps']]==[('build',0),('test-portable',0),('test-smoke',0),('test-graph',0),('test-relay-policy',0),('test-e2e-nip07',0),('peer-recovery',1)]
    failed=load(old/'peer-results.json');records=[]
    def walk(suite):
        for case in suite.get('specs',[]):
            for test in case.get('tests',[]):records.append((case,test))
        for child in suite.get('suites',[]):walk(child)
    walk(failed);assert len(records)==1
    case,test=records[0];assert case['title']=='three browsers share cached encrypted files on the existing FIPS node with Blossom unavailable'
    assert pathlib.PurePosixPath(case['file']).name=='peer-attachments.spec.ts' and test['projectName']=='chromium'
    assert test['status']=='unexpected' and len(test['results'])==1
    result=test['results'][0];assert result['status']=='failed' and result['retry']==0
    assert "getByAltText('peer.jpg')" in result['errors'][0]['message']
    assert failed['stats']['unexpected']==1 and failed['stats']['expected']==failed['stats']['flaky']==failed['stats']['skipped']==0
    archive=old/'qualified-dist.tar.gz';assert sha(archive)==binding['distArchiveSha256']
    manifest=load(old/'dist-manifest.json');assert manifest['source']==spec['commit']
    files={f['path']:f for f in manifest['files']};assert len(files)==len(manifest['files'])
    with tarfile.open(archive) as tar:
        members=tar.getmembers();assert len(members)==len(files) and {m.name for m in members}==set(files)
        for member in members:
            assert member.isfile() and not pathlib.PurePosixPath(member.name).is_absolute() and '..' not in pathlib.PurePosixPath(member.name).parts
            raw=tar.extractfile(member).read();row=files[member.name]
            assert len(raw)==row['bytes'] and hashlib.sha256(raw).hexdigest()==row['sha256']
    receipt={'passed':True,'source':spec['commit'],'runId':binding['runId'],'carrier':binding['carrier'],
        'artifactId':binding['artifactId'],'artifactSha256':binding['artifactSha256'],
        'archiveMembersVerified':len(load(base/'procedure/prior-members.json')),
        'reusedScopes':binding['passingScopes'],'excludedFailedScope':binding['excludedFailedScope'],
        'unitCases':262,'browserPassed':62,'exactPlatformSkips':3,'smokesPassed':3,
        'sameSource':True,'sameBuiltBytes':True,'sameInstall':False,
        'distArchiveSha256':sha(archive),'distManifestSha256':sha(old/'dist-manifest.json'),
        'priorFailureRetained':True,'priorFailureAccepted':False}
    (evidence/'reused-gates.json').write_text(json.dumps(receipt,indent=2)+'\n')
    return receipt
