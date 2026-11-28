import hashlib,json,os,pathlib,subprocess,time
root=pathlib.Path.cwd(); stage=pathlib.Path('/stage'); evidence=stage/'evidence/social';evidence.mkdir()
spec=json.loads((stage/'procedure/manifest.json').read_text())['apps'][0]
env=dict(os.environ,CI='1',**spec['env']); records=[]
def run(cmd,name,expected=0):
 start=time.monotonic()
 with (evidence/(name+'.log')).open('wb') as out: r=subprocess.run(cmd,env=env,stdout=out,stderr=subprocess.STDOUT,timeout=800)
 records.append(dict(name=name,exit=r.returncode,seconds=time.monotonic()-start))
 (evidence/'checks.json').write_text(json.dumps(dict(source=spec['commit'],baseline=spec['baseline'],records=records),indent=2))
 assert (r.returncode==0) if expected==0 else (r.returncode!=0),name
cmd=['pnpm','exec','playwright','test','--config=playwright.unit.config.ts','--workers=1','--reporter=list']
run(['node','scripts/verify-dependency-lock.mjs'],'verify-pins')
run(['pnpm','exec','eslint',*spec['changedLintFiles'],'--max-warnings','0'],'lint-changed')
run(['pnpm','run','build'],'build')
run(['pnpm','run','check'],'check')
config=root/'playwright.review.config.ts'
config.write_text("import { defineConfig } from '@playwright/test'; import original from './playwright.config'; export default defineConfig({...original,workers:1,retries:0,outputDir:'/stage/evidence/social/browser-output',reporter:[['list'],['json',{outputFile:'/stage/evidence/social/browser.json'}]],use:{...original.use,trace:'retain-on-failure'}});")
run(['pnpm','exec','playwright','test','--config='+config.name,'tests/notifications-pagination-ui.spec.ts','tests/post-search-consent-ui.spec.ts','tests/post-search-sheet.spec.ts','--grep','a read-only account loads grouped notifications|resolving a multi-line notification target|public search consent|runs only for an explicitly selected query|empty posts search loads recommendations'],'browser')
files={str(p.relative_to(root/'dist')):hashlib.sha256(p.read_bytes()).hexdigest() for p in (root/'dist').rglob('*') if p.is_file()}
(evidence/'artifact.json').write_text(json.dumps(dict(source=spec['commit'],files=files),indent=2))

