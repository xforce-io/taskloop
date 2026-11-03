mod common;
use async_trait::async_trait;
use common::*;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use taskloop::*;
use tokio_util::sync::CancellationToken;
struct Capture;
#[async_trait]
impl DomainTool for Capture {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            id: "test.capture".into(),
            description: "Save an instance- and visit-specific snapshot.".into(),
            input_schema: json!({"type":"object","properties":{},"additionalProperties":false}),
        }
    }
    async fn call(&self, c: ToolContext, _: Value) -> Result<ToolOutput> {
        let text = format!(
            "{}:{}:{}",
            c.state_id,
            c.visit,
            c.inputs["nonce"].as_str().unwrap()
        );
        Ok(ToolOutput {
            payload: json!({"snapshot_slot":"capture.snapshot"}),
            files: vec![Candidate::bytes("capture.snapshot", text.into_bytes())],
        })
    }
}
struct Gate;
#[async_trait]
impl DomainGate for Gate {
    async fn check(&self, c: GateContext) -> CheckResult {
        assert_eq!(c.output("result").unwrap()["nonce"], c.inputs["nonce"]);
        CheckResult::pass(
            if c.state_id == "work" && c.visit == 1 {
                "more"
            } else {
                "ready"
            },
            vec![c.check_evidence_ref],
        )
    }
}
struct Ex {
    include: bool,
    all: bool,
    observed: Arc<AtomicUsize>,
}
#[async_trait]
impl Executor for Ex {
    fn capabilities(&self) -> ExecutorCapabilities {
        ExecutorCapabilities::MANAGED
    }
    async fn execute(
        &self,
        inv: Invocation,
        bridge: Arc<dyn ToolBridge>,
        _: CancellationToken,
    ) -> ExecutionOutcome {
        assert_eq!(inv.stage_input["flow_input"], inv.inputs);
        if inv.state_id == "analyze" {
            assert!(
                inv.artifacts.iter().all(|a| a.state_id == "work"),
                "unrelated producer artifacts must never be granted"
            );
            let snapshots: Vec<_> = inv
                .artifacts
                .iter()
                .filter(|a| a.slot == "capture.snapshot")
                .collect();
            assert_eq!(
                snapshots.len(),
                if self.include {
                    if self.all { 2 } else { 1 }
                } else {
                    0
                }
            );
            let output_count = inv.artifacts.iter().filter(|a| a.slot == "result").count();
            assert_eq!(output_count, if self.all { 2 } else { 1 });
            for a in snapshots {
                assert!(
                    self.all || a.visit == 2,
                    "latest must not grant an older visit"
                );
                let read = bridge
                    .call("artifact.read", json!({"id":a.id}))
                    .await
                    .unwrap();
                assert_eq!(
                    read["text"],
                    format!("work:{}:{}", a.visit, inv.inputs["nonce"].as_str().unwrap())
                );
            }
            self.observed.fetch_add(1, Ordering::SeqCst);
        } else {
            bridge.call("test.capture", json!({})).await.unwrap();
        }
        bridge
            .call(
                "artifact.write",
                json!({"slot":"result","value":{"nonce":inv.inputs["nonce"]}}),
            )
            .await
            .unwrap();
        ExecutionOutcome::Completed {
            usage: json!({"kind":"deterministic-attachment-test"}),
        }
    }
}
fn fixture(include: bool, selection: &str) -> Fixture {
    let mut f = Fixture::new(false);
    let p = f.root.path().join("flow.json");
    let mut b: Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
    let mut producer = b["entries"]["test.work"].clone();
    producer["attachments"] =
        json!({"slot_prefix":"capture.","path_prefix":"captures/","max_files":2,"max_bytes":4096});
    producer["tools"] = json!(["artifact.read", "artifact.write", "test.capture"]);
    b["entries"]["test.other"] = producer.clone();
    let mut analyze = b["entries"]["test.work"].clone();
    analyze["requires"] = json!([{"state":"work","slot":"result","selection":selection,"include_attachments":include}]);
    b["entries"]["test.analyze"] = analyze;
    producer["outcomes"] = json!(["ready", "more"]);
    b["entries"]["test.work"] = producer;
    b["flow"]["states"]["work"]["accepted"] = json!({"more":"work","ready":"other"});
    b["flow"]["states"]["other"] = json!({"kind":"work","entry":"test.other","accepted":{"ready":"analyze"},"rejected":"failed","failed":"failed"});
    b["flow"]["states"]["analyze"] = json!({"kind":"work","entry":"test.analyze","accepted":{"ready":"done"},"rejected":"failed","failed":"failed"});
    std::fs::write(&p, serde_json::to_vec(&b).unwrap()).unwrap();
    f.definition = Definition::load(
        &p,
        &BTreeMap::from([("demo-skill@0.1.0".into(), f.root.path().join("demo-skill"))]),
    )
    .unwrap();
    f
}
#[tokio::test]
async fn attachments_require_explicit_grant_and_stay_with_selected_producer_visit_and_instance() {
    for (include, selection) in [(false, "latest"), (true, "latest"), (true, "all")] {
        let f = fixture(include, selection);
        let observed = Arc::new(AtomicUsize::new(0));
        let mut bindings = f.bindings("pass");
        bindings.executors.insert(
            "test.executor".into(),
            Arc::new(Ex {
                include,
                all: selection == "all",
                observed: observed.clone(),
            }),
        );
        bindings.gates.insert("test.gate".into(), Arc::new(Gate));
        bindings
            .tools
            .insert("test.capture".into(), Arc::new(Capture));
        bindings
            .grants
            .get_mut("local-readonly")
            .unwrap()
            .tools
            .push("test.capture".into());
        let rt = Runtime::open(
            RuntimeOptions::new(f.root.path().join("store")),
            vec![f.definition.clone()],
            bindings,
        )
        .await
        .unwrap();
        let a = rt
            .start(f.request("a", "INSTANCE-A", false), &Caller::local())
            .await
            .unwrap();
        let b = rt
            .start(f.request("b", "INSTANCE-B", false), &Caller::local())
            .await
            .unwrap();
        for id in [a, b] {
            assert_eq!(
                wait_for(&rt, &id, |s| s.status.terminal()).await.status,
                Status::Completed
            );
        }
        assert_eq!(observed.load(Ordering::SeqCst), 2);
        rt.shutdown().await.unwrap();
    }
}

