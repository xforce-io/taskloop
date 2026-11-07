#!/usr/bin/env python3
"""Deterministic Milkie protocol peer, not evidence for a native Harness."""
import json,os,sys,time,uuid
args=sys.argv[1:]
root=args[args.index('--data-dir')+1]
os.makedirs(root,exist_ok=True)
file=os.path.join(root,'fixture-record.json')
mode=os.environ.get('MILKIE_FIXTURE_MODE','normal')
try:
 with open(file) as f:r=json.load(f)
except FileNotFoundError:r={}
pending=0

def save():
 with open(file,'w') as f:json.dump(r,f)
def send(m):
 text=json.dumps(dict(jsonrpc='2.0',**m))+'\n'
 if mode=='fragment':
  mid=len(text)//2;sys.stdout.write(text[:mid]);sys.stdout.flush();time.sleep(.02);sys.stdout.write(text[mid:]);sys.stdout.flush()
 else:sys.stdout.write(text);sys.stdout.flush()
def finish(status='succeeded',stopped=True):
 r.update(status=status,stopped=stopped,finishedAt=int(time.time()*1000));save()
def call(call_id='host-call',request_id='tool:1',wrong=False):
 send({'id':request_id,'method':'tool.call','params':{'callId':call_id,'nativeCallId':'native-'+call_id,'name':r['tool'],'input':{'slot':'result','value':{'nonce':r['nonce']}},'runId':r['runId'],'contextId':'wrong' if wrong else r['contextId'],'traceparent':r.get('traceparent')}})
for line in sys.stdin:
 m=json.loads(line);method=m.get('method');params=m.get('params',{})
 if not method:
  pending-=1
  if pending<=0:finish()
  continue
 if method=='initialize':
  send({'id':m['id'],'result':{'protocolVersion':99 if mode=='bad_version' else 1,'capabilities':{'supported':True,'hostTools':True,'cancel':True,'timeout':True,'observation':{'version':1}}}})
 elif method=='context.create':
  r['contextId']=str(uuid.uuid4());save();send({'id':m['id'],'result':{'contextId':r['contextId']}})
 elif method=='execution.start':
  payload=json.loads(params['input']);r.update(runId=str(uuid.uuid4()),status='running',stopped=False,nonce=payload['flow_input'].get('nonce','direct'),tool=next(t['name'] for t in params['constraints']['tools'] if payload['tool_mapping'][t['name']]=='artifact.write'))
  r['traceparent']=params['constraints'].get('observation',{}).get('traceparent')
  r.pop('finishedAt',None)
  if os.environ.get('MILKIE_FIXTURE_STARTED_FILE'):
   with open(os.environ['MILKIE_FIXTURE_STARTED_FILE'],'a') as marker:marker.write(r['runId']+'\n')
  save();send({'id':m['id'],'result':{'runId':r['runId']}})
  if r.get('traceparent'):send({'method':'execution.observation','params':{'version':1,'id':str(uuid.uuid4()),'runId':r['runId'],'contextId':r['contextId'],'traceId':r['traceparent'][3:35],'kind':'agent.activity','data':{'category':'message'}}})
  if mode=='crash':os._exit(7)
  if mode=='harness_exit':finish('failed');continue
  if payload['flow_input'].get('hold') or mode=='stop_unknown':continue
  if mode=='empty':finish();continue
  pending=2 if mode in ['duplicate','duplicate_wire'] else 1
  call(wrong=mode=='wrong_context')
  if mode=='duplicate':call(request_id='tool:2')
  if mode=='duplicate_wire':call(call_id='different-call',request_id='tool:1')
 elif method=='execution.query':send({'id':m['id'],'result':r})
 elif method=='execution.cancel':
  finish('unknown' if mode=='stop_unknown' else 'cancelled',mode!='stop_unknown')
  send({'id':m['id'],'result':r})
  if mode=='late':call(call_id='late-call',request_id='tool:late')
 elif method=='tool.pending':send({'id':m['id'],'result':[{'callId':'unknown-effect'}] if mode=='pending_effect' else []})
 elif method=='observation.read':send({'id':m['id'],'result':{'events':[],'nextSequence':0,'status':{'complete':False}}})
 else:send({'id':m['id'],'error':{'code':-32601,'message':'unknown fixture method'}})
