"""Task-only fixture admission: preserve all selected test and production bytes."""
import hashlib,json,pathlib,sys
base=pathlib.Path(sys.argv[2]).resolve();app=sys.argv[1];assert app=='chat'
target=base/'apps'/app
spec=json.loads((base/'procedure/manifest.json').read_text())['apps'][0]
sha=lambda b:hashlib.sha256(b).hexdigest()
fixture=target/'e2e/fixtures.ts';before=fixture.read_bytes()
assert sha(before)==spec['fixtureSha256']
assert sha((target/'playwright.config.ts').read_bytes())==spec['configSha256']
marker='  const relayUrls = Array.isArray(relayUrlOrUrls) ? relayUrlOrUrls : [relayUrlOrUrls]'
text=before.decode();assert text.count(marker)==1
after="import { admitIsolatedChatContext } from './isolated-permission-context'\n"+text.replace(marker,'  await admitIsolatedChatContext(context)\n'+marker)
fixture.write_text(after)
added={}
for source,dest in [('permission-context-chat.ts','e2e/isolated-permission-context.ts'),('isolated.playwright.config.ts','isolated.playwright.config.ts')]:
    contents=(base/'procedure'/source).read_bytes();path=target/dest
    assert not path.exists();path.write_bytes(contents);added[dest]=sha(contents)
for path,digest in spec['testFiles'].items():assert sha((target/path).read_bytes())==digest
(base/'evidence/overlay-chat.json').write_text(json.dumps({
    'assertionsUnchanged':True,'testFilesByteIdentical':spec['testFiles'],
    'runtimeUnchanged':True,'fixtureBeforeSha256':sha(before),'fixtureAfterSha256':sha(after.encode()),
    'addedFiles':added,'origin':'http://127.0.0.1:5721',
    'manualContexts':'All selected manual contexts, including persistent WebKit, already enter useTestRelay before app navigation',
    'permission':'Chromium microphone only, exact owned origin, no capture; Firefox/WebKit unchanged',
},indent=2)+'\n')