struct EvidenceGate {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
    evidence: Arc<std::sync::Mutex<String>>,
    allowed: Arc<std::sync::Mutex<Vec<String>>>,
}
#[async_trait]
impl DomainGate for EvidenceGate {
    async fn check(&self, c: GateContext) -> CheckResult {
        if c.state_id != "analyze" {
            return Gate.check(c).await;
        }
        *self.allowed.lock().unwrap() = c.accepted.iter().map(|a| a.reference.id.clone()).collect();
        self.entered.notify_one();
        self.release.notified().await;
        CheckResult::pass("ready", vec![self.evidence.lock().unwrap().clone()])
    }
}
struct EvidenceExecutor(Ex);
#[async_trait]
impl Executor for EvidenceExecutor {
    fn capabilities(&self) -> ExecutorCapabilities {
        ExecutorCapabilities::MANAGED
    }
    async fn execute(
        &self,
        inv: Invocation,
        bridge: Arc<dyn ToolBridge>,
        cancel: CancellationToken,
    ) -> ExecutionOutcome {
        let extra = (inv.state_id == "work").then(|| inv.inputs["nonce"].clone());
        let outcome = self.0.execute(inv, bridge.clone(), cancel).await;
        if let Some(nonce) = extra {
            bridge
                .call(
                    "artifact.write",
                    json!({"slot":"extra","value":{"nonce":nonce}}),
                )
                .await
                .unwrap();
        }
        outcome
    }
}
#[tokio::test]
async fn gate_evidence_obeys_the_same_visit_slot_and_attachment_grants() {
    for (include, selection, producer, visit, slot, authorized) in [
        (false, "latest", "work", 1, "result", false),
        (false, "latest", "work", 2, "capture.snapshot", false),
        (true, "latest", "other", 1, "result", false),
        (false, "latest", "work", 2, "extra", false),
        (true, "all", "work", 1, "result", true),
        (true, "latest", "work", 2, "capture.snapshot", true),
    ] {
        let mut f = fixture(include, selection);
        let path = f.root.path().join("flow.json");
        let mut b: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        b["entries"]["test.work"]["outputs"]["extra"] =
            json!({"schema":"result.schema.json","required":true});
        std::fs::write(&path, serde_json::to_vec(&b).unwrap()).unwrap();
        f.definition = Definition::load(
            &path,
            &BTreeMap::from([("demo-skill@0.1.0".into(), f.root.path().join("demo-skill"))]),
        )
        .unwrap();
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let evidence = Arc::new(std::sync::Mutex::new(String::new()));
        let allowed = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut b = f.bindings("pass");
        b.executors.insert(
            "test.executor".into(),
            Arc::new(EvidenceExecutor(Ex {
                include,
                all: selection == "all",
                observed: Arc::new(AtomicUsize::new(0)),
            })),
        );
        b.gates.insert(
            "test.gate".into(),
            Arc::new(EvidenceGate {
                entered: entered.clone(),
                release: release.clone(),
                evidence: evidence.clone(),
                allowed: allowed.clone(),
            }),
        );
        b.tools.insert("test.capture".into(), Arc::new(Capture));
        b.grants
            .get_mut("local-readonly")
            .unwrap()
            .tools
            .push("test.capture".into());
        let rt = Runtime::open(
            RuntimeOptions::new(f.root.path().join("store")),
            vec![f.definition.clone()],
            b,
        )
        .await
        .unwrap();
        let id = rt
            .start(f.request("evidence", "A", false), &Caller::local())
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), entered.notified())
            .await
            .unwrap();
        let snapshot = rt.get(&id, &Caller::local()).await.unwrap();
        let victim = snapshot
            .accepted_artifacts
            .iter()
            .find(|a| a.state_id == producer && a.visit == visit && a.slot == slot)
            .unwrap()
            .id
            .clone();
        assert_eq!(allowed.lock().unwrap().contains(&victim), authorized);
        *evidence.lock().unwrap() = victim;
        release.notify_one();
        let result = wait_for(&rt, &id, |s| {
            s.status.terminal() || s.status == Status::Blocked
        })
        .await;
        if authorized {
            assert_eq!(result.status, Status::Completed);
        } else {
            assert_eq!(result.status, Status::Blocked, "{producer}/{visit}/{slot}");
            assert_eq!(result.error.unwrap().code, "gate_protocol_invalid");
        }
        rt.shutdown().await.unwrap();
    }
}
