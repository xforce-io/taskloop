//! Three actual DAG shapes through public SDK, with a controlled in-process Executor.
use async_trait::async_trait;
use serde_json::{Value, json};
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
#[derive(Clone, Copy, Debug)]
enum Shape {
    Sequence,
    Fork,
    Join,
    Wait,
    FailureFork,
    Repair,
    Rejected(&'static str),
}
#[derive(Default)]
struct Observed {
    candidate_sha: Mutex<Option<String>>,
    left_started: tokio::sync::Notify,
    release_left: tokio::sync::Notify,
    active: AtomicUsize,
    peak: AtomicUsize,
    events: Mutex<Vec<(String, bool)>>,
    pack_inputs: Mutex<Vec<ArtifactRef>>,
}
struct GraphExecutor {
    shape: Shape,
    observed: Arc<Observed>,
}
fn source(node: &str) -> Value {
    json!({"kind":"node","node_id":node,"port":"value","pointer":""})
}
fn seed() -> Value {
    json!({"capability_ref":"seed@1","depends_on":[],"inputs":{"source":{"kind":"goal","pointer":"/inputs/text"}}})
}
fn relay(parent: &str) -> Value {
    json!({"capability_ref":"relay@1","depends_on":[parent],"inputs":{"source":source(parent)}})
}
fn nodes(shape: Shape) -> Value {
    match shape {
        Shape::Sequence => {
            json!({"seed":seed(),"relay":relay("seed"),"pack":{"capability_ref":"pack@1","depends_on":["relay"],"inputs":{"a":source("relay"),"b":source("relay")}}})
        }
        Shape::Fork | Shape::FailureFork | Shape::Repair | Shape::Rejected(_) => {
            json!({"left":seed(),"right":seed(),"pack":{"capability_ref":"pack@1","depends_on":["left","right"],"inputs":{"a":source("left"),"b":source("right")}}})
        }
        Shape::Join => {
            json!({"seed":seed(),"left":relay("seed"),"right":relay("seed"),"pack":{"capability_ref":"pack@1","depends_on":["left","right"],"inputs":{"a":source("left"),"b":source("right")}}})
        }
        Shape::Wait => {
            json!({"prompt":{"capability_ref":"input@1","depends_on":[],"inputs":{}},"left":seed(),"pack":{"capability_ref":"pack@1","depends_on":["left","prompt"],"inputs":{"a":source("left"),"b":source("prompt")}}})
        }
    }
}
fn expected(shape: Shape) -> Value {
    match shape {
        Shape::Sequence => json!({"a":"x:S:B","b":"x:S:B"}),
        Shape::Fork | Shape::FailureFork | Shape::Repair | Shape::Rejected(_) => {
            json!({"a":"x:L","b":"x:R"})
        }
        Shape::Wait => json!({"a":"x:L","b":"provided"}),
        Shape::Join => json!({"a":"x:S:L","b":"x:S:R"}),
    }
}
#[async_trait]
impl Executor for GraphExecutor {
    fn capabilities(&self) -> ExecutorCapabilities {
        ExecutorCapabilities::MANAGED
    }
    fn enforces_json_candidate_limit(&self) -> bool {
        true
    }
    fn identity(&self) -> Value {
        json!({"implementation":"controlled-dag-test","shape":format!("{:?}",self.shape)})
    }
    async fn execute(
        &self,
        inv: Invocation,
        tools: Arc<dyn ToolBridge>,
        _: CancellationToken,
    ) -> ExecutionOutcome {
        let role = tools.goal_context().unwrap().role;
        let attempt = if let GoalRunRole::Planner { attempt } = &role {
            Some(*attempt)
        } else {
            None
        };
        let (slot, mut value) = match role {
            GoalRunRole::Planner { .. } => (
                "proposal",
                json!({"protocol_version":1,"kind":"proposal","proposal":{"protocol_version":1,"goal_spec_digest":inv.stage_input["goal_spec_digest"],"profile_digest":inv.stage_input["profile_digest"],"planning_mode":"autonomous","nodes":nodes(self.shape),"outputs":{"result":{"node_id":"pack","port":"result"}},"checks":{"quality":{"phase":"goal_final","checker_ref":"quality@1","targets":["result"]}}}}),
            ),
            GoalRunRole::NodeMember { node_id, .. } => {
                let n = self.observed.active.fetch_add(1, Ordering::SeqCst) + 1;
                self.observed.peak.fetch_max(n, Ordering::SeqCst);
                self.observed
                    .events
                    .lock()
                    .unwrap()
                    .push((node_id.clone(), true));
                tokio::time::sleep(Duration::from_millis(if node_id == "left" {
                    150
                } else {
                    40
                }))
                .await;
                if matches!(self.shape, Shape::Wait) && node_id == "left" {
                    self.observed.left_started.notify_one();
                    self.observed.release_left.notified().await;
                }
                if matches!(self.shape, Shape::FailureFork) && node_id == "left" {
                    self.observed.events.lock().unwrap().push((node_id, false));
                    self.observed.active.fetch_sub(1, Ordering::SeqCst);
                    return ExecutionOutcome::Failed {
                        code: "controlled_failure".into(),
                        message: "Upstream fixture failure".into(),
                    };
                }
                let value = if node_id == "pack" {
                    *self.observed.pack_inputs.lock().unwrap() = inv.artifacts.clone();
                    inv.stage_input.clone()
                } else {
                    let suffix = match node_id.as_str() {
                        "seed" => "S",
                        "left" => "L",
                        "right" => "R",
                        "relay" => "B",
                        _ => panic!("unknown fixture node"),
                    };
                    json!(format!(
                        "{}:{suffix}",
                        inv.stage_input["source"].as_str().unwrap()
                    ))
                };
                tools
                    .call(
                        "artifact.write",
                        json!({"slot":if node_id=="pack"{"result"}else{"value"},"value":value}),
                    )
                    .await
                    .unwrap();
                self.observed.events.lock().unwrap().push((node_id, false));
                self.observed.active.fetch_sub(1, Ordering::SeqCst);
                return ExecutionOutcome::Completed {
                    usage: json!({"controlled":true}),
                };
            }
            GoalRunRole::FinalCheck { .. } => {
                let input: CheckInput = serde_json::from_value(inv.stage_input.clone()).unwrap();
                let CheckSubject::Artifacts { targets, .. } = &input.subject else {
                    panic!("artifact check")
                };
                assert_eq!(targets["result"].value, expected(self.shape));
                (
                    "evaluation",
                    json!({"protocol_version":1,"verdict":"pass","results":[{"id":"joined","verdict":"pass","message":"Every exact dependency value survived public output acceptance","evidence_refs":input.evidence_refs}],"evidence_refs":input.evidence_refs,"violations":[]}),
                )
            }
            GoalRunRole::PlanCheck { .. } => panic!("no plan check registered"),
        };
        if matches!(self.shape, Shape::Repair) {
            if attempt == Some(1) {
                value["proposal"]["nodes"]["left"]["depends_on"] = json!(["left"]);
            } else if attempt == Some(2) {
                assert!(
                    !inv.stage_input["planning_feedback"]["errors"]
                        .as_array()
                        .unwrap()
                        .is_empty()
                );
                assert_eq!(
                    inv.stage_input["planning_feedback"]["previous_candidate_sha"],
                    self.observed.candidate_sha.lock().unwrap().clone().unwrap()
                );
                assert!(
                    self.observed.events.lock().unwrap().is_empty(),
                    "invalid proposal must not dispatch business work"
                );
            }
        }
        if let Shape::Rejected(kind) = self.shape {
            let plan = &mut value["proposal"];
            match kind {
                "format" => {
                    for i in 0..14 {
                        plan["nodes"][format!("extra{i}")] = seed();
                    }
                }
                "cycle" => plan["nodes"]["left"]["depends_on"] = json!(["left"]),
                "reference" => plan["outputs"]["result"]["port"] = json!("absent"),
                "connection" => {
                    plan["nodes"]["left"]["inputs"]
                        .as_object_mut()
                        .unwrap()
                        .remove("source");
                }
                "permission" => (), // Host grant excludes seed@1 while the frozen Profile still permits it.
                "acceptance" => plan["goal_spec_digest"] = json!("0".repeat(64)),
                _ => panic!("unknown rejection fixture"),
            }
        }
        let receipt = tools
            .call("artifact.write", json!({"slot":slot,"value":value}))
            .await
            .unwrap();
        if matches!(self.shape, Shape::Repair) && attempt == Some(1) {
            *self.observed.candidate_sha.lock().unwrap() =
                Some(receipt["sha256"].as_str().unwrap().into());
        }
        ExecutionOutcome::Completed {
            usage: json!({"controlled":true}),
        }
    }
}
fn entry(input: &str, slot: &str, schema: &str) -> EntryContract {
    serde_json::from_value(json!({"instruction":"Controlled graph validation","executor":"fixture","skills":[],"tools":["artifact.write"],"input_schema":input,"outputs":{slot:{"schema":schema,"required":true}},"gate":"_tl.schema@1","outcomes":["completed"]})).unwrap()
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
async fn open(root: &std::path::Path, shape: Shape, observed: Arc<Observed>) -> (Runtime, Caller) {
    let mut registry = OrchestrationRegistry::new();
    for (id, schema) in [
        ("text", json!({"type":"string"})),
        ("planner", json!({"type":"object"})),
        (
            "goal",
            json!({"type":"object","properties":{"text":{"$ref":"text"}},"required":["text"],"additionalProperties":false}),
        ),
        (
            "worker",
            json!({"type":"object","properties":{"source":{"$ref":"text"}},"required":["source"],"additionalProperties":false}),
        ),
        (
            "pair",
            json!({"type":"object","properties":{"a":{"$ref":"text"},"b":{"$ref":"text"}},"required":["a","b"],"additionalProperties":false}),
        ),
    ] {
        registry
            .register_schema(id, &serde_json::to_vec(&schema).unwrap())
            .unwrap();
    }
    let policy = RunBudgetPolicy {
        policy_id: "graph-test".into(),
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
            entry("planner", "proposal", "_tl.planner-result@1"),
        ),
        ("worker@1", entry("worker", "value", "text")),
        ("pack@1", entry("pair", "result", "pair")),
        (
            "quality@1",
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
    for (id, source_policy) in [
        ("seed", json!({"kind":"goal","pointer":"/inputs/text"})),
        ("relay", json!({"kind":"artifact"})),
    ] {
        registry.register_capability(serde_json::from_value(json!({"protocol_version":1,"id":id,"version":"1","kind":"work","inputs":{"source":{"schema_ref":"text","required":true,"source_policy":source_policy}},"outputs":{"value":{"schema_ref":"text"}},"gate":"_tl.schema@1","max_rule_check_ms":1000,"members":{"worker":{"entry_ref":"worker@1","input_mode":"node_ports","input_bindings":{"source":{"port":"source","pointer":""}}}},"join":{"kind":"object-by-worker","outputs":{"value":{"kind":"member","member_id":"worker","slot":"value"}}}})).unwrap()).unwrap();
    }
    registry.register_capability(serde_json::from_value(json!({"protocol_version":1,"id":"pack","version":"1","kind":"work","inputs":{"a":{"schema_ref":"text","required":true,"source_policy":{"kind":"artifact"}},"b":{"schema_ref":"text","required":true,"source_policy":{"kind":"artifact"}}},"outputs":{"result":{"schema_ref":"pair"}},"gate":"_tl.schema@1","max_rule_check_ms":1000,"members":{"worker":{"entry_ref":"pack@1","input_mode":"node_ports","input_bindings":{"a":{"port":"a","pointer":""},"b":{"port":"b","pointer":""}}}},"join":{"kind":"object-by-worker","outputs":{"result":{"kind":"member","member_id":"worker","slot":"result"}}}})).unwrap()).unwrap();
    registry.register_capability(serde_json::from_value(json!({"protocol_version":1,"id":"input","version":"1","kind":"input","inputs":{},"outputs":{"value":{"schema_ref":"text"}},"gate":"_tl.schema@1","max_rule_check_ms":1000,"signal_schema_ref":"text","max_wait_ms":5000})).unwrap()).unwrap();
    let digest=registry.register_rules("quality",&serde_json::to_vec(&json!({"protocol_version":1,"id":"quality","version":"1","rules":[{"id":"joined","instruction":"Check accepted dependency values","pass_condition":"Exact expected values for this graph","evidence_required":true}]})).unwrap()).unwrap();
    registry
        .register_checker(
            "quality@1",
            CheckerContract {
                entry_ref: "quality@1".into(),
                rules_ref: "quality".into(),
                result_slot: "evaluation".into(),
                fresh_context: true,
                goal_input_fields: vec![],
            },
        )
        .unwrap();
    registry.register_profile(serde_json::from_value(json!({"id":"graphs","version":"1","goal_input_schema":"goal","goal_selectors":{"/inputs/text":"text"},"planner_entry_ref":"planner@1","capabilities":["seed@1","relay@1","pack@1","input@1"],"planning_input_schemas":[],"templates":[],"outputs":{"result":{"schema_ref":"pair","allowed_capabilities":["pack@1"]}},"final_outputs":["result"],"checks":[{"id":"quality","phase":"goal_final","checker_ref":"quality@1","rules_digest":digest,"target_outputs":["result"],"required":true}],"allowed_tools":["artifact.write"]})).unwrap()).unwrap();
    let bindings = HostBindings::default()
        .executor("fixture", Arc::new(GraphExecutor { shape, observed }))
        .grant(
            "local",
            Grant {
                definitions: vec![],
                tools: vec!["artifact.write".into()],
                limits: ResourceLimits {
                    max_wall_ms: 10000,
                    max_active_ms: 20000,
                    max_runs: 8,
                    max_transitions: 1000,
                },
            },
        );
    let receipt = registry.validate_and_freeze(&bindings).unwrap();
    let options = OrchestrationOptions {
        grants: BTreeMap::from([(
            "local".into(),
            GoalGrant {
                profile_digests: vec![receipt.profiles["graphs@1"].clone()],
                capabilities: if matches!(shape, Shape::Rejected("permission")) {
                    vec!["relay@1".into(), "pack@1".into(), "input@1".into()]
                } else {
                    vec![
                        "seed@1".into(),
                        "relay@1".into(),
                        "pack@1".into(),
                        "input@1".into(),
                    ]
                },
                tools: vec!["artifact.write".into()],
                limits: limits(),
            },
        )]),
        limits: limits(),
        max_admitted_goals: 4,
        max_spec_bytes: 1048576,
        max_plan_bytes: 4194304,
        max_nodes: 16,
        max_edges: 64,
        max_workers_per_node: 2,
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
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sequential_fork_and_all_join_graphs_execute_with_exact_public_dependencies() {
    for shape in [Shape::Sequence, Shape::Fork, Shape::Join] {
        let root = tempfile::tempdir().unwrap();
        let observed = Arc::new(Observed::default());
        let (runtime, caller) = open(&root.path().join("store"), shape, observed.clone()).await;
        let goals = runtime.goals().unwrap();
        let id = goals
            .submit(
                GoalRequest {
                    protocol_version: 1,
                    request_key: "graph".into(),
                    grant_ref: "local".into(),
                    spec: GoalSpec {
                        goal: "Execute and verify this bounded graph".into(),
                        inputs: json!({"text":"x"}),
                        profile_ref: "graphs@1".into(),
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
                },
                &caller,
            )
            .await
            .unwrap()
            .goal_id;
        let snapshot = tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                let s = goals.get(&id, &caller).await.unwrap();
                if s.status == GoalStatus::Finished {
                    break s;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            snapshot.outcome,
            Some(GoalOutcome::Achieved),
            "{shape:?}: {snapshot:?}"
        );
        assert!(
            snapshot
                .nodes
                .values()
                .all(|n| n.status == GoalNodeStatus::Succeeded)
        );
        let output = &snapshot.nodes["pack"].outputs["result"];
        assert_eq!(
            parse_unique(
                &goals
                    .read_artifact(&output.id, &caller)
                    .await
                    .unwrap()
                    .bytes
            )
            .unwrap(),
            expected(shape)
        );
        let plan = goals.plan(&id, &caller).await.unwrap();
        let events = observed.events.lock().unwrap().clone();
        for (node, spec) in &plan.proposal.nodes {
            for parent in &spec.depends_on {
                let done = events
                    .iter()
                    .position(|e| e == &(parent.clone(), false))
                    .unwrap();
                let start = events
                    .iter()
                    .position(|e| e == &(node.clone(), true))
                    .unwrap();
                assert!(done < start, "{parent} must complete before {node}");
            }
        }
        let pack = observed.pack_inputs.lock().unwrap().clone();
        for input in pack {
            assert!(
                snapshot
                    .artifacts
                    .iter()
                    .any(|a| a.accepted && a.member_id.is_none() && a.artifact.id == input.id),
                "downstream receives only public outputs"
            );
        }
        assert_eq!(
            observed.peak.load(Ordering::SeqCst),
            if matches!(shape, Shape::Sequence) {
                1
            } else {
                2
            }
        );
        assert_eq!(
            snapshot.budget.runs_used,
            if matches!(shape, Shape::Join) { 6 } else { 5 }
        );
        runtime.shutdown().await.unwrap();
    }
}

fn request() -> GoalRequest {
    GoalRequest {
        protocol_version: 1,
        request_key: "branches".into(),
        grant_ref: "local".into(),
        spec: GoalSpec {
            goal: "Verify independent branch progress".into(),
            inputs: json!({"text":"x"}),
            profile_ref: "graphs@1".into(),
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
                break s;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap()
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn input_waiting_and_independent_running_branch_have_distinct_status_and_slot_facts() {
    let root = tempfile::tempdir().unwrap();
    let observed = Arc::new(Observed::default());
    let (runtime, caller) = open(&root.path().join("store"), Shape::Wait, observed.clone()).await;
    let goals = runtime.goals().unwrap();
    let id = goals.submit(request(), &caller).await.unwrap().goal_id;
    tokio::time::timeout(Duration::from_secs(3), observed.left_started.notified())
        .await
        .unwrap();
    let running = goals.get(&id, &caller).await.unwrap();
    assert_eq!(running.status, GoalStatus::Running);
    assert_eq!(running.nodes["prompt"].status, GoalNodeStatus::WaitingInput);
    assert_eq!(running.nodes["left"].status, GoalNodeStatus::Running);
    assert_eq!(
        running.progress.goal_occupied_slots, 1,
        "input wait must not consume a slot"
    );
    assert!(
        !running
            .runs
            .values()
            .any(|r| matches!(&r.role,GoalRunRole::NodeMember{node_id,..} if node_id=="prompt"))
    );
    let wait = &running.waiting[0];
    goals
        .signal(
            &id,
            GoalSignalRequest {
                protocol_version: 1,
                wait_id: wait.wait_id.clone(),
                nonce: wait.nonce.clone(),
                payload: json!("provided"),
                request_key: "answer".into(),
                expected_revision: running.revision,
            },
            &caller,
        )
        .await
        .unwrap();
    assert_eq!(
        goals.get(&id, &caller).await.unwrap().nodes["pack"].status,
        GoalNodeStatus::Pending
    );
    observed.release_left.notify_one();
    let snapshot = finished(&goals, &id, &caller).await;
    assert_eq!(snapshot.outcome, Some(GoalOutcome::Achieved));
    assert_eq!(
        snapshot.budget.runs_used, 4,
        "planner, left, pack, final; no input Run"
    );
    assert_eq!(observed.peak.load(Ordering::SeqCst), 1);
    runtime.shutdown().await.unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_branch_prevents_join_and_retains_an_already_accepted_sibling_output() {
    let root = tempfile::tempdir().unwrap();
    let observed = Arc::new(Observed::default());
    let (runtime, caller) = open(
        &root.path().join("store"),
        Shape::FailureFork,
        observed.clone(),
    )
    .await;
    let goals = runtime.goals().unwrap();
    let id = goals.submit(request(), &caller).await.unwrap().goal_id;
    let snapshot = finished(&goals, &id, &caller).await;
    assert_eq!(snapshot.outcome, Some(GoalOutcome::Failed));
    assert_eq!(
        snapshot.nodes["pack"].status,
        GoalNodeStatus::DependencyFailed
    );
    assert!(
        !observed
            .events
            .lock()
            .unwrap()
            .iter()
            .any(|(node, _)| node == "pack")
    );
    let right = &snapshot.nodes["right"].outputs["value"];
    assert_eq!(
        parse_unique(&goals.read_artifact(&right.id, &caller).await.unwrap().bytes).unwrap(),
        "x:R"
    );
    assert_eq!(snapshot.budget.runs_used, 3);
    assert_eq!(snapshot.progress.goal_occupied_slots, 0);
    runtime.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_invalid_plan_is_corrected_once_with_exact_feedback_before_business_dispatch() {
    let root = tempfile::tempdir().unwrap();
    let observed = Arc::new(Observed::default());
    let (runtime, caller) = open(&root.path().join("store"), Shape::Repair, observed.clone()).await;
    let goals = runtime.goals().unwrap();
    let mut request = request();
    request.spec.planning.max_revisions = 1;
    let original = request.spec.clone();
    let id = goals.submit(request, &caller).await.unwrap().goal_id;
    let snapshot = finished(&goals, &id, &caller).await;
    assert_eq!(snapshot.outcome, Some(GoalOutcome::Achieved));
    assert_eq!(snapshot.budget.runs_used, 6);
    let rejected = snapshot
        .runs
        .values()
        .find(|r| matches!(r.role, GoalRunRole::Planner { attempt: 1 }))
        .unwrap();
    assert!(
        rejected
            .plan_validation_failure
            .as_ref()
            .unwrap()
            .issues
            .iter()
            .any(|i| i.category == PlanIssueCategory::Cycle && i.path.starts_with('/'))
    );
    let repaired = snapshot
        .runs
        .values()
        .find(|r| matches!(r.role, GoalRunRole::Planner { attempt: 2 }))
        .unwrap();
    assert!(repaired.plan_validation_failure.is_none());
    let readonly = GoalClient::open(root.path().join("store"), caller.clone()).unwrap();
    let persisted = readonly.get(&id).unwrap();
    assert_eq!(
        serde_json::to_value(&persisted.runs[&rejected.run_id].plan_validation_failure).unwrap(),
        serde_json::to_value(&rejected.plan_validation_failure).unwrap()
    );
    assert_eq!(
        snapshot
            .runs
            .values()
            .filter(|r| matches!(r.role, GoalRunRole::Planner { .. }))
            .count(),
        2
    );
    assert_eq!(
        serde_json::to_value(snapshot.spec).unwrap(),
        serde_json::to_value(original).unwrap()
    );
    assert_eq!(
        snapshot
            .artifacts
            .iter()
            .filter(|a| a.producer_kind == "planner_candidate")
            .count(),
        2
    );
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn six_invalid_plan_categories_have_public_locations_and_zero_business_dispatch() {
    for (kind, category) in [
        ("format", PlanIssueCategory::Format),
        ("cycle", PlanIssueCategory::Cycle),
        ("reference", PlanIssueCategory::Reference),
        ("connection", PlanIssueCategory::InputConnection),
        ("permission", PlanIssueCategory::CapabilityPermission),
        ("acceptance", PlanIssueCategory::BudgetAcceptance),
    ] {
        let root = tempfile::tempdir().unwrap();
        let observed = Arc::new(Observed::default());
        let (runtime, caller) = open(
            &root.path().join("store"),
            Shape::Rejected(kind),
            observed.clone(),
        )
        .await;
        let goals = runtime.goals().unwrap();
        let id = goals.submit(request(), &caller).await.unwrap().goal_id;
        let snapshot = finished(&goals, &id, &caller).await;
        assert_eq!(
            snapshot.outcome,
            Some(GoalOutcome::Failed),
            "{kind}: {snapshot:?}"
        );
        assert_eq!(snapshot.budget.runs_used, 1);
        assert!(snapshot.nodes.is_empty());
        assert!(observed.events.lock().unwrap().is_empty());
        assert_eq!(observed.peak.load(Ordering::SeqCst), 0);
        let run = snapshot.runs.values().next().unwrap();
        let failure = run
            .plan_validation_failure
            .as_ref()
            .expect("original structured rejection must remain public");
        assert!(
            failure
                .issues
                .iter()
                .any(|issue| issue.category == category),
            "{kind}: {failure:?}"
        );
        assert!(
            failure
                .issues
                .iter()
                .all(|issue| issue.path.starts_with('/') && !issue.message.is_empty())
        );
        assert!(!snapshot.artifacts.iter().any(|a| a.accepted));
        assert_eq!(snapshot.progress.goal_occupied_slots, 0);
        assert!(snapshot.budget.reserved.is_empty());
        runtime.shutdown().await.unwrap();
    }
}
