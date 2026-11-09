"""Preserve a composed source profile while retaining each zero-retry result."""
import json,os,pathlib,re,subprocess,sys,time
root=pathlib.Path.cwd();app=sys.argv[1]
spec=next(a for a in json.loads(pathlib.Path('/stage/procedure/manifest.json').read_text())['apps'] if a['id']==app)
binding=spec['composedBrowserProfile'];scripts=json.loads((root/'package.json').read_text())['scripts']
assert all(scripts[name]==text for name,text in binding['sourceScripts'].items())
output=pathlib.Path(os.environ['PLAYWRIGHT_JSON_OUTPUT_FILE'])
assert output.name.endswith('-playwright.json') and not output.exists()
deadline=time.monotonic()+1800;phases=[]
for phase in binding['phases']:
    safe_phase=phase.replace(':','-')
    assert re.fullmatch(r'[A-Za-z0-9_-]+',safe_phase)
    report=output.with_name(output.name.removesuffix('-playwright.json')+'-'+safe_phase+'-playwright.json')
    assert not report.exists()
    command=['pnpm','run',phase,'--retries=0']
    subprocess.run(command,env=dict(os.environ,PLAYWRIGHT_JSON_OUTPUT_FILE=str(report)),check=True,timeout=max(1,deadline-time.monotonic()))
    assert report.is_file()
    phases.append({'sourceScript':phase,'command':command,'report':report.name})
(output.parent/'browser-profile-phases.json').write_text(json.dumps({'source':spec['commit'],'originalCommand':['pnpm','run','test:e2e:release'],'sourceScripts':binding['sourceScripts'],'phaseDeadlineSeconds':1800,'phases':phases},indent=2)+'\n')
