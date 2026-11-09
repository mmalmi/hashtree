"""Build from empty task-owned dist, prove package/source mapping; reuse accepted tests."""
from pathlib import Path, PurePosixPath
import base64,hashlib,io,json,os,shutil,socket,subprocess,tarfile,time
ROOT=Path('/stage/workspace');WORK=ROOT/'ts';PKG=WORK/'packages/nostr-pubsub';E=Path('/stage/evidence');START=time.monotonic()
M=json.loads(Path('/stage/procedure/manifest.json').read_text());FILES=M['source']['files'];DIST_PREFIX='ts/packages/nostr-pubsub/dist/'
sha=lambda p:hashlib.sha256(Path(p).read_bytes()).hexdigest()
steps=[]
def write(name,value):(E/name).write_text(json.dumps(value,indent=2)+'\n')
def source(allow_dist=False):
 for name,row in FILES.items():
  if allow_dist and name.startswith(DIST_PREFIX):continue
  p=ROOT/name;assert p.is_file() and not p.is_symlink() and sha(p)==row['sha256'],name
  assert ('100755' if p.stat().st_mode&0o111 else '100644')==row['mode'],name
 assert sha(WORK/'pnpm-lock.yaml')==sha(WORK/'node_modules/.pnpm/lock.yaml')==M['lockSha256']
def run(args,label,seconds=180):
 assert time.monotonic()-START<550 and shutil.disk_usage(ROOT).free>=20*1024**3
 log=E/(label+'.log');assert not log.exists();started=time.monotonic()
 with log.open('wb') as f:result=subprocess.run(args,cwd=WORK,stdout=f,stderr=subprocess.STDOUT,timeout=seconds)
 row={'name':label,'args':args,'exitCode':result.returncode,'elapsedSeconds':time.monotonic()-started,'logSha256':sha(log)};steps.append(row);write('steps.json',steps)
 assert result.returncode==0,f'{label} exited {result.returncode}'
def network_proof(label):
 assert sha('/stage/procedure/network-proof.py')==M['networkProofSha256']
 run(['python3','/stage/procedure/network-proof.py',str(E/('network-'+label+'.json'))],'network-'+label,30)
def memory_proof(label):
 path=Path('/sys/fs/cgroup/memory.events');events={k:int(v) for k,v in (line.split() for line in path.read_text().splitlines())}
 values={n:Path('/sys/fs/cgroup/'+n).read_text().strip() for n in ['memory.current','memory.peak','memory.max']}
 write('memory-'+label+'.json',{'scope':'whole package container and descendants','cgroup':Path('/proc/self/cgroup').read_text(),'events':events,'values':values})
 assert events['oom_kill']==0 and events['oom']==0,'Package container cgroup memory OOM counter'
