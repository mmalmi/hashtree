import base64,hashlib,json,pathlib,subprocess,tarfile,urllib.request
root=pathlib.Path.cwd();out=pathlib.Path('/stage/evidence/installed');out.mkdir();archives=pathlib.Path('/stage/public-archives');archives.mkdir()
cohort=json.loads((root/'scripts/runtime-cohort.json').read_text());records=[]
for name,pin in cohort['packages'].items():
 archive=archives/(name.replace('/','-')+'.tgz')
 with urllib.request.urlopen(pin['url'],timeout=60) as response:data=response.read()
 assert hashlib.sha256(data).hexdigest()==pin['sha256'],name
 assert 'sha512-'+base64.b64encode(hashlib.sha512(data).digest()).decode()==pin['integrity'],name
 archive.write_bytes(data);unpacked=archives/(name.replace('/','-')+'-expected');unpacked.mkdir()
 with tarfile.open(archive) as tar:tar.extractall(unpacked,filter='data')
 expected=unpacked/'package'
 patched=[]
 if name=='nostr-pubsub':
  patch=root/'patches/nostr-pubsub-0.5.38-query-progress.patch'
  subprocess.run(['patch','--batch','-p1','-i',str(patch)],cwd=expected,check=True)
  patched=[l[6:] for l in patch.read_text().splitlines() if l.startswith('+++ b/')]
  assert len(patched)==8
 candidates=list((root/'node_modules/.pnpm').glob('*/node_modules/'+name))
 if (root/'node_modules'/name).exists():candidates.append(root/'node_modules'/name)
 actual=sorted(set(p.resolve() for p in candidates if p.is_dir()));files={str(p.relative_to(expected)):hashlib.sha256(p.read_bytes()).hexdigest() for p in expected.rglob('*') if p.is_file()}
 for p in actual:
  assert json.loads((p/'package.json').read_text())['version']==pin['version'],str(p)
  for file,sha in files.items():assert (p/file).is_file() and hashlib.sha256((p/file).read_bytes()).hexdigest()==sha,name+' '+file
 records.append({'package':name,'version':pin['version'],'archiveSHA256':pin['sha256'],'installedRoots':[str(p.relative_to(root)) for p in actual],'files':files if actual else {},'patchedTargets':patched,'used':bool(actual)})
for name in json.loads((root/'package.json').read_text())['dependencies']:
 if name in cohort['packages']:assert next(x for x in records if x['package']==name)['used'],name
(out/'bindings.json').write_text(json.dumps({'passed':True,'cohortMap':cohort['acceptedPinMapSHA256'],'records':records},indent=2)+'\n')
