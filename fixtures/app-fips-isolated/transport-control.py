import json,pathlib,re,sys
app=pathlib.Path(sys.argv[1]);report=json.loads((app/'private.json').read_text());found=[]
def visit(s):
 for spec in s.get('specs',[]):
  for test in spec['tests']:
   for result in test.get('results',[]):
    for item in result.get('stdout',[]):
     for line in item.get('text','').splitlines():
      if line.startswith('PRIVATE_PUBLICATION_DIAGNOSTIC '):found.append(json.loads(line.split(' ',1)[1]))
 for child in s.get('suites',[]):visit(child)
visit(report);assert len(found)==1,'Missing unique publication diagnostic'
d=found[0];assert set(d)=={'devices','collectionFailures','browserTransport','relayPublications','relayPublicationObservation'}
assert d['collectionFailures']==0 and d['relayPublicationObservation']=={'dropped':0}
assert len(d['devices'])==len(d['browserTransport'])==2
hexid=lambda x:isinstance(x,str) and re.fullmatch('[0-9a-f]{64}',x)
stored=set()
for r in d['relayPublications']:
 assert set(r)=={'at','id','accepted','reason'} and hexid(r['id']) and type(r['accepted']) is bool and r['reason'] in ['stored','temporarily-unavailable','invalid-signature']
 if r['reason']=='stored':assert r['accepted'];stored.add(r['id'])
assert stored and len(d['relayPublications'])<=1000
schemas={'network-enabled':{'at','stage','worker'},'socket-created':{'at','stage','worker','connection','destination'},'socket-handshake':{'at','stage','worker','connection','destination','status'},'socket-error':{'at','stage','worker','connection','destination','code'},'socket-closed':{'at','stage','worker','connection','destination'},'socket-event':{'at','stage','worker','connection','destination','id'},'socket-ok':{'at','stage','worker','connection','destination','id','accepted','reason'}}
errors={'ERR_CONNECTION_REFUSED','ERR_CONNECTION_CLOSED','ERR_CONNECTION_RESET','ERR_NETWORK_CHANGED','ERR_INTERNET_DISCONNECTED','ERR_INSUFFICIENT_RESOURCES','ERR_WS_PROTOCOL_ERROR','ERR_TIMED_OUT','ERR_FAILED','closed-before-open','other'}
controls=[]
for device,network in zip(d['devices'],d['browserTransport']):
 assert set(network)=={'dropped','protocolErrors','parseErrors','workers','networkEnabled','rows'}
 assert network['dropped']==network['protocolErrors']==network['parseErrors']==0
 assert 0<network['networkEnabled']<=network['workers']<=16 and len(network['rows'])<=1000
 for r in network['rows']:
  assert r['stage'] in schemas and set(r)==schemas[r['stage']] and type(r['at']) is int and 1<=r['worker']<=16
  if 'connection' in r:assert type(r['connection']) is int and 1<=r['connection']<=512 and r['destination'] in ['private','other']
  if 'status' in r:assert type(r['status']) is int and 0<=r['status']<=999
  if 'code' in r:assert r['code'] in errors
  if 'id' in r:assert hexid(r['id'])
  if r['stage']=='socket-ok':assert type(r['accepted']) is bool and r['reason'] in ['stored','temporarily-unavailable','auth-required','other']
 sent={r['id'] for r in network['rows'] if r['stage']=='socket-event' and r['destination']=='private'}
 accepted={r['id'] for r in network['rows'] if r['stage']=='socket-ok' and r['destination']=='private' and r['accepted']}
 controls.append({'workers':network['workers'],'networkEnabled':network['networkEnabled'],'sentIds':len(sent),'positiveOkIds':len(accepted),'matchingRelayStoredIds':len(sent&accepted&stored)})
 assert sent&accepted&stored,'Worker observer did not cover a real private EVENT/positive OK'
 for r in device['rows']:
  if r['stage']=='worker-publish':assert type(r['destinationCount']) is int and 0<=r['destinationCount']<=16 and r['destination'] in ['empty','private-only','mixed-or-other']
(app/'transport-control.json').write_text(json.dumps({'passed':True,'diagnosticOnly':True,'releaseAccepted':False,'fixedSchemaVerified':True,'positiveControls':controls},indent=2)+'\n')
