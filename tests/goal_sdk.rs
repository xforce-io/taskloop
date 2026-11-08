//! External-crate SDK consumers with measured concurrent work and a semantic isolation check.
use async_trait::async_trait;
use serde_json::json;
use std::{
    collections::BTreeMap,
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
    calls: AtomicUsize,
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
struct ExecutorFixture(Arc<Stats>, bool);
#[async_trait]
impl Executor for ExecutorFixture {
    fn capabilities(&self) -> ExecutorCapabilities {
        ExecutorCapabilities::MANAGED
    }
    fn enforces_json_candidate_limit(&self) -> bool {
        true
    }
    async fn execute(
        &self,
        invocation: Invocation,
        tools: Arc<dyn ToolBridge>,
        _: CancellationToken,
    ) -> ExecutionOutcome {
        let context = tools.goal_context().expect("trusted Goal scope");
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
        let (slot, value) = match context.role {
            GoalRunRole::Planner { .. } => {
                tokio::time::sleep(Duration::from_millis(20)).await;
                (
                    "proposal",
                    json!({"protocol_version":1,"kind":"proposal","proposal":{"protocol_version":1,"goal_spec_digest":invocation.stage_input["goal_spec_digest"],"profile_digest":invocation.stage_input["profile_digest"],"planning_mode":"autonomous","nodes":{"combine":{"capability_ref":"pair@1","depends_on":[],"inputs":{"source":{"kind":"goal","pointer":"/inputs/text"}}}},"outputs":{"result":{"node_id":"combine","port":"result"}},"checks":{"joined":{"phase":"goal_final","checker_ref":"joined@1","targets":["result"]}}}}),
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
                    json!(format!(
                        "{}:{member_id}",
                        invocation.stage_input["source"].as_str().unwrap()
                    )),
                )
            }
            GoalRunRole::FinalCheck { .. } | GoalRunRole::PlanCheck { .. } => {
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
        tools
            .call("artifact.write", json!({"slot":slot,"value":value}))
            .await
            .unwrap();
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
    let mut registry = OrchestrationRegistry::new();
    for (name, schema) in [
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
        registry
            .register_entry(id, contract, policy.clone())
            .unwrap();
    }
    let rules=registry.register_rules("joined",&serde_json::to_vec(&json!({"protocol_version":1,"id":"joined","version":"1","rules":[{"id":"joined","instruction":"Check Goal input ownership","pass_condition":"Both members belong to this input","evidence_required":true}]})).unwrap()).unwrap();
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
    registry.register_capability(serde_json::from_value(json!({"protocol_version":1,"id":"pair","version":"1","kind":"work","inputs":{"source":{"schema_ref":"text","required":true,"source_policy":{"kind":"goal","pointer":"/inputs/text"}}},"outputs":{"result":{"schema_ref":"pair"}},"gate":"_tl.schema@1","max_rule_check_ms":1000,"members":{"a":{"input_mode":"node_ports","entry_ref":"worker@1","input_bindings":{"source":{"port":"source","pointer":""}}},"b":{"input_mode":"node_ports","entry_ref":"worker@1","input_bindings":{"source":{"port":"source","pointer":""}}}},"join":{"kind":"object-by-worker","outputs":{"result":{"kind":"members","slots":{"a":"result","b":"result"}}}}})).unwrap()).unwrap();
    registry.register_profile(serde_json::from_value(json!({"id":"pair","version":"1","goal_input_schema":"goal-input","goal_selectors":{"/inputs/text":"text"},"planner_entry_ref":"planner@1","capabilities":["pair@1"],"planning_input_schemas":[],"templates":[],"outputs":{"result":{"schema_ref":"pair","allowed_capabilities":["pair@1"]}},"final_outputs":["result"],"checks":[{"id":"joined","phase":"goal_final","checker_ref":"joined@1","rules_digest":rules,"target_outputs":["result"],"required":true}],"allowed_tools":["artifact.write"]})).unwrap()).unwrap();
    let mut limit = limits();
    limit.max_concurrent_runs = capacity;
    let bindings = HostBindings::default()
        .executor("fixture", Arc::new(ExecutorFixture(stats, unknown)))
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
    let registration = registry.validate_and_freeze(&bindings).unwrap();
    let options = OrchestrationOptions {
        grants: BTreeMap::from([(
            "local".into(),
            GoalGrant {
                profile_digests: vec![registration.profiles["pair@1"].clone()],
                capabilities: vec!["pair@1".into()],
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
        vec![],
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
