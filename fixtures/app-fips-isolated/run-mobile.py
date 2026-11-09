"""Anonymous mobile shell proof from exact retained dist, in a separate lo-only container."""
import hashlib,json,os,pathlib,subprocess
base=pathlib.Path('/stage');root=pathlib.Path.cwd();evidence=base/'evidence/chat-mobile'
spec=json.loads((base/'procedure/manifest.json').read_text())['apps'][0]
sha=lambda p:hashlib.sha256(p.read_bytes()).hexdigest()
assert not evidence.exists();evidence.mkdir()
assert os.environ['FIPS_DUMMY']=='0' and os.environ['FIPS_MEDIA_PERMISSION']=='0'
memory=pathlib.Path('/sys/fs/cgroup')
read_events=lambda:dict(line.split() for line in (memory/'memory.events').read_text().splitlines())
beforeEvents=read_events()
assert int(beforeEvents['oom'])==int(beforeEvents['oom_kill'])==0
for name,digest in spec['mobileSourceInputs'].items():assert sha(root/name)==digest
assert sha(root/'package.json')==spec['packageSha256'] and sha(root/'pnpm-lock.yaml')==spec['lockSha256']
archive=base/'earlier/chat/qualified-dist.tar.gz';assert sha(archive)==spec['priorEvidence']['distArchiveSha256']
manifest=base/'earlier/chat/dist-manifest.json';rows=json.loads(manifest.read_text());assert rows['source']==spec['commit']
def verify_dist():
    actual={p.relative_to(root/'dist').as_posix():p for p in (root/'dist').rglob('*') if p.is_file()}
    assert set(actual)=={r['path'] for r in rows['files']}
    for row in rows['files']:
        p=actual[row['path']];assert not p.is_symlink() and p.stat().st_size==row['bytes'] and sha(p)==row['sha256']
verify_dist()
subprocess.run(['node','/stage/procedure/installed-runtime-proof.cjs',str(base/'evidence/mobile-installed-runtime.json')],check=True,timeout=15)
plan={'source':spec['commit'],'sourceRoot':str(root),'site':'https://chat.iris.to',
    'distManifest':str(manifest),'distManifestSha256':sha(manifest),
    'sourceFiles':spec['mobileSourceInputs'],'productionVerified':False}
planPath=base/'mobile-source-plan.json';assert not planPath.exists();planPath.write_text(json.dumps(plan,indent=2)+'\n')
try:
    with (base/'evidence/mobile.log').open('xb') as log:
        run=subprocess.run(['node','/stage/procedure/mobile-predeploy.mjs',str(planPath),sha(planPath),
            '/stage/procedure/network-proof.py',str(evidence)],stdout=log,stderr=subprocess.STDOUT,timeout=180)
    assert run.returncode==0,'Mobile shell proof failed'
    receipt=json.loads((evidence/'mobile-offline.json').read_text())
    assert receipt['passed'] and receipt['source']==spec['commit'] and not receipt['productionVerified']
    assert receipt['offlineReload'] and receipt['serviceWorkerControlled'] and receipt['anonymous']
    assert receipt['permissionsGranted']==[] and not receipt['captureInvoked']
    assert receipt['rawWorkerDenial']==[True]*4
    assert all(receipt[k] for k in ('downloadPromptDismissedThroughUI','downloadPromptAbsentOnline','downloadPromptAbsentOffline','visibleShell'))
    assert receipt['geometry']['documentWidth']<=receipt['geometry']['width']==390
    assert receipt['sourceWorkerResponseFiles']==len(rows['files'])
    (evidence/'source-plan.json').write_bytes(planPath.read_bytes())
finally:
    verify_dist()
    for name,digest in spec['mobileSourceInputs'].items():assert sha(root/name)==digest
    events=read_events()
    assert int(events['oom'])==int(events['oom_kill'])==0
    (evidence/'resource-final.json').write_text(json.dumps({'noChildOOM':True,'noOOM':True,'beforeAndAfterWholeContainer':True,'beforeEvents':beforeEvents,'afterEvents':events,'events':events,'memoryPeak':int((memory/'memory.peak').read_text())},indent=2)+'\n')
