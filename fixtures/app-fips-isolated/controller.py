"""Task-only, hash-pinned browser qualification. Never builds native code."""
import hashlib,json,os,pathlib,shutil,signal,subprocess,sys,tarfile,time,urllib.request
HERE=pathlib.Path(__file__).resolve().parent
M=json.loads((HERE/'manifest.json').read_text())
STAGE=pathlib.Path(os.environ['FIPS_STAGE']).resolve()
EVIDENCE=STAGE/'evidence'
IMAGE=M['image']
CONTAINERS=set();PROCESS=None;START=time.monotonic()
SAFE_ENV={k:v for k,v in os.environ.items() if k in ('PATH','HOME','USER','DOCKER_HOST','DOCKER_CONFIG','XDG_RUNTIME_DIR')}
SELECTED=os.environ.get('FIPS_APPS','').split(',') if os.environ.get('FIPS_APPS') else [a['id'] for a in M['apps']]
assert len(SELECTED)==len(set(SELECTED)) and set(SELECTED)<=set(a['id'] for a in M['apps'])
APPS=[next(a for a in M['apps'] if a['id']==name) for name in SELECTED]
def sha(p):return hashlib.sha256(pathlib.Path(p).read_bytes()).hexdigest()
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
def run(args,log,timeout=300,allow_failure=False):
    global PROCESS
    assert time.monotonic()-START<2400,'Overall deadline'
    assert shutil.disk_usage(STAGE).free>8*1024**3,'Hosted disk reserve'
    p=EVIDENCE/log;assert not p.exists(),'Preserve prior evidence'
    with p.open('wb') as output:
        PROCESS=subprocess.Popen(args,env=SAFE_ENV,stdout=output,stderr=subprocess.STDOUT,start_new_session=True)
        try:code=PROCESS.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            os.killpg(PROCESS.pid,signal.SIGTERM)
            try:PROCESS.wait(timeout=5)
            except subprocess.TimeoutExpired:os.killpg(PROCESS.pid,signal.SIGKILL);PROCESS.wait(timeout=5)
            raise
        finally:PROCESS=None
    if not allow_failure:assert code==0,f'{log} exited {code}'
    return code
def fetch(item,path):
    with urllib.request.urlopen(item['url'],timeout=60) as response,path.open('wb') as output:shutil.copyfileobj(response,output)
    assert sha(path)==item['sha256'],str(path)
def extract(archive,destination):
    destination.mkdir(parents=True,exist_ok=True)
    with tarfile.open(archive) as tar:tar.extractall(destination,filter='data')
def docker(name,command,*,network='none',cwd='/stage',seconds=300,dummy=False,evidence_suffix='',allow_failure=False):
    assert not dummy or (network=='none' and name=='iris-fips-diagnostic-docs-dummy')
    if dummy:command=['python3','/stage/procedure/await-start.py',name,*command]
    command=['timeout','--signal=TERM','--kill-after=5s',str(seconds-10)+'s',*command]
    args=['docker','create','--name',name,'--label','iris.task=app-fips-isolated','--network',network,'--ipc','private','--cpus','2','--memory','4g','--memory-swap','4g','--pids-limit','512','--cap-drop','ALL','--security-opt','no-new-privileges','--user',f'{os.getuid()}:{os.getgid()}','--read-only','--tmpfs','/tmp:rw,nosuid,nodev,size=512m','--shm-size','512m','--init','--stop-timeout','5','--mount',f'type=bind,src={STAGE},dst=/stage','--workdir',cwd,'--env','HOME=/stage/home','--env','PLAYWRIGHT_BROWSERS_PATH=/ms-playwright','--env','PATH=/stage/tools/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin','--env','FIPS_DUMMY='+str(int(dummy)),'--env','FIPS_EVIDENCE_SUFFIX='+evidence_suffix,IMAGE,*command]
    CONTAINERS.add(name)
    run(args,name+'-create.log',30)
    inspect=json.loads(subprocess.check_output(['docker','inspect',name],env=SAFE_ENV,text=True,timeout=5))[0]
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
        assert terminal['Id']==inspect['Id'] and not terminal['State']['Running'],'No terminal container identity'
        (EVIDENCE/(name+'-terminal.json')).write_text(json.dumps(terminal['State'],indent=2)+'\n')
        code=terminal['State']['ExitCode']
        if not allow_failure:assert code==0,f'{name} exited {code}'
        return code
    finally:cleanup()

