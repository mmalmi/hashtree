"""Check actual single-attempt cases and minimal two-endpoint recovery proof."""
import json,pathlib,re,sys
app=sys.argv[1];evidence=pathlib.Path(sys.argv[2])
spec=next(a for a in json.loads(pathlib.Path('/stage/procedure/manifest.json').read_text())['apps'] if a['id']==app)
reports=sorted(evidence.glob('*-playwright.json'))
assert len(reports)==spec['expectedBrowserReports'],'Missing or unexpected browser phase report'
allowed_skips={('video-feed-follows.spec.ts','debug: trace follow list fetching'),('video-stream-create.spec.ts','viewer watches full 20+ second stream with live updates')} if app=='video' else set()
seen_skips=set();cases=[]
def visit(suite,report):
    for case in suite.get('specs',[]):
        for test in case.get('tests',[]):
            results=test['results'];assert len(results)==1 and results[0]['retry']==0,'Browser case retried or omitted'
            result=results[0];identity=(pathlib.Path(case['file']).name,case['title'])
            if result['status']=='skipped':
                assert test['expectedStatus']=='skipped' and identity in allowed_skips,'Unexpected browser skip'
                assert identity not in seen_skips;seen_skips.add(identity)
            else:
                assert test['status']=='expected' and test['expectedStatus']==result['status']=='passed','Browser case did not pass once'
            cases.append({'report':report,'file':case['file'],'title':case['title'],'project':test['projectName'],'status':result['status'],'attempts':len(results),'retry':result['retry']})
    for child in suite.get('suites',[]):visit(child,report)
for report in reports:
    data=json.loads(report.read_text());assert not data.get('errors')
    assert data['stats']['unexpected']==data['stats']['flaky']==0
    before=len(cases);visit(data,report.name);assert len(cases)>before
assert seen_skips==allowed_skips
proofs=[];label='FIPS_BROWSER_RELOAD_PROOF '
for log in sorted(evidence.glob('*.log')):
    for line in log.read_text(errors='replace').splitlines():
        line=re.sub(r'\x1b\[[0-?]*[ -/]*[@-~]','',line)
        if label not in line:continue
        raw=line.split(label,1)[1].strip();proof=json.loads(raw)
        assert isinstance(proof,list) and len(proof)==2
        assert [p['reloadedEndpoint'] for p in proof]==['first','second']
        assert proof[0]['localPeerId']==proof[1]['otherPeerId'] and proof[1]['localPeerId']==proof[0]['otherPeerId']
        assert proof[0]['localPeerId']!=proof[1]['localPeerId'] and proof[0]['blockHash']!=proof[1]['blockHash']
        for p in proof:
            assert set(p)=={'reloadedEndpoint','localPeerId','otherPeerId','identityRetained','bothConnected','receiverCachedBefore','blockHash','verifiedBytes'}
            assert all(re.fullmatch(r'(02|03)[0-9a-f]{64}',p[k]) for k in ['localPeerId','otherPeerId'])
            assert p['identityRetained'] is True and p['bothConnected'] is True and p['receiverCachedBefore'] is False
            assert re.fullmatch(r'[0-9a-f]{64}',p['blockHash']) and isinstance(p['verifiedBytes'],int) and p['verifiedBytes']>0
        proofs.append(proof)
assert len(proofs)==spec['expectedReloadProofGroups'],'Missing or repeated two-endpoint proof'
(evidence/'browser-result-policy.json').write_text(json.dumps({'passed':True,'source':spec['commit'],'reports':len(reports),'cases':cases,'retries':0,'declaredSkips':len(seen_skips)},indent=2)+'\n')
(evidence/'browser-reload-proof.json').write_text(json.dumps({'passed':True,'source':spec['commit'],'proofs':proofs},indent=2)+'\n')
