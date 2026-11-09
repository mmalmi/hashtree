"""Compile and run one reviewed mixed-file fixture. No library or app publication."""
from pathlib import Path
import hashlib,json,os,shutil,signal,struct,subprocess,sys,tarfile,time,tomllib,urllib.request
from system_setup import prepare_system
HERE=Path(__file__).resolve().parent;M=json.loads((HERE/'manifest.json').read_text())
STAGE=Path(os.environ['MIXED_FILE_STAGE']).resolve();E=STAGE/'evidence';NAMES=set();PROCESS=None
ENV={k:v for k,v in os.environ.items() if k in ('PATH','HOME','USER','DOCKER_HOST','DOCKER_CONFIG','XDG_RUNTIME_DIR')}
sha=lambda p:hashlib.sha256(Path(p).read_bytes()).hexdigest()
def write(name,value):
 target=E/name;assert not target.exists();target.write_text(json.dumps(value,indent=2)+'\n')
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
  assert probe.returncode==0 and not probe.stdout.strip(),'Owned container reaping unproven';NAMES.remove(name)
def interrupted(signum,frame):cleanup();raise SystemExit(128+signum)
for sig in (signal.SIGTERM,signal.SIGINT):signal.signal(sig,interrupted)
def run(args,log,seconds=300,allow_failure=False):
 global PROCESS
 target=E/log;assert not target.exists();started=time.monotonic();minimum=shutil.disk_usage(STAGE).free;code=None;error=None
 assert minimum>=20*1024**3,'Hosted disk floor before command'
 try:
  with target.open('wb') as output:
   PROCESS=subprocess.Popen(args,env=ENV,stdout=output,stderr=subprocess.STDOUT,start_new_session=True)
   while PROCESS.poll() is None:
    minimum=min(minimum,shutil.disk_usage(STAGE).free)
    assert minimum>=20*1024**3,'Hosted disk floor'
    assert time.monotonic()-started<seconds,'Bounded command deadline';time.sleep(.5)
   code=PROCESS.wait()
   minimum=min(minimum,shutil.disk_usage(STAGE).free);assert minimum>=20*1024**3,'Hosted disk floor after command'
 except BaseException as exc:error=repr(exc);raise
 finally:
  if PROCESS is not None and PROCESS.poll() is None:
   os.killpg(PROCESS.pid,signal.SIGTERM)
   try:PROCESS.wait(timeout=5)
   except subprocess.TimeoutExpired:os.killpg(PROCESS.pid,signal.SIGKILL);PROCESS.wait(timeout=5)
  if PROCESS is not None:code=PROCESS.returncode
  PROCESS=None
  with (E/'commands.jsonl').open('a') as output:output.write(json.dumps({'log':log,'exitCode':code,'error':error,'seconds':time.monotonic()-started,'minimumFreeBytes':minimum})+'\n')
 if not allow_failure:assert code==0,f'{log} exited {code}'
 return code
