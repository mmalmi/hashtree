"""Task-exclusive RED/GREEN TS qualification; never builds native code."""
from pathlib import Path
import hashlib,json,os,shutil,signal,subprocess,sys,tarfile,time,urllib.request
HERE=Path(__file__).resolve().parent
M=json.loads((HERE/'manifest.json').read_text())
STAGE=Path(os.environ['PUBSUB_STAGE']).resolve(); E=STAGE/'evidence'; START=time.monotonic()
ENV={k:v for k,v in os.environ.items() if k in ('PATH','HOME','USER','DOCKER_HOST','DOCKER_CONFIG','XDG_RUNTIME_DIR')}
NAMES=set(); PROCESS=None
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
  probe=subprocess.run(['docker','ps','-aq','--filter','name=^/'+name+'$'],env=ENV,capture_output=True,text=True,timeout=10)
  assert probe.returncode==0 and not probe.stdout.strip(),'Container cleanup unproven'
  NAMES.remove(name)
def interrupted(signum,frame):cleanup();raise SystemExit(128+signum)
for signum in (signal.SIGTERM,signal.SIGINT):signal.signal(signum,interrupted)
def run(args,log,seconds=300,allow_failure=False):
 global PROCESS
 assert time.monotonic()-START<2100,'Overall deadline'
 assert shutil.disk_usage(STAGE).free>M['resources']['reserveBytes'],'Hosted disk reserve'
 output=E/log;assert not output.exists(),'Preserve evidence'
 with output.open('wb') as f:
  PROCESS=subprocess.Popen(args,env=ENV,stdout=f,stderr=subprocess.STDOUT,start_new_session=True)
  try:code=PROCESS.wait(timeout=seconds)
  except subprocess.TimeoutExpired:
   os.killpg(PROCESS.pid,signal.SIGTERM)
   try:PROCESS.wait(timeout=5)
   except subprocess.TimeoutExpired:os.killpg(PROCESS.pid,signal.SIGKILL);PROCESS.wait(timeout=5)
   raise
  finally:PROCESS=None
 if not allow_failure:assert code==0,f'{log} exited {code}'
 return code
def extract(archive,target):
 target.mkdir(parents=True,exist_ok=True)
 with tarfile.open(archive) as tar:tar.extractall(target,filter='data')
def docker(name,command,network='none',cwd='/stage',seconds=300,allow_failure=False):
 assert network in ('none','bridge')
 command=['timeout','--signal=TERM','--kill-after=5s',str(seconds-10)+'s',*command]
 args=['docker','create','--name',name,'--label','iris.task=pubsub-scope-regression','--network',network,'--ipc','private','--cpus','2','--memory','4g','--memory-swap','4g','--pids-limit','512','--cap-drop','ALL','--security-opt','no-new-privileges','--user',f'{os.getuid()}:{os.getgid()}','--read-only','--tmpfs','/tmp:rw,nosuid,nodev,size=512m','--shm-size','512m','--init','--stop-timeout','5','--mount',f'type=bind,src={STAGE},dst=/stage','--workdir',cwd,'--env','HOME=/stage/home','--env','CI=true','--env','npm_config_manage_package_manager_versions=false','--env','npm_config_package_manager_strict_version=true','--env','PATH=/stage/tools/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin',M['image'],*command]
 NAMES.add(name);run(args,name+'-create.log',30)
 info=json.loads(subprocess.check_output(['docker','inspect',name],env=ENV,text=True,timeout=5))[0];h=info['HostConfig']
 assert info['Image']==M['imageId'] and h['NetworkMode']==network and h['PidMode']==''
 assert h['Memory']==4294967296 and h['MemorySwap']==h['Memory'] and h['NanoCpus']==2000000000 and h['PidsLimit']==512
 assert h['ReadonlyRootfs'] and h['CapDrop']==['ALL'] and info['Config']['User']==f'{os.getuid()}:{os.getgid()}'
 assert not any('TOKEN=' in v or 'SECRET=' in v or 'NOSTR_PUBSUB_LEGACY_MODULE=' in v for v in info['Config']['Env'])
 (E/(name+'-container.json')).write_text(json.dumps({'id':info['Id'],'imageId':info['Image'],'config':info['Config'],'hostConfig':h},indent=2)+'\n')
 try:
  run(['docker','start','--attach',name],name+'.log',seconds,True)
  result=json.loads(subprocess.check_output(['docker','inspect',name],env=ENV,text=True,timeout=5))[0]
  assert result['Id']==info['Id'] and not result['State']['Running']
  (E/(name+'-terminal.json')).write_text(json.dumps(result['State'],indent=2)+'\n')
  assert not result['State']['OOMKilled'];code=result['State']['ExitCode']
  if not allow_failure:assert code==0,f'{name} exited {code}'
  return code
 finally:cleanup()
