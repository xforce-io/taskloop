//! External-crate SDK consumers with measured concurrent work and a semantic isolation check.
mod common;
use async_trait::async_trait;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use taskloop::*;
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct Stats {
    plan_verdict: Option<&'static str>,
    bad_member: Option<&'static str>,
    forbidden_join: bool,
    rules_version: Option<&'static str>,
    input_wait_ms: Option<u64>,
    goal_wall_ms: Option<u64>,
    budget_case: Option<BudgetScenario>,
    native_runs: Mutex<BTreeSet<String>>,
    replay_evaluation: bool,
    previous_evaluation: Mutex<Option<serde_json::Value>>,
    flow_started: tokio::sync::Notify,
    calls: AtomicUsize,
    check_started: tokio::sync::Notify,
    check_release: tokio::sync::Notify,
    active: AtomicUsize,
    peak: AtomicUsize,
    members: Mutex<BTreeMap<String, (usize, usize)>>,
}
struct Active {
    stats: Arc<Stats>,
    goal: String,
    member: bool,
}
impl Active {
    fn new(stats: Arc<Stats>, goal: String, member: bool) -> Self {
        let active = stats.active.fetch_add(1, Ordering::SeqCst) + 1;
        stats.peak.fetch_max(active, Ordering::SeqCst);
        if member {
            let mut all = stats.members.lock().unwrap();
            let state = all.entry(goal.clone()).or_default();
            state.0 += 1;
            state.1 = state.1.max(state.0);
        }
        Self {
            stats,
            goal,
            member,
        }
    }
}
impl Drop for Active {
    fn drop(&mut self) {
        self.stats.active.fetch_sub(1, Ordering::SeqCst);
        if self.member {
            self.stats
                .members
                .lock()
                .unwrap()
                .get_mut(&self.goal)
                .unwrap()
                .0 -= 1;
        }
    }
}
#[derive(Clone, Copy)]
struct NodeScenario {
    checkpoint: bool,
    verdict: &'static str,
    hold: bool,
}
#[derive(Clone, Copy)]
struct BudgetScenario {
    phase: &'static str,
    dimension: &'static str,
}
struct NativeCall(Arc<Stats>, String);
impl Drop for NativeCall {
    fn drop(&mut self) {
        self.0.native_runs.lock().unwrap().remove(&self.1);
    }
}
fn budget_phase(role: &GoalRunRole, phase: &str) -> bool {
    match (phase, role) {
        ("planner", GoalRunRole::Planner { .. })
        | ("review", GoalRunRole::PlanCheck { .. })
        | ("final", GoalRunRole::FinalCheck { .. }) => true,
        ("worker", GoalRunRole::NodeMember { member_id, .. }) => member_id == "a",
        _ => false,
    }
}
struct ExecutorFixture(Arc<Stats>, bool, Option<NodeScenario>, bool);
#[async_trait]
impl Executor for ExecutorFixture {
    fn capabilities(&self) -> ExecutorCapabilities {
        ExecutorCapabilities::MANAGED
    }
    fn enforces_json_candidate_limit(&self) -> bool {
        true
    }
    async fn stop_or_confirm(&self, run_id: &str) -> StopState {
        // This opt-in fixture owns only its Rust invocation future, with no child processes/tasks.
        if self.0.budget_case.is_some() && !self.0.native_runs.lock().unwrap().contains(run_id) {
            StopState::Confirmed
        } else {
            StopState::Unknown
        }
    }
    async fn execute(
        &self,
        invocation: Invocation,
        tools: Arc<dyn ToolBridge>,
        _: CancellationToken,
    ) -> ExecutionOutcome {
        let context = tools.goal_context().expect("trusted Goal scope");
        let is_final = matches!(&context.role, GoalRunRole::FinalCheck { .. });
        let target = self
            .0
            .budget_case
            .is_some_and(|c| budget_phase(&context.role, c.phase));
        self.0
            .native_runs
            .lock()
            .unwrap()
            .insert(invocation.run_id.clone());
        let _native = NativeCall(self.0.clone(), invocation.run_id.clone());
        self.0.calls.fetch_add(1, Ordering::SeqCst);
        if self.1
            && matches!(context.role, GoalRunRole::Planner { .. })
            && invocation.inputs["text"] == "hold"
        {
            return ExecutionOutcome::StopUnknown {
                message: "controlled inability to prove native stop".into(),
            };
        }

        let _active = Active::new(
            self.0.clone(),
            context.goal_id.clone(),
            matches!(context.role, GoalRunRole::NodeMember { .. }),
        );
        if target && self.0.budget_case.is_some_and(|c| c.dimension == "time") {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let (slot, mut value) = match context.role {
            GoalRunRole::Planner { .. } => {
                tokio::time::sleep(Duration::from_millis(20)).await;
                let mut value = json!({"protocol_version":1,"kind":"proposal","proposal":{"protocol_version":1,"goal_spec_digest":invocation.stage_input["goal_spec_digest"],"profile_digest":invocation.stage_input["profile_digest"],"planning_mode":"autonomous","nodes":{"combine":{"capability_ref":"pair@1","depends_on":[],"inputs":{"source":{"kind":"goal","pointer":"/inputs/text"}}}},"outputs":{"result":{"node_id":"combine","port":"result"}},"checks":{"joined":{"phase":"goal_final","checker_ref":"joined@1","targets":["result"]}}}});
                if let Some(scenario) = self.2 {
                    let plan = &mut value["proposal"];
                    plan["nodes"]["check"] = json!({"capability_ref":"evaluation@1","depends_on":["combine"],"inputs":{"draft":{"kind":"node","node_id":"combine","port":"result","pointer":""}}});
                    plan["nodes"]["after"] = plan["nodes"]["combine"].clone();
                    plan["nodes"]["after"]["depends_on"] = json!(["check"]);
                    plan["outputs"]["draft"] = json!({"node_id":"combine","port":"result"});
                    plan["outputs"]["result"]["node_id"] = json!("after");
                    if scenario.checkpoint {
                        plan["checks"]["draft-check"] = json!({"phase":"node_checkpoint","checker_ref":"joined@1","targets":["draft"],"evaluation_node_id":"check"});
                    }
                }
                if invocation.stage_input["planning_mode"] == "workflow" {
                    let template: WorkflowTemplate =
                        serde_json::from_value(invocation.stage_input["template"].clone()).unwrap();
                    value["proposal"]["planning_mode"] = json!("workflow");
                    let expected_digest = format!(
                        "{:x}",
                        Sha256::digest(serde_json::to_vec(&template).unwrap())
                    );
                    assert_eq!(
                        invocation.stage_input["template_digest"], expected_digest,
                        "Runtime must supply the frozen digest for the Planner to echo"
                    );
                    value["proposal"]["template_digest"] =
                        invocation.stage_input["template_digest"].clone();
                    assert_eq!(template.nodes["combine"].capabilities, vec!["pair@1"]);
                }
                if self.3 {
                    let plan = &mut value["proposal"];
                    plan["nodes"]["prompt"] =
                        json!({"capability_ref":"user-input@1","depends_on":[],"inputs":{}});
                    plan["nodes"]["combine"]["depends_on"] = json!(["prompt"]);
                    plan["nodes"]["combine"]["inputs"]["source"] =
                        json!({"kind":"node","node_id":"prompt","port":"value","pointer":""});
                }
                ("proposal", value)
            }
            GoalRunRole::NodeMember { member_id, .. } if member_id == "checker" => {
                let scenario = self.2.expect("explicit evaluation scenario");
                if scenario.hold {
                    self.0.check_started.notify_one();
                    self.0.check_release.notified().await;
                }
                let input: CheckInput = serde_json::from_value(invocation.stage_input).unwrap();
                assert_eq!(
                    input.phase,
                    if scenario.checkpoint {
                        CheckInputPhase::NodeCheckpoint
                    } else {
                        CheckInputPhase::NodeEvaluation
                    }
                );
                let CheckSubject::Artifacts { targets, .. } = &input.subject else {
                    panic!("evaluation needs exact artifacts")
                };
                assert_eq!(targets.keys().collect::<Vec<_>>(), vec!["draft"]);
                assert_eq!(invocation.artifacts.len(), 1);
                assert_eq!(
                    invocation.inputs,
                    serde_json::to_value(&input.goal_inputs).unwrap()
                );
                assert!(invocation.signal_facts.is_empty());
                (
                    "evaluation",
                    json!({"protocol_version":1,"verdict":scenario.verdict,"results":[{"id":"joined","verdict":scenario.verdict,"message":"Controlled evaluation-node result","evidence_refs":input.evidence_refs}],"evidence_refs":input.evidence_refs,"violations":[]}),
                )
            }
            GoalRunRole::NodeMember { member_id, .. } => {
                tokio::time::sleep(Duration::from_millis(if member_id == "a" {
                    150
                } else {
                    20
                }))
                .await;
                (
                    "result",
                    if self.0.bad_member == Some(member_id.as_str()) {
                        json!({"invalid":"the registered member result must be a string"})
                    } else {
                        json!(format!(
                            "{}:{member_id}",
                            invocation.stage_input["source"].as_str().unwrap()
                        ))
                    },
                )
            }
            GoalRunRole::PlanCheck { .. } => {
                let input: CheckInput = serde_json::from_value(invocation.stage_input).unwrap();
                assert_eq!(input.phase, CheckInputPhase::Plan);
                let CheckSubject::Proposal {
                    proposal_digest,
                    proposal,
                } = &input.subject
                else {
                    panic!("plan checker must receive the exact proposal, not artifact targets")
                };
                assert_eq!(proposal.goal_spec_digest, input.goal_spec_digest);
                assert_eq!(proposal_digest.len(), 64);
                assert_eq!(invocation.artifacts.len(), 0);
                assert_eq!(input.evidence_refs, vec!["goal-input:text"]);
                assert_eq!(input.goal_inputs.keys().collect::<Vec<_>>(), vec!["text"]);
                assert!(
                    self.0.members.lock().unwrap().is_empty(),
                    "plan review precedes all business dispatch"
                );
                let verdict = self.0.plan_verdict.expect("explicit plan-review scenario");
                (
                    "evaluation",
                    json!({"protocol_version":1,"verdict":verdict,"results":[{"id":"joined","verdict":verdict,"message":"Controlled proposal review","evidence_refs":input.evidence_refs}],"evidence_refs":input.evidence_refs,"violations":[]}),
                )
            }
            GoalRunRole::FinalCheck { .. } => {
                let input: CheckInput = serde_json::from_value(invocation.stage_input).unwrap();
                assert_eq!(
                    invocation.inputs,
                    serde_json::to_value(&input.goal_inputs).unwrap()
                );
                assert!(invocation.signal_facts.is_empty());
                let source = input.goal_inputs["text"].as_str().unwrap();
                let CheckSubject::Artifacts { targets, .. } = &input.subject else {
                    panic!("this fixture registers only a final artifact check")
                };
                let target = &targets["result"].value;
                let verdict = if target["a"] == format!("{source}:a")
                    && target["b"] == format!("{source}:b")
                {
                    "pass"
                } else {
                    "fail"
                };
                (
                    "evaluation",
                    json!({"protocol_version":1,"verdict":verdict,"results":[{"id":"joined","verdict":verdict,"message":"Both outputs must belong to this Goal input","evidence_refs":input.evidence_refs}],"evidence_refs":input.evidence_refs,"violations":[]}),
                )
            }
        };
        if is_final && self.0.replay_evaluation {
            let mut previous = self.0.previous_evaluation.lock().unwrap();
            if let Some(old) = previous.as_ref() {
                value = old.clone();
            } else {
                *previous = Some(value.clone());
            }
        }
        if target && self.0.budget_case.is_some_and(|c| c.dimension == "tools") {
            for _ in 0..2 {
                tools
                    .call("artifact.write", json!({"slot":slot,"value":value}))
                    .await
                    .unwrap();
            }
            let error = tools
                .call("artifact.write", json!({"slot":slot,"value":value}))
                .await
                .unwrap_err();
            return ExecutionOutcome::Failed {
                code: error.code,
                message: error.message,
            };
        }
        let written = tools
            .call("artifact.write", json!({"slot":slot,"value":value}))
            .await;
        if self.0.bad_member.is_some() && value.is_object() && value.get("invalid").is_some() {
            assert_eq!(written.unwrap_err().code, "input_invalid");
        } else if let Err(error) = written {
            if self.0.budget_case.is_some() {
                return ExecutionOutcome::Failed {
                    code: error.code,
                    message: error.message,
                };
            }
            panic!("unexpected fixture write failure: {error:?}");
        }
        ExecutionOutcome::Completed {
            usage: json!({"fixture":true}),
        }
    }
}
fn limits() -> GoalLimits {
    GoalLimits {
        max_wall_ms: 10000,
        max_active_ms: 20000,
        max_runs: 8,
        max_transitions: 1000,
        max_tool_calls: 100,
        max_total_candidate_bytes: 100000,
        max_concurrent_runs: 2,
    }
}
fn entry(input: &str, output: &str, schema: &str) -> EntryContract {
    EntryContract {
        instruction: "Controlled SDK fixture".into(),
        executor: "fixture".into(),
        skills: vec![],
        tools: vec!["artifact.write".into()],
        input_schema: input.into(),
        requires: vec![],
        outputs: BTreeMap::from([(
            output.into(),
            OutputSlot {
                schema: schema.into(),
                required: true,
            },
        )]),
        attachments: None,
        gate: "_tl.schema@1".into(),
        outcomes: vec!["completed".into()],
    }
}
async fn host(root: &std::path::Path, stats: Arc<Stats>) -> (Runtime, Caller) {
    host_mode(root, stats, 2, false).await
}
async fn host_mode(
    root: &std::path::Path,
    stats: Arc<Stats>,
    capacity: u64,
    unknown: bool,
) -> (Runtime, Caller) {
    host_scenario(root, stats, capacity, unknown, None).await
}
async fn host_scenario(
    root: &std::path::Path,
    stats: Arc<Stats>,
    capacity: u64,
    unknown: bool,
    scenario: Option<NodeScenario>,
) -> (Runtime, Caller) {
    host_variant(root, stats, capacity, unknown, scenario, false).await
}
async fn host_variant(
    root: &std::path::Path,
    stats: Arc<Stats>,
    capacity: u64,
    unknown: bool,
    scenario: Option<NodeScenario>,
    input: bool,
) -> (Runtime, Caller) {
    host_composed(root, stats, capacity, unknown, scenario, input, None).await
}
async fn host_composed(
    root: &std::path::Path,
    stats: Arc<Stats>,
    capacity: u64,
    unknown: bool,
    scenario: Option<NodeScenario>,
    input: bool,
    flow: Option<&common::Fixture>,
) -> (Runtime, Caller) {
    let mut registry = OrchestrationRegistry::new();
    for (name, mut schema) in [
        ("text", json!({"type":"string"})),
        ("planner-input", json!({"type":"object"})),
        (
            "goal-input",
            json!({"type":"object","properties":{"text":{"$ref":"text"}},"required":["text"],"additionalProperties":false}),
        ),
        (
            "worker-input",
            json!({"type":"object","properties":{"source":{"$ref":"text"}},"required":["source"],"additionalProperties":false}),
        ),
        (
            "pair",
            json!({"type":"object","properties":{"a":{"$ref":"text"},"b":{"$ref":"text"}},"required":["a","b"],"additionalProperties":false}),
        ),
    ] {
        if name == "pair" && stats.forbidden_join {
            schema["not"] = json!({"properties":{"a":{"const":"owned:a"},"b":{"const":"owned:b"}},"required":["a","b"]});
        }
        registry
            .register_schema(name, &serde_json::to_vec(&schema).unwrap())
            .unwrap();
    }
    let policy = RunBudgetPolicy {
        policy_id: "sdk-fixture".into(),
        max_run_ms: Some(1000),
        max_check_ms: Some(100),
        max_tool_calls: 2,
        submission_reserve: 1,
        finalize_before_ms: 0,
        max_model_iterations: None,
        max_candidate_bytes: 4096,
    };
    for (id, contract) in [
        (
            "planner@1",
            entry("planner-input", "proposal", "_tl.planner-result@1"),
        ),
        ("worker@1", entry("worker-input", "result", "text")),
        (
            "joined@1",
            entry(
                "_tl.check-input@1",
                "evaluation",
                "_tl.evaluation-candidate@1",
            ),
        ),
    ] {
        let mut entry_policy = policy.clone();
        if let Some(case) = stats.budget_case {
            let target_entry = match case.phase {
                "planner" => "planner@1",
                "worker" => "worker@1",
                _ => "joined@1",
            };
            if id == target_entry {
                match case.dimension {
                    "bytes" => entry_policy.max_candidate_bytes = 1,
                    "time" => entry_policy.max_run_ms = Some(1),
                    _ => (),
                }
            }
        }
        registry.register_entry(id, contract, entry_policy).unwrap();
    }
    let rules=registry.register_rules("joined",&serde_json::to_vec(&json!({"protocol_version":1,"id":"joined","version":stats.rules_version.unwrap_or("1"),"rules":[{"id":"joined","instruction":"Check Goal input ownership","pass_condition":"Both members belong to this input","evidence_required":true}]})).unwrap()).unwrap();
    registry
        .register_checker(
            "joined@1",
            CheckerContract {
                entry_ref: "joined@1".into(),
                rules_ref: "joined".into(),
                result_slot: "evaluation".into(),
                fresh_context: true,
                goal_input_fields: vec!["text".into()],
            },
        )
        .unwrap();
    let mut pair = json!({"protocol_version":1,"id":"pair","version":"1","kind":"work","inputs":{"source":{"schema_ref":"text","required":true,"source_policy":{"kind":"goal","pointer":"/inputs/text"}}},"outputs":{"result":{"schema_ref":"pair"}},"gate":"_tl.schema@1","max_rule_check_ms":1000,"members":{"a":{"input_mode":"node_ports","entry_ref":"worker@1","input_bindings":{"source":{"port":"source","pointer":""}}},"b":{"input_mode":"node_ports","entry_ref":"worker@1","input_bindings":{"source":{"port":"source","pointer":""}}}},"join":{"kind":"object-by-worker","outputs":{"result":{"kind":"members","slots":{"a":"result","b":"result"}}}}});
    if input {
        pair["inputs"]["source"]["source_policy"] = json!({"kind":"artifact"});
        registry.register_capability(serde_json::from_value(json!({"protocol_version":1,"id":"user-input","version":"1","kind":"input","inputs":{},"outputs":{"value":{"schema_ref":"text"}},"gate":"_tl.schema@1","max_rule_check_ms":1000,"signal_schema_ref":"text","max_wait_ms":stats.input_wait_ms.unwrap_or(5000)})).unwrap()).unwrap();
    }
    registry
        .register_capability(serde_json::from_value(pair).unwrap())
        .unwrap();
    let mut profile = json!({"id":"pair","version":"1","goal_input_schema":"goal-input","goal_selectors":{"/inputs/text":"text"},"planner_entry_ref":"planner@1","capabilities":["pair@1"],"planning_input_schemas":[],"templates":[],"outputs":{"result":{"schema_ref":"pair","allowed_capabilities":["pair@1"]}},"final_outputs":["result"],"checks":[{"id":"joined","phase":"goal_final","checker_ref":"joined@1","rules_digest":rules,"target_outputs":["result"],"required":true}],"allowed_tools":["artifact.write"]});
    if stats.plan_verdict.is_some() {
        profile["checks"].as_array_mut().unwrap().push(json!({"id":"proposal-review","phase":"plan","checker_ref":"joined@1","rules_digest":rules,"target_outputs":[],"required":true}));
    }
    if let Some(scenario) = scenario {
        registry.register_capability(serde_json::from_value(json!({"protocol_version":1,"id":"evaluation","version":"1","kind":"work","inputs":{"draft":{"schema_ref":"pair","required":true,"source_policy":{"kind":"artifact"}}},"outputs":{"evaluation":{"schema_ref":"_tl.evaluation-candidate@1"}},"gate":"_tl.schema@1","max_rule_check_ms":1000,"members":{"checker":{"entry_ref":"joined@1","input_mode":"check_request"}},"join":{"kind":"object-by-worker","outputs":{"evaluation":{"kind":"member","member_id":"checker","slot":"evaluation"}}},"evaluation":{"purpose":if scenario.checkpoint {"checkpoint"} else {"data"},"checker_ref":"joined@1","target_ports":{"draft":"draft"}}})).unwrap()).unwrap();
        profile["capabilities"]
            .as_array_mut()
            .unwrap()
            .push(json!("evaluation@1"));
        profile["outputs"]["draft"] = profile["outputs"]["result"].clone();
        if scenario.checkpoint {
            profile["checks"].as_array_mut().unwrap().push(json!({"id":"draft-check","phase":"node_checkpoint","checker_ref":"joined@1","rules_digest":rules,"target_outputs":["draft"],"required":true,"before_outputs":["result"]}));
        }
    }
    if input {
        profile["capabilities"]
            .as_array_mut()
            .unwrap()
            .push(json!("user-input@1"));
    }
    if scenario.is_none() && !input {
        let template: WorkflowTemplate = serde_json::from_value(json!({"id":"fixed-pair","version":"1","nodes":{"combine":{"capabilities":["pair@1"],"depends_on":[],"inputs":{"source":{"kind":"goal","pointer":"/inputs/text"}},"fillable_inputs":[]}},"outputs":{"result":{"node_id":"combine","port":"result"}},"checks":{"joined":{"phase":"goal_final","checker_ref":"joined@1","targets":["result"]}}})).unwrap();
        registry.register_template(template).unwrap();
        profile["templates"] = json!(["fixed-pair@1"]);
    }
    registry
        .register_profile(serde_json::from_value(profile).unwrap())
        .unwrap();
    let mut limit = limits();
    limit.max_concurrent_runs = capacity;
    limit.max_wall_ms = stats.goal_wall_ms.unwrap_or(limit.max_wall_ms);
    let mut bindings = HostBindings::default()
        .executor(
            "fixture",
            Arc::new(ExecutorFixture(stats.clone(), unknown, scenario, input)),
        )
        .grant(
            "local",
            Grant {
                definitions: vec![],
                tools: vec!["artifact.write".into()],
                limits: ResourceLimits {
                    max_wall_ms: limit.max_wall_ms,
                    max_active_ms: limit.max_active_ms,
                    max_runs: limit.max_runs,
                    max_transitions: limit.max_transitions,
                },
            },
        );
    if let Some(flow) = flow {
        let legacy = flow.bindings("pass");
        bindings.gates.extend(legacy.gates);
        bindings.grants.extend(legacy.grants);
        bindings
            .executors
            .insert("test.executor".into(), Arc::new(HoldingFlow(stats)));
    }
    let registration = registry.validate_and_freeze(&bindings).unwrap();
    let options = OrchestrationOptions {
        grants: BTreeMap::from([(
            "local".into(),
            GoalGrant {
                profile_digests: vec![registration.profiles["pair@1"].clone()],
                capabilities: if scenario.is_some() {
                    vec!["pair@1".into(), "evaluation@1".into()]
                } else if input {
                    vec!["pair@1".into(), "user-input@1".into()]
                } else {
                    vec!["pair@1".into()]
                },
                tools: vec!["artifact.write".into()],
                limits: limit.clone(),
            },
        )]),
        limits: limit,
        max_admitted_goals: 8,
        max_spec_bytes: 1048576,
        max_plan_bytes: 4194304,
        max_nodes: 256,
        max_edges: 4096,
        max_workers_per_node: 32,
        max_json_candidate_bytes: 4194304,
    };
    let runtime = Runtime::open_orchestrated(
        RuntimeOptions::new(root),
        flow.map(|f| vec![f.definition.clone()]).unwrap_or_default(),
        bindings,
        registry,
        options,
        None,
    )
    .await
    .unwrap();
    (
        runtime,
        Caller {
            principal: "alice".into(),
            grants: vec!["local".into()],
        },
    )
}
fn request(key: &str, text: &str) -> GoalRequest {
    GoalRequest {
        protocol_version: 1,
        request_key: key.into(),
        grant_ref: "local".into(),
        spec: GoalSpec {
            goal: "Combine two independent results and check their input identity".into(),
            inputs: json!({"text":text}),
            profile_ref: "pair@1".into(),
            planning: GoalPlanning {
                mode: PlanningMode::Autonomous,
                max_revisions: 0,
                max_planning_ms: 5000,
                template_ref: None,
            },
            limits: limits(),
            parent_goal_id: None,
            imports: vec![],
        },
    }
}
async fn finished(goals: &Goals, id: &str, caller: &Caller) -> GoalSnapshot {
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let s = goals.get(id, caller).await.unwrap();
            if s.status == GoalStatus::Finished {
                return s;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("SDK must finish without a step call")
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn members_execute_concurrently_and_merge_by_registered_identity_instead_of_completion_order()
{
    let root = tempfile::tempdir().unwrap();
    let stats = Arc::new(Stats::default());
    let (runtime, caller) = host(&root.path().join("store"), stats.clone()).await;
    let goals = runtime.goals().unwrap();
    let id = goals
        .submit(request("one", "A"), &caller)
        .await
        .unwrap()
        .goal_id;
    let result = finished(&goals, &id, &caller).await;
    assert_eq!(result.outcome, Some(GoalOutcome::Achieved));
    assert_eq!(result.budget.runs_used, 4);
    assert_eq!(stats.members.lock().unwrap()[&id].1, 2);
    assert_eq!(stats.peak.load(Ordering::SeqCst), 2);
    let output = &result.nodes["combine"].outputs["result"];
    let artifact = goals.read_artifact(&output.id, &caller).await.unwrap();
    assert_eq!(
        parse_unique(&artifact.bytes).unwrap(),
        json!({"a":"A:a","b":"A:b"})
    );
    runtime.shutdown().await.unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn independent_goals_share_the_global_capacity_and_cannot_mix_results_or_read_another_owner()
{
    let root = tempfile::tempdir().unwrap();
    let stats = Arc::new(Stats::default());
    let (runtime, alice) = host(&root.path().join("store"), stats.clone()).await;
    let goals = runtime.goals().unwrap();
    let bob = Caller {
        principal: "bob".into(),
        grants: vec!["local".into()],
    };
    let a = goals
        .submit(request("A", "alpha"), &alice)
        .await
        .unwrap()
        .goal_id;
    let b = goals
        .submit(request("B", "beta"), &bob)
        .await
        .unwrap()
        .goal_id;
    let (a, b) = tokio::join!(finished(&goals, &a, &alice), finished(&goals, &b, &bob));
    assert_eq!(a.outcome, Some(GoalOutcome::Achieved));
    assert_eq!(b.outcome, Some(GoalOutcome::Achieved));
    assert_eq!(stats.peak.load(Ordering::SeqCst), 2);
    assert_eq!(
        goals.get(&b.goal_id, &alice).await.unwrap_err().code,
        "not_found"
    );
    assert_eq!(
        goals
            .read_artifact(&b.nodes["combine"].outputs["result"].id, &alice)
            .await
            .unwrap_err()
            .code,
        "not_found"
    );
    assert_eq!(a.budget.runs_used, 4);
    assert_eq!(b.budget.runs_used, 4);
    runtime.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unknown_run_keeps_its_slot_without_blocking_another_queued_goals_deadline() {
    let root = tempfile::tempdir().unwrap();
    let stats = Arc::new(Stats::default());
    let (runtime, caller) = host_mode(&root.path().join("store"), stats.clone(), 1, true).await;
    let goals = runtime.goals().unwrap();
    let held = goals
        .submit(request("held", "hold"), &caller)
        .await
        .unwrap()
        .goal_id;
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if goals.get(&held, &caller).await.unwrap().status == GoalStatus::Blocked {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let mut waiting = request("expires", "queued");
    waiting.spec.limits.max_wall_ms = 200;
    let queued = goals.submit(waiting, &caller).await.unwrap().goal_id;
    let progress = goals.get(&queued, &caller).await.unwrap().progress;
    assert_eq!(progress.pending_requests, 1);
    assert_eq!(
        progress.queue_reason.as_deref(),
        Some("run_admission_pending")
    );
    assert_eq!(progress.goal_occupied_slots, 0);
    assert_eq!(progress.host_occupied_slots, 1);
    assert_eq!(progress.host_stop_unknown_slots, 1);
    let client = GoalClient::open(root.path().join("store"), caller.clone()).unwrap();
    let readonly = client.get(&queued).unwrap().progress;
    assert_eq!(
        serde_json::to_value(progress).unwrap(),
        serde_json::to_value(readonly).unwrap()
    );
    let expired = finished(&goals, &queued, &caller).await;
    assert_eq!(expired.outcome, Some(GoalOutcome::Failed));
    assert_eq!(expired.terminal_intent.unwrap().reason, "wall_timeout");
    assert_eq!(expired.budget.runs_used, 0);
    assert_eq!(stats.calls.load(Ordering::SeqCst), 1);
    let held = goals.get(&held, &caller).await.unwrap();
    assert_eq!(held.status, GoalStatus::Blocked);
    assert!(
        held.runs
            .values()
            .any(|r| r.stop_unknown && !r.native_stop_confirmed)
    );
    assert_eq!(
        runtime.shutdown().await.unwrap_err().code,
        "stop_unconfirmed"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn evaluation_nodes_distinguish_checkpoint_fail_data_fail_and_check_errors() {
    for (checkpoint, verdict, expected) in [
        (true, "pass", GoalOutcome::Achieved),
        (true, "fail", GoalOutcome::NotMet),
        (true, "error", GoalOutcome::Failed),
        (false, "fail", GoalOutcome::Achieved),
        (false, "error", GoalOutcome::Failed),
    ] {
        let root = tempfile::tempdir().unwrap();
        let stats = Arc::new(Stats::default());
        let (runtime, caller) = host_scenario(
            &root.path().join("store"),
            stats.clone(),
            2,
            false,
            Some(NodeScenario {
                checkpoint,
                verdict,
                hold: false,
            }),
        )
        .await;
        let goals = runtime.goals().unwrap();
        let id = goals
            .submit(request("evaluation", "source"), &caller)
            .await
            .unwrap()
            .goal_id;
        let snapshot = finished(&goals, &id, &caller).await;
        assert_eq!(
            snapshot.outcome,
            Some(expected),
            "checkpoint={checkpoint}, verdict={verdict}: {snapshot:?}"
        );
        assert_eq!(snapshot.nodes["combine"].status, GoalNodeStatus::Succeeded);
        let draft = &snapshot.nodes["combine"].outputs["result"];
        assert_eq!(
            parse_unique(&goals.read_artifact(&draft.id, &caller).await.unwrap().bytes).unwrap(),
            json!({"a":"source:a","b":"source:b"})
        );
        let check = snapshot
            .evaluations
            .iter()
            .find(|e| {
                e.phase
                    == if checkpoint {
                        CheckInputPhase::NodeCheckpoint
                    } else {
                        CheckInputPhase::NodeEvaluation
                    }
            })
            .unwrap();
        assert_eq!(
            serde_json::to_value(&check.verdict).unwrap(),
            json!(verdict)
        );
        assert!(check.receipt_id.is_some());
        if verdict == "error" {
            assert!(snapshot.nodes["check"].outputs.is_empty());
            assert_eq!(
                snapshot.terminal_intent.as_ref().unwrap().reason,
                "check_unavailable"
            );
        } else {
            let reference = &snapshot.nodes["check"].outputs["evaluation"];
            let evaluation = parse_unique(
                &goals
                    .read_artifact(&reference.id, &caller)
                    .await
                    .unwrap()
                    .bytes,
            )
            .unwrap();
            assert_eq!(evaluation["verdict"], verdict);
        }
        if expected == GoalOutcome::Achieved {
            assert_eq!(snapshot.nodes["after"].status, GoalNodeStatus::Succeeded);
            assert_eq!(stats.calls.load(Ordering::SeqCst), 7);
        } else {
            assert!(snapshot.nodes["after"].outputs.is_empty());
            assert!(!snapshot.runs.values().any(
                |r| matches!(&r.role, GoalRunRole::NodeMember { node_id, .. } if node_id == "after")
            ));
            assert_eq!(stats.calls.load(Ordering::SeqCst), 4);
        }
        runtime.shutdown().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pause_accepts_started_checkpoint_results_without_hiding_fail_or_error() {
    for (verdict, outcome) in [
        ("pass", None),
        ("fail", Some(GoalOutcome::NotMet)),
        ("error", Some(GoalOutcome::Failed)),
    ] {
        let root = tempfile::tempdir().unwrap();
        let stats = Arc::new(Stats::default());
        let (runtime, caller) = host_scenario(
            &root.path().join("store"),
            stats.clone(),
            2,
            false,
            Some(NodeScenario {
                checkpoint: true,
                verdict,
                hold: true,
            }),
        )
        .await;
        let goals = runtime.goals().unwrap();
        let id = goals
            .submit(request("pause-check", "source"), &caller)
            .await
            .unwrap()
            .goal_id;
        tokio::time::timeout(Duration::from_secs(4), stats.check_started.notified())
            .await
            .unwrap();
        let snapshot = goals.get(&id, &caller).await.unwrap();
        goals
            .stop(&id, "pause", snapshot.revision, &caller)
            .await
            .unwrap();
        stats.check_release.notify_one();
        let paused = tokio::time::timeout(Duration::from_secs(4), async {
            loop {
                let s = goals.get(&id, &caller).await.unwrap();
                if matches!(
                    s.status,
                    GoalStatus::Stopped | GoalStatus::Finished | GoalStatus::Blocked
                ) {
                    break s;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(paused.outcome, outcome, "{verdict}: {paused:?}");
        assert_eq!(
            paused.status,
            if outcome.is_some() {
                GoalStatus::Finished
            } else {
                GoalStatus::Stopped
            }
        );
        assert_eq!(
            paused
                .evaluations
                .iter()
                .find(|c| c.phase == CheckInputPhase::NodeCheckpoint)
                .unwrap()
                .status,
            "accepted"
        );
        assert!(!paused.runs.values().any(
            |r| matches!(&r.role, GoalRunRole::NodeMember { node_id, .. } if node_id == "after")
        ));
        assert_eq!(stats.calls.load(Ordering::SeqCst), 4);
        if verdict == "error" {
            assert!(paused.nodes["check"].outputs.is_empty());
        } else {
            assert!(paused.nodes["check"].outputs.contains_key("evaluation"));
        }
        if outcome.is_none() {
            goals
                .resume(&id, "resume", paused.revision, &caller)
                .await
                .unwrap();
            let resumed = finished(&goals, &id, &caller).await;
            assert_eq!(resumed.outcome, Some(GoalOutcome::Achieved));
            assert_eq!(
                stats.calls.load(Ordering::SeqCst),
                7,
                "accepted checkpoint must not be rerun"
            );
        }
        runtime.shutdown().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn input_nodes_wait_without_a_run_preserve_nonce_across_pause_and_accept_signal_once() {
    let root = tempfile::tempdir().unwrap();
    let stats = Arc::new(Stats::default());
    let (runtime, caller) = host_variant(
        &root.path().join("store"),
        stats.clone(),
        2,
        false,
        None,
        true,
    )
    .await;
    let goals = runtime.goals().unwrap();
    let id = goals
        .submit(request("input", "provided"), &caller)
        .await
        .unwrap()
        .goal_id;
    let waiting = tokio::time::timeout(Duration::from_secs(4), async {
        loop {
            let s = goals.get(&id, &caller).await.unwrap();
            if s.status == GoalStatus::Waiting {
                break s;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(stats.calls.load(Ordering::SeqCst), 1);
    assert_eq!(waiting.nodes["prompt"].status, GoalNodeStatus::WaitingInput);
    let wait = waiting.waiting[0].clone();
    assert_eq!(wait.node_id.as_deref(), Some("prompt"));
    goals
        .stop(&id, "pause", waiting.revision, &caller)
        .await
        .unwrap();
    let stopped = goals.get(&id, &caller).await.unwrap();
    assert_eq!(stopped.status, GoalStatus::Stopped);
    assert_eq!(stopped.waiting[0].nonce, wait.nonce);
    assert_eq!(stopped.waiting[0].expires_at_utc_ms, wait.expires_at_utc_ms);
    let signal = |key: &str, revision| GoalSignalRequest {
        protocol_version: 1,
        wait_id: wait.wait_id.clone(),
        nonce: wait.nonce.clone(),
        payload: json!("provided"),
        request_key: key.into(),
        expected_revision: revision,
    };
    assert!(
        goals
            .signal(&id, signal("paused", stopped.revision), &caller)
            .await
            .is_err()
    );
    let stopped = goals.get(&id, &caller).await.unwrap();
    goals
        .resume(&id, "resume", stopped.revision, &caller)
        .await
        .unwrap();
    let resumed = goals.get(&id, &caller).await.unwrap();
    let signal = signal("answer", resumed.revision);
    let receipt = goals.signal(&id, signal.clone(), &caller).await.unwrap();
    let replayed = goals.signal(&id, signal, &caller).await.unwrap();
    assert_eq!(receipt.applied_revision, replayed.applied_revision);
    let result = finished(&goals, &id, &caller).await;
    assert_eq!(result.outcome, Some(GoalOutcome::Achieved));
    assert_eq!(result.budget.runs_used, 4);
    assert_eq!(stats.calls.load(Ordering::SeqCst), 4);
    assert!(result.waiting.is_empty());
    let accepted = &result.nodes["prompt"].outputs["value"];
    assert_eq!(
        parse_unique(
            &goals
                .read_artifact(&accepted.id, &caller)
                .await
                .unwrap()
                .bytes
        )
        .unwrap(),
        json!("provided")
    );
    runtime.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn workflow_planning_executes_the_frozen_template_through_the_same_runtime() {
    let root = tempfile::tempdir().unwrap();
    let stats = Arc::new(Stats::default());
    let (runtime, caller) = host(&root.path().join("store"), stats.clone()).await;
    let goals = runtime.goals().unwrap();
    let mut workflow = request("workflow", "template");
    workflow.spec.planning.mode = PlanningMode::Workflow;
    workflow.spec.planning.template_ref = Some("fixed-pair@1".into());
    let id = goals.submit(workflow, &caller).await.unwrap().goal_id;
    let snapshot = finished(&goals, &id, &caller).await;
    assert_eq!(
        snapshot.outcome,
        Some(GoalOutcome::Achieved),
        "{snapshot:?}"
    );
    assert_eq!(stats.calls.load(Ordering::SeqCst), 4);
    let plan = goals.plan(&id, &caller).await.unwrap();
    assert_eq!(plan.proposal.planning_mode, PlanningMode::Workflow);
    assert!(plan.proposal.template_digest.is_some());
    assert_eq!(
        plan.proposal.nodes.keys().collect::<Vec<_>>(),
        vec!["combine"]
    );
    assert_eq!(snapshot.progress.host_occupied_slots, 0);
    assert!(snapshot.progress.queue_reason.is_none());
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn plan_reviews_distinguish_pass_revisable_fail_and_unavailable_error_before_business() {
    for (verdict, expected, run_count, reason) in [
        ("pass", GoalOutcome::Achieved, 5, None),
        ("fail", GoalOutcome::Failed, 4, Some("planning_exhausted")),
        ("error", GoalOutcome::Failed, 2, Some("check_unavailable")),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let stats = Arc::new(Stats {
            plan_verdict: Some(verdict),
            ..Default::default()
        });
        let (runtime, caller) = host(&directory.path().join("store"), stats.clone()).await;
        let goals = runtime.goals().unwrap();
        let mut req = request("plan-review", "reviewed");
        req.spec.planning.max_revisions = 1;
        let original = req.spec.clone();
        let id = goals.submit(req, &caller).await.unwrap().goal_id;
        let snapshot = finished(&goals, &id, &caller).await;
        assert_eq!(snapshot.outcome, Some(expected), "{verdict}: {snapshot:?}");
        assert_eq!(snapshot.budget.runs_used, run_count, "{verdict}");
        assert_eq!(
            serde_json::to_value(&snapshot.spec).unwrap(),
            serde_json::to_value(original).unwrap()
        );
        assert_eq!(snapshot.progress.goal_occupied_slots, 0);
        assert!(snapshot.budget.reserved.is_empty());
        let checks = goals.evaluations(&id, &caller).await.unwrap();
        let plans: Vec<_> = checks
            .iter()
            .filter(|c| c.phase == CheckInputPhase::Plan)
            .collect();
        assert_eq!(plans.len(), if verdict == "fail" { 2 } else { 1 });
        assert!(
            plans
                .iter()
                .all(|c| serde_json::to_value(&c.verdict).unwrap() == verdict)
        );
        if let Some(reason) = reason {
            assert_eq!(snapshot.terminal_intent.as_ref().unwrap().reason, reason);
            assert!(stats.members.lock().unwrap().is_empty());
            assert!(snapshot.nodes.is_empty());
            assert!(!snapshot.artifacts.iter().any(|a| a.accepted));
        } else {
            assert_eq!(checks.len(), 2);
            assert_eq!(snapshot.nodes["combine"].status, GoalNodeStatus::Succeeded);
        }
        runtime.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn planning_attempts_share_goal_run_and_tool_limits_instead_of_resetting_them() {
    for dimension in ["runs", "tools"] {
        let directory = tempfile::tempdir().unwrap();
        let stats = Arc::new(Stats {
            plan_verdict: Some("fail"),
            ..Default::default()
        });
        let (runtime, caller) = host(&directory.path().join("store"), stats.clone()).await;
        let goals = runtime.goals().unwrap();
        let mut req = request("bounded-reviews", "reviewed");
        req.spec.planning.max_revisions = 2;
        if dimension == "runs" {
            req.spec.limits.max_runs = 3;
        } else {
            req.spec.limits.max_tool_calls = 4;
        }
        let limits = req.spec.limits.clone();
        let id = goals.submit(req, &caller).await.unwrap().goal_id;
        let snapshot = finished(&goals, &id, &caller).await;
        assert_eq!(
            snapshot.outcome,
            Some(GoalOutcome::Failed),
            "{dimension}: {snapshot:?}"
        );
        assert_eq!(
            snapshot.terminal_intent.as_ref().unwrap().reason,
            "budget_exhausted"
        );
        assert_eq!(snapshot.budget.runs_used, 2);
        assert_eq!(snapshot.budget.used.tool_calls, 2);
        assert_eq!(stats.calls.load(Ordering::SeqCst), 2);
        assert!(stats.members.lock().unwrap().is_empty());
        assert_eq!(
            serde_json::to_value(&snapshot.limits).unwrap(),
            serde_json::to_value(&limits).unwrap()
        );
        assert_eq!(snapshot.progress.goal_occupied_slots, 0);
        assert!(snapshot.budget.reserved.is_empty());
        assert!(snapshot.budget.protected_final.is_empty());
        let receipts = goals.evaluations(&id, &caller).await.unwrap();
        if dimension == "runs" {
            // No proposal has enough remaining Runs for the business graph and required final check.
            assert!(receipts.is_empty());
            assert!(
                snapshot
                    .runs
                    .values()
                    .all(|run| matches!(run.role, GoalRunRole::Planner { .. }))
            );
        } else {
            assert_eq!(receipts.len(), 1);
            assert_eq!(receipts[0].phase, CheckInputPhase::Plan);
            assert_eq!(receipts[0].verdict, EvaluationVerdict::Fail);
        }
        runtime.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn schema_invalid_member_submission_is_rejected_before_public_join_or_final_check() {
    let directory = tempfile::tempdir().unwrap();
    let stats = Arc::new(Stats {
        bad_member: Some("a"),
        ..Default::default()
    });
    let (runtime, caller) = host(&directory.path().join("store"), stats).await;
    let goals = runtime.goals().unwrap();
    let id = goals
        .submit(request("invalid-member", "owned"), &caller)
        .await
        .unwrap()
        .goal_id;
    let snapshot = finished(&goals, &id, &caller).await;
    assert_eq!(snapshot.outcome, Some(GoalOutcome::Failed));
    assert_eq!(snapshot.nodes["combine"].status, GoalNodeStatus::Failed);
    assert_eq!(snapshot.budget.runs_used, 3);
    assert!(
        !snapshot
            .artifacts
            .iter()
            .any(|a| a.member_id.as_deref() == Some("a"))
    );
    let rejected = snapshot
        .runs
        .values()
        .find(|r| matches!(&r.role, GoalRunRole::NodeMember { member_id, .. } if member_id == "a"))
        .unwrap();
    assert_eq!(rejected.budget.as_ref().unwrap().calls_used, 1);
    assert_eq!(rejected.budget.as_ref().unwrap().candidate_bytes_used, 0);
    assert!(
        !snapshot
            .artifacts
            .iter()
            .any(|a| a.accepted && a.member_id.is_none())
    );
    assert!(
        !snapshot
            .runs
            .values()
            .any(|r| matches!(r.role, GoalRunRole::FinalCheck { .. }))
    );
    assert!(goals.evaluations(&id, &caller).await.unwrap().is_empty());
    assert_eq!(snapshot.progress.goal_occupied_slots, 0);
    assert!(snapshot.budget.reserved.is_empty());
    runtime.shutdown().await.unwrap();
}

struct HoldingFlow(Arc<Stats>);
#[async_trait]
impl Executor for HoldingFlow {
    fn capabilities(&self) -> ExecutorCapabilities {
        ExecutorCapabilities::MANAGED
    }
    async fn execute(
        &self,
        _: Invocation,
        tools: Arc<dyn ToolBridge>,
        cancel: CancellationToken,
    ) -> ExecutionOutcome {
        assert!(tools.goal_context().is_none());
        let _active = Active::new(self.0.clone(), "legacy-flow".into(), false);
        self.0.flow_started.notify_one();
        cancel.cancelled().await;
        ExecutionOutcome::Cancelled
    }
    async fn stop_or_confirm(&self, _: &str) -> StopState {
        StopState::Confirmed
    }
}

#[tokio::test]
async fn legacy_flow_and_goal_share_one_physical_slot_until_the_flow_confirms_stop() {
    let directory = tempfile::tempdir().unwrap();
    let flow = common::Fixture::new(false);
    let stats = Arc::new(Stats::default());
    let (runtime, caller) = host_composed(
        &directory.path().join("store"),
        stats.clone(),
        1,
        false,
        None,
        false,
        Some(&flow),
    )
    .await;
    let flow_id = runtime
        .start(
            flow.request("shared-flow", "legacy", true),
            &Caller::local(),
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), stats.flow_started.notified())
        .await
        .unwrap();
    let goals = runtime.goals().unwrap();
    let mut req = request("shared-goal", "dag");
    req.spec.limits.max_concurrent_runs = 1;
    let id = goals.submit(req, &caller).await.unwrap().goal_id;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let queued = goals.get(&id, &caller).await.unwrap();
    assert_eq!(queued.progress.host_occupied_slots, 1);
    assert_eq!(queued.progress.goal_occupied_slots, 0);
    assert_eq!(queued.progress.pending_requests, 1);
    assert_eq!(queued.budget.runs_used, 0);
    assert_eq!(stats.calls.load(Ordering::SeqCst), 0);
    runtime
        .cancel(&flow_id, "release-shared", &Caller::local())
        .await
        .unwrap();
    let ended = finished(&goals, &id, &caller).await;
    assert_eq!(ended.outcome, Some(GoalOutcome::Achieved));
    assert_eq!(stats.peak.load(Ordering::SeqCst), 1);
    assert_eq!(ended.progress.host_occupied_slots, 0);
    assert_eq!(ended.budget.runs_used, 4);
    assert_eq!(
        runtime
            .get(&flow_id, &Caller::local())
            .await
            .unwrap()
            .status,
        Status::Cancelled
    );
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn individually_valid_member_outputs_cannot_accept_a_schema_invalid_join() {
    let directory = tempfile::tempdir().unwrap();
    let stats = Arc::new(Stats {
        forbidden_join: true,
        ..Default::default()
    });
    let (runtime, caller) = host(&directory.path().join("store"), stats).await;
    let goals = runtime.goals().unwrap();
    let id = goals
        .submit(request("forbidden-pair", "owned"), &caller)
        .await
        .unwrap()
        .goal_id;
    let snapshot = finished(&goals, &id, &caller).await;
    assert_eq!(snapshot.outcome, Some(GoalOutcome::Failed));
    let node = &snapshot.nodes["combine"];
    assert_eq!(node.status, GoalNodeStatus::Failed);
    assert!(
        node.members
            .values()
            .all(|m| m.status == GoalMemberStatus::Succeeded)
    );
    assert!(node.outputs.is_empty());
    assert_eq!(snapshot.budget.runs_used, 3);
    for (id, member) in &node.members {
        let artifact = &member.outputs["result"];
        let bytes = goals.read_artifact(&artifact.id, &caller).await.unwrap();
        assert_eq!(parse_unique(&bytes.bytes).unwrap(), format!("owned:{id}"));
    }
    assert!(!snapshot.artifacts.iter().any(|a| a.accepted));
    assert!(
        !snapshot
            .runs
            .values()
            .any(|r| matches!(r.role, GoalRunRole::FinalCheck { .. }))
    );
    assert!(goals.evaluations(&id, &caller).await.unwrap().is_empty());
    assert_eq!(snapshot.progress.goal_occupied_slots, 0);
    assert!(snapshot.budget.reserved.is_empty());
    assert_eq!(node.error.as_ref().unwrap().code, "input_invalid");
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_previous_goals_passing_evaluation_cannot_approve_new_public_artifacts() {
    let directory = tempfile::tempdir().unwrap();
    let stats = Arc::new(Stats {
        replay_evaluation: true,
        ..Default::default()
    });
    let (runtime, caller) = host(&directory.path().join("store"), stats).await;
    let goals = runtime.goals().unwrap();
    let a = goals
        .submit(request("original-evaluation", "A"), &caller)
        .await
        .unwrap()
        .goal_id;
    let first = finished(&goals, &a, &caller).await;
    assert_eq!(first.outcome, Some(GoalOutcome::Achieved));
    let receipts = goals.evaluations(&a, &caller).await.unwrap();
    assert_eq!(receipts.len(), 1);
    let original_digest = &receipts[0].candidate_digest;
    let b = goals
        .submit(request("replayed-evaluation", "B"), &caller)
        .await
        .unwrap()
        .goal_id;
    let second = finished(&goals, &b, &caller).await;
    assert_eq!(second.outcome, Some(GoalOutcome::Failed));
    assert_eq!(second.nodes["combine"].status, GoalNodeStatus::Succeeded);
    let output = &second.nodes["combine"].outputs["result"];
    assert_eq!(
        parse_unique(
            &goals
                .read_artifact(&output.id, &caller)
                .await
                .unwrap()
                .bytes
        )
        .unwrap(),
        json!({"a":"B:a","b":"B:b"})
    );
    assert!(goals.evaluations(&b, &caller).await.unwrap().is_empty());
    let replay = second
        .artifacts
        .iter()
        .find(|a| a.producer_kind == "check_candidate")
        .unwrap();
    assert_eq!(&replay.artifact.sha256, original_digest);
    assert!(!replay.accepted);
    let run = second
        .runs
        .values()
        .find(|r| matches!(r.role, GoalRunRole::FinalCheck { .. }))
        .unwrap();
    assert!(run.native_stop_confirmed);
    assert_eq!(
        run.budget.as_ref().unwrap().execution_outcome.as_deref(),
        Some("completed")
    );
    let error = run.error.as_ref().unwrap();
    assert_eq!(error.code, "check_unavailable");
    assert_eq!(error.path, "/evidence_refs");
    assert_eq!(second.budget.runs_used, 4);
    assert_eq!(second.progress.goal_occupied_slots, 0);
    assert!(second.budget.reserved.is_empty());
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn changed_rules_cannot_resume_an_old_goal_or_reinterpret_its_locked_contract() {
    let directory = tempfile::tempdir().unwrap();
    let store = directory.path().join("store");
    // This is a contract-version test, not an expiry test. Both hosts declare the same valid window.
    let first_stats = Arc::new(Stats {
        input_wait_ms: Some(60_000),
        goal_wall_ms: Some(60_000),
        ..Default::default()
    });
    let (runtime, caller) = host_variant(&store, first_stats, 2, false, None, true).await;
    let goals = runtime.goals().unwrap();
    let mut req = request("versioned-goal", "unchanged-input");
    req.spec.limits.max_wall_ms = 60_000;
    let id = goals.submit(req.clone(), &caller).await.unwrap().goal_id;
    let waiting = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let s = goals.get(&id, &caller).await.unwrap();
            if s.status == GoalStatus::Waiting && s.progress.goal_occupied_slots == 0 {
                break s;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    goals
        .stop(&id, "pause-versioned-goal", waiting.revision, &caller)
        .await
        .unwrap();
    let stopped = goals.get(&id, &caller).await.unwrap();
    assert_eq!(stopped.status, GoalStatus::Stopped);
    runtime.shutdown().await.unwrap();
    drop(goals);
    drop(runtime);
    let stats = Arc::new(Stats {
        rules_version: Some("2"),
        input_wait_ms: Some(60_000),
        goal_wall_ms: Some(60_000),
        ..Default::default()
    });
    let (runtime, caller) = host_variant(&store, stats.clone(), 2, false, None, true).await;
    let goals = runtime.goals().unwrap();
    let reopened = goals.get(&id, &caller).await.unwrap();
    assert_eq!(reopened.status, GoalStatus::Stopped, "{reopened:?}");
    assert!(!reopened.deadline_expired);
    assert!(reopened.blockers.is_empty());
    assert!(
        reopened
            .runs
            .values()
            .all(|r| r.phase == GoalRunPhase::Settled)
    );
    assert_eq!(reopened.spec_digest, stopped.spec_digest);
    assert_eq!(reopened.profile_digest, stopped.profile_digest);
    assert_eq!(reopened.plan_digest, stopped.plan_digest);
    assert_eq!(reopened.waiting[0].nonce, stopped.waiting[0].nonce);
    assert_eq!(
        reopened.waiting[0].expires_at_utc_ms,
        stopped.waiting[0].expires_at_utc_ms
    );
    let error = goals
        .resume(&id, "resume-under-new-rules", reopened.revision, &caller)
        .await
        .unwrap_err();
    assert_eq!(error.code, "not_resumable");
    let replay = goals.submit(req.clone(), &caller).await.unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.goal_id, id);
    let mut changed = req;
    changed.spec.inputs["text"] = json!("changed-input");
    assert_eq!(
        goals.submit(changed, &caller).await.unwrap_err().code,
        "request_conflict"
    );
    assert_eq!(stats.calls.load(Ordering::SeqCst), 0);
    let final_view = goals.get(&id, &caller).await.unwrap();
    assert_eq!(final_view.budget.runs_used, 1);
    assert_eq!(final_view.profile_digest, stopped.profile_digest);
    assert_eq!(final_view.plan_digest, stopped.plan_digest);
    goals
        .cancel(&id, "close-versioned-goal", final_view.revision, &caller)
        .await
        .unwrap();
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn input_deadlines_expire_online_or_after_reopen_without_accepting_a_late_signal() {
    for reopen in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let store = root.path().join("store");
        let mut stats = Arc::new(Stats {
            input_wait_ms: Some(200),
            ..Default::default()
        });
        let (mut runtime, caller) = host_variant(&store, stats.clone(), 2, false, None, true).await;
        let mut goals = runtime.goals().unwrap();
        let id = goals
            .submit(request("expiring-input", "owned"), &caller)
            .await
            .unwrap()
            .goal_id;
        let waiting = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let s = goals.get(&id, &caller).await.unwrap();
                if s.status == GoalStatus::Waiting && !s.waiting.is_empty() {
                    break s;
                }
                assert_ne!(s.status, GoalStatus::Finished);
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap();
        let wait = waiting.waiting[0].clone();
        assert_eq!(waiting.progress.goal_occupied_slots, 0);
        if reopen {
            goals
                .stop(&id, "pause-expiring-input", waiting.revision, &caller)
                .await
                .unwrap();
            runtime.shutdown().await.unwrap();
            drop(goals);
            drop(runtime);
            tokio::time::sleep(Duration::from_millis(240)).await;
            stats = Arc::new(Stats {
                input_wait_ms: Some(200),
                ..Default::default()
            });
            let fresh = host_variant(&store, stats.clone(), 2, false, None, true).await;
            runtime = fresh.0;
            goals = runtime.goals().unwrap();
        }
        let ended = finished(&goals, &id, &caller).await;
        assert_eq!(ended.outcome, Some(GoalOutcome::Failed));
        assert_eq!(
            ended.terminal_intent.as_ref().unwrap().reason,
            "input_timeout"
        );
        assert_eq!(ended.nodes["prompt"].status, GoalNodeStatus::Failed);
        assert_eq!(
            ended.nodes["combine"].status,
            GoalNodeStatus::DependencyFailed
        );
        assert_eq!(ended.budget.runs_used, 1);
        assert_eq!(ended.progress.goal_occupied_slots, 0);
        assert!(ended.budget.reserved.is_empty());
        assert!(ended.waiting.is_empty());
        assert!(!ended.artifacts.iter().any(|a| a.accepted));
        assert!(stats.members.lock().unwrap().is_empty());
        assert_eq!(
            stats.calls.load(Ordering::SeqCst),
            if reopen { 0 } else { 1 }
        );
        let late = GoalSignalRequest {
            protocol_version: 1,
            wait_id: wait.wait_id,
            nonce: wait.nonce,
            payload: json!("too late"),
            request_key: "late-input".into(),
            expected_revision: ended.revision,
        };
        assert_eq!(
            goals.signal(&id, late, &caller).await.unwrap_err().code,
            "signal_invalid"
        );
        assert_eq!(
            goals.get(&id, &caller).await.unwrap().outcome,
            Some(GoalOutcome::Failed)
        );
        assert_eq!(goals.get(&id, &caller).await.unwrap().budget.runs_used, 1);
        runtime.shutdown().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn planner_review_workers_and_final_checks_obey_declared_tool_byte_and_time_budgets() {
    for phase in ["planner", "review", "worker", "final"] {
        for dimension in ["tools", "bytes", "time"] {
            let root = tempfile::tempdir().unwrap();
            let stats = Arc::new(Stats {
                budget_case: Some(BudgetScenario { phase, dimension }),
                plan_verdict: (phase != "final").then_some("pass"),
                ..Default::default()
            });
            let (runtime, caller) = host(&root.path().join("store"), stats.clone()).await;
            let goals = runtime.goals().unwrap();
            let req = request("bounded-role", "owned");
            let original = serde_json::to_value(&req.spec).unwrap();
            let id = goals.submit(req, &caller).await.unwrap().goal_id;
            let ended = finished(&goals, &id, &caller).await;
            assert_eq!(
                ended.outcome,
                Some(GoalOutcome::Failed),
                "{phase}/{dimension}: {ended:?}"
            );
            assert_eq!(serde_json::to_value(&ended.spec).unwrap(), original);
            assert_eq!(ended.progress.goal_occupied_slots, 0);
            assert!(ended.budget.reserved.is_empty());
            assert!(stats.native_runs.lock().unwrap().is_empty());
            let expected = match dimension {
                "tools" => "tool_calls",
                "bytes" => "candidate_bytes",
                _ => "run_time",
            };
            assert!(
                ended
                    .runs
                    .values()
                    .filter_map(|r| r.budget.as_ref())
                    .any(|b| serde_json::to_value(&b.limits_hit)
                        .unwrap()
                        .as_array()
                        .unwrap()
                        .contains(&json!(expected))),
                "{phase}/{dimension}: {ended:?}"
            );
            let target_entry = match phase {
                "planner" => "planner@1",
                "worker" => "worker@1",
                _ => "joined@1",
            };
            assert!(ended.runs.values().any(|r| r.entry_ref == target_entry));
            assert!(
                !ended
                    .artifacts
                    .iter()
                    .any(|a| a.producer_kind == "check_candidate" && a.accepted)
            );
            runtime.shutdown().await.unwrap();
        }
    }
}
