"""Task-only, hash-pinned browser qualification. Never builds native code."""
import hashlib,json,os,pathlib,shutil,signal,subprocess,sys,tarfile,time,urllib.request
HERE=pathlib.Path(__file__).resolve().parent
M=json.loads((HERE/'manifest.json').read_text())
STAGE=pathlib.Path(os.environ['FIPS_STAGE']).resolve()
EVIDENCE=STAGE/'evidence'
IMAGE=M['image']
CONTAINERS=set();PROCESS=None;START=time.monotonic()
SAFE_ENV={k:v for k,v in os.environ.items() if k in ('PATH','HOME','USER','DOCKER_HOST','DOCKER_CONFIG','XDG_RUNTIME_DIR')}
def sha(p):return hashlib.sha256(pathlib.Path(p).read_bytes()).hexdigest()
def cleanup():
    global PROCESS
    if PROCESS is not None and PROCESS.poll() is None:
        try:os.killpg(PROCESS.pid,signal.SIGTERM)
        except ProcessLookupError:pass
    for name in list(CONTAINERS):
        subprocess.run(['docker','rm','-f',name],env=SAFE_ENV,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,timeout=20)
        probe=subprocess.run(['docker','ps','-aq','--filter','name=^/'+name+'$'],env=SAFE_ENV,capture_output=True,text=True,timeout=10)
        if probe.returncode or probe.stdout.strip():raise RuntimeError('Container cleanup unproven: '+name)
        CONTAINERS.remove(name)
def interrupted(signum,frame):
    cleanup();raise SystemExit(128+signum)
for signum in (signal.SIGTERM,signal.SIGINT):signal.signal(signum,interrupted)
def run(args,log,timeout=300):
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
    assert code==0,f'{log} exited {code}'
def fetch(item,path):
    with urllib.request.urlopen(item['url'],timeout=60) as response,path.open('wb') as output:shutil.copyfileobj(response,output)
    assert sha(path)==item['sha256'],str(path)
def extract(archive,destination):
    destination.mkdir(parents=True,exist_ok=True)
    with tarfile.open(archive) as tar:tar.extractall(destination,filter='data')
def docker(name,command,*,network='none',cwd='/stage',seconds=300):
    args=['docker','create','--name',name,'--label','iris.task=app-fips-isolated','--network',network,'--pid','private','--ipc','private','--cpus','2','--memory','4g','--memory-swap','4g','--pids-limit','512','--cap-drop','ALL','--security-opt','no-new-privileges','--user',f'{os.getuid()}:{os.getgid()}','--read-only','--tmpfs','/tmp:rw,nosuid,nodev,size=512m','--shm-size','512m','--init','--stop-timeout','5','--mount',f'type=bind,src={STAGE},dst=/stage','--workdir',cwd,'--env','HOME=/stage/home','--env','PLAYWRIGHT_BROWSERS_PATH=/ms-playwright','--env','PATH=/stage/tools/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin',IMAGE,*command]
    CONTAINERS.add(name)
    run(args,name+'-create.log',30)
    inspect=json.loads(subprocess.check_output(['docker','inspect',name],env=SAFE_ENV,text=True))[0]
    h=inspect['HostConfig'];assert h['NetworkMode']==network and h['PidMode']=='private' and h['Memory']==4294967296 and h['MemorySwap']==h['Memory'] and h['NanoCpus']==2000000000 and h['PidsLimit']==512
    assert h['CapDrop']==['ALL'] and h['ReadonlyRootfs'] and inspect['Config']['User']==f'{os.getuid()}:{os.getgid()}'
    (EVIDENCE/(name+'-container.json')).write_text(json.dumps({'imageId':inspect['Image'],'config':inspect['Config'],'hostConfig':h},indent=2)+'\n')
    try:run(['docker','start','--attach',name],name+'.log',seconds)
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
    for app in M['apps']:
        archive=HERE/app['archive'];assert sha(archive)==app['archiveSha256']
        target=STAGE/'apps'/app['id'];extract(archive,target)
        assert sha(target/'pnpm-lock.yaml')==app['lockSha256']
        for file,digest in app['testFiles'].items():assert sha(target/file)==digest
        dist=HERE/app['distArchive'];assert sha(dist)==app['distArchiveSha256'];extract(dist,target/app['distDir'])
        for file in app['distFiles']:assert sha(target/app['distDir']/file['path'])==file['sha256']
        (target/'isolated.playwright.config.ts').write_text("import original from './playwright.config';\nexport default {...original, use:{...original.use, launchOptions:{...original.use?.launchOptions,args:[...(original.use?.launchOptions?.args??[]),'--disable-features=WebRtcHideLocalIpsWithMdns','--allow-loopback-in-peer-connection']}}};\n")
        docker('iris-fips-install-'+app['id'],['pnpm','install','--frozen-lockfile','--ignore-scripts','--store-dir','/stage/pnpm-store'],network='bridge',cwd='/stage/apps/'+app['id'],seconds=300)
    (EVIDENCE/'prepared.json').write_text(json.dumps({'manifestSha256':sha(HERE/'manifest.json'),'image':IMAGE,'sourceInputsVerified':True,'distInputsVerified':True},indent=2)+'\n')

def test():
    assert json.loads((EVIDENCE/'prepared.json').read_text())['manifestSha256']==sha(HERE/'manifest.json')
    for app in M['apps']:
        docker('iris-fips-test-'+app['id'],['python3','/stage/procedure/run-one.py',app['id']],cwd='/stage/apps/'+app['id'],seconds=300)
    (EVIDENCE/'passed.json').write_text(json.dumps({'passed':True,'manifestSha256':sha(HERE/'manifest.json'),'apps':[a['id'] for a in M['apps']],'containersReaped':not CONTAINERS},indent=2)+'\n')
try:
    {'prepare':prepare,'test':test}[sys.argv[1]]()
finally:cleanup()
