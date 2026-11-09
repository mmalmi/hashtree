"""Fresh TS package build from qualified source; no native or unit-test replay."""
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
 assert time.monotonic()-START<1200;output=E/log;assert not output.exists();started=time.monotonic();minimum=shutil.disk_usage(STAGE).free
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
 tool=M['node'];assert not rust and network in ('none','bridge')
 extra=['npm_config_manage_package_manager_versions=false','npm_config_package_manager_strict_version=true']
 path='/stage/tools/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin'
 args=['docker','create','--name',name,'--label','iris.task=pubsub-successor-package','--network',network,'--ipc','private','--cpus','2','--memory','4g','--memory-swap','4g','--pids-limit','512','--cap-drop','ALL','--security-opt','no-new-privileges','--user',f'{os.getuid()}:{os.getgid()}','--read-only','--tmpfs','/tmp:rw,nosuid,nodev,size=512m','--shm-size','512m','--init','--stop-timeout','5','--mount',f'type=bind,src={STAGE},dst=/stage','--workdir',cwd]
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
def write(name,value):(E/name).write_text(json.dumps(value,indent=2)+'\n')
def source():
 for name,row in M['source']['files'].items():
  p=STAGE/'workspace'/name
  assert p.is_file() and not p.is_symlink() and sha(p)==row['sha256'],name
  assert ('100755' if p.stat().st_mode&0o111 else '100644')==row['mode'],name
 return {'commit':M['source']['commit'],'archiveSha256':M['source']['archiveSha256'],'files':len(M['source']['files'])}
def prepare():
 assert not STAGE.exists() and shutil.disk_usage(STAGE.parent).free>=30*1024**3
 STAGE.mkdir(mode=0o700);E.mkdir()
 for p in ['home','tools/bin']:(STAGE/p).mkdir(parents=True)
 shutil.copytree(HERE,STAGE/'procedure',ignore=shutil.ignore_patterns('archives'))
 item=M['source'];archive=HERE/'archives/source.tar.gz'
 assert sha(archive)==item['archiveSha256'] and archive.stat().st_size==item['archiveBytes'];extract(archive,STAGE/'workspace')
 write('source-before.json',source())
 with urllib.request.urlopen(M['pnpm']['url'],timeout=60) as response:(STAGE/'pnpm.tgz').write_bytes(response.read(16*1024**2))
 assert sha(STAGE/'pnpm.tgz')==M['pnpm']['sha256'] and (STAGE/'pnpm.tgz').stat().st_size==M['pnpm']['bytes']
 extract(STAGE/'pnpm.tgz',STAGE/'tools/pnpm')
 (STAGE/'tools/bin/pnpm').write_text('#!/bin/sh\nexec node /stage/tools/pnpm/package/bin/pnpm.mjs "$@"\n');(STAGE/'tools/bin/pnpm').chmod(0o755)
 run(['docker','pull',M['node']['image']],'node-image-pull.log',300)
 docker('pubsub-pack-node-proof',['node','/stage/procedure/toolchain.cjs'],seconds=30)
 node=json.loads((E/'toolchain.json').read_text());assert node==M['priorGreen']['toolchain']
 docker('pubsub-pack-install',['pnpm','install','--frozen-lockfile','--ignore-scripts','--store-dir','/stage/pnpm-store'],network='bridge',cwd='/stage/workspace/ts',seconds=300)
 source();write('prepared.json',{'manifestSha256':sha(HERE/'manifest.json'),'node':node,'sourceUnchangedAfterFrozenInstall':True})
def package():
 assert json.loads((E/'prepared.json').read_text())['manifestSha256']==sha(HERE/'manifest.json');source()
 code=docker('pubsub-pack-build',['python3','/stage/procedure/run-package.py'],cwd='/stage/workspace/ts',seconds=600,allow_failure=True)
 write('hosted-result.json',{'exitCode':code,'passed':code==0,'manifestSha256':sha(HERE/'manifest.json'),'containersReaped':not NAMES,'releaseAccepted':False});assert code==0
try:{'prepare':prepare,'package':package}[sys.argv[1]]()
finally:cleanup()
