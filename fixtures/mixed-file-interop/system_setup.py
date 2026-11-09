"""Pinned Debian closure, authenticated before an offline task-only image setup."""
from pathlib import Path
import hashlib,json,lzma,os,shutil,stat,subprocess,urllib.request
sha=lambda p:hashlib.sha256(Path(p).read_bytes()).hexdigest()
def fields(text):
 result=[]
 for paragraph in text.split('\n\n'):
  row={};key=None
  for line in paragraph.splitlines():
   if line.startswith((' ','\t')) and key:row[key]+='\n'+line
   elif ':' in line:key,value=line.split(':',1);row[key]=value.lstrip()
  if row:result.append(row)
 return result
def prepare_system(HERE,STAGE,E,M,run,docker,cleanup,ENV,NAMES):
 closure=HERE/'system/closure.json';assert sha(closure)==M['system']['closureSha256'];C=json.loads(closure.read_text());target=STAGE/'system-inputs';target.mkdir()
 for name in ['InRelease','Packages.xz']:shutil.copyfile(HERE/'system'/name,target/name)
 for row in C['packages']:
  assert row['url']=='https://deb.debian.org/debian/'+row['filename'] and row['filename'].startswith('pool/')
  assert shutil.disk_usage(STAGE).free>20*1024**3+row['bytes']
  with urllib.request.urlopen(row['url'],timeout=60) as response:raw=response.read(row['bytes']+1)
  assert len(raw)==row['bytes'] and hashlib.sha256(raw).hexdigest()==row['sha256'];(target/Path(row['filename']).name).write_bytes(raw)
 docker('pubsub-wire-system-verify',['sh','/stage/procedure/verify-system.sh'],rust=True,seconds=60)
 status=(E/'debian-gpg-status.txt').read_text();assert '[GNUPG:] VALIDSIG ' in status and not any(s in status for s in ['BADSIG','ERRSIG','NO_PUBKEY','EXPKEYSIG','REVKEYSIG'])
 release=fields((E/'signed-Release').read_text());assert len(release)==1 and release[0]['Codename']=='bookworm' and 'amd64' in release[0]['Architectures'].split()
 rows=[line.split() for line in release[0]['SHA256'].splitlines() if line.strip().endswith('main/binary-amd64/Packages.xz')];assert len(rows)==1;digest,size,path=rows[0]
 assert digest==sha(target/'Packages.xz')==C['metadata']['packagesSha256'] and int(size)==(target/'Packages.xz').stat().st_size
 index={p['Package']:p for p in fields(lzma.decompress((target/'Packages.xz').read_bytes()).decode()) if p.get('Architecture') in ('amd64','all')}
 for row in C['packages']:
  p=index[row['name']]
  assert (p['Version'],p['Filename'],p['SHA256'],int(p['Size']),p['Architecture'],p.get('Depends',''),p.get('Pre-Depends',''))==(row['version'],row['filename'],row['sha256'],row['bytes'],row['architecture'],row['depends'],row['preDepends'])
 def installed(path):
  result={}
  for line in path.read_text().splitlines():
   name,version,arch,state=line.split('\t');
   if state=='install ok installed':result[name]={'version':version,'architecture':arch}
  return result
 before=installed(E/'base-system-packages.tsv');assert {k:v['version'] for k,v in before.items()}==C['basePackageVersions']
 proof={'signatureVerified':True,'inReleaseSha256':sha(target/'InRelease'),'packagesSha256':digest,'closureSha256':sha(closure),'baseImageId':M['rust']['imageId'],'baseDpkgStatusSha256':C['baseDpkgStatusSha256'],'newPackages':len(C['packages']),'existingPackagesChanged':[],'keyringProofSha256':sha(E/'debian-keyring-sha256.txt')}
 (E/'system-inputs-verified.json').write_text(json.dumps(proof,indent=2)+'\n')
 assert stat.S_IMODE(STAGE.stat().st_mode)==0o700 and STAGE.stat().st_uid==os.getuid()
 output=STAGE/'system-setup-output';output.mkdir(mode=0o770);output.chmod(0o770);custody=output.stat()
 assert custody.st_uid==os.getuid() and custody.st_gid==os.getgid() and stat.S_IMODE(custody.st_mode)==0o770
 proofFile=E/'system-inputs-verified.json';proofFile.chmod(0o444)
 name='pubsub-wire-system-setup';args=['docker','create','--name',name,'--label','iris.task=pubsub-wire-fixture','--network','none','--ipc','private','--cpus','2','--memory','4g','--memory-swap','4g','--pids-limit','512','--cap-drop','ALL','--security-opt','no-new-privileges','--user','0:0','--group-add',str(os.getgid()),'--tmpfs','/tmp:rw,nosuid,nodev,size=512m','--shm-size','512m','--init','--stop-timeout','5','--mount',f'type=bind,src={STAGE}/procedure,dst=/stage/procedure,readonly','--mount',f'type=bind,src={target},dst=/stage/system-inputs,readonly','--mount',f'type=bind,src={proofFile},dst=/stage/evidence/system-inputs-verified.json,readonly','--mount',f'type=bind,src={output},dst=/stage/setup-output','--workdir','/','--env','HOME=/tmp','--env','DEBIAN_FRONTEND=noninteractive',M['rust']['image'],'timeout','--signal=TERM','--kill-after=5s','170s','sh','/stage/procedure/install-system.sh']
 NAMES.add(name);run(args,name+'-create.log',30)
 info=json.loads(subprocess.check_output(['docker','inspect',name],env=ENV,text=True,timeout=5))[0];h=info['HostConfig'];assert info['Image']==M['rust']['imageId'] and info['Config']['User']=='0:0' and h['NetworkMode']=='none' and not h['ReadonlyRootfs'] and h['CapDrop']==['ALL'] and h['Memory']==h['MemorySwap']==4294967296 and h['NanoCpus']==2000000000 and h['PidsLimit']==512
 mounts={m['Destination']:m['RW'] for m in info['Mounts'] if m['Type']=='bind'}
 assert mounts=={'/stage/procedure':False,'/stage/system-inputs':False,'/stage/evidence/system-inputs-verified.json':False,'/stage/setup-output':True} and h['GroupAdd']==[str(os.getgid())]
 (E/'system-output-custody.json').write_text(json.dumps({'stageMode':'0700','outputMode':'0770','outputOwnerUid':custody.st_uid,'outputGroupGid':custody.st_gid,'setupSupplementaryGroup':os.getgid(),'outputContainerPath':'/stage/setup-output','readOnlyInputs':True,'wholeStageMounted':False},indent=2)+'\n')
 (E/(name+'-container.json')).write_text(json.dumps({'id':info['Id'],'imageId':info['Image'],'config':info['Config'],'hostConfig':h,'rootPurpose':'offline installation of eight authenticated new system packages only; no compilation'},indent=2)+'\n')
 try:
  run(['docker','start','--attach',name],name+'.log',180,True)
  terminal=json.loads(subprocess.check_output(['docker','inspect',name],env=ENV,text=True,timeout=5))[0];assert terminal['Id']==info['Id'];(E/(name+'-terminal.json')).write_text(json.dumps(terminal['State'],indent=2)+'\n');assert not terminal['State']['Running'] and not terminal['State']['OOMKilled'] and terminal['State']['ExitCode']==0
  expectedOutputs=['derived-dpkg-audit.txt','derived-system-packages.tsv','dbus-version.txt','libclang-sha256.txt','libclang-linkage.txt','derived-os-release.txt']
  assert {f.name for f in output.iterdir()}==set(expectedOutputs)
  for filename in expectedOutputs:
   file=output/filename;assert file.is_file() and not file.is_symlink() and file.stat().st_uid==0 and file.stat().st_mode&0o004
   assert not (E/filename).exists();shutil.copyfile(file,E/filename)
  assert stat.S_IMODE(STAGE.stat().st_mode)==0o700 and STAGE.stat().st_uid==os.getuid()
  after=installed(E/'derived-system-packages.tsv');assert {k:after[k] for k in before}==before
  added={r['name']:{'version':r['version'],'architecture':r['architecture']} for r in C['packages']};assert {k:v for k,v in after.items() if k not in before}==added
  assert not (E/'derived-dpkg-audit.txt').read_text().strip() and (E/'dbus-version.txt').read_text().strip()=='1.14.10'
  run(['docker','diff',name],'system-image-files.diff',30)
  run(['docker','commit',name],'system-image-id.txt',60);imageId=(E/'system-image-id.txt').read_text().strip();assert imageId.startswith('sha256:') and len(imageId)==71
  image=json.loads(subprocess.check_output(['docker','image','inspect',imageId],env=ENV,text=True,timeout=5))[0];base=json.loads(subprocess.check_output(['docker','image','inspect',M['rust']['imageId']],env=ENV,text=True,timeout=5))[0]
  assert image['Id']==imageId and image['Architecture']=='amd64' and image['Os']=='linux' and image['RootFS']['Layers'][:-1]==base['RootFS']['Layers']
  receipt={'imageId':imageId,'baseImageId':M['rust']['imageId'],'baseManifest':M['rust']['image'],'closureSha256':sha(closure),'sourceRecipeSha256':sha(HERE/'install-system.sh'),'image':image,'addedPackages':added,'existingPackagesUnchanged':True,'network':'none','publishedImageByteEquivalenceClaimed':False,'published':False}
  (E/'system-image.json').write_text(json.dumps(receipt,indent=2)+'\n')
 finally:cleanup()
