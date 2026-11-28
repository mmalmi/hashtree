import base64,hashlib,json,pathlib,re,subprocess,urllib.request
root=pathlib.Path.cwd();out=pathlib.Path('/stage/evidence/lock');out.mkdir();archives=pathlib.Path('/stage/public-archives');archives.mkdir()
cohort=json.loads((root/'scripts/runtime-cohort.json').read_text());verified={}
for name,pin in cohort['packages'].items():
 with urllib.request.urlopen(pin['url'],timeout=60) as response: data=response.read()
 assert hashlib.sha256(data).hexdigest()==pin['sha256'],name
 assert 'sha512-'+base64.b64encode(hashlib.sha512(data).digest()).decode()==pin['integrity'],name
 (archives/(name.replace('/','-')+'.tgz')).write_bytes(data)
 verified[name]={'sha256':pin['sha256'],'integrity':pin['integrity'],'bytes':len(data)}
(out/'archive-verification.json').write_text(json.dumps(verified,indent=2)+'\n')
before=(root/'pnpm-lock.yaml').read_text()
subprocess.run(['pnpm','install','--lockfile-only','--ignore-scripts','--no-frozen-lockfile','--store-dir','/stage/pnpm-store'],check=True,timeout=240)
after=(root/'pnpm-lock.yaml').read_text();(out/'pnpm-lock.yaml').write_text(after)
def resolutions(text):
 section=text.split('\npackages:\n')[1].split('\nsnapshots:')[0]
 return {m.group(1):m.group(2) for m in re.finditer(r'^  (.+):\n    resolution: (.+)$',section,re.M)}
old=resolutions(before);new=resolutions(after);changed=[k for k in old.keys()&new.keys() if old[k]!=new[k]]
assert not changed,'Existing package resolutions changed: '+repr(changed)
subprocess.run(['node','scripts/verify-dependency-lock.mjs'],check=True)
(out/'resolution.json').write_text(json.dumps({'passed':True,'phase':'lock preparation only','beforeSha256':hashlib.sha256(before.encode()).hexdigest(),'lockSha256':hashlib.sha256(after.encode()).hexdigest(),'packageSha256':hashlib.sha256((root/'package.json').read_bytes()).hexdigest(),'preservedExistingResolutions':len(old.keys()&new.keys()),'addedPackages':sorted(new.keys()-old.keys()),'removedPackages':sorted(old.keys()-new.keys()),'cohortMap':cohort['acceptedPinMapSHA256']},indent=2)+'\n')
