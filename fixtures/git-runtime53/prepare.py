import hashlib, pathlib, json, urllib.request, tarfile, os, shutil, subprocess
HERE=pathlib.Path(__file__).resolve().parent
STAGE=pathlib.Path(os.environ['FIPS_STAGE'])
M=json.loads((HERE/'manifest.json').read_text())
assert shutil.disk_usage(STAGE.parent).free > 8 * 1024**3
STAGE.mkdir()
(STAGE/'evidence').mkdir()
(STAGE/'home').mkdir()
(STAGE/'tools/bin').mkdir(parents=True)
def digest(p):return hashlib.sha256(p.read_bytes()).hexdigest()
def fetch(spec,path):
 with urllib.request.urlopen(spec['url'],timeout=90) as source:path.write_bytes(source.read())
 assert digest(path)==spec['sha256']
def extract(archive,path):
 with tarfile.open(archive) as t:t.extractall(path,filter='data')
source=HERE/'source.tar.gz'
assert digest(source)==M['archiveSha256']
extract(source,STAGE/'app')
assert digest(HERE/'baseline.tar.gz')==M['baseline']['sha256']
extract(HERE/'baseline.tar.gz',STAGE/'baseline')
for name in ['pnpm','native']:
 file=STAGE/(name+'.tgz');fetch(M[name],file);extract(file,STAGE/'tools'/name)
for name,sha in M['native']['executables'].items():assert digest(STAGE/'tools/native/hashtree'/name)==sha
for file in ['qualify.mjs','manifest.json','consumer-pins.json','verify-cohort.py','runtime53-performance.spec.ts']:
 shutil.copyfile(HERE/file,STAGE/file)
wrapper=STAGE/'tools/bin/pnpm'
wrapper.write_text('#!/bin/sh\nexec node /stage/tools/pnpm/package/bin/pnpm.cjs "$@"\n');wrapper.chmod(0o755)
