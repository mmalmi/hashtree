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
env=dict(os.environ,CI='1',FIPS_APP_EVIDENCE=str(evidence),**spec['env'],REJECT_PROXY='http://127.0.0.1:'+str(proxy.server_port))
def run(command,log,timeout):
    with (evidence/(log+'.log')).open('wb') as out:
        subprocess.run(command,stdout=out,stderr=subprocess.STDOUT,env=env,check=True,timeout=timeout)
try:
    run(['python3','/stage/procedure/network-proof.py',str(evidence/'network.json')],'network',15)
    run(['node','/stage/procedure/browser-proof.cjs',str(evidence/'browser.json')],'browser',50)
    run(['node','/stage/procedure/browser-default-origin-proof.cjs',str(evidence/'browser-default-origin.json')],'browser-default-origin',60)
    assert json.loads((evidence/'browser-default-origin.json').read_text())['passed']
    assert proxy.rejected>0,'Unsigned rejecting-proxy probe received no requests'
    import hashlib,tarfile
    file_sha=lambda p:hashlib.sha256(p.read_bytes()).hexdigest()
    assert file_sha(root/'package.json')==spec['packageSha256'] and file_sha(root/'pnpm-lock.yaml')==spec['lockSha256']
    assert file_sha(root/'scripts/verify-dependency-lock.mjs')==spec['dependencyGuardSha256']
    run(['node','scripts/verify-dependency-lock.mjs'],'check-lock',30)
    run(['node','/stage/procedure/installed-runtime-proof.cjs',str(evidence/'installed-runtime.json')],'installed-runtime',30)
    run(['node','--import','tsx','scripts/verify-social-graph-snapshot.ts'],'check-seed',45)
    binding=spec['diagnosticBuild']
    assert binding['diagnosticOnly'] and binding['originRunPassed'] is False and binding['notReleaseAccepted']
    archive=pathlib.Path('/stage/procedure')/binding['archive'];assert file_sha(archive)==binding['archiveSha256'] and archive.stat().st_size==binding['archiveBytes']
    dist=root/spec['distDir'];assert not dist.exists()
    with tarfile.open(archive) as bundle:
        members=bundle.getmembers()
        assert all(m.isfile() and not m.name.startswith('/') and '..' not in pathlib.PurePosixPath(m.name).parts for m in members)
        assert len({m.name for m in members})==len(members)
        assert {m.name:hashlib.sha256(bundle.extractfile(m).read()).hexdigest() for m in members}==binding['files']
        bundle.extractall(dist,filter='data')
    new={str(f.relative_to(dist)):file_sha(f) for f in sorted(dist.rglob('*')) if f.is_file()}
    assert new==binding['files']
    (evidence/'diagnostic-build.json').write_text(json.dumps(binding,indent=2)+'\n')
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
    for name,cases,expected,seconds in [('private',spec['specs'],4,940)]:
        env['GRAPH_REPORT']=str(evidence/(name+'.json'));env['GRAPH_OUTPUT']=str(evidence/name)
        with (evidence/(name+'.log')).open('wb') as log:
            result=subprocess.run(command+cases,env=env,stdout=log,stderr=subprocess.STDOUT,timeout=seconds)
        stats=json.loads((evidence/(name+'.json')).read_text())['stats']
        counts[name]=dict(exit=result.returncode,stats=stats,expected=expected)
    passed=all(row['exit']==0 and row['stats']['expected']==row['expected'] and row['stats']['unexpected']==row['stats']['flaky']==row['stats']['skipped']==0 for row in counts.values())
    (evidence/'diagnostic-gates.json').write_text(json.dumps(dict(passed=passed,source=spec['commit'],counts=counts,diagnosticOnly=True,releaseAccepted=False))+'\n')
    assert file_sha(root/'pnpm-lock.yaml')==spec['lockSha256'] and file_sha(root/'package.json')==spec['packageSha256']
    assert {str(f.relative_to(dist)):file_sha(f) for f in sorted(dist.rglob('*')) if f.is_file()}==new,'Qualified distribution mutated'
    run(['python3','/stage/procedure/transport-control.py',str(evidence)],'transport-control',20)
    assert passed,'Original private contact cases failed; preserve diagnostic evidence'

finally:
    run(['python3','/stage/procedure/network-proof.py',str(evidence/'network-after.json')],'network-after',15)
    proxy.shutdown();proxy.server_close()
