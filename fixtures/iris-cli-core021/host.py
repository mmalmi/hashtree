"""Bounded native tests in the accepted nonroot network/PID namespace pattern."""
import hashlib,json,os,pathlib,signal,subprocess,sys,time,shutil,tarfile
HERE=pathlib.Path(__file__).resolve().parent

def save(path,value):path.write_text(json.dumps(value,indent=2)+'\n')
def sha(path):return hashlib.sha256(path.read_bytes()).hexdigest()
def inner(config):
    stage=pathlib.Path(config['stage']);evidence=stage/'evidence';env=config['env'];manifest=json.loads((HERE/'manifest.json').read_text())
    subprocess.run([sys.executable,str(HERE/'network-proof.py'),str(evidence/'network-before.json')],check=True,timeout=15)
    runtime={'source':manifest['sourceCommit'],'recipe':env['GITHUB_SHA'],'commands':[],'passed':False}
    try:
        for tool in ['rustc','cargo']:
            runtime[tool]=subprocess.check_output([tool,'--version'],text=True).strip();assert runtime[tool]==manifest[tool]
            binary=pathlib.Path(subprocess.check_output(['rustup','which',tool],text=True).strip());runtime[tool+'Sha256']=sha(binary)
        commands=[
            ('negative',['cargo','test','--locked','--offline','--test','default_bootstrap','pinned_default_preserves_follow_and_mute_revisions','--','--nocapture'],'negative'),
            ('fmt',['cargo','fmt','--all','--','--check'],'source'),
            ('clippy',['cargo','clippy','--locked','--offline','--all-targets','--','-D','warnings'],'source'),
            ('tests',['cargo','test','--locked','--offline'],'source'),
            ('package',['cargo','package','--locked','--offline'],'source'),
            ('package-list',['cargo','package','--locked','--offline','--list'],'source'),
        ]
        for name,command,directory in commands:
            log=evidence/(name+'.log');start=time.monotonic()
            with log.open('xb') as output:result=subprocess.run(command,cwd=stage/directory,stdout=output,stderr=subprocess.STDOUT,timeout=1500)
            text=log.read_text(errors='replace');runtime['commands'].append({'name':name,'argv':command,'cwd':directory,'exitCode':result.returncode,'seconds':time.monotonic()-start,'logSha256':sha(log)})
            print(text[-4000:],flush=True)
            if name=='negative':
                assert result.returncode!=0 and 'test result: FAILED. 0 passed; 1 failed;' in text,'Negative control must fail assertions, not compilation'
                assert 'pinned_default_preserves_follow_and_mute_revisions' in text and 'snapshot SHA-256 mismatch' in text
            else:assert result.returncode==0,name+' failed'
        packages=list((stage/'target/package').glob('iris-nostr-*.crate'));assert len(packages)==1
        runtime['cargoPackage']={'name':packages[0].name,'bytes':packages[0].stat().st_size,'sha256':sha(packages[0])}
        with tarfile.open(packages[0]) as tar:
            names=[pathlib.PurePosixPath(v.name).parts[1:] for v in tar.getmembers() if v.isfile()]
            assert all(parts and '..' not in parts and (parts[0] in ['src','tests'] or '/'.join(parts) in ['Cargo.toml','Cargo.toml.orig','Cargo.lock','README.md','LICENSE','rust-toolchain.toml']) for parts in names)
            assert ('tests','graph_aliases.rs') in names and ('tests','default_bootstrap.rs') in names and ('tests','data','default-bootstrap-v4.bin') in names
            runtime['cargoPackage']['files']=['/'.join(parts) for parts in names]
        runtime['passed']=True
    except BaseException as error:
        runtime['error']=repr(error);raise
    finally:
        subprocess.run([sys.executable,str(HERE/'network-proof.py'),str(evidence/'network-after.json')],check=True,timeout=15)
        save(evidence/'runtime.json',runtime)

