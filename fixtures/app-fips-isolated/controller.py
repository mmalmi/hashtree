import zipfile,stat
"""Task-only, hash-pinned browser qualification. Never builds native code."""
import hashlib,json,os,pathlib,shutil,signal,subprocess,sys,tarfile,time,urllib.request
HERE=pathlib.Path(__file__).resolve().parent
M=json.loads((HERE/'manifest.json').read_text())
STAGE=pathlib.Path(os.environ['FIPS_STAGE']).resolve()
EVIDENCE=STAGE/'evidence'
IMAGE=M['image']
CONTAINERS=set();PROCESS=None;START=time.monotonic();MIN_FREE=None;FIRST_FREE=None;DISK_SAMPLES=0
SAFE_ENV={k:v for k,v in os.environ.items() if k in ('PATH','HOME','USER','DOCKER_HOST','DOCKER_CONFIG','XDG_RUNTIME_DIR')}
SELECTED=os.environ.get('FIPS_APPS','').split(',') if os.environ.get('FIPS_APPS') else [a['id'] for a in M['apps']]
assert len(SELECTED)==len(set(SELECTED)) and set(SELECTED)<=set(a['id'] for a in M['apps'])
APPS=[next(a for a in M['apps'] if a['id']==name) for name in SELECTED]
APP_CONTAINERS={'iris-fips-test-chat'}
assert SELECTED==['chat']
def sha(p):return hashlib.sha256(pathlib.Path(p).read_bytes()).hexdigest()
proof=M['acceptedImageProof'];assert sha(HERE/proof['path'])==proof['sha256']
ACCEPTED_IMAGE=json.loads((HERE/proof['path']).read_text())
assert ACCEPTED_IMAGE['configImage']==IMAGE and ACCEPTED_IMAGE['node']==M['node']
def disk_guard(*,entry=False):
    global MIN_FREE,FIRST_FREE,DISK_SAMPLES
    free=shutil.disk_usage(STAGE if STAGE.exists() else STAGE.parent).free
    if FIRST_FREE is None:FIRST_FREE=free
    MIN_FREE=free if MIN_FREE is None else min(MIN_FREE,free);DISK_SAMPLES+=1
    limit=M['hostedDiskPolicy']['entryBytes' if entry else 'floorBytes']
    if EVIDENCE.is_dir():
        (EVIDENCE/('disk-'+sys.argv[1]+'.json')).write_text(json.dumps({'firstFreeBytes':FIRST_FREE,'minimumFreeBytes':MIN_FREE,'lastFreeBytes':free,'samples':DISK_SAMPLES,'entryBytes':M['hostedDiskPolicy']['entryBytes'],'floorBytes':M['hostedDiskPolicy']['floorBytes'],'sampleSeconds':M['hostedDiskPolicy']['commandSampleSeconds'],'floorViolated':MIN_FREE<M['hostedDiskPolicy']['floorBytes']},indent=2)+'\n')
    assert free>=limit,'Hosted disk entry/floor gate'
def cleanup():
    global PROCESS
    if PROCESS is not None and PROCESS.poll() is None:
        try:os.killpg(PROCESS.pid,signal.SIGTERM)
        except ProcessLookupError:pass
        try:PROCESS.wait(timeout=5)
        except subprocess.TimeoutExpired:
            os.killpg(PROCESS.pid,signal.SIGKILL);PROCESS.wait(timeout=5)
    for name in list(CONTAINERS):
        subprocess.run(['docker','rm','-f',name],env=SAFE_ENV,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,timeout=20)
        probe=subprocess.run(['docker','ps','-aq','--filter','name=^/'+name+'$'],env=SAFE_ENV,capture_output=True,text=True,timeout=10)
        if probe.returncode or probe.stdout.strip():raise RuntimeError('Container cleanup unproven: '+name)
        CONTAINERS.remove(name)
def interrupted(signum,frame):
    cleanup();raise SystemExit(128+signum)
