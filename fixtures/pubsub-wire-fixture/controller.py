"""One published-baseline Rust example and one unchanged mandatory TS interoperability case."""
from pathlib import Path
import hashlib,json,os,shutil,signal,struct,subprocess,sys,tarfile,time,urllib.request
HERE=Path(__file__).resolve().parent;M=json.loads((HERE/'manifest.json').read_text())
STAGE=Path(os.environ['PUBSUB_STAGE']).resolve();E=STAGE/'evidence';START=time.monotonic();NAMES=set();PROCESS=None
ENV={k:v for k,v in os.environ.items() if k in ('PATH','HOME','USER','DOCKER_HOST','DOCKER_CONFIG','XDG_RUNTIME_DIR')}
sha=lambda p:hashlib.sha256(Path(p).read_bytes()).hexdigest()
def cleanup():
 global PROCESS
 if PROCESS is not None and PROCESS.poll() is None:
  try:os.killpg(PROCESS.pid,signal.SIGTERM)
  except ProcessLookupError:pass
  try:PROCESS.wait(timeout=5)
  except subprocess.TimeoutExpired:os.killpg(PROCESS.pid,signal.SIGKILL);PROCESS.wait(timeout=5)
 for name in list(NAMES):
  subprocess.run(['docker','rm','-f',name],env=ENV,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,timeout=20)
  p=subprocess.run(['docker','ps','-aq','--filter','name=^/'+name+'$'],env=ENV,capture_output=True,text=True,timeout=10)
  assert p.returncode==0 and not p.stdout.strip(),'Container reaping unproven';NAMES.remove(name)
def interrupted(signum,frame):cleanup();raise SystemExit(128+signum)
for sig in (signal.SIGTERM,signal.SIGINT):signal.signal(sig,interrupted)
def run(args,log,seconds=300,allow_failure=False):
 global PROCESS
 assert time.monotonic()-START<2400;output=E/log;assert not output.exists();started=time.monotonic();minimum=shutil.disk_usage(STAGE).free
 with output.open('wb') as f:
  PROCESS=subprocess.Popen(args,env=ENV,stdout=f,stderr=subprocess.STDOUT,start_new_session=True)
  try:
   while PROCESS.poll() is None:
    minimum=min(minimum,shutil.disk_usage(STAGE).free)
    assert minimum>=20*1024**3,'Hosted disk floor';assert time.monotonic()-started<seconds,'Bounded command deadline';time.sleep(1)
   code=PROCESS.wait()
  finally:
   if PROCESS.poll() is None:
    os.killpg(PROCESS.pid,signal.SIGTERM)
    try:PROCESS.wait(timeout=5)
    except subprocess.TimeoutExpired:os.killpg(PROCESS.pid,signal.SIGKILL);PROCESS.wait(timeout=5)
   PROCESS=None
 with (E/'commands.jsonl').open('a') as f:f.write(json.dumps({'log':log,'exitCode':code,'seconds':time.monotonic()-started,'minimumFreeBytes':minimum})+'\n')
 if not allow_failure:assert code==0,f'{log} exited {code}'
 return code
def docker(name,command,*,rust=False,network='none',cwd='/stage',seconds=300,allow_failure=False):
 tool=M['rust'] if rust else M['node'];assert network in ('none','bridge')
 extra=['CARGO_HOME=/stage/cargo-home','CARGO_TARGET_DIR=/stage/cargo-target','CARGO_BUILD_JOBS=1','CARGO_INCREMENTAL=0','CARGO_PROFILE_DEV_DEBUG=0','RUSTC_WRAPPER=','RUSTC_WORKSPACE_WRAPPER='] if rust else ['npm_config_manage_package_manager_versions=false','npm_config_package_manager_strict_version=true']
 path='/usr/local/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin' if rust else '/stage/tools/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin'
 args=['docker','create','--name',name,'--label','iris.task=pubsub-wire-fixture','--network',network,'--ipc','private','--cpus','2','--memory','4g','--memory-swap','4g','--pids-limit','512','--cap-drop','ALL','--security-opt','no-new-privileges','--user',f'{os.getuid()}:{os.getgid()}','--read-only','--tmpfs','/tmp:rw,nosuid,nodev,size=512m','--shm-size','512m','--init','--stop-timeout','5','--mount',f'type=bind,src={STAGE},dst=/stage','--workdir',cwd]
 for value in ['HOME=/stage/home','CI=true','PATH='+path,*extra]:args+=['--env',value]
 args += [tool['image'],'timeout','--signal=TERM','--kill-after=5s',str(seconds-10)+'s',*command]
 NAMES.add(name);run(args,name+'-create.log',30)
 info=json.loads(subprocess.check_output(['docker','inspect',name],env=ENV,text=True,timeout=5))[0];h=info['HostConfig']
 assert info['Image']==tool['imageId'] and h['NetworkMode']==network and h['PidMode']==''
 assert h['Memory']==h['MemorySwap']==4294967296 and h['NanoCpus']==2000000000 and h['PidsLimit']==512 and h['ReadonlyRootfs'] and h['CapDrop']==['ALL']
 assert info['Config']['User']==f'{os.getuid()}:{os.getgid()}' and not any('TOKEN=' in s or 'SECRET=' in s for s in info['Config']['Env'])
 (E/(name+'-container.json')).write_text(json.dumps({'id':info['Id'],'imageId':info['Image'],'config':info['Config'],'hostConfig':h},indent=2)+'\n')
 try:
  run(['docker','start','--attach',name],name+'.log',seconds,True)
  end=json.loads(subprocess.check_output(['docker','inspect',name],env=ENV,text=True,timeout=5))[0]
  assert end['Id']==info['Id'] and not end['State']['Running'] and not end['State']['OOMKilled'];(E/(name+'-terminal.json')).write_text(json.dumps(end['State'],indent=2)+'\n');code=end['State']['ExitCode']
  if not allow_failure:assert code==0,f'{name} exited {code}'
  return code
 finally:cleanup()
