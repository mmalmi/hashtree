"""Exercise the selected Social release checks under the proven namespace."""
import http.server,json,os,pathlib,subprocess,sys,threading
base=pathlib.Path('/stage');root=pathlib.Path.cwd()
spec=json.loads((base/'procedure/manifest.json').read_text())['apps'][0]
evidence=base/'evidence/social';evidence.mkdir()
class Reject(http.server.BaseHTTPRequestHandler):
    def reject(self):
        self.server.rejected+=1
        self.send_error(403)
    do_CONNECT=do_GET=do_POST=reject
    def log_message(self,*args):pass
proxy=http.server.ThreadingHTTPServer(('127.0.0.1',0),Reject)
proxy.rejected=0
threading.Thread(target=proxy.serve_forever,daemon=True).start()
env=dict(os.environ,CI='1',**spec['env'],REJECT_PROXY='http://127.0.0.1:'+str(proxy.server_port))
def run(command,log,timeout):
    with (evidence/(log+'.log')).open('wb') as out:
        subprocess.run(command,stdout=out,stderr=subprocess.STDOUT,env=env,check=True,timeout=timeout)
try:
    run(['python3','/stage/procedure/network-proof.py',str(evidence/'network.json')],'network',15)
    run(['node','/stage/procedure/browser-proof.cjs',str(evidence/'browser.json')],'browser',50)
    run(['node','/stage/procedure/browser-default-origin-proof.cjs',str(evidence/'browser-default-origin.json')],'browser-default-origin',60)
    assert json.loads((evidence/'browser-default-origin.json').read_text())['passed']
    assert proxy.rejected>0,'Unsigned rejecting-proxy probe received no requests'
    # Rebuild the new source under the verified network boundary. The original
    # dist was verified before entry and is retained solely as a byte comparator.
    import hashlib,tarfile
    dist=root/spec['distDir'];old={r['path']:r['sha256'] for r in spec['distFiles']}
    run(['node','scripts/verify-dependency-lock.mjs'],'check-lock',30)
    run(['node','--import','tsx','scripts/verify-social-graph-snapshot.ts'],'check-seed',45)
    run(['pnpm','run','build'],'build-candidate',300)
    new={str(f.relative_to(dist)):hashlib.sha256(f.read_bytes()).hexdigest() for f in sorted(dist.rglob('*')) if f.is_file()}
    changed=sorted(k for k in set(new)|set(old) if new.get(k)!=old.get(k))
    assert new and 'index.html' in changed
    (evidence/'candidate-dist.json').write_text(json.dumps(dict(source=spec['commit'],files=new,changed=changed),indent=2))
    with tarfile.open(evidence/'new-dist.tar.gz','w:gz') as archive:
        for name in new:archive.add(dist/name,arcname=name,recursive=False)
    # Match the paired unsigned RTC control only in the two ephemeral source-runtime
    # peer contexts, after the routing and raw-worker denial checks. No capture.
    assert os.environ['FIPS_DUMMY']=='1' and spec['env']['PLAYWRIGHT_RUNTIME_PORT']=='4181'
    case=root/'tests/fips-p2p.spec.ts';raw=case.read_bytes();text=raw.decode()
    sha=lambda b:hashlib.sha256(b).hexdigest()
    assert sha(raw)==spec['testFiles']['tests/fips-p2p.spec.ts']
    split=text.index("test('automatically exchanges live events")
    head,body=text[:split],text[split:]
    origin="http://127.0.0.1:4181"
    for expected,ctx in [("  await page.goto('/');",'page.context()'),("  const secondPage = await secondContext.newPage();",'secondContext')]:
        assert body.count(expected)==1
        body=body.replace(expected, f"  await {ctx}.grantPermissions(['microphone'], {{ origin: '{origin}' }});\n"+expected)
    case.write_text(head+body)
    assert (head+body).count("grantPermissions(['microphone']")==2
    (evidence/'fixture-origin.json').write_text(json.dumps(dict(source=spec['commit'],before=sha(raw),after=sha(case.read_bytes()),origin=origin,contexts=2,permission='microphone',captureInvoked=False)))
    config=root/'playwright.graph-qual.config.ts'
    assert not config.exists()
    config.write_text("""import { defineConfig } from '@playwright/test';
import original from './playwright.config';
export default defineConfig({...original,workers:1,retries:0,
 reporter:[['list'],['json',{outputFile:process.env.GRAPH_REPORT}]],
 use:{...original.use,trace:'retain-on-failure',launchOptions:{channel:'chromium',
 args:['--disable-features=WebRtcHideLocalIpsWithMdns','--allow-loopback-in-peer-connection',
 '--proxy-server='+process.env.REJECT_PROXY,'--proxy-bypass-list=localhost;127.0.0.1;[::1]']}}});
""")
    t=root/'src/lib/appFeedPreferences.ts';raw=t.read_text();assert raw.count('  async function handleRefreshHome(): Promise<void> {')==1;t.write_text(raw.replace('  async function handleRefreshHome(): Promise<void> {',"  async function handleRefreshHome(): Promise<void> {\n    console.log('BUFFER_OBS refresh', deps.route.kind, deps.homeRefreshPending);"))
    t=root/'src/lib/appLiveHomeController.ts';raw=t.read_text();assert raw.count('  function resetLiveHomeFeedBuffer(): void {')==1;t.write_text(raw.replace('  function resetLiveHomeFeedBuffer(): void {',"  function resetLiveHomeFeedBuffer(): void {\n    console.log('BUFFER_OBS reset', state.liveHomeFeedBuffer.events.length);"))
    t=root/'src/lib/appHistoryNavigation.ts';raw=t.read_text();assert raw.count('  function navigateHomeTab(tab: HomeTab): void {')==1;t.write_text(raw.replace('  function navigateHomeTab(tab: HomeTab): void {',"  function navigateHomeTab(tab: HomeTab): void {\n    console.log('BUFFER_OBS navigate', tab, deps.route.kind, deps.route.tab);"))
    t=root/'src/lib/appLiveHomeController.ts';raw=t.read_text();assert raw.count('    state.liveHomeFeedBuffer = bufferLiveHomeFeedEvent(')==1;t.write_text(raw.replace('    state.liveHomeFeedBuffer = bufferLiveHomeFeedEvent(',"    console.log('BUFFER_OBS insert', /^(buffered|route-reset|tab-refresh-reset|live)-fips-/.exec(event.content)?.[1] ?? 'other', state.homeRefreshPending, state.liveHomeFeedBuffer.events.some(e => e.id === event.id));\n    state.liveHomeFeedBuffer = bufferLiveHomeFeedEvent("))
    t=root/'tests/fips-p2p.spec.ts';raw=t.read_text();assert raw.count('  const secondPage = await secondContext.newPage();')==1;t.write_text(raw.replace('  const secondPage = await secondContext.newPage();',"  const secondPage = await secondContext.newPage();\n    secondPage.on('console', msg => { if (msg.text().startsWith('BUFFER_OBS')) console.log(msg.text()); });"))
    t=root/'src/lib/components/app-projection/AppLiveProjection.svelte';raw=t.read_text();assert raw.count('        homeState.bufferedLiveHomePostCount = nextBufferedEvents.length;')==1;t.write_text(raw.replace('        homeState.bufferedLiveHomePostCount = nextBufferedEvents.length;',"        homeState.bufferedLiveHomePostCount = nextBufferedEvents.length;\n    console.log('BUFFER_OBS count', nextBufferedEvents.length, nextBufferedEvents.map(e => /^(buffered|route-reset|tab-refresh-reset|live)-fips-/.exec(e.content)?.[1] ?? 'other'));"))
    command=['pnpm','exec','playwright','test','--config',config.name]
    env['GRAPH_REPORT']=str(evidence/'buffer-observation.json')
    run(command+['tests/fips-p2p.spec.ts','--grep','automatically exchanges live events'],'buffer-observation',230)
finally:
    run(['python3','/stage/procedure/network-proof.py',str(evidence/'network-after.json')],'network-after',15)
    proxy.shutdown();proxy.server_close()
