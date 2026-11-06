"""Task-only, hash-pinned browser qualification. Never builds native code."""
import concurrent.futures,hashlib,json,os,pathlib,re,shutil,signal,subprocess,sys,tarfile,time,urllib.request
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
APP_CONTAINERS={'iris-fips-test-'+name for name in ('docs','calendar','boards','git','video','drive','maps')}
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
    assert time.monotonic()-START<4200,'Overall deadline'
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
class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self,*args,**kwargs):raise RuntimeError('Unexpected redirect')
def fetch(item,path,*,exact=False):
    deadline=time.monotonic()+180;limit=item.get('bytes',256*1024**2);size=0
    opener=urllib.request.build_opener(NoRedirect()) if exact else urllib.request.build_opener()
    with opener.open(item['url'],timeout=30) as response,path.open('xb') as output:
        while True:
            assert time.monotonic()<deadline,'Fetch deadline'
            chunk=response.read(1024**2)
            if not chunk:break
            size+=len(chunk);assert size<=limit,'Download byte ceiling';output.write(chunk)
    assert sha(path)==item['sha256'],str(path)
    if 'bytes' in item:assert size==item['bytes']
def maps_assets():
    cache=STAGE/'map-cache';cache.mkdir();items=M['mapsAssets']['assets']
    assert sum(i['bytes'] for i in items)<=M['mapsAssets']['maximumBytes']<=128*1024**2
    expected={hashlib.sha256(i['pathname'].encode()).hexdigest():i for i in items}
    seen=set()
    for packed in M['mapsAssets']['archives']:
        archive=HERE/packed['archive'];assert archive.stat().st_size==packed['bytes'] and sha(archive)==packed['sha256']
        with tarfile.open(archive) as tar:
            members=tar.getmembers();assert {m.name for m in members}==set(packed['files'])
            for member in members:
                assert member.isfile() and member.name in expected and member.name not in seen
                item=expected[member.name];assert member.size==item['bytes']
                body=tar.extractfile(member).read();assert hashlib.sha256(body).hexdigest()==item['sha256']
                (cache/member.name).write_bytes(body);seen.add(member.name)
    assert seen==set(expected)
    receipt={'assets':items,'methods':[],'externalFetches':0,'receivedBytes':sum(i['bytes'] for i in items),'allByteHashesVerified':True}
    (cache/'receipt.json').write_text(json.dumps(receipt,indent=2)+'\n');(EVIDENCE/'maps-assets.json').write_text(json.dumps(receipt,indent=2)+'\n')
def extract(archive,destination):
    destination.mkdir(parents=True,exist_ok=True)
    with tarfile.open(archive) as tar:tar.extractall(destination,filter='data')
def docker(name,command,*,network='none',cwd='/stage',seconds=300,dummy=False,evidence_suffix='',allow_failure=False):
    assert not dummy or (network=='none' and name in APP_CONTAINERS|{'iris-fips-diagnostic-docs-dummy','iris-fips-diagnostic-docs-permission'})
    if dummy:command=['python3','/stage/procedure/await-start.py',name,*command]
    command=['timeout','--signal=TERM','--kill-after=5s',str(seconds-10)+'s',*command]
    args=['docker','create','--name',name,'--label','iris.task=app-fips-isolated','--network',network,'--ipc','private','--cpus','2','--memory','4g','--memory-swap','4g','--pids-limit','512','--cap-drop','ALL','--security-opt','no-new-privileges','--user',f'{os.getuid()}:{os.getgid()}','--read-only','--tmpfs','/tmp:rw,nosuid,nodev,size=512m','--shm-size','512m','--init','--stop-timeout','5','--mount',f'type=bind,src={STAGE},dst=/stage','--workdir',cwd,'--env','HOME=/stage/home','--env','PLAYWRIGHT_BROWSERS_PATH=/ms-playwright','--env','PATH=/stage/tools/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin','--env','FIPS_DUMMY='+str(int(dummy)),'--env','FIPS_EVIDENCE_SUFFIX='+evidence_suffix,'--env','FIPS_MEDIA_PERMISSION='+str(int(name=='iris-fips-diagnostic-docs-permission' or name in APP_CONTAINERS)),IMAGE,*command]
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
        assert sha(target/'package.json')==app['packageSha256']
        package=json.loads((target/'package.json').read_text());assert package['packageManager']==app['packageManager']=='pnpm@10.23.0'
        assert sha(target/app['releaseProfilePath'])==app['releaseProfileSha256']
        for file,digest in app['testFiles'].items():assert sha(target/file)==digest
        assert app['buildFromSource'] is True
        run(['python3',str(HERE/'apply-app-overlay.py'),app['id'],str(STAGE)],'overlay-'+app['id']+'.log',10)
        original=target/'playwright.config.ts';raw=original.read_text()
        assert raw.count('export default ')==1
        wrapped=raw.replace('export default ','const taskOriginalConfig = ',1)+"\nexport default {...taskOriginalConfig, workers:1, retries:0, reporter:[...(Array.isArray(taskOriginalConfig.reporter) ? taskOriginalConfig.reporter : [[taskOriginalConfig.reporter ?? 'list']]),['json']], use:{...taskOriginalConfig.use, trace:'retain-on-failure',screenshot:'only-on-failure', launchOptions:{...taskOriginalConfig.use?.launchOptions,args:[...(taskOriginalConfig.use?.launchOptions?.args??[]),'--disable-features=WebRtcHideLocalIpsWithMdns','--allow-loopback-in-peer-connection']}}};\n"
        original.write_text(wrapped)
        (EVIDENCE/('config-overlay-'+app['id']+'.json')).write_text(json.dumps({'beforeSha256':hashlib.sha256(raw.encode()).hexdigest(),'afterSha256':sha(original),'scope':'Task fixture only; unchanged selectors/assertions; bounded1worker0retry and accepted loopback RTC flags'})+'\n')
        docker('iris-fips-install-'+app['id'],['pnpm','install','--frozen-lockfile','--ignore-scripts','--store-dir','/stage/pnpm-store'],network='bridge',cwd='/stage/apps/'+app['id'],seconds=300)
    if 'maps' in SELECTED:maps_assets()
    (EVIDENCE/'prepared.json').write_text(json.dumps({'manifestSha256':sha(HERE/'manifest.json'),'image':IMAGE,'apps':SELECTED,'sourceInputsVerified':True,'buildFromExactSource':True},indent=2)+'\n')

def test():
    assert json.loads((EVIDENCE/'prepared.json').read_text())['manifestSha256']==sha(HERE/'manifest.json')
    assert set(SELECTED)<=set(json.loads((EVIDENCE/'prepared.json').read_text())['apps'])
    budgets=json.loads((HERE/'app-budgets.json').read_text()); results=[]
    for app in APPS:
        code=docker('iris-fips-test-'+app['id'],['python3','/stage/procedure/run-one.py',app['id']],cwd='/stage/apps/'+app['id'],seconds=budgets[app['id']]+90,dummy=True,allow_failure=True)
        assert not CONTAINERS,'No later app may start before cleanup'
        results.append({'app':app['id'],'exitCode':code,'passed':code==0})
    passed=all(row['passed'] for row in results)
    (EVIDENCE/'passed.json').write_text(json.dumps({'passed':passed,'manifestSha256':sha(HERE/'manifest.json'),'apps':SELECTED,'results':results,'containersReaped':not CONTAINERS},indent=2)+'\n')
    assert passed,'One or more unchanged app tests failed; see per-app evidence'
try:
    {'prepare':prepare,'test':test}[sys.argv[1]]()
finally:cleanup()