def extract(archive,target):
 target.mkdir(parents=True,exist_ok=True)
 with tarfile.open(archive) as tar:tar.extractall(target,filter='data')
def source(which):
 item=M['nativeSource'] if which=='native' else M['greenSource'];root=STAGE/('native-fixture' if which=='native' else 'workspace')
 for name,row in item['files'].items():
  p=root/name;assert p.is_file() and sha(p)==row['sha256'],f'{which} source changed: {name}'
  assert ('100755' if p.stat().st_mode&0o111 else '100644')==row['mode']
 return {'commit':item['commit'],'archiveSha256':item['archiveSha256'],'files':len(item['files'])}
def write(name,value):(E/name).write_text(json.dumps(value,indent=2)+'\n')
def prepare():
 assert not STAGE.exists() and shutil.disk_usage(STAGE.parent).free>=30*1024**3
 assert sha(HERE/'rust-image-config.json')==M['rust']['imageConfigSha256']
 assert sha(HERE/'rust-image-manifest.json')==M['rust']['manifestSha256']
 assert 'sha256:'+sha(HERE/'rust-image-index.json')==M['rust']['imageIndexDigest']
 imageConfig=json.loads((HERE/'rust-image-config.json').read_text());assert imageConfig['architecture']=='amd64' and imageConfig['os']=='linux' and 'RUST_VERSION=1.99.0' in imageConfig['config']['Env']
 imageManifest=json.loads((HERE/'rust-image-manifest.json').read_text());assert imageManifest['config']['digest']==M['rust']['imageId']
 assert M['rust']['image'].endswith('@sha256:'+M['rust']['manifestSha256'])
 STAGE.mkdir(mode=0o700);E.mkdir()
 for p in ['home','tools/bin','cargo-home','cargo-target']: (STAGE/p).mkdir(parents=True)
 shutil.copytree(HERE,STAGE/'procedure',ignore=shutil.ignore_patterns('archives'))
 for key,target in [('greenSource','workspace'),('nativeSource','native-fixture')]:
  item=M[key];archive=HERE/'archives'/item['archive'];assert sha(archive)==item['archiveSha256'] and archive.stat().st_size==item['archiveBytes'];extract(archive,STAGE/target)
 write('source-before.json',{'native':source('native'),'green':source('green')})
 with urllib.request.urlopen(M['pnpm']['url'],timeout=60) as response:(STAGE/'pnpm.tgz').write_bytes(response.read(16*1024**2))
 assert sha(STAGE/'pnpm.tgz')==M['pnpm']['sha256'];extract(STAGE/'pnpm.tgz',STAGE/'tools/pnpm')
 (STAGE/'tools/bin/pnpm').write_text('#!/bin/sh\nexec node /stage/tools/pnpm/package/bin/pnpm.mjs "$@"\n');(STAGE/'tools/bin/pnpm').chmod(0o755)
 for label in ['node','rust']:run(['docker','pull',M[label]['image']],label+'-image-pull.log',300)
 docker('pubsub-wire-node-proof',['node','/stage/procedure/toolchain.cjs'],seconds=30)
 node=json.loads((E/'toolchain.json').read_text());assert node==M['priorGreen']['toolchain']
 docker('pubsub-wire-rust-proof',['sh','/stage/procedure/rust-proof.sh'],rust=True,seconds=30)
 assert (E/'rustc-version.txt').read_text()==M['rust']['rustcVersion'];assert (E/'cargo-version.txt').read_text().strip()==M['rust']['cargoVersion']
 docker('pubsub-wire-install',['pnpm','install','--frozen-lockfile','--ignore-scripts','--store-dir','/stage/pnpm-store'],network='bridge',cwd='/stage/workspace/ts',seconds=300)
 docker('pubsub-wire-fetch',['cargo','fetch','--locked','--target','x86_64-unknown-linux-gnu'],rust=True,network='bridge',cwd='/stage/native-fixture',seconds=600)
 source('native');source('green')
 write('prepared.json',{'manifestSha256':sha(HERE/'manifest.json'),'cargoLockSha256':sha(STAGE/'native-fixture/Cargo.lock'),'node':node,'rustImage':M['rust']['image'],'rustImageId':M['rust']['imageId'],'sourceUnchangedAfterLockedFetch':True})