def package():
 source();network_proof('before')
 installed={}
 for name,row in M['priorGreen']['installed']['packages'].items():
  p=PKG/'node_modules'/name/'package.json';j=json.loads(p.read_text());assert j['name']==name
  installed[name]={'version':j['version'],'manifestSha256':sha(p)};assert installed[name]==row,name
 assert Path('/etc/os-release').read_text()==M['priorGreen']['installed']['osRelease']
 write('installed-inputs.json',{'packages':installed,'sourceLockSha256':sha(WORK/'pnpm-lock.yaml'),'installedLockSha256':sha(WORK/'node_modules/.pnpm/lock.yaml'),'osRelease':Path('/etc/os-release').read_text()})
 expected_dist={n.removeprefix(DIST_PREFIX) for n in FILES if n.startswith(DIST_PREFIX)};dist=PKG/'dist'
 assert len(expected_dist)==232 and {p.relative_to(dist).as_posix() for p in dist.rglob('*') if p.is_file()}==expected_dist
 # Only the sealed archive's generated files are removed; no shared checkout/cache exists here.
 for name in sorted(expected_dist):(dist/name).unlink()
 assert not list(dist.iterdir());dist.rmdir();assert not dist.exists()
 write('fresh-output-start.json',{'trackedGeneratedInputs':232,'distAbsentBeforeBuild':True,'removedOnlyExtractedGeneratedFiles':True})
 run(['pnpm','check'],'package-check');run(['pnpm','build'],'package-build');source(allow_dist=True)
 assert not any(p.is_symlink() for p in dist.rglob('*'))
 built={p.relative_to(dist).as_posix():{'bytes':p.stat().st_size,'sha256':sha(p),'mode':'100755' if p.stat().st_mode&0o111 else '100644'} for p in sorted(dist.rglob('*')) if p.is_file()}
 assert set(built)==expected_dist and all(r['mode']=='100644' for r in built.values())
 changed={n:{'beforeSha256':FILES[DIST_PREFIX+n]['sha256'],'after':r} for n,r in built.items() if r['sha256']!=FILES[DIST_PREFIX+n]['sha256']}
 assert 'private-relay-publisher.js' in changed,'Fixed source did not regenerate the private publisher'
 write('generated-files.json',built);write('generated-changes.json',changed)
 with tarfile.open(E/'generated-dist.tar.gz','w:gz') as tar:
  for name in built:
   raw=(dist/name).read_bytes();info=tarfile.TarInfo(DIST_PREFIX+name);info.size=len(raw);info.mode=0o644;info.mtime=0;tar.addfile(info,io.BytesIO(raw))
 out=E/'package';out.mkdir();run(['pnpm','--filter','nostr-pubsub','pack','--pack-destination',str(out)],'package-pack')
 archives=list(out.iterdir());assert len(archives)==1 and archives[0].name=='nostr-pubsub-0.5.25.tgz';archive=archives[0]
 expected={'package/'+n.removeprefix('ts/packages/nostr-pubsub/'):ROOT/n for n in FILES if n.startswith(('ts/packages/nostr-pubsub/src/',DIST_PREFIX))}
 for name in ['package.json','README.md','LICENSE','CHANGELOG.md']:expected['package/'+name]=PKG/name
 members={};extract=Path('/stage/package-inspection');extract.mkdir()
 with tarfile.open(archive) as tar:
  for member in tar:
   name=member.name;assert name not in members and member.isfile() and not member.issym() and not member.islnk(),name
   assert name in expected and '..' not in PurePosixPath(name).parts and not PurePosixPath(name).is_absolute(),name
   raw=tar.extractfile(member).read();original=expected[name].read_bytes()
   if name=='package/package.json':assert json.loads(raw)==json.loads(original)
   else:assert raw==original,name
   assert member.mode&0o777==0o644,name
   members[name]={'bytes':len(raw),'sha256':hashlib.sha256(raw).hexdigest(),'mode':member.mode&0o777,'sourceSha256':hashlib.sha256(original).hexdigest(),'exactSourceBytes':raw==original}
   p=extract/name;p.parent.mkdir(parents=True,exist_ok=True);p.write_bytes(raw)
 assert set(members)==set(expected)
 metadata=json.loads((extract/'package/package.json').read_text());assert metadata['name']=='nostr-pubsub' and metadata['version']=='0.5.25'
 for export in metadata['exports'].values():
  for target in export.values():assert 'package/'+target.removeprefix('./') in members
 (extract/'package/node_modules').symlink_to(PKG/'node_modules',target_is_directory=True)
 run(['node','--input-type=module','-e',"const root=await import('/stage/package-inspection/package/dist/index.js'); const priv=await import('/stage/package-inspection/package/dist/private-relays.js'); if(typeof root.createNostrRuntime!=='function'||typeof priv.createPrivateRelayPublisher!=='function')throw new Error('missing package exports'); console.log('root and private-relays package entry points loaded');"],'package-entry-points',30)
 source(allow_dist=True);network_proof('after')
 assert {p.relative_to(dist).as_posix():sha(p) for p in dist.rglob('*') if p.is_file()}=={n:r['sha256'] for n,r in built.items()}
 write('package-members.json',members)
 raw=archive.read_bytes();write('package-qualification.json',{'passed':True,'releaseAccepted':False,'source':M['source']['commit'],'sourceArchiveSha256':M['source']['archiveSha256'],'version':'0.5.25','artifact':{'file':archive.name,'bytes':len(raw),'sha256':hashlib.sha256(raw).hexdigest(),'sha512':hashlib.sha512(raw).hexdigest(),'integrity':'sha512-'+base64.b64encode(hashlib.sha512(raw).digest()).decode()},'members':len(members),'packageMemberManifestSha256':sha(E/'package-members.json'),'generatedFiles':len(built),'generatedChanges':len(changed),'generatedFilesManifestSha256':sha(E/'generated-files.json'),'generatedChangesManifestSha256':sha(E/'generated-changes.json'),'generatedArchiveSha256':sha(E/'generated-dist.tar.gz'),'generatedArchiveBytes':(E/'generated-dist.tar.gz').stat().st_size,'nonDistSourceAndAllModesUnchanged':True,'allGeneratedFilesBuiltFromEmptyOutput':True,'allPackageMembersMapped':True,'entryPointsLoaded':True,'frozenInstalledInputsEqualPriorGreen':True,'testReuse':M['testReuse'],'nativeCompilePerformed':False,'unitTestsRepeated':False,'generatedSourceIntegrationPending':True,'steps':steps})
memory_proof('before')
try:package()
finally:memory_proof('after')
