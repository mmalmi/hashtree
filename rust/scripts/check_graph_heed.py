import re,hashlib,json,os,pathlib,signal,subprocess,sys,time,shutil
SCRIPT=pathlib.Path(__file__).resolve()
ROOT=SCRIPT.parents[1]
HERE=pathlib.Path(__file__).resolve().parent
if sys.argv[1]=='namespace':
 uid,gid,home,netns,pidns,proof,binary,*args=sys.argv[2:]
 assert os.getuid()==0
 subprocess.run(['ip','link','set','lo','up'],check=True)
 subprocess.run(['mount','-t','sysfs','-o','ro,nosuid,nodev,noexec','sysfs','/sys'],check=True)
 env=dict(os.environ,HOME=home,HTREE_CONFIG_DIR=home+'/config',GRAPH_HOST_NETNS=netns,GRAPH_HOST_PIDNS=pidns)
 os.execve('/usr/bin/setpriv',['setpriv','--reuid='+uid,'--regid='+gid,'--clear-groups','--bounding-set=-all','--inh-caps=-all','--ambient-caps=-all','--no-new-privs','/usr/bin/python3',str(SCRIPT),'inner',proof,binary,*args],env)
if sys.argv[1]=='inner':
 proof,binary,*args=sys.argv[2:]
 subprocess.run([sys.executable,str(HERE/'graph-network-proof.py'),proof],check=True,timeout=15)
 os.execv(binary,[binary,*args])
assert sys.argv[1]=='run'
OUT=pathlib.Path(sys.argv[2]).resolve();OUT.mkdir(parents=True,exist_ok=False)
os.chdir(ROOT)
record={'source':subprocess.check_output(['git','rev-parse','HEAD'],text=True).strip(),'steps':[],'tests':{},'passed':False}
def sha(p):return hashlib.sha256(pathlib.Path(p).read_bytes()).hexdigest()
def run(name,cmd,seconds):
 out=OUT/(name+'.out');err=OUT/(name+'.log');start=time.monotonic()
 with out.open('xb') as stdout,err.open('xb') as stderr:
  process=subprocess.Popen(cmd,stdout=stdout,stderr=stderr,start_new_session=True)
  try:
   while process.poll() is None:
    assert shutil.disk_usage(ROOT).free>=20*1024**3,'Hosted disk floor'
    assert time.monotonic()-start<seconds,'Gate deadline: '+name
    time.sleep(1)
  finally:
   if process.poll() is None:os.killpg(process.pid,signal.SIGKILL)
   code=process.wait()
 record['steps'].append({'name':name,'exitCode':code,'seconds':round(time.monotonic()-start,2)})
 if code:print(out.read_text(errors='replace')[-3000:]+err.read_text(errors='replace')[-3000:]);raise RuntimeError(name+' failed')
 return out.read_text()
def check_test_result(label,result,count):
 summaries=re.findall(r'test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored;',result)
 assert summaries,'Missing final test summary'
 passed,failed,ignored=map(int,summaries[-1])
 expected={'socialgraph::tests::benchmark_nostr_btree_query_tradeoffs','socialgraph::tests::benchmark_query_events_large_dataset'} if label=='cli-hashtree_cli' else set()
 actual={line.split(' ... ',1)[0].removeprefix('test ') for line in result.splitlines() if ' ... ignored' in line}
 assert actual==expected and ignored==len(expected),'Unexpected ignored test'
 assert failed==0 and passed+ignored==count,'Incomplete or failed test selection'
 return passed,ignored
try:
 assert shutil.disk_usage(ROOT).free>=30*1024**3
 record['cargoLockSha256']=sha('Cargo.lock')
 record['rustc']=subprocess.check_output(['rustc','-Vv'],text=True)
 assert record['rustc'].startswith('rustc 1.99.0 ')
 record['systemPackages']=subprocess.check_output(['dpkg-query','-W'],text=True)
 # Compilation may fetch locked packages; no test binary is executed until isolated.
 built={}
 for label,cmd in [('fork',['cargo','test','--locked','-p','hashtree-nostr-social-graph-heed','--no-run','--message-format=json']),('cli',['cargo','test','--locked','-p','hashtree-cli','--lib','--no-run','--message-format=json'])]:
  raw=run('build-'+label,cmd,2400)
  for line in raw.splitlines():
   x=json.loads(line)
   if x.get('reason')=='compiler-artifact' and x.get('profile',{}).get('test') and x.get('executable'):
    if label=='cli' and x['target']['name']!='hashtree_cli':continue
    built[label+'-'+x['target']['name']]=(x['executable'],['socialgraph::'] if label=='cli' else [])
 assert len(built)==6,built
 for label,(binary,filters) in built.items():
  listing=subprocess.check_output([binary,*filters,'--list'],text=True);count=sum(s.endswith(': test') for s in listing.splitlines());assert count>0
  if label=='cli-hashtree_cli':
   assert listing.count('socialgraph::alias_cache_tests::')==2
  home=OUT/('home-'+label);home.mkdir()
  cmd=['sudo','unshare','--net','--mount','--pid','--fork','--mount-proc','--propagation','private','--kill-child=KILL','/usr/bin/python3',str(SCRIPT),'namespace',str(os.getuid()),str(os.getgid()),str(home),os.readlink('/proc/self/ns/net'),os.readlink('/proc/self/ns/pid'),str(OUT/(label+'-network.json')),binary,*filters,'--nocapture','--test-threads=1']
  result=run(label,cmd,600);passed,ignored=check_test_result(label,result,count)
  record['tests'][label]={'passed':passed,'ignored':ignored,'binarySha256':sha(binary),'loopbackOnly':True,'isolatedHome':True}
 assert sum(v['passed'] for k,v in record['tests'].items() if k.startswith('fork-'))==24
 assert sha('Cargo.lock')==record['cargoLockSha256']
 subprocess.run(['git','diff','--exit-code'],check=True)
 record['passed']=True
finally:
 (OUT/'receipt.json').write_text(json.dumps(record,indent=2)+'\n')
 print(json.dumps({'passed':record['passed'],'tests':record['tests']}))
