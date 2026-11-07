//! An independent package consuming only TaskLoop's public API.
use async_trait::async_trait;
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use taskloop::*;
use tokio_util::sync::CancellationToken;
struct ExecutorImpl;
#[async_trait]
impl Executor for ExecutorImpl {
    fn capabilities(&self) -> ExecutorCapabilities {
        ExecutorCapabilities::MANAGED
    }
    async fn execute(
        &self,
        inv: Invocation,
        bridge: Arc<dyn ToolBridge>,
        _: CancellationToken,
    ) -> ExecutionOutcome {
        match bridge
            .call(
                "artifact.write",
                json!({"slot":"result","value":{"answer":inv.inputs["question"]}}),
            )
            .await
        {
            Ok(_) => ExecutionOutcome::Completed {
                usage: json!({"kind":"deterministic-consumer"}),
            },
            Err(e) => ExecutionOutcome::Failed {
                code: e.code,
                message: e.message,
            },
        }
    }
    async fn stop_or_confirm(&self, _: &str) -> StopState {
        StopState::Confirmed
    }
}
struct ContentFilter;
#[async_trait]
impl TraceContentFilter for ContentFilter {
    async fn redact(&self, _: &str, _: &str, value: Value) -> Result<Value> {
        Ok(value)
    }
}
struct Gate;
#[async_trait]
impl DomainGate for Gate {
    async fn check(&self, c: GateContext) -> CheckResult {
        if c.output("result").unwrap()["answer"] == c.inputs["question"] {
            CheckResult::pass("ready", vec![c.check_evidence_ref])
        } else {
            CheckResult::fail("mismatch", "result", "wrong input")
        }
    }
}
async fn waiting(rt: &Runtime, id: &str) -> ExecutionSnapshot {
    for _ in 0..100 {
        let s = rt.get(id, &Caller::local()).await.unwrap();
        if s.status == Status::Waiting {
            return s;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("consumer never reached waiting")
}
#[tokio::main]
async fn main() {
    let root = tempfile::tempdir().unwrap();
    let skill = root.path().join("consumer-method");
    std::fs::create_dir(&skill).unwrap();
    std::fs::write(skill.join("SKILL.md"),"---\nname: consumer-method\ndescription: External consumer method.\n---\nReturn the current input only.\n").unwrap();
    assert_eq!(validate_skill(&skill).unwrap().name, "consumer-method");
    for (name, schema) in [
        (
            "input.schema.json",
            json!({"type":"object","properties":{"question":{"type":"string"}},"required":["question"],"additionalProperties":false}),
        ),
        (
            "result.schema.json",
            json!({"type":"object","properties":{"answer":{"type":"string"}},"required":["answer"],"additionalProperties":false}),
        ),
        (
            "signal.schema.json",
            json!({"type":"object","properties":{},"additionalProperties":false}),
        ),
    ] {
        std::fs::write(root.path().join(name), serde_json::to_vec(&schema).unwrap()).unwrap();
    }
    let bundle = json!({"contract_version":1,"entries":{"consumer.work":{"instruction":"Copy question to result.answer using artifact.write.","executor":"consumer.executor","skills":["consumer-method@1.0.0"],"tools":["artifact.write"],"input_schema":"task-input.schema.json","outputs":{"result":{"schema":"result.schema.json","required":true}},"gate":"consumer.gate","outcomes":["ready"]}},"flow":{"id":"consumer.flow","version":"1.0.0","input_schema":"input.schema.json","initial":"work","states":{"work":{"kind":"work","entry":"consumer.work","accepted":{"ready":"await"},"rejected":"failed","failed":"failed"},"await":{"kind":"wait","signals":{"confirm":{"schema":"signal.schema.json","target":"done"}},"timeout_seconds":60,"timeout_target":"failed"},"done":{"kind":"terminal","status":"completed","outcome":"ready","requires":[{"state":"work","slot":"result","selection":"latest"}]},"failed":{"kind":"terminal","status":"failed","outcome":"failure","requires":[]}},"limits":{"max_transitions":8,"max_visits_per_state":2,"max_runs":3}}});
    let file = root.path().join("flow.json");
    std::fs::write(&file, serde_json::to_vec(&bundle).unwrap()).unwrap();
    let definition = Definition::load(
        file,
        &BTreeMap::from([("consumer-method@1.0.0".into(), skill)]),
    )
    .unwrap();
    let bindings = HostBindings::default()
        .configuration_version("consumer@1")
        .executor("consumer.executor", Arc::new(ExecutorImpl))
        .gate("consumer.gate", Arc::new(Gate))
        .grant(
            "local-readonly",
            Grant {
                definitions: vec![definition.digest.clone()],
                tools: vec!["artifact.write".into()],
                limits: ResourceLimits::default(),
            },
        );
    assert!(validate(&definition, &bindings).unwrap().valid);
    let digest = definition.digest.clone();
    let mut options = RuntimeOptions::new(root.path().join("store"));
    options.entry_budgets.insert(
        "consumer.work".into(),
        RunBudgetPolicy {
            policy_id: "consumer-budget-v1".into(),
            max_run_ms: None,
            max_check_ms: None,
            max_tool_calls: 4,
            submission_reserve: 2,
            finalize_before_ms: 0,
            max_model_iterations: None,
            max_candidate_bytes: 4096,
        },
    );
    let rt = Runtime::open_with_trace_policy(
        options,
        vec![definition],
        bindings,
        TraceOptions {
            max_events_per_run: 1000,
            max_bytes_per_run: 1048576,
            source_max_events: 1000,
            source_max_bytes: 262144,
        },
        TracePolicy {
            policy_id: "consumer-v1".into(),
            max_total_events: 10000,
            max_total_bytes: 4194304,
            max_runs: 16,
            retention_ms: 60000,
            max_content_bytes: 4096,
            content_tools: BTreeMap::from([(
                "artifact.write".into(),
                TraceToolContent {
                    input: vec!["/value".into()],
                    output: vec![],
                },
            )]),
            filter_version: Some("consumer-fields-v1".into()),
            filter: Some(Arc::new(ContentFilter)),
        },
    )
    .await
    .unwrap();
    let request = |key: &str| StartRequest {
        definition_digest: digest.clone(),
        inputs: json!({"question":"external-host"}),
        grant_ref: "local-readonly".into(),
        request_key: key.into(),
        limits: ResourceLimits::default(),
    };
    let a = rt.start(request("a"), &Caller::local()).await.unwrap();
    let snapshot = waiting(&rt, &a).await;
    let artifact = rt
        .read_artifact(&snapshot.accepted_artifacts[0].id, &Caller::local())
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&artifact.bytes).unwrap()["answer"],
        "external-host"
    );
    rt.signal(
        &a,
        SignalRequest {
            event_name: "confirm".into(),
            payload: json!({}),
            request_key: "confirm-a".into(),
            expected_revision: snapshot.revision,
        },
        &Caller::local(),
    )
    .await
    .unwrap();
    assert_eq!(
        rt.get(&a, &Caller::local()).await.unwrap().status,
        Status::Completed
    );
    let b = rt.start(request("b"), &Caller::local()).await.unwrap();
    waiting(&rt, &b).await;
    assert_eq!(
        rt.cancel(&b, "cancel-b", &Caller::local())
            .await
            .unwrap()
            .status,
        Status::Cancelled
    );
    for _ in 0..100 {
        let pending = rt
            .trace(
                &a,
                TraceQuery {
                    limit: 100,
                    ..Default::default()
                },
                &Caller::local(),
            )
            .unwrap();
        if !pending
            .issues
            .iter()
            .any(|s| s.starts_with("content_pending:"))
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let trace = rt
        .trace(
            &a,
            TraceQuery {
                limit: 100,
                ..Default::default()
            },
            &Caller::local(),
        )
        .unwrap();
    assert!(trace.enabled);
    assert!(trace.events.iter().any(|e| e.event.kind == "tool.started"));
    assert!(
        trace
            .events
            .iter()
            .any(|e| e.event.kind == "check.finished")
    );
    assert!(
        trace
            .issues
            .iter()
            .any(|s| s.starts_with("internal_unavailable:"))
    );
    let exported: TracePage = serde_json::from_slice(
        &rt.export_trace(
            &a,
            TraceQuery {
                limit: 100,
                ..Default::default()
            },
            &Caller::local(),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(exported.events.len(), trace.events.len());
    let content_ref: TraceContentRef = serde_json::from_value(
        trace
            .events
            .iter()
            .find_map(|e| {
                let value =
                    if e.event.kind == "tool.content" && e.event.data["direction"] == "input" {
                        e.event.data.get("content")
                    } else if e.event.kind == "tool.started" {
                        e.event.data.get("input_content")
                    } else {
                        None
                    };
                value.filter(|v| !v.is_null()).cloned()
            })
            .expect("no settled input content reference"),
    )
    .unwrap();
    let content = rt
        .read_trace_content(&a, &content_ref.id, &Caller::local())
        .unwrap();
    assert_eq!(content.value["/value"]["answer"], "external-host");
    assert_eq!(
        rt.read_trace_content(&b, &content_ref.id, &Caller::local())
            .unwrap_err()
            .code,
        "not_found"
    );
    let budget = rt.budgets(&a, &Caller::local()).unwrap();
    assert_eq!(budget.runs.len(), 1);
    assert_eq!(budget.runs[0].policy.policy_id, "consumer-budget-v1");
    assert_eq!(budget.runs[0].calls_used, 1);
    assert_eq!(budget.runs[0].policy.submission_reserve, 2);
    rt.maintain_trace().unwrap();
    rt.shutdown().await.unwrap();
    println!(
        "{}",
        json!({"external_consumer":"pass","operations":["start","get","signal","cancel","read_artifact","trace","export_trace","read_trace_content","maintain_trace","budgets"],"private_modules_imported":false,"execution":"deterministic; real Harness is tested separately"})
    );
}
