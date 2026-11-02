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
