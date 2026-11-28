import hashlib,json,os,pathlib,subprocess,tarfile,time
root=pathlib.Path.cwd();stage=pathlib.Path('/stage');evidence=stage/'evidence/social';evidence.mkdir();spec=json.loads((stage/'procedure/manifest.json').read_text())['apps'][0];tmp=stage/'task-tmp';tmp.mkdir();env=dict(os.environ,CI='1',TMPDIR=str(tmp),**spec['env']);records=[]
sha=lambda b:hashlib.sha256(b).hexdigest()
def run(cmd,name):
 start=time.monotonic()
 with (evidence/(name+'.log')).open('wb') as log:r=subprocess.run(cmd,cwd=root,env=env,stdout=log,stderr=subprocess.STDOUT,timeout=500)
 records.append({'name':name,'exit':r.returncode,'seconds':time.monotonic()-start});(evidence/'checks.json').write_text(json.dumps({'source':spec['commit'],'records':records},indent=2)+'\n');assert r.returncode==0,name
run(['python3','/stage/procedure/network-proof.py',str(evidence/'network.json')],'network')
run(['node','scripts/verify-dependency-lock.mjs'],'verify-pins')
run(['pnpm','run','lint'],'lint')
run(['pnpm','run','check'],'typecheck')
run(['pnpm','exec','playwright','test','--config=playwright.unit.config.ts','--workers=1','--reporter=list'],'unit')
run(['pnpm','run','build'],'build')
files={str(p.relative_to(root/'dist')):sha(p.read_bytes()) for p in (root/'dist').rglob('*') if p.is_file()};(evidence/'artifact.json').write_text(json.dumps({'source':spec['commit'],'archiveSha256':spec['archiveSha256'],'env':spec['env'],'files':files},indent=2)+'\n')
with tarfile.open(evidence/'dist.tar.gz','w:gz') as tar:tar.add(root/'dist',arcname='dist')
run(['python3','/stage/procedure/network-proof.py',str(evidence/'network-after.json')],'network-after')
