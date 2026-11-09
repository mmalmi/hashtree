"""Run one bounded command; retain whole-container child OOM evidence."""
from pathlib import Path
import json,os,subprocess,sys,threading,time
name=sys.argv[1];command=sys.argv[2:];E=Path('/stage/evidence');errors=[];stop=threading.Event();samples=[]
def sample():
 cg=Path('/sys/fs/cgroup')
 return {'at':time.monotonic(),'current':int((cg/'memory.current').read_text()),'peak':int((cg/'memory.peak').read_text()),'events':dict(line.split() for line in (cg/'memory.events').read_text().splitlines())}
def watch():
 while True:
  try:samples.append(sample())
  except Exception as error:errors.append(repr(error));return
  if stop.wait(2):return
thread=threading.Thread(target=watch);thread.start();code=None
try:code=subprocess.run(command,check=False).returncode
finally:
 stop.set();thread.join(timeout=5);samples.append(sample())
 result={'exitCode':code,'samplerErrors':errors,'samples':samples,'noChildOom':all(int(row['events']['oom_kill'])==0 for row in samples)}
 (E/(name+'-resources.json')).write_text(json.dumps(result,indent=2)+'\n')
assert not errors and not thread.is_alive() and result['noChildOom']
sys.exit(code if code is not None else 1)
