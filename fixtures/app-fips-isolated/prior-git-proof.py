"""Reuse only the 81 successful dev-source cases from one immutable failed run."""
import hashlib,json,pathlib
sha=lambda p:hashlib.sha256(p.read_bytes()).hexdigest()
def verify(base,evidence,spec):
 previous=base/'earlier';root=previous/'git';load=lambda p:json.loads(p.read_text());binding=spec['reusedBrowserEvidence']
 assert sha(base/'earlier.zip')==binding['artifactSha256']
 prior_manifest=load(base/'procedure/prior-manifest.json');old=prior_manifest['apps'][0]
 assert sha(base/'procedure/prior-manifest.json')==binding['manifestSha256']==load(previous/'prepared.json')['manifestSha256']
 for key in ['commit','archiveSha256','lockSha256','packageSha256','releaseProfileSha256','testFiles','packageManager','playwright','installedRuntimeExpectations','acceptedDependencyRelease']:
  assert old[key]==spec[key],key
 assert old['commit']==binding['source']
 assert load(root/'inputs.json')==load(evidence/'inputs.json')
 assert load(root/'packages.json')==load(evidence/'packages.json')
 assert load(root/'installed-runtime.json')==load(evidence/'installed-runtime.json')
 old_browser=load(root/'browser.json');new_browser=load(evidence/'browser.json')
 for key in ['node','playwright','browser','actualBrowserExecutables']:assert old_browser[key]==new_browser[key],key
 assert old_browser['passed'] and old_browser['rtc']['echoed']=='loopback-only' and old_browser['candidateAddressesAllowed'] and len(old_browser['rawWorkerDenial'])==4
 for name in ['network.json','network-after-attempt.json']:
  net=load(root/name);assert net['interfaces']==['fixture0','lo'] and net['uid']!=0 and net['effectiveAndBoundingCapabilities']==0
  for rows in [net['externalDenied'],net['childDenied']]:
   assert {(r['family'],r['kind']) for r in rows}=={(2,1),(2,2),(10,1),(10,2)} and all(r['errno'] in (101,113,99,97) for r in rows)
  assert not net['route4'].splitlines()[1:] and all(line.split()[-1]=='lo' for line in net['route6'].splitlines())
 container=load(previous/'iris-fips-test-git-container.json');h=container['hostConfig'];terminal=load(previous/'iris-fips-test-git-terminal.json')
 assert container['config']['Image']==prior_manifest['image'] and h['NetworkMode']=='none' and h['Memory']==h['MemorySwap']==4294967296 and h['NanoCpus']==2000000000 and h['PidsLimit']==512
 assert h['CapDrop']==['ALL'] and h['ReadonlyRootfs'] and h['PidMode']=='' and 'no-new-privileges' in h['SecurityOpt']
 assert terminal['Status']=='exited' and terminal['ExitCode']==1 and not terminal['Running'] and not terminal['OOMKilled']
 resource=load(root/'resource-final.json');assert not resource['samplerErrors'] and int(resource['sample']['events']['oom_kill'])==0
 contexts=[json.loads(line) for line in (root/'context-permissions.jsonl').read_text().splitlines()]
 assert contexts and all(c['origin']=='http://localhost:43810' and c['permission']=='microphone' and c['state']=='granted' and not c['captureInvoked'] for c in contexts)
 for context in contexts:
  for name,state in context['before'].items():
   if name!='microphone':assert (state=='granted')==(context['after'][name]=='granted')
 expected=load(base/'procedure/git-case-identities.json')['cases'];actual=[];passing=[];failed=[];report_hashes={};peer_report=None
 def walk(suite,report_name):
  for case in suite.get('specs',[]):
   for test in case.get('tests',[]):
    assert len(test['results'])==1 and test['results'][0]['retry']==0
    record={k:case[k] for k in ('id','file','line','column','title')}|{'projectName':test['projectName'],'expectedStatus':test['expectedStatus']}
    assert record['expectedStatus']=='passed';actual.append(record);result=test['results'][0]
    if result['status']=='passed':
     assert test['status']=='expected';passing.append(record|{'fromRun':binding['runId'],'report':report_name})
    else:
     assert result['status']=='failed' and test['status']=='unexpected'
     assert record==binding['failedCase'] and 'tracing.start: Tracing has been already started' in result['errors'][0]['message']
     failed.append(record)
  for child in suite.get('suites',[]):walk(child,report_name)
 for i in range(1,13):
  name=f'release-check-2-shard-{i}-playwright.json';path=root/name;report=load(path);assert report['stats']['flaky']==report['stats']['skipped']==0
  report_hashes[name]=sha(path);walk(report,name)
  if 'PEER_RELOAD_RECOVERY' in path.read_text():assert peer_report is None;peer_report=path
 assert len(actual)==82 and len(passing)==81 and len(failed)==1
 assert sorted(actual,key=lambda c:c['id'])==sorted(expected,key=lambda c:c['id']) and len({c['id'] for c in actual})==82
 assert peer_report is not None
 paths=[root/'inputs.json',root/'packages.json',root/'installed-runtime.json',root/'browser.json',root/'network.json',root/'network-after-attempt.json',root/'resource-final.json',root/'context-permissions.jsonl',previous/'iris-fips-test-git-container.json',previous/'iris-fips-test-git-terminal.json']
 receipt={'source':spec['commit'],'runId':binding['runId'],'carrier':binding['carrier'],'artifactId':binding['artifactId'],'artifactSha256':binding['artifactSha256'],
  'executionMode':'Original Playwright webServer invokes Vite dev source; not the built distribution','passingCases':passing,'excludedFailedCase':failed[0],
  'reportHashes':report_hashes,'proofHashes':{p.relative_to(previous).as_posix():sha(p) for p in paths},'installedRuntimeAndToolchainIdentical':True,'originalCaseCount':82,'reusedPassingCaseCount':81}
 (evidence/'reused-browser-evidence.json').write_text(json.dumps(receipt,indent=2)+'\n')
 return receipt,peer_report
