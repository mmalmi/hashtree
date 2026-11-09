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
    import hashlib
    proof=json.loads((base/'procedure/private-qualified-inputs.json').read_text());dist=root/spec['distDir']
    files=[dict(path=str(f.relative_to(dist)),sha256=hashlib.sha256(f.read_bytes()).hexdigest(),bytes=f.stat().st_size)for f in sorted(dist.rglob('*'))if f.is_file()]
    assert proof['source']==spec['commit']and files==proof['distFiles']==spec['distFiles']
    raw=(base/'procedure/reused-dist.tar.gz').read_bytes();assert hashlib.sha256(raw).hexdigest()==proof['distArchiveSha256']
    (evidence/'new-dist.tar.gz').write_bytes(raw);(evidence/'reused-dist.json').write_text(json.dumps(proof)+'\n')
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
export default defineConfig({...original,workers:1,retries:0,outputDir:process.env.GRAPH_OUTPUT,
 reporter:[['list'],['json',{outputFile:process.env.GRAPH_REPORT}]],
 use:{...original.use,screenshot:'on',trace:'retain-on-failure',launchOptions:{channel:'chromium',
 args:['--disable-features=WebRtcHideLocalIpsWithMdns','--allow-loopback-in-peer-connection',
 '--proxy-server='+process.env.REJECT_PROXY,'--proxy-bypass-list=localhost;127.0.0.1;[::1]']}}});
""")
    command=['pnpm','exec','playwright','test','--config',config.name]
    counts={}
    for name,cases,expected,seconds in [('selected',[case for case in spec['specs']if case!='tests/private-contact-sync-ui.spec.ts'],18,940),('idle',['tests/idle-cpu.spec.ts'],1,150)]:
        env['GRAPH_REPORT']=str(evidence/(name+'.json'));env['GRAPH_OUTPUT']=str(evidence/name)
        with (evidence/(name+'.log')).open('wb') as log:
            result=subprocess.run(command+cases,env=env,stdout=log,stderr=subprocess.STDOUT,timeout=seconds)
        stats=json.loads((evidence/(name+'.json')).read_text())['stats']
        counts[name]=dict(exit=result.returncode,stats=stats,expected=expected)
    with (evidence/'full-profile.log').open('wb') as log:
        remaining=subprocess.run(['python3','/stage/procedure/finish-full.py'],env=env,stdout=log,stderr=subprocess.STDOUT,timeout=1700)
    passed=remaining.returncode==0 and all(row['exit']==0 and row['stats']['expected']==row['expected']
        and row['stats']['unexpected']==row['stats']['flaky']==row['stats']['skipped']==0 for row in counts.values())
    (evidence/'completed-gates.json').write_text(json.dumps(dict(passed=passed,source=spec['commit'],counts=counts,
        remainingExit=remaining.returncode,scope='Remaining original release profile on the private-sync-qualified exact build'))+'\n')
    assert passed,'Original release profile failed'

finally:
    run(['python3','/stage/procedure/network-proof.py',str(evidence/'network-after.json')],'network-after',15)
    proxy.shutdown();proxy.server_close()