def docker(name,command,*,rust=False,network='none',cwd='/stage',seconds=300,allow_failure=False):
 assert network in ('none','bridge');tool=M['rust'] if rust else M['node']
 if rust and (E/'system-image.json').exists():
  image=json.loads((E/'system-image.json').read_text());assert image['baseImageId']==M['rust']['imageId'] and image['closureSha256']==M['system']['closureSha256']
  tool={'image':image['imageId'],'imageId':image['imageId']}
 path='/stage/tools/bin:/usr/local/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin'
 extra=['CARGO_HOME=/stage/cargo-home','CARGO_TARGET_DIR=/stage/cargo-target','CARGO_BUILD_JOBS=1','CARGO_INCREMENTAL=0','CARGO_PROFILE_DEV_DEBUG=0','RUSTC_WRAPPER=','RUSTC_WORKSPACE_WRAPPER=','LIBCLANG_PATH=/usr/lib/llvm-14/lib'] if rust else []
 args=['docker','create','--name',name,'--label','iris.task=mixed-file-interop','--network',network,'--ipc','private','--cpus','2','--memory','4g','--memory-swap','4g','--pids-limit','512','--cap-drop','ALL','--security-opt','no-new-privileges','--user',f'{os.getuid()}:{os.getgid()}','--read-only','--tmpfs','/tmp:rw,nosuid,nodev,size=512m','--shm-size','512m','--init','--stop-timeout','5','--mount',f'type=bind,src={STAGE},dst=/stage','--workdir',cwd]
 for value in ['HOME=/stage/home','CI=true','PATH='+path,'NODE_OPTIONS=','npm_config_manage_package_manager_versions=false','npm_config_package_manager_strict_version=true',*extra]:args+=['--env',value]
 args += [tool['image'],'timeout','--signal=TERM','--kill-after=5s',str(seconds-10)+'s',*command]
 NAMES.add(name);run(args,name+'-create.log',30)
 info=json.loads(subprocess.check_output(['docker','inspect',name],env=ENV,text=True,timeout=5))[0];h=info['HostConfig']
 assert info['Image']==tool['imageId'] and h['NetworkMode']==network and h['PidMode']==''
 assert h['Memory']==h['MemorySwap']==4294967296 and h['NanoCpus']==2000000000 and h['PidsLimit']==512 and h['ReadonlyRootfs'] and h['CapDrop']==['ALL']
 assert info['Config']['User']==f'{os.getuid()}:{os.getgid()}' and os.getuid()!=0
 assert not any('TOKEN=' in value or 'SECRET=' in value for value in info['Config']['Env'])
 write(name+'-container.json',{'id':info['Id'],'imageId':info['Image'],'config':info['Config'],'hostConfig':h})
 try:
  try:run(['docker','start','--attach',name],name+'.log',seconds,True)
  except BaseException:
   subprocess.run(['docker','stop','--time','5',name],env=ENV,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,timeout=15)
   raise
  finally:
   end=json.loads(subprocess.check_output(['docker','inspect',name],env=ENV,text=True,timeout=5))[0]
   assert end['Id']==info['Id'];write(name+'-terminal.json',end['State'])
  assert not end['State']['Running'] and not end['State']['OOMKilled'];code=end['State']['ExitCode']
  if not allow_failure:assert code==0,f'{name} exited {code}'
  return code
 finally:cleanup()
def inputs():
 for path,row in M['fixtureFiles'].items():
  file=STAGE/'fixture'/path;assert file.is_file() and not file.is_symlink() and sha(file)==row['sha256'] and file.stat().st_size==row['bytes'],path
 assert sha(STAGE/'fixture/package.json')==M['nodeClosure']['packageSha256']
 assert sha(STAGE/'fixture/pnpm-lock.yaml')==M['nodeClosure']['lockSha256']
 assert sha(STAGE/'fixture/vendor/fips-file-transfer-0.1.1.tgz')==M['sdk']['archiveSha256']
 return {'source':M['sdk']['source'],'fixtureFiles':len(M['fixtureFiles']),'fixtureFileHashesUnchanged':True,'cargoLockSha256':sha(STAGE/'fixture/rust/Cargo.lock'),'nodeLockSha256':sha(STAGE/'fixture/pnpm-lock.yaml')}
def crate_proof():
 lock=tomllib.loads((STAGE/'fixture/rust/Cargo.lock').read_text());wanted={p['name']+'-'+p['version']+'.crate':p['checksum'] for p in lock['package'] if 'checksum' in p}
 seen=[]
 for file in sorted((STAGE/'cargo-home/registry/cache').glob('*/*.crate')):
  assert file.name in wanted and sha(file)==wanted[file.name],file.name;seen.append({'name':file.name,'sha256':sha(file),'bytes':file.stat().st_size})
 transfer=next(row for row in seen if row['name']=='nvpn-fips-file-transfer-0.1.5.crate');assert transfer['sha256']==M['sdk']['rustCrateSha256']
 return {'passed':True,'packages':seen,'publishedFileTransferSha256':transfer['sha256'],'cargoLockSha256':sha(STAGE/'fixture/rust/Cargo.lock')}
