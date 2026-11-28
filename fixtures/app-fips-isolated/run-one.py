import hashlib,json,os,pathlib,subprocess,tarfile,time
root=pathlib.Path.cwd();stage=pathlib.Path('/stage');evidence=stage/'evidence/social';evidence.mkdir();spec=json.loads((stage/'procedure/manifest.json').read_text())['apps'][0];tmp=stage/'task-tmp';tmp.mkdir();env=dict(os.environ,CI='1',TMPDIR=str(tmp),**spec['env'])
sha=lambda b:hashlib.sha256(b).hexdigest()
started=time.monotonic()
with (evidence/'build.log').open('wb') as out: result=subprocess.run(['pnpm','run','build'],cwd=root,env=env,stdout=out,stderr=subprocess.STDOUT,timeout=300)
assert result.returncode==0
files={str(p.relative_to(root/'dist')):sha(p.read_bytes()) for p in (root/'dist').rglob('*') if p.is_file()}
prior=json.loads((stage/'procedure/expected-artifact.json').read_text());assert prior['source']=='657c1ecccffdd782144b22e278e994888e9260b1' and prior['env']==spec['env'];assert files==prior['files'],'Test-only change must preserve all qualified runtime files'
(evidence/'artifact.json').write_text(json.dumps({'source':spec['commit'],'archiveSha256':spec['archiveSha256'],'env':spec['env'],'files':files,'byteIdenticalTo':prior['source'],'reusedEvidenceRun':46118425889,'equalitySeconds':time.monotonic()-started},indent=2)+'\n')
with tarfile.open(evidence/'dist.tar.gz','w:gz') as tar:tar.add(root/'dist',arcname='dist')
subprocess.run(['python3','/stage/procedure/run-final-closure.py'],cwd=root,env=env,check=True,timeout=1800)
