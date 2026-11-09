"""Host-only unsigned diagnostic: add one unconnected interface to an owned netns."""
import json,os,pathlib,subprocess,sys

container_id,name,image_id,uid=sys.argv[1:]
allowed_names=('iris-fips-test-contacts','iris-fips-test-sites','iris-fips-test-cc')
assert len(container_id)==64 and name in allowed_names
assert uid.isdecimal() and int(uid)!=0
def inspect():
    value=json.loads(subprocess.check_output(['docker','inspect',container_id],text=True,timeout=5))[0]
    assert value['Id']==container_id and value['Name']=='/'+name and value['Image']==image_id
    assert value['Config']['Labels']['iris.task']=='app-fips-isolated'
    assert value['Config']['User'].split(':')[0]==uid
    h=value['HostConfig']
    assert h['NetworkMode']=='none' and h['PidMode']=='' and h['CapDrop']==['ALL']
    assert value['State']['Running'] and value['State']['Pid']>1
    return value
initial=inspect();pid=initial['State']['Pid']
def identity():
    current=inspect();assert current['State']['Pid']==pid
    stat=pathlib.Path(f'/proc/{pid}/stat').read_text()
    return {'pid':pid,'startTicks':stat.rsplit(')',1)[1].split()[19],
        'net':os.readlink(f'/proc/{pid}/ns/net'),'pidNamespace':os.readlink(f'/proc/{pid}/ns/pid')}
before=identity()
assert before['net']!=os.readlink('/proc/self/ns/net')
assert before['pidNamespace']!=os.readlink('/proc/self/ns/pid')
fd=os.open(f'/proc/{pid}/ns/net',os.O_RDONLY)
try:
    assert os.fstat(fd).st_ino==os.stat(f'/proc/{pid}/ns/net').st_ino
    def namespace_command(*args):
        assert identity()==before,'Container identity changed before namespace command'
        result=subprocess.check_output(['nsenter',f'--net=/proc/self/fd/{fd}','--',*args],pass_fds=(fd,),text=True,timeout=5)
        assert identity()==before,'Container identity changed during namespace command'
        return result
    def ip(*args):return namespace_command('ip',*args)
    links=json.loads(ip('-j','link','show'));assert [v['ifname'] for v in links]==['lo']
    ip('link','add','fixture0','type','dummy')
    namespace_command('sysctl','-q','-w','net.ipv6.conf.fixture0.disable_ipv6=1')
    assert namespace_command('sysctl','-n','net.ipv6.conf.fixture0.disable_ipv6').strip()=='1'
    ip('link','set','dev','fixture0','addrgenmode','none')
    ip('addr','add','192.0.2.1/32','dev','fixture0','noprefixroute')
    ip('link','set','dev','fixture0','up')
    links=json.loads(ip('-j','-d','link','show'))
    assert sorted(v['ifname'] for v in links)==['fixture0','lo']
    fixture=next(v for v in links if v['ifname']=='fixture0')
    assert fixture['linkinfo']['info_kind']=='dummy' and 'UP' in fixture['flags']
    addresses=json.loads(ip('-j','addr','show','dev','fixture0'))[0]['addr_info']
    assert len(addresses)==1 and addresses[0]['local']=='192.0.2.1' and addresses[0]['prefixlen']==32
    routes=json.loads(ip('-j','route','show','table','all'))
    routes6=json.loads(ip('-j','-6','route','show','table','all'))
    print(json.dumps({'phase':'beforeRouteAdmission','identity':before,'dummyIpv6Disabled':True,'interfaces':links,'fixtureAddresses':addresses,'routes4':routes,'routes6':routes6},indent=2),flush=True)
    assert all(v.get('dev')=='lo' or (v.get('dev')=='fixture0' and v.get('type')=='local' and v['dst']=='192.0.2.1') for v in routes)
    assert all(v.get('dev')=='lo' for v in routes6)
    print(json.dumps({'containerId':container_id,'containerName':name,'identity':before,'hostNetworkNamespace':os.readlink('/proc/self/ns/net'),'interfaces':links,'fixtureAddresses':addresses,'routes4':routes,'routes6':routes6},indent=2))
finally:os.close(fd)
