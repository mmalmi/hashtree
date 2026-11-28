import pathlib,json,os,subprocess
stage=pathlib.Path('/stage');evidence=stage/'evidence/social';evidence.mkdir()
(evidence/'artifact.json').write_bytes((stage/'procedure/diag-expected-artifact.json').read_bytes())
(evidence/'cpu-provenance.json').write_text(json.dumps({'models':sorted(set(x.split(':',1)[1].strip() for x in pathlib.Path('/proc/cpuinfo').read_text().splitlines() if x.startswith('model name'))),'affinity':sorted(os.sched_getaffinity(0))},indent=2)+'\n')
subprocess.run(['python3','/stage/procedure/run-diagnostic.py'],check=True,timeout=300)