for signum in (signal.SIGTERM,signal.SIGINT):signal.signal(signum,interrupted)
def run(args,log,timeout=300,allow_failure=False,extra_env=None):
    global PROCESS
    assert time.monotonic()-START<4200,'Overall deadline'
    disk_guard()
    p=EVIDENCE/log;assert not p.exists(),'Preserve prior evidence'
    with p.open('wb') as output:
        assert extra_env is None or set(extra_env)=={'GH_TOKEN'}
        PROCESS=subprocess.Popen(args,env=SAFE_ENV|(extra_env or {}),stdout=output,stderr=subprocess.STDOUT,start_new_session=True)
        process=PROCESS;deadline=time.monotonic()+timeout
        try:
            while True:
                disk_guard()
                code=process.poll()
                if code is not None:break
                remaining=deadline-time.monotonic()
                if remaining<=0:raise subprocess.TimeoutExpired(args,timeout)
                try:process.wait(timeout=min(M['hostedDiskPolicy']['commandSampleSeconds'],remaining))
                except subprocess.TimeoutExpired:pass
        except BaseException:
            if process.poll() is None:
                try:os.killpg(process.pid,signal.SIGTERM)
                except ProcessLookupError:pass
                try:process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    try:os.killpg(process.pid,signal.SIGKILL)
                    except ProcessLookupError:pass
                    process.wait(timeout=5)
            raise
        finally:PROCESS=None
    if not allow_failure:assert code==0,f'{log} exited {code}'
    return code
def fetch(item,path):
    with urllib.request.urlopen(item['url'],timeout=60) as response,path.open('wb') as output:
        while True:
            disk_guard();chunk=response.read(1024*1024)
            if not chunk:break
            output.write(chunk)
    assert sha(path)==item['sha256'],str(path)
def extract(archive,destination):
    destination.mkdir(parents=True,exist_ok=True)
    with tarfile.open(archive) as tar:tar.extractall(destination,filter='data')
def docker(name,command,*,network='none',cwd='/stage',seconds=300,dummy=False,evidence_suffix='',allow_failure=False):
    assert not dummy or (network=='none' and name in APP_CONTAINERS)
    if dummy:command=['python3','/stage/procedure/await-start.py',name,*command]
    command=['timeout','--signal=TERM','--kill-after=5s',str(seconds-10)+'s',*command]
    args=['docker','create','--name',name,'--label','iris.task=app-fips-isolated','--network',network,'--ipc','private','--cpus','2','--memory','4g','--memory-swap','4g','--pids-limit','512','--cap-drop','ALL','--security-opt','no-new-privileges','--user',f'{os.getuid()}:{os.getgid()}','--read-only','--tmpfs','/tmp:rw,nosuid,nodev,size=512m','--shm-size','512m','--init','--stop-timeout','5','--mount',f'type=bind,src={STAGE},dst=/stage','--workdir',cwd,'--env','HOME=/stage/home/chat','--env','PLAYWRIGHT_BROWSERS_PATH=/ms-playwright','--env','npm_config_manage_package_manager_versions=false','--env','npm_config_package_manager_strict_version=true','--env','PATH=/stage/tools/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin','--env','FIPS_DUMMY='+str(int(dummy)),'--env','FIPS_EVIDENCE_SUFFIX='+evidence_suffix,'--env','FIPS_MEDIA_PERMISSION='+str(int(name in APP_CONTAINERS)),IMAGE,*command]
    CONTAINERS.add(name)
    run(args,name+'-create.log',30)
    inspect=json.loads(subprocess.check_output(['docker','inspect',name],env=SAFE_ENV,text=True,timeout=5))[0]
    assert inspect['Image']==ACCEPTED_IMAGE['imageId'] and inspect['Config']['Image']==ACCEPTED_IMAGE['configImage'],'Accepted Chat image identity changed'
    h=inspect['HostConfig'];assert h['NetworkMode']==network and h['PidMode']=='' and h['Memory']==4294967296 and h['MemorySwap']==h['Memory'] and h['NanoCpus']==2000000000 and h['PidsLimit']==512
    assert h['CapDrop']==['ALL'] and h['ReadonlyRootfs'] and inspect['Config']['User']==f'{os.getuid()}:{os.getgid()}'
    (EVIDENCE/(name+'-container.json')).write_text(json.dumps({'imageId':inspect['Image'],'config':inspect['Config'],'hostConfig':h},indent=2)+'\n')
    try:
        if dummy:
            (EVIDENCE/'barriers').mkdir(exist_ok=True)
            barrier=EVIDENCE/'barriers'/(name+'.ready');assert not barrier.exists()
            run(['docker','start',name],name+'-start.log',10)
            run(['sudo','-n','python3',str(HERE/'namespace-setup.py'),inspect['Id'],name,inspect['Image'],str(os.getuid())],name+'-namespace.json',25)
            barrier.touch(exist_ok=False)
            run(['docker','attach',name],name+'.log',seconds,allow_failure=True)
        else:run(['docker','start','--attach',name],name+'.log',seconds,allow_failure=True)
        terminal=json.loads(subprocess.check_output(['docker','inspect',name],env=SAFE_ENV,text=True,timeout=5))[0]
        assert terminal['Id']==inspect['Id'] and not terminal['State']['Running'] and not terminal['State']['OOMKilled'],'No terminal container identity'
        (EVIDENCE/(name+'-terminal.json')).write_text(json.dumps(terminal['State'],indent=2)+'\n')
        code=terminal['State']['ExitCode']
        if not allow_failure:assert code==0,f'{name} exited {code}'
        return code
    finally:cleanup()