def prepare():
 assert not STAGE.exists() and shutil.disk_usage(STAGE.parent).free>=30*1024**3
 assert sha(HERE/'rust-image-config.json')==M['rust']['imageConfigSha256'] and sha(HERE/'rust-image-manifest.json')==M['rust']['manifestSha256']
 assert 'sha256:'+sha(HERE/'rust-image-index.json')==M['rust']['imageIndexDigest']
 assert json.loads((HERE/'rust-image-manifest.json').read_text())['config']['digest']==M['rust']['imageId']
 assert M['rust']['image'].endswith('@sha256:'+M['rust']['manifestSha256'])
 assert sha(HERE/'sdk-root-proof.json')==M['sdk']['projectedRootProofSha256']
 proof=json.loads((HERE/'sdk-root-proof.json').read_text());assert proof['passed']==27 and proof['rootAccepted'] and proof['source']==M['sdk']['source'] and proof['rootProofSha256']==M['sdk']['rootProofSha256']
 STAGE.mkdir(mode=0o700);E.mkdir()
 for name in ['home','tools/bin','cargo-home','cargo-target']:(STAGE/name).mkdir(parents=True)
 shutil.copytree(HERE,STAGE/'procedure',ignore=shutil.ignore_patterns('fixture','vendor'))
 shutil.copytree(HERE/'fixture',STAGE/'fixture')
 for name in ['package.json','pnpm-lock.yaml']:shutil.copyfile(HERE/name,STAGE/'fixture'/name)
 shutil.copytree(HERE/'vendor',STAGE/'fixture/vendor');write('inputs-before.json',inputs())
 with urllib.request.urlopen(M['pnpm']['url'],timeout=60) as response:data=response.read(16*1024**2)
 assert hashlib.sha256(data).hexdigest()==M['pnpm']['sha256'];(STAGE/'pnpm.tgz').write_bytes(data)
 with tarfile.open(STAGE/'pnpm.tgz') as tar:tar.extractall(STAGE/'tools/pnpm',filter='data')
 (STAGE/'tools/bin/pnpm').write_text('#!/bin/sh\nexec /stage/tools/bin/node /stage/tools/pnpm/package/bin/pnpm.cjs "$@"\n');(STAGE/'tools/bin/pnpm').chmod(0o755)
 for label in ['node','rust']:run(['docker','pull',M[label]['image']],label+'-image-pull.log',300)
 docker('mixed-node-extract',['node','/stage/procedure/node-proof.mjs','extract'],seconds=30)
 docker('mixed-rust-base-proof',['sh','/stage/procedure/rust-proof.sh','base'],rust=True,seconds=30)
 assert (E/'base-rustc-version.txt').read_text()==M['rust']['rustcVersion'] and (E/'base-cargo-version.txt').read_text().strip()==M['rust']['cargoVersion']
 prepare_system(HERE,STAGE,E,M,run,docker,cleanup,ENV,NAMES)
 docker('mixed-rust-derived-proof',['sh','/stage/procedure/rust-proof.sh','derived'],rust=True,seconds=30)
 for name in ['rustc-version.txt','cargo-version.txt','rust-tool-binary-sha256.txt','cc-version.txt','ld-version.txt','cc-ld-binary-sha256.txt']:
  assert (E/('base-'+name)).read_bytes()==(E/('derived-'+name)).read_bytes(),name
 docker('mixed-node-bookworm-proof',['/stage/tools/bin/node','/stage/procedure/node-proof.mjs','derived'],rust=True,seconds=30)
 docker('mixed-node-install',['pnpm','install','--frozen-lockfile','--ignore-scripts','--store-dir','/stage/pnpm-store'],rust=True,network='bridge',cwd='/stage/fixture',seconds=300)
 docker('mixed-rust-fetch',['cargo','fetch','--locked','--target','x86_64-unknown-linux-gnu'],rust=True,network='bridge',cwd='/stage/fixture/rust',seconds=600)
 write('crate-inputs.json',crate_proof());write('prepared.json',{'passed':True,'manifestSha256':sha(HERE/'manifest.json'),'inputs':inputs(),'systemImage':json.loads((E/'system-image.json').read_text())['imageId'],'rootSdkProofSha256':M['sdk']['rootProofSha256'],'nodeBinarySha256':M['node']['binarySha256']})
