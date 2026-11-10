"""Verify the exact previously qualified worker archive before OIDC publication.
No application code, package hooks, or signed test fixtures are executed.
"""
import argparse,base64,hashlib,json,pathlib,subprocess,tarfile,urllib.request,urllib.error
p=argparse.ArgumentParser();p.add_argument('--directory',required=True);p.add_argument('--download',action='store_true');p.add_argument('--source-ref',default='HEAD',help='Immutable source ref for local receipt verification; hosted publication uses HEAD');a=p.parse_args()
root=pathlib.Path.cwd();out=pathlib.Path(a.directory);m=json.loads((root/'.github/releases/worker-0.4.25.json').read_text())
assert m['name']=='@hashtree/worker' and m['version']=='0.4.25'
assert m['source']=='3b5ec0e2ea9f1da2036bf4c894a770bd211add30'
assert m['tag']=='hashtree-ts-runtime-v0.5.41'
assert m['archive']['sha256']=='b9ea6d34c73f7005e862bbbec0a4a6164649cc24306ea53684413cf57a54de4e'
assert m['archive']['bytes']==255911
assert m['coldAudit'] and isinstance(m['coldAudit']['run'],int)
q=m['qualification'];assert q['workerTests']==268 and q['workerFiles']==45 and q['actualIifeModes']==15 and q['networkIsolationControlsPassed']
assert q['nativePassed']==126 and q['nativeIgnoredBenchmarks']==2 and q['nativeWorkflowConclusion']=='failure'
assert q['nativeFailureWasReporterOnly'] and q['nativePostRunDiffCheckOmitted']
def git(*args):return subprocess.check_output(['git',*args],text=True).strip()
def sha(path):return hashlib.sha256(pathlib.Path(path).read_bytes()).hexdigest()
assert git('rev-parse',a.source_ref+':ts')==m['tsTree'] and git('rev-parse',a.source_ref+':rust')==m['rustTree']
if a.download:
 assert not out.exists();out.mkdir(parents=True)
 run=json.loads(subprocess.check_output(['gh','run','view',str(m['coldAudit']['run']),'--repo','mmalmi/hashtree','--json','conclusion,headSha'],text=True,timeout=45))
 assert run['conclusion']=='success' and run['headSha']==m['coldAudit']['opsCommit']
 subprocess.run(['gh','run','download',str(m['coldAudit']['run']),'--repo','mmalmi/hashtree','--name','source-update-evidence','--dir',str(out/'source-audit')],check=True,timeout=180)
 subprocess.run(['gh','release','download',m['tag'],'--repo','mmalmi/hashtree','--pattern',m['archive']['filename'],'--dir',str(out)],check=True,timeout=180)
receipt=out/'source-audit/receipt.json';assert sha(receipt)==m['coldAudit']['receiptSha256']
r=json.loads(receipt.read_text());assert r['status']=='passed' and r['source']==m['mirror']
expected=dict(r['expected']);expected.pop('publication',None);assert expected==m['sourceExpected']
v=r['public-reader'];assert v['refs']==len(expected['refs']) and v['objects']==expected['objects'] and v['payloads_sha256']==expected['payloads_sha256']
assert v['strict_fsck'] and v['nonshallow'] and not v['alternates'] and r['public_root']['signatureVerified']
archive=out/m['archive']['filename'];data=archive.read_bytes();assert len(data)==m['archive']['bytes'] and hashlib.sha256(data).hexdigest()==m['archive']['sha256']
assert 'sha512-'+base64.b64encode(hashlib.sha512(data).digest()).decode()==m['archive']['integrity']
with tarfile.open(archive) as tar:
 members=tar.getmembers();assert len({x.name for x in members})==len(members)
 files={}
 for member in members:
  assert member.name.startswith('package/') and '..' not in pathlib.PurePosixPath(member.name).parts
  assert member.isfile(),'Only qualified regular package files are allowed'
  payload=tar.extractfile(member).read();entry={'sha256':hashlib.sha256(payload).hexdigest(),'mode':member.mode,'bytes':len(payload)};files[member.name]=entry
  assert not any(v in payload for v in (b'/Users/',b'/private/tmp/',b'.git/task-evidence/'))
  if member.name=='package/package.json':manifest=json.loads(payload)
assert files==m['files'] and len(files)==229
assert manifest['name']==m['name'] and manifest['version']==m['version']
assert manifest['peerDependencies']=={'nostr-social-graph':'>=4.0.4'} and manifest['peerDependenciesMeta']=={'nostr-social-graph':{'optional':True}}
assert manifest['dependencies']['@hashtree/nostr']=='0.2.7'
assert manifest['repository']=={'type':'git','url':'git+https://github.com/mmalmi/hashtree.git','directory':'ts/packages/hashtree-worker'}
for field in ('dependencies','devDependencies','peerDependencies','optionalDependencies'):
 for value in manifest.get(field,{}).values():assert not value.startswith(('file:','link:','workspace:'))
url='https://registry.npmjs.org/@hashtree%2fworker/0.4.25'
try:
 with urllib.request.urlopen(url,timeout=20) as response:raise RuntimeError('Worker version already exists; do not republish')
except urllib.error.HTTPError as error:assert error.code==404
proof={'passed':True,'source':m['source'],'publicationWorkflowCommit':git('rev-parse',a.source_ref),'archiveSha256':m['archive']['sha256'],'archiveBytes':len(data),'verifiedFiles':len(files),'qualifiedRuntimeFiles':228,'sourceColdAuditRun':m['coldAudit']['run'],'sourceColdAuditReceiptSha256':sha(receipt),'sourceFullHistoryFsck':True,'qualifiedTestsReused':q,'noProductCodeOrFixtureExecution':True,'noRebuild':True}
(out/'verification.json').write_text(json.dumps(proof,indent=2)+'\n');print(json.dumps(proof))
