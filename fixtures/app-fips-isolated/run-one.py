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
    assert proxy.rejected>0,'Unsigned rejecting-proxy probe received no requests'
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
    command=['pnpm','exec','playwright','test','--config',config.name]
    cases=spec['specs'];env['GRAPH_REPORT']=str(evidence/'selected.json')
    selected=subprocess.run(command+cases,env=env,stdout=(evidence/'selected.log').open('wb'),stderr=subprocess.STDOUT,timeout=940)
    env['GRAPH_REPORT']=str(evidence/'idle.json')
    idle=subprocess.run(command+['tests/idle-cpu.spec.ts'],env=env,stdout=(evidence/'idle.log').open('wb'),stderr=subprocess.STDOUT,timeout=150)
    counts={}
    for name,code,expected in [('selected',selected.returncode,spec['expectedCases']),('idle',idle.returncode,1)]:
        report=json.loads((evidence/(name+'.json')).read_text());counts[name]=dict(exit=code,stats=report['stats'])
    (evidence/'results.json').write_text(json.dumps(dict(source=spec['commit'],counts=counts,rejected=proxy.rejected),indent=2)+'\n')
    assert all(row['exit']==0 for row in counts.values())
    assert counts['selected']['stats']['expected']==spec['expectedCases'] and counts['idle']['stats']['expected']==1
finally:
    run(['python3','/stage/procedure/network-proof.py',str(evidence/'network-after.json')],'network-after',15)
    proxy.shutdown();proxy.server_close()