def prior_artifact(app):
    binding=app['priorEvidence'];assert binding['source']==app['commit']
    assert binding['artifactBytes']<512*1024**2 and binding['uncompressedBytes']<2*1024**3
    run(['gh','api',f"repos/mmalmi/hashtree/actions/artifacts/{binding['artifactId']}/zip"],
        'prior-artifact.zip',180,extra_env={'GH_TOKEN':os.environ['GH_TOKEN']})
    archive=EVIDENCE/'prior-artifact.zip'
    assert archive.stat().st_size==binding['artifactBytes'] and sha(archive)==binding['artifactSha256']
    assert sha(HERE/'prior-members.json')==binding['membersSha256']
    expected={r['path']:r for r in json.loads((HERE/'prior-members.json').read_text())}
    prior=STAGE/'earlier';assert not prior.exists();prior.mkdir()
    with zipfile.ZipFile(archive) as zipped:
        infos=zipped.infolist();names=[i.filename for i in infos];assert len(names)==len(set(names))
        assert sum(i.file_size for i in infos)==binding['uncompressedBytes']
        assert {i.filename for i in infos if not i.is_dir()}==set(expected)
        for info in infos:
            name=pathlib.PurePosixPath(info.filename)
            assert not name.is_absolute() and '..' not in name.parts and not stat.S_ISLNK(info.external_attr>>16)
            if info.is_dir():continue
            data=zipped.read(info);row=expected[info.filename]
            assert len(data)==row['bytes'] and hashlib.sha256(data).hexdigest()==row['sha256']
            target=prior/info.filename;target.parent.mkdir(parents=True,exist_ok=True);target.write_bytes(data)
    assert sha(prior/'chat/qualified-dist.tar.gz')==binding['distArchiveSha256']
    return prior

