"""Apply only the previously qualified owned-origin permission fixtures."""
import hashlib,json,pathlib,sys
base=pathlib.Path(sys.argv[2]).resolve();app=sys.argv[1];target=base/'apps'/app
spec=next(a for a in json.loads((base/'procedure/manifest.json').read_text())['apps'] if a['id']==app)
sha=lambda b:hashlib.sha256(b).hexdigest()
changed=[];added={}
for name,digest in spec['testFiles'].items():
    assert sha((target/name).read_bytes())==digest
if app=='drive':
    expected=json.loads((base/'procedure/app-fixture-inputs.json').read_text())[app]
    fixture=target/expected['path'];before=fixture.read_bytes()
    assert sha(before)==expected['beforeSha256']
    marker='async function initializeContext(context: BrowserContext, relayUrl: string, renderLoopErrors: Set<string>) {'
    text=before.decode();assert text.count(marker)==1
    after="import { grantIsolatedPermission } from './isolated-permission-context';\n"+text.replace(marker,marker+'\n  await grantIsolatedPermission(context);')
    fixture.write_text(after)
    changed.append({'path':expected['path'],'beforeSha256':sha(before),'afterSha256':sha(after.encode()),'change':'Only origin-scoped permission before existing context initialization'})
    copies=[('permission-context.ts','isolated-permission-context.ts')]
elif app=='meet':
    for name in spec['permissionTestFiles']:
        path=target/name;before=path.read_bytes();text=before.decode()
        assert text.count("from '@playwright/test'")==1
        after=text.replace("from '@playwright/test'","from './isolated-permissions'")
        path.write_text(after)
        changed.append({'path':name,'beforeSha256':sha(before),'afterSha256':sha(after.encode()),'change':'Only fixture import; statements and assertions unchanged'})
    copies=[('permission-context-meet.ts','isolated-permission-context.ts'),('isolated-permissions-meet.ts','isolated-permissions.ts')]
else:
    raise AssertionError('Unknown app')
for source,dest in copies:
    path=target/'e2e'/dest;assert not path.exists()
    data=(base/'procedure'/source).read_bytes();path.write_bytes(data);added[str(path.relative_to(target))]=sha(data)
receipt={'scope':'Test-only owned-origin permission; runtime and assertions unchanged','origin':spec['ownedOrigin'],'permission':'microphone','permissionProbeCaptureInvoked':False,'changedFiles':changed,'addedFiles':added,'manualContexts':'Existing Drive initializeContext; Meet fixture plus wrapped browser.newContext'}
(base/'evidence'/('overlay-'+app+'.json')).write_text(json.dumps(receipt,indent=2)+'\n')
