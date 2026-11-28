import base64,hashlib,json,pathlib,re,tarfile,urllib.request
def apply_exact_patch(directory, patch):
 lines=patch.read_bytes().splitlines(keepends=True); i=0; targets=[]
 while i<len(lines):
  assert lines[i].startswith(b'--- a/'),lines[i]; i+=1
  assert lines[i].startswith(b'+++ b/'),lines[i]
  name=lines[i][6:].strip().decode();assert '..' not in pathlib.PurePosixPath(name).parts and not name.startswith('/')
  i+=1;path=directory/name;original=path.read_bytes().splitlines(keepends=True);output=[];cursor=0
  while i<len(lines) and not lines[i].startswith(b'--- a/'):
   h=re.match(rb'@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@',lines[i]);assert h,lines[i];i+=1;old=[];new=[]
   while i<len(lines) and not lines[i].startswith((b'@@ ',b'--- a/')):
    line=lines[i];assert line[:1] in (b' ',b'-',b'+'),line
    if line[:1] in (b' ',b'-'):old.append(line[1:])
    if line[:1] in (b' ',b'+'):new.append(line[1:])
    i+=1
   assert len(old)==int(h[2] or 1) and len(new)==int(h[4] or 1)
   matches=[j for j in range(cursor,len(original)-len(old)+1) if original[j:j+len(old)]==old]
   assert len(matches)==1,(name,'ambiguous or missing exact patch context',matches)
   at=matches[0];output.extend(original[cursor:at]);output.extend(new);cursor=at+len(old)
  output.extend(original[cursor:]);path.write_bytes(b''.join(output));targets.append(name)
 return targets
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
  patched=apply_exact_patch(expected,patch)
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
