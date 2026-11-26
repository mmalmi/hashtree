import pathlib,json,hashlib,base64,tarfile,urllib.request,re,sys,subprocess
ROOT=pathlib.Path('/stage');APP=pathlib.Path.cwd();E=ROOT/'evidence'
pinsBytes=(ROOT/'consumer-pins.json').read_bytes()
assert hashlib.sha256(pinsBytes).hexdigest()=='a5dcd8feb2530643e7430cfe7bdae38a192b63ab832f4f082222f092c4b0bf6f'
pins=json.loads(pinsBytes)['packages'];mode=sys.argv[1]
lock=(APP/'pnpm-lock.yaml').read_text();records=[]
cache=ROOT/'verified-cohort';cache.mkdir(exist_ok=True)
for name,s in pins.items():
 if name+'@'+s['url'] not in lock:continue
 def digest(data):return hashlib.sha256(data).hexdigest()
 archive=cache/(name.replace('/','-').replace('@','')+'.tgz')
 if not archive.exists():
  with urllib.request.urlopen(s['url'],timeout=90) as response:archive.write_bytes(response.read(32*1024**2))
 data=archive.read_bytes();assert digest(data)==s['sha256'],name
 assert 'sha512-'+base64.b64encode(hashlib.sha512(data).digest()).decode()==s['integrity'],name
 registry='https://registry.npmjs.org/'+name+'/-/'+name.split('/')[-1]+'-'+s['version']+'.tgz'
 with urllib.request.urlopen(registry,timeout=90) as response:registryBytes=response.read(32*1024**2)
 assert registryBytes==data, name+' registry/release bytes differ'
 old='resolution: {tarball: '+s['url']+'}'
 lock=lock.replace(old,'resolution: {integrity: '+s['integrity']+', tarball: '+s['url']+'}')
 record={'name':name,'version':s['version'],'archiveSHA256':digest(data),'integrity':s['integrity'],'registryEqualsRelease':True}
 if mode=='installed':
  expected=cache/(name.replace('/','-').replace('@','')+'-expected');expected.mkdir(exist_ok=True)
  with tarfile.open(archive) as t:t.extractall(expected,filter='data')
  package=expected/'package'
  if name=='@fips/transport-webrtc':
   with (E/'verified-replay-patch.log').open('w') as log:
    subprocess.run(['patch','--batch','--forward','-p1','-i',str(APP/'patches/fips-transport-webrtc-bounded-replay.patch')],cwd=package,stdout=log,stderr=log,check=True)
   record['patchSHA256']=digest((APP/'patches/fips-transport-webrtc-bounded-replay.patch').read_bytes())
  paths=list((APP/'node_modules/.pnpm').glob('*/node_modules/'+name+'/package.json'))
  canonical={p.resolve().parent for p in paths};assert len(canonical)==1,(name,canonical)
  installed=next(iter(canonical));assert json.loads((installed/'package.json').read_text())['version']==s['version']
  files={}
  for file in package.rglob('*'):
   if file.is_file():
    relative=str(file.relative_to(package));assert (installed/relative).read_bytes()==file.read_bytes(),(name,relative)
    files[relative]=digest(file.read_bytes())
  record.update(installed=str(installed.relative_to(APP)),references=sorted(str(p.parent.relative_to(APP)) for p in paths),files=files)
 records.append(record)
assert len(records)==17,(len(records),[r['name'] for r in records])
if mode=='lock':(APP/'pnpm-lock.yaml').write_text(lock)
else:assert (APP/'pnpm-lock.yaml').read_text()==lock,'Installed lock lacks verified pins'
(E/('cohort-'+mode+'.json')).write_text(json.dumps({'cohortSHA256':digest(pinsBytes),'packages':records,'unused':['@hashtree/merge','@fips/transport-ethernet','@fips/transport-memory']},indent=2)+'\n')
print('Verified',len(records),'applicable archives, registry equality and '+mode+' bindings')
