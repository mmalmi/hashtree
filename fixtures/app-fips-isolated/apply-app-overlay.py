"""Only add exact-origin permission to the existing context initializer seam."""
import hashlib,json,pathlib,sys
base=pathlib.Path(sys.argv[2]).resolve();app=sys.argv[1];target=base/'apps'/app
expected=json.loads((base/'procedure/app-fixture-inputs.json').read_text())[app]
fixture=target/expected['path'];before=fixture.read_bytes()
sha=lambda b:hashlib.sha256(b).hexdigest()
assert sha(before)==expected['beforeSha256']
marker='async function initializeContext(context: BrowserContext, relayUrl: string, renderLoopErrors: Set<string>) {'
text=before.decode();assert text.count(marker)==1
insertion=(base/'procedure/permission-context.ts').read_text()
after="import { appendFileSync } from 'node:fs';\nconst isolatedFipsContexts = new WeakSet<object>();\n"+text.replace(marker,marker+'\n'+insertion)
fixture.write_text(after)
receipt={'scope':'test fixture only; original tests/assertions and runtime/dist unchanged','path':expected['path'],'beforeSha256':sha(before),'afterSha256':sha(after.encode()),'insertionSha256':sha(insertion.encode()),'grantOrigin':'http://127.0.0.1:43810','permission':'microphone','captureInvoked':False,'manualContexts':'Existing initializeContext is invoked for both fixture context and wrapped browser.newContext; inspected all selected peer creation sites'}
(base/'evidence'/('overlay-'+app+'.json')).write_text(json.dumps(receipt,indent=2)+'\n')
