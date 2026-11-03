from pathlib import Path
import json,hashlib,re,sys
from jsonschema import Draft202012Validator
app=Path(sys.argv[1]).resolve();base=Path(__file__).resolve().parents[3];a=app/'docs/design/1-minimal-research-loop/contract';t=base/'docs/design/2-minimal-runtime-contract/contract'
checks=[]
def check(name, value):
    assert value,name
    checks.append({'name':name,'result':'pass'})
for path in list(a.glob('*.schema.json'))+list(t.glob('*.schema.json')):Draft202012Validator.check_schema(json.loads(path.read_text()))
check('business_and_framework_schemas_valid',True)
bundle=json.loads((a/'research.bundle.json').read_text());Draft202012Validator(json.loads((t/'bundle.schema.json').read_text())).validate(bundle)
check('active_inquiry_bundle_consumes_taskloop_format',True)
refs={bundle['flow']['input_schema']}
for entry in bundle['entries'].values():
 refs.add(entry['input_schema']);refs.update(o['schema'] for o in entry['outputs'].values())
for state in bundle['flow']['states'].values():
 if state['kind']=='wait':refs.update(v['schema'] for v in state['signals'].values())
check('business_schema_refs_resolve_without_vendored_builtins',all((a/n).is_file() if n!='task-input.schema.json' else (t/n).is_file() for n in refs))
check('six_research_stages_preserved',sum(s['kind']=='work' for s in bundle['flow']['states'].values())==6)
check('business_skill_ref_matches_local_version',all(e['skills']==['inquiry-deep-research@0.2.0'] for e in bundle['entries'].values()) and 'version: "0.2.0"' in (a/'skills/inquiry-deep-research/SKILL.md').read_text())
core=['bundle.schema.json','start-request.schema.json','snapshot.schema.json','signal-request.schema.json','cancel-request.schema.json','task-input.schema.json','submission.schema.json','check-result.schema.json','tool-receipt.schema.json']
check('framework_protocol_only_in_taskloop',all((t/n).is_file() and not(a/n).exists() for n in core))
check('framework_witness_and_second_business_removed_from_inquiry',not(a/'verify_contract.py').exists() and not(a/'document-review.bundle.json').exists() and not(a/'skills/document-review').exists())
view=json.loads((t/'snapshot.schema.json').read_text());ar=view['properties']['accepted_artifacts']['items']
check('public_snapshot_sufficient_for_app_projection','inputs' in view['required'] and {'state_id','visit','slot','id','sha256'}<=set(ar['required']) and 'producer_run_id' not in ar['required'])
acq=json.loads((a/'acquisition-fact.schema.json').read_text());tool=json.loads((t/'tool-receipt.schema.json').read_text())
check('domain_acquisition_has_no_private_execution_identity','run_id' not in acq['properties'])
check('generic_tool_receipt_has_no_http_fields',not {'http_status','requested_url','source_id'} & set(tool['properties']) and 'payload' in tool['required'])
tdoc=(base/'docs/design/2-minimal-runtime-contract.md').read_text();adoc=(app/'docs/design/1-minimal-research-loop.md').read_text()
check('five_public_instance_artifact_operations',set(re.findall(r'^\| (start|get|signal|cancel|read_artifact) \|',tdoc,re.M))=={'start','get','signal','cancel','read_artifact'})
check('application_avoids_private_runtime_protocol',not any(x in adoc for x in ['run_epoch','RunSubmission','lease_epoch','fencing_token','outbox']))
check('consumer_has_all_five_dependency_mappings',all('| D'+str(i)+' ' in adoc for i in range(1,6)))
for root,p in [(app,app/'docs/design/1-minimal-research-loop.md'),(base,base/'docs/design/2-minimal-runtime-contract.md')]:
 text=p.read_text();check(root.name+'_l2_has_13_sections',len(re.findall(r'^## \d+ ',text,re.M))==13)
 for target in re.findall(r'\]\(([^)]+)\)',text):
  if '://' in target or target.startswith('#'):continue
  assert (p.parent/target.split('#')[0]).exists(),str(p)+':'+target
 check(root.name+'_local_design_links_resolve',True)
for root,name,count in [(a,'research-verification.json',21),(t,'verification-result.json',85)]:
 result=json.loads((root/name).read_text());check(root.parent.name+'_witness_passed',result['passed'] and result['tests']==count)
 for f,h in result['source_sha256'].items():assert hashlib.sha256((root/f).read_bytes()).hexdigest()==h,f
 check(root.parent.name+'_witness_sources_match',True)
trace=json.loads((t/'trace.examples.json').read_text());proof=json.loads((t/'verification-result.json').read_text())
check('framework_trace_matches_current_model',trace['model_sha256']==proof['source_sha256']['verify_contract.py'] and trace['research_lock']==proof['bundle_locks']['research.bundle.json'])
summary={'scope':'跨仓设计/资源/Schema/依赖静态检查；不证明实际 Rust/Harness/UI 或质量能力','checks':checks,'passed':True,'framework_model_cases':85,'inquiry_domain_cases':21,'product_tests':'not_run'}
summary['source_sha256']={'inquiry_l2':hashlib.sha256((app/'docs/design/1-minimal-research-loop.md').read_bytes()).hexdigest(),'taskloop_l2':hashlib.sha256((base/'docs/design/2-minimal-runtime-contract.md').read_bytes()).hexdigest(),'checker':hashlib.sha256(Path(__file__).read_bytes()).hexdigest()};p=Path(__file__).with_name('consumer-check.json');p.write_text(json.dumps(summary,ensure_ascii=False,indent=2)+'\n');print(json.dumps({'checks':len(checks),'passed':True,'framework_model_cases':85,'inquiry_domain_cases':21,'product_tests':'not_run'}))
