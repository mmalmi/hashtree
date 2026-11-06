"""Grant exact app-origin permission, including tests using raw Playwright fixtures."""
import hashlib,json,pathlib,sys
base=pathlib.Path(sys.argv[2]).resolve();app=sys.argv[1];target=base/'apps'/app
expected=json.loads((base/'procedure/app-fixture-inputs.json').read_text())[app]
fixture=target/expected['path'];before=fixture.read_bytes()
sha=lambda b:hashlib.sha256(b).hexdigest()
assert sha(before)==expected['beforeSha256']
marker='async function initializeContext(context: BrowserContext, relayUrl: string, renderLoopErrors: Set<string>) {'
text=before.decode();assert text.count(marker)==1
insertion='  await grantIsolatedPermission(context);'
after="import { grantIsolatedPermission } from './isolated-permission-context';\n"+text.replace(marker,marker+'\n'+insertion)
fixture.write_text(after)
added={}
for source,dest in [('permission-context.ts','isolated-permission-context.ts'),('isolated-permissions.ts','isolated-permissions.ts')]:
    contents=(base/'procedure'/source).read_bytes();path=target/'e2e'/dest
    assert not path.exists();path.write_bytes(contents);added[str(path.relative_to(target))]=sha(contents)
changed_tests=[]
manifest=json.loads((base/'procedure/manifest.json').read_text())
spec=next(row for row in manifest['apps'] if row['id']==app)
for name,digest in spec['testFiles'].items():
    path=target/name;original=path.read_bytes();assert sha(original)==digest
    raw="import { expect, test } from '@playwright/test';"
    if app in ('calendar','boards','video'):
        assert original.decode().count(raw)==1
        updated=original.decode().replace(raw,"import { expect, test } from './isolated-permissions';")
        path.write_text(updated)
        changed_tests.append({'path':name,'beforeSha256':digest,'afterSha256':sha(updated.encode()),'change':'Only fixture import; all test statements and assertions unchanged'})
receipt={'scope':'test fixture permission only; assertions and runtime/dist unchanged','path':expected['path'],'beforeSha256':sha(before),'afterSha256':sha(after.encode()),'insertionSha256':sha(insertion.encode()),'grantOrigin':'http://localhost:43810','permission':'microphone','captureInvoked':False,'addedFiles':added,'testImports':changed_tests,'manualContexts':'Both existing initializeContext and raw Playwright wrapper cover fixture context and browser.newContext'}
(base/'evidence'/('overlay-'+app+'.json')).write_text(json.dumps(receipt,indent=2)+'\n')