def build():
 assert json.loads((E/'prepared.json').read_text())['manifestSha256']==sha(HERE/'manifest.json');source('native')
 code=docker('pubsub-wire-build',['sh','/stage/procedure/build-fixture.sh'],rust=True,cwd='/stage/native-fixture',seconds=1200,allow_failure=True)
 write('native-build-result.json',{'exitCode':code,'containersReaped':not NAMES,'cargoLockUnchanged':sha(STAGE/'native-fixture/Cargo.lock')==M['nativeSource']['files']['Cargo.lock']['sha256']});assert code==0
 source('native')
 records=[json.loads(line) for line in (E/'cargo-build.jsonl').read_text().splitlines() if line.strip()];artifacts=[r for r in records if r.get('reason')=='compiler-artifact'];found=[r for r in artifacts if r['target']['name']=='fips-pubsub-wire-stdio' and r.get('executable')];assert len(found)==1 and records[-1]=={'reason':'build-finished','success':True}
 artifact=found[0];absolute=Path(artifact['executable']);relative=absolute.relative_to('/stage');binary=STAGE/relative;assert relative.parts[0]=='cargo-target' and binary.is_file();data=binary.read_bytes()
 assert data[:6]==b'\x7fELF\x02\x01';machine=struct.unpack_from('<H',data,18)[0];assert machine==62
 offset=struct.unpack_from('<Q',data,32)[0];size,count=struct.unpack_from('<HH',data,54);interpreters=[]
 for n in range(count):
  start=offset+n*size
  if struct.unpack_from('<I',data,start)[0]==3:
   begin=struct.unpack_from('<Q',data,start+8)[0];length=struct.unpack_from('<Q',data,start+32)[0];interpreters.append(data[begin:begin+length].rstrip(b'\0').decode())
 assert interpreters==['/lib64/ld-linux-x86-64.so.2']
 write('compiled-artifacts.json',[{k:r[k] for k in ['package_id','target','profile','features','filenames','executable']} for r in artifacts])
 write('wire-binary.json',{'path':str(absolute),'sha256':sha(binary),'bytes':len(data),'architecture':'ELF64 little-endian x86_64','loader':interpreters[0],'source':M['nativeSource']['commit'],'sourceArchiveSha256':M['nativeSource']['archiveSha256'],'cargoLockSha256':sha(STAGE/'native-fixture/Cargo.lock'),'rustImage':M['rust']['image'],'artifact':artifact,'buildCommand':M['fixtureBuildCommand'],'defaultFeatures':True,'explicitFeatures':[],'nativeScope':'one original example; no native tests or consumers'})
 shutil.copyfile(binary,E/'fips-pubsub-wire-stdio');(E/'fips-pubsub-wire-stdio').chmod(binary.stat().st_mode&0o777)
 write('native-source-after.json',source('native'))
def test():
 assert json.loads((E/'prepared.json').read_text())['manifestSha256']==sha(HERE/'manifest.json');source('native');source('green')
 shutil.copyfile(HERE/'cargo-launcher.py',STAGE/'tools/bin/cargo');(STAGE/'tools/bin/cargo').chmod(0o755)
 code=docker('pubsub-wire-test',['python3','/stage/procedure/run-wire.py'],cwd='/stage/workspace/ts',seconds=150,allow_failure=True)
 source('native');source('green');write('hosted-result.json',{'passed':code==0,'exitCode':code,'manifestSha256':sha(HERE/'manifest.json'),'containersReaped':not NAMES,'sourceUnchangedAfterCase':True,'releaseAccepted':False});assert code==0
try:{'prepare':prepare,'build':build,'test':test}[sys.argv[1]]()
finally:cleanup()