def run():
    assert os.getuid()!=0
    stage=pathlib.Path(os.environ['IRIS_CLI_STAGE']).resolve();evidence=stage/'evidence'
    env=dict(PATH=os.environ['PATH'],HOME=str(stage/'home'),TMPDIR=str(stage/'tmp'),CI='true',LANG='C.UTF-8',IRIS_CLI_STAGE=str(stage),CARGO_HOME=str(stage/'cargo'),CARGO_TARGET_DIR=str(stage/'target'),RUSTUP_HOME=os.environ.get('RUSTUP_HOME',str(pathlib.Path.home()/'.rustup')),RUSTUP_TOOLCHAIN='1.99.0',CARGO_NET_OFFLINE='true',CARGO_BUILD_JOBS='1',GRAPH_HOST_NETNS=os.readlink('/proc/self/ns/net'),GRAPH_HOST_PIDNS=os.readlink('/proc/self/ns/pid'),GRAPH_HOST_MNTNS=os.readlink('/proc/self/ns/mnt'),GITHUB_SHA=os.environ['GITHUB_SHA'])
    config={'stage':str(stage),'uid':os.getuid(),'gid':os.getgid(),'env':env};path=evidence/'config.json';save(path,config)
    command=['sudo','-n','/usr/bin/timeout','--signal=TERM','--kill-after=10s','2100s','/usr/bin/unshare','--mount','--net','--pid','--fork','--mount-proc','--propagation','private','--kill-child=KILL','/usr/bin/python3',str(HERE/'host.py'),'root',str(path)]
    process=subprocess.Popen(command,start_new_session=True);start=time.monotonic();reason=None;peak_rss=0;peak_disk=0;last_disk=0
    def stop(signum=None,frame=None):
        if process.poll() is None:
            os.killpg(process.pid,signal.SIGTERM)
            try:process.wait(timeout=10)
            except subprocess.TimeoutExpired:os.killpg(process.pid,signal.SIGKILL);process.wait(timeout=5)
    for sig in [signal.SIGTERM,signal.SIGINT]:signal.signal(sig,lambda s,f:(stop(),sys.exit(128+s)))
    try:
        while process.poll() is None:
            pids={process.pid};stats={}
            for entry in pathlib.Path('/proc').iterdir():
                if not entry.name.isdigit():continue
                try:
                    parts=(entry/'stat').read_text().rsplit(')',1)[1].split();stats[int(entry.name)]=(int(parts[1]),int(parts[21])*os.sysconf('SC_PAGE_SIZE'))
                except (OSError,ValueError,IndexError):pass
            changed=True
            while changed:
                added={pid for pid,(parent,_) in stats.items() if parent in pids}-pids;changed=bool(added);pids|=added
            rss=sum(stats.get(pid,(0,0))[1] for pid in pids);peak_rss=max(peak_rss,rss)
            if rss>5*1024**3:reason='Own process-tree RSS exceeded 5 GiB'
            if shutil.disk_usage(stage).free<8*1024**3:reason='Hosted disk reserve below 8 GiB'
            if time.monotonic()-last_disk>=10:
                size=0
                for p in stage.rglob('*'):
                    try:
                        if p.is_file():size+=p.stat().st_blocks*512
                    except FileNotFoundError:pass
                peak_disk=max(peak_disk,size);last_disk=time.monotonic()
                if size>8*1024**3:reason='Task stage growth exceeded 8 GiB'
            if time.monotonic()-start>2120:reason='Outer qualification deadline'
            if reason:stop();break
            time.sleep(2)
        code=process.wait(timeout=5)
    finally:
        stop();save(evidence/'exit.json',{'exitCode':process.returncode,'abortReason':reason,'peakOwnRssBytes':peak_rss,'peakStageBytes':peak_disk,'seconds':time.monotonic()-start,'controllerProcessReaped':process.poll() is not None,'namespaceClosure':'unshare kill-child and PID-init teardown'})
    assert not reason and code==0,'Isolated gate failed'

if __name__=='__main__':
    if sys.argv[1]=='run':run()
    elif sys.argv[1]=='root':
        c=json.loads(pathlib.Path(sys.argv[2]).read_text());assert os.getuid()==0
        for namespace in ['net','pid','mnt']:assert os.readlink('/proc/self/ns/'+namespace)!=c['env']['GRAPH_HOST_'+namespace.upper()+'NS']
        links=json.loads(subprocess.check_output(['/usr/sbin/ip','-j','link','show'],text=True));assert [v['ifname'] for v in links]==['lo']
        subprocess.run(['/usr/bin/mount','-t','sysfs','-o','ro,nosuid,nodev,noexec','sysfs','/sys'],check=True)
        subprocess.run(['/usr/sbin/ip','link','set','lo','up'],check=True)
        os.execve('/usr/bin/setpriv',['setpriv','--reuid='+str(c['uid']),'--regid='+str(c['gid']),'--clear-groups','--bounding-set=-all','--inh-caps=-all','--ambient-caps=-all','--no-new-privs','/usr/bin/python3',str(HERE/'host.py'),'inner',sys.argv[2]],c['env'])
    elif sys.argv[1]=='inner':inner(json.loads(pathlib.Path(sys.argv[2]).read_text()))
    else:raise ValueError('Unsupported mode')
