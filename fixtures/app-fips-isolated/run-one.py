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
    env['GRAPH_REPORT']=str(evidence/'peer-diagnostic.json')
    with (evidence/'peer-diagnostic.log').open('wb') as output:
        run_case=subprocess.run(command+['tests/fips-p2p.spec.ts','--grep','automatically exchanges live events'],env=env,stdout=output,stderr=subprocess.STDOUT,timeout=260)
    assert run_case.returncode==0
finally:
    run(['python3','/stage/procedure/network-proof.py',str(evidence/'network-after.json')],'network-after',15)
    proxy.shutdown();proxy.server_close()