def prepare():
    assert not STAGE.exists(),'Use one empty task stage'
    STAGE.mkdir(mode=0o700);EVIDENCE.mkdir();(STAGE/'home').mkdir();(STAGE/'tools/bin').mkdir(parents=True)
    shutil.copytree(HERE,STAGE/'procedure',ignore=shutil.ignore_patterns('archives'))
    fetch(M['pnpm'],STAGE/'pnpm.tgz');extract(STAGE/'pnpm.tgz',STAGE/'tools/pnpm')
    (STAGE/'tools/bin/pnpm').write_text('#!/bin/sh\nexec node /stage/tools/pnpm/package/bin/pnpm.cjs "$@"\n');(STAGE/'tools/bin/pnpm').chmod(0o755)
    fetch(M['native'],STAGE/'native.tgz');extract(STAGE/'native.tgz',STAGE/'tools/native')
    for name,digest in M['native']['executables'].items():assert sha(STAGE/'tools/native/hashtree'/name)==digest
    run(['docker','pull',IMAGE],'image-pull.log',300)
    docker('iris-fips-image-proof',['node','-e',"const fs=require('node:fs'),c=require('node:crypto');if(process.versions.node.split('.')[0]!=='24')throw Error('Unexpected image Node');fs.writeFileSync('/stage/evidence/image-toolchain.json',JSON.stringify({node:process.version,nodeSha256:c.createHash('sha256').update(fs.readFileSync(process.execPath)).digest('hex')}))"],seconds=30)
    for app in APPS:
        archive=HERE/app['archive'];assert sha(archive)==app['archiveSha256']
        target=STAGE/'apps'/app['id'];extract(archive,target)
        assert sha(target/'pnpm-lock.yaml')==app['lockSha256']
        for file,digest in app['testFiles'].items():assert sha(target/file)==digest
        dist=HERE/app['distArchive'];assert sha(dist)==app['distArchiveSha256'];extract(dist,target/app['distDir'])
        for file in app['distFiles']:assert sha(target/app['distDir']/file['path'])==file['sha256']
        (target/'isolated.playwright.config.ts').write_text("import original from './playwright.config';\nexport default {...original, use:{...original.use, launchOptions:{...original.use?.launchOptions,args:[...(original.use?.launchOptions?.args??[]),'--disable-features=WebRtcHideLocalIpsWithMdns','--allow-loopback-in-peer-connection']}}};\n")
        docker('iris-fips-install-'+app['id'],['pnpm','install','--frozen-lockfile','--ignore-scripts','--store-dir','/stage/pnpm-store'],network='bridge',cwd='/stage/apps/'+app['id'],seconds=300)
    (EVIDENCE/'prepared.json').write_text(json.dumps({'manifestSha256':sha(HERE/'manifest.json'),'image':IMAGE,'apps':SELECTED,'sourceInputsVerified':True,'distInputsVerified':True},indent=2)+'\n')

def test():
    assert json.loads((EVIDENCE/'prepared.json').read_text())['manifestSha256']==sha(HERE/'manifest.json')
    assert set(SELECTED)<=set(json.loads((EVIDENCE/'prepared.json').read_text())['apps'])
    for app in APPS:
        docker('iris-fips-test-'+app['id'],['python3','/stage/procedure/run-one.py',app['id']],cwd='/stage/apps/'+app['id'],seconds=300)
    (EVIDENCE/'passed.json').write_text(json.dumps({'passed':True,'manifestSha256':sha(HERE/'manifest.json'),'apps':SELECTED,'containersReaped':not CONTAINERS},indent=2)+'\n')
def diagnose():
    prepared=json.loads((EVIDENCE/'prepared.json').read_text())
    assert SELECTED==['docs'] and prepared['apps']==SELECTED and prepared['manifestSha256']==sha(HERE/'manifest.json')
    command=['python3','/stage/procedure/run-one.py','docs','--unsigned-only']
    code=docker('iris-fips-diagnostic-docs-loopback',command,cwd='/stage/apps/docs',seconds=75,evidence_suffix='-loopback',allow_failure=True)
    baseline=json.loads((EVIDENCE/'docs-loopback/browser.json').read_text())
    network=json.loads((EVIDENCE/'docs-loopback/network.json').read_text());assert network['interfaces']==['lo']
    assert len(baseline['rawWorkerDenial'])==4 and all(v.get('https')=='rejected' or v.get('wss')=='rejected' for v in baseline['rawWorkerDenial'])
    assert baseline['candidateAddressesAllowed']
    if code:
        assert baseline['rtc']['error']=='Loopback browser datachannel unavailable','Only diagnosed RTC failure permits dummy proof'
        docker('iris-fips-diagnostic-docs-dummy',command,cwd='/stage/apps/docs',seconds=90,dummy=True,evidence_suffix='-dummy')
    (EVIDENCE/'unsigned-diagnostic-passed.json').write_text(json.dumps({'passed':True,'unsignedOnly':True,'apps':SELECTED,'loopbackPassed':code==0,'dummyRequired':code!=0,'containersReaped':not CONTAINERS},indent=2)+'\n')
try:
    {'prepare':prepare,'test':test,'diagnose':diagnose}[sys.argv[1]]()
finally:cleanup()
