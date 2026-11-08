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
tail_events=[]
replay_attempts=0

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
 send({'id':request_id,'method':'tool.call','params':{'callId':call_id,'nativeCallId':'native-'+call_id,'name':r['tool'],'input':r.get('goal_output',{'slot':'result','value':{'nonce':r['nonce']}}),'runId':r['runId'],'contextId':'wrong' if wrong else r['contextId'],'traceparent':r.get('tooltrace',r.get('traceparent'))}})
for line in sys.stdin:
 m=json.loads(line);method=m.get('method');params=m.get('params',{})
 if not method:
  pending-=1
  if pending<=0:finish()
  continue
 if method=='initialize':
  if mode=='slow_handshake':
   with open(os.environ['MILKIE_FIXTURE_HANDSHAKE_FILE'],'w') as marker:marker.write('waiting')
   time.sleep(1.2)
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
  if mode.startswith('tail_'):
   total=600 if mode=='tail_overflow' else 126
   parent=r['traceparent']
   r['tooltrace']=parent[:36]+'2222222222222222'+parent[52:]
   tail_events=[{'version':1,'id':f"{r['runId']}:{n}",'sourceId':r['runId'],'sequence':n,'runId':r['runId'],'contextId':r['contextId'],'traceId':parent[3:35],'timestamp':int(time.time()*1000),'monotonicNs':str(n),'spanId':'1111111111111111','parentSpanId':parent[36:52],'kind':'execution.started' if n==1 else 'execution.finished' if n==total else 'agent.activity','data':{'runtime':'fixture'} if n==1 else {'status':'succeeded','stopped':True} if n==total else {'category':'message'}} for n in range(1,total+1)]
   for event in tail_events[:-1]:send({'method':'execution.observation','params':event})
  elif r.get('traceparent'):send({'method':'execution.observation','params':{'version':1,'id':str(uuid.uuid4()),'runId':r['runId'],'contextId':r['contextId'],'traceId':r['traceparent'][3:35],'kind':'agent.activity','data':{'category':'message'}}})
  if mode.startswith('goal_'):
   stage=payload['task_input']
   if payload['state_id']=='_tl.planner':
    phase='planner'
    proposal={'protocol_version':1,'goal_spec_digest':stage['goal_spec_digest'],'profile_digest':stage['profile_digest'],'planning_mode':stage['planning_mode'],'nodes':{'review':{'capability_ref':'review@1','depends_on':['review'] if mode=='goal_bad_plan'else [],'inputs':{'question':{'kind':'goal','pointer':'/goal'},'document':{'kind':'goal','pointer':'/inputs/document_text'}}}},'outputs':{'report':{'node_id':'review','port':'report'}},'checks':{'quality':{'phase':'goal_final','checker_ref':'quality@1','targets':['report']}}}
    r['goal_output']={'slot':'proposal','value':{'protocol_version':1,'kind':'proposal','proposal':proposal}}
    if mode=='goal_need_input' and payload['visit']==1:r['goal_output']={'slot':'proposal','value':{'protocol_version':1,'kind':'needs_input','input_schema_ref':'text','reason':'请确认审阅关注点'}}
    elif mode=='goal_need_input':assert len(payload['signal_facts'])==1
   elif payload['state_id'].startswith('_tl.check.'):
    phase='final'
    verdict='fail'if mode=='goal_not_met'else'error'if mode=='goal_check_error'else'pass'
    r['goal_output']={'slot':'evaluation','value':{'protocol_version':1,'verdict':verdict,'results':[{'id':rule['id'],'verdict':verdict,'message':'Controlled protocol-fixture result','evidence_refs':stage['evidence_refs']}for rule in stage['rules']['rules']],'evidence_refs':stage['evidence_refs'],'violations':[]}}
   else:
    phase='work'
    r['goal_output']={'slot':'report','value':{'summary':stage['document'][:3000],'findings':['Controlled protocol-fixture finding']}}
   save()
   if os.environ.get('MILKIE_FIXTURE_GOAL_HOLD_STAGE')==phase:time.sleep(1.5)
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
 elif method=='tool.pending':
  send({'id':m['id'],'result':[{'callId':'unknown-effect'}] if mode in ['pending_effect','tail_pending_effect'] else []})
  if mode in ['late_after_finish','tail_pending_effect']:
   time.sleep(.05);call(call_id='after-finish',request_id='tool:after-finish')
 elif method=='observation.read':
  if mode.startswith('tail_'):
   replay_attempts+=1
   if mode=='tail_delayed':time.sleep(.65)
   if mode=='tail_retry' and replay_attempts==1:time.sleep(1.2)
   if mode=='tail_timeout':time.sleep(5)
   if mode=='tail_protocol':
    send({'id':m['id'],'result':{'events':'invalid','nextSequence':0,'status':{'complete':True}}});continue
   source=tail_events[:-1] if mode=='tail_missing' else tail_events
   page=[e for e in source if e['sequence']>params.get('afterSequence',0)][:params.get('limit',100)]
   send({'id':m['id'],'result':{'events':page,'nextSequence':page[-1]['sequence']if page else params.get('afterSequence',0),'status':{'complete':mode!='tail_missing','lastSequence':source[-1]['sequence'],'dropped':1 if mode=='tail_missing'else 0,'state':'incomplete'if mode=='tail_missing'else'finished'}}});continue
  if mode in ['slow_observation','late_after_finish']:
   with open(os.environ['MILKIE_FIXTURE_OBSERVATION_FILE'],'w') as marker:marker.write('native result and pending tools were checked')
   time.sleep(2)
  send({'id':m['id'],'result':{'events':'malformed-observation-page' if mode=='bad_observation' else [],'nextSequence':0,'status':{'complete':False}}})
 else:send({'id':m['id'],'error':{'code':-32601,'message':'unknown fixture method'}})