def build():
 assert json.loads((E/'prepared.json').read_text())['manifestSha256']==sha(HERE/'manifest.json');inputs();crate_proof()
 code=docker('mixed-file-build',['python3','/stage/procedure/phase.py','compile','sh','/stage/procedure/build-fixture.sh'],rust=True,cwd='/stage/fixture/rust',seconds=1200,allow_failure=True)
 write('build-result.json',{'exitCode':code,'containersReaped':not NAMES,'inputs':inputs()});assert code==0
 resources=json.loads((E/'compile-resources.json').read_text());assert resources['noChildOom'] and not resources['samplerErrors']
 metadata=json.loads((E/'cargo-metadata.json').read_text());native=[p for p in metadata['packages'] if p['name']=='nvpn-fips-file-transfer'];assert len(native)==1 and native[0]['version']=='0.1.5' and native[0]['source']=='registry+https://github.com/rust-lang/crates.io-index'
 roots=[p for p in metadata['packages'] if p['id'] in metadata['workspace_members']]
 assert len(roots)==1 and roots[0]['name']=='runtime61-mixed-file-fixture' and roots[0]['version']=='0.0.0' and roots[0]['source'] is None and roots[0]['manifest_path']=='/stage/fixture/rust/Cargo.toml'
 rows=[json.loads(line) for line in (E/'cargo-build.jsonl').read_text().splitlines() if line];assert rows[-1]=={'reason':'build-finished','success':True}
 artifacts=[r for r in rows if r.get('reason')=='compiler-artifact'];found=[r for r in artifacts if r['target']['name']=='runtime61-mixed-file-fixture' and r.get('executable')];assert len(found)==1
 record=found[0];relative=Path(record['executable']).relative_to('/stage');assert relative.parts[0]=='cargo-target';binary=STAGE/relative;data=binary.read_bytes()
 assert data[:6]==b'\x7fELF\x02\x01' and struct.unpack_from('<H',data,18)[0]==62
 write('compiled-artifacts.json',[{k:r[k] for k in ['package_id','target','profile','features','filenames','executable']} for r in artifacts])
 write('file-binary.json',{'path':record['executable'],'sha256':sha(binary),'bytes':len(data),'source':M['rustSource'],'crateSha256':M['sdk']['rustCrateSha256'],'cargoLockSha256':sha(STAGE/'fixture/rust/Cargo.lock'),'systemImage':json.loads((E/'system-image.json').read_text())['imageId'],'fixtureManifestSha256':sha(HERE/'manifest.json'),'artifact':record,'nativeScope':'Actual published transfer crate linked into one private fixture; no native consumer rebuild'})
 shutil.copyfile(binary,E/'mixed-file-fixture');(E/'mixed-file-fixture').chmod(binary.stat().st_mode&0o777)
 write('build-storage.json',{'cargoHomeBytes':sum(p.stat().st_size for p in (STAGE/'cargo-home').rglob('*') if p.is_file()),'cargoTargetBytes':sum(p.stat().st_size for p in (STAGE/'cargo-target').rglob('*') if p.is_file()),'pnpmStoreBytes':sum(p.stat().st_size for p in (STAGE/'pnpm-store').rglob('*') if p.is_file())})
def test():
 assert json.loads((E/'prepared.json').read_text())['manifestSha256']==sha(HERE/'manifest.json');inputs()
 docker('mixed-file-seal',['/stage/tools/bin/node','/stage/procedure/installed-inputs.mjs'],rust=True,seconds=30)
 code=docker('mixed-file-runtime',['python3','/stage/procedure/phase.py','runtime','python3','/stage/procedure/runtime.py'],rust=True,cwd='/stage/fixture',seconds=600,allow_failure=True)
 write('runtime-result.json',{'exitCode':code,'containersReaped':not NAMES,'inputs':inputs()});assert code==0
 docker('mixed-file-installed-after',['/stage/tools/bin/node','/stage/procedure/installed-inputs.mjs','after'],rust=True,seconds=30)
 assert json.loads((E/'runtime-resources.json').read_text())['noChildOom']
 write('hosted-result.json',{'passed':True,'manifestSha256':sha(HERE/'manifest.json'),'source':M['sdk']['source'],'sdkPackageSha256':M['sdk']['archiveSha256'],'rootSdkProofSha256':M['sdk']['rootProofSha256'],'containersReaped':not NAMES,'fourMixedFileScenarios':True,'nativeSourceUnchanged':True,'releaseAccepted':False,'publicationPerformed':False})
try:{'prepare':prepare,'build':build,'test':test}[sys.argv[1]]()
finally:cleanup()
