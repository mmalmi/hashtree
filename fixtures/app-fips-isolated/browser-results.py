"""Validate actual, single-attempt browser outcomes and bounded recovery evidence."""
import json,pathlib,re,sys

def cases(report):
    rows=[]
    def walk(suite):
        for case in suite.get('specs',[]):
            for test in case.get('tests',[]):
                results=test.get('results',[])
                assert len(results)==1 and results[0].get('retry')==0,'Each browser case must have one original attempt'
                result=results[0]
                assert result['status'] in ('passed','skipped') and test['status'] in ('expected','skipped')
                assert not result.get('errors') and not result.get('error')
                if result['status']=='passed':assert test['expectedStatus']=='passed'
                else:assert test['expectedStatus']=='skipped'
                file=pathlib.PurePosixPath(case['file'])
                assert not file.is_absolute() and '..' not in file.parts
                rows.append({'file':file.name,'title':case['title'],'project':test['projectName'],'status':result['status'],
                             'skipReasons':[a.get('description','') for a in test.get('annotations',[]) if a['type']=='skip'],
                             'stdout':result.get('stdout',[])})
        for child in suite.get('suites',[]):walk(child)
    walk(report)
    assert rows and not report.get('errors')
    stats=report['stats'];assert stats['unexpected']==stats['flaky']==0
    assert stats['expected']==sum(r['status']=='passed' for r in rows)
    assert stats['skipped']==sum(r['status']=='skipped' for r in rows)
    identities=[(r['file'],r['title'],r['project']) for r in rows]
    assert len(identities)==len(set(identities)),'Duplicate browser identity'
    return rows

def verify(report_path,passed,skipped,expected=None):
    report=json.loads(pathlib.Path(report_path).read_text());rows=cases(report)
    assert sum(r['status']=='passed' for r in rows)==passed
    assert sum(r['status']=='skipped' for r in rows)==skipped
    if expected is not None:
        ledger=json.loads(pathlib.Path(expected).read_text())
        fields=('file','title','project','status')
        key=lambda r:tuple(r[k] for k in fields)
        assert sorted(map(key,rows))==sorted(map(key,ledger['cases'])),'Original browser case identities/outcomes changed'
        for row in rows:
            before=next(r for r in ledger['cases'] if key(r)==key(row))
            if 'skipReasons' in before:assert row['skipReasons']==before['skipReasons'],'Original skip predicate/reason changed'
    return rows

def recovery(rows,app):
    evidence=[]
    for row in rows:
        for output in row['stdout']:
            for line in output.get('text','').splitlines():
                if 'PEER_RELOAD_RECOVERY ' in line:
                    value=json.loads(line.split('PEER_RELOAD_RECOVERY ',1)[1]);assert row['status']=='passed'
                    assert value['app']=='iris-'+app and value['reloaded'] is True and value['peerRecovered'] is True
                    assert re.fullmatch('[0-9a-f]{64}',value['freshHash']) and value['bytes']>0
                    if app=='meet':
                        assert all(value[k] is True for k in ('accountIdentityPreserved','sessionIdentityRotated','oldSessionRemoved'))
                        assert value['transport']=='direct-fips'
                    else:assert value['identityPreserved'] is True
                    if app=='client':assert value['receiverCacheMiss'] is True and value['bytes']==32000 and value['webrtcBytesReceived']>=32000
                    if app=='git':assert value['receiverPath']=='targeted-p2p-provider' and value['bytes']==32000
                    if app=='chat':
                        assert value['transport']=='routed-fips'
                        assert all(value[k] is True for k in ('explicitConsent','interruptedTransferResumed','sameTransferAfterReload'))
                        assert 2*1024**2 < value['durablePrefixBytes'] < value['originalTransferBytes']
                        assert 0 < value['resumedWireBytes'] < value['originalTransferBytes']
                        assert value['reloadRole']==('sender' if value['endpoint']==0 else 'receiver')
                    evidence.append(value)
    assert len(evidence)==2 and {e['endpoint'] for e in evidence}=={0,1}
    assert len({e['freshHash'] for e in evidence})==2
    return {'passed':True,'app':app,'endpoints':evidence,'scope':'Two actual browser endpoint reloads and fresh directional transfers; no cryptographic offer-role inference',
            'identityPolicy':'Participant identity retained; ephemeral FIPS session rotated' if app=='meet' else 'FIPS device identity retained'}

if __name__=='__main__':
    report,passed,skipped=sys.argv[1:4]
    rows=verify(report,int(passed),int(skipped),sys.argv[4] if len(sys.argv)>4 else None)
    print(json.dumps({'passed':True,'passedCases':int(passed),'originalSkips':int(skipped),'retries':0}))