def prepare():
 assert not STAGE.exists();assert shutil.disk_usage(STAGE.parent).free>M['resources']['entryBytes']
 STAGE.mkdir(mode=0o700);E.mkdir();(STAGE/'home').mkdir();(STAGE/'tools/bin').mkdir(parents=True)
 shutil.copytree(HERE,STAGE/'procedure',ignore=shutil.ignore_patterns('archives'))
 for name in ('red','green'):
  item=M['sources'][name];p=HERE/'archives'/item['archive'];assert sha(p)==item['archiveSha256'] and p.stat().st_size==item['archiveBytes']
  if name=='red':extract(p,STAGE/'workspace')
  else:shutil.copyfile(p,STAGE/'green-source.tar.gz')
 source=STAGE/'workspace';assert sha(source/'ts/pnpm-lock.yaml')==M['lockSha256']
 assert json.loads((source/'ts/package.json').read_text())['packageManager']=='pnpm@11.7.0'
 with urllib.request.urlopen(M['pnpm']['url'],timeout=60) as response:(STAGE/'pnpm.tgz').write_bytes(response.read(16*1024**2))
 assert sha(STAGE/'pnpm.tgz')==M['pnpm']['sha256'];extract(STAGE/'pnpm.tgz',STAGE/'tools/pnpm')
 (STAGE/'tools/bin/pnpm').write_text('#!/bin/sh\nexec node /stage/tools/pnpm/package/bin/pnpm.cjs "$@"\n');(STAGE/'tools/bin/pnpm').chmod(0o755)
 run(['docker','pull',M['image']],'image-pull.log',300)
 docker('pubsub-scope-tool-proof',['node','/stage/procedure/toolchain.cjs'],seconds=30)
 tool=json.loads((E/'toolchain.json').read_text());assert tool['node']==M['node']['version'] and tool['nodeSha256']==M['node']['sha256'] and tool['pnpm']=='11.7.0'
 docker('pubsub-scope-install',['pnpm','install','--frozen-lockfile','--ignore-scripts','--store-dir','/stage/pnpm-store'],network='bridge',cwd='/stage/workspace/ts',seconds=300)
 assert sha(source/'ts/pnpm-lock.yaml')==M['lockSha256']
 (E/'prepared.json').write_text(json.dumps({'manifestSha256':sha(HERE/'manifest.json'),'sources':{k:{q:v[q] for q in ('commit','archiveSha256','archiveBytes')} for k,v in M['sources'].items()},'toolchain':tool,'sourceLockSha256':M['lockSha256'],'installedLockSha256':sha(source/'ts/node_modules/.pnpm/lock.yaml')},indent=2)+'\n')
def test():
 assert json.loads((E/'prepared.json').read_text())['manifestSha256']==sha(HERE/'manifest.json')
 code=docker('pubsub-scope-red-green',['python3','/stage/procedure/run-gates.py'],cwd='/stage/workspace/ts',seconds=1500,allow_failure=True)
 (E/'hosted-result.json').write_text(json.dumps({'exitCode':code,'passed':code==0,'manifestSha256':sha(HERE/'manifest.json'),'containersReaped':not NAMES,'releaseAccepted':False},indent=2)+'\n');assert code==0,'RED/GREEN qualification failed'
try:{'prepare':prepare,'test':test}[sys.argv[1]]()
finally:cleanup()