def prepare():
    assert not STAGE.exists(),'Use one empty task stage'
    disk_guard(entry=True)
    STAGE.mkdir(mode=0o700);EVIDENCE.mkdir();(STAGE/'home/chat').mkdir(parents=True);(STAGE/'tools/bin').mkdir(parents=True)
    disk_guard()
    shutil.copytree(HERE,STAGE/'procedure',ignore=shutil.ignore_patterns('archives'))
    fetch(M['pnpm'],STAGE/'pnpm.tgz');extract(STAGE/'pnpm.tgz',STAGE/'tools/pnpm')
    (STAGE/'tools/bin/pnpm').write_text('#!/bin/sh\nexec node /stage/tools/pnpm/package/bin/pnpm.cjs "$@"\n');(STAGE/'tools/bin/pnpm').chmod(0o755)
    run(['docker','pull',IMAGE],'image-pull.log',300)
    docker('iris-fips-image-proof',['node','-e',"const fs=require('node:fs'),c=require('node:crypto');fs.writeFileSync('/stage/evidence/image-toolchain.json',JSON.stringify({node:process.version,nodeSha256:c.createHash('sha256').update(fs.readFileSync(process.execPath)).digest('hex')}))"],seconds=30)
    assert json.loads((EVIDENCE/'image-toolchain.json').read_text())=={k:ACCEPTED_IMAGE[k] for k in ('node','nodeSha256')}
    for app in APPS:
        archive=HERE/app['archive'];assert sha(archive)==app['archiveSha256']
        target=STAGE/'apps'/app['id'];extract(archive,target)
        prior=prior_artifact(app)
        assert not (target/'dist').exists()
        extract(prior/'chat/qualified-dist.tar.gz',target/'dist')
        assert sha(target/'pnpm-lock.yaml')==app['lockSha256']
        assert json.loads((target/'package.json').read_text())['packageManager']==app['packageManager']
        for file,digest in app['testFiles'].items():assert sha(target/file)==digest
        for file,digest in app['sourceInputs'].items():assert sha(target/file)==digest
        for relative,item in app['testOverlays'].items():
            assert sha(HERE/item['file'])==item['sha256']
            destination=target/relative;assert not destination.exists()
            destination.parent.mkdir(parents=True,exist_ok=True);shutil.copyfile(HERE/item['file'],destination)
        assert sha(target/'package.json')==app['packageSha256']
        run(['python3',str(HERE/'apply-app-overlay.py'),app['id'],str(STAGE)],'overlay-'+app['id']+'.log',10)
        docker('iris-fips-install-'+app['id'],['pnpm','install','--frozen-lockfile','--ignore-scripts','--store-dir','/stage/pnpm-store'],network='bridge',cwd='/stage/apps/'+app['id'],seconds=300)
    (EVIDENCE/'prepared.json').write_text(json.dumps({'manifestSha256':sha(HERE/'manifest.json'),'image':IMAGE,'apps':SELECTED,'sourceInputsVerified':True,'buildFromExactSource':False,'reusedExactBuiltArchive':True},indent=2)+'\n')

def test():
    assert json.loads((EVIDENCE/'prepared.json').read_text())['manifestSha256']==sha(HERE/'manifest.json')
    assert set(SELECTED)<=set(json.loads((EVIDENCE/'prepared.json').read_text())['apps'])
    budgets=json.loads((HERE/'app-budgets.json').read_text()); results=[]
    for app in APPS:
        code=docker('iris-fips-test-'+app['id'],['python3','/stage/procedure/run-one.py',app['id']],cwd='/stage/apps/'+app['id'],seconds=budgets[app['id']]+240,dummy=True,allow_failure=True)
        assert not CONTAINERS,'No later app may start before cleanup'
        results.append({'app':app['id'],'exitCode':code,'passed':code==0})
    passed=all(row['passed'] for row in results)
    (EVIDENCE/'passed.json').write_text(json.dumps({'passed':passed,'manifestSha256':sha(HERE/'manifest.json'),'apps':SELECTED,'results':results,'containersReaped':not CONTAINERS},indent=2)+'\n')
    assert passed,'One or more unchanged app tests failed; see per-app evidence'

try:
    {'prepare':prepare,'test':test}[sys.argv[1]]()
finally:cleanup()
