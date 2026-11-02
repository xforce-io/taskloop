#![allow(dead_code)]
use async_trait::async_trait;
use serde_json::json;
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use taskloop::*;
use tokio_util::sync::CancellationToken;
pub struct Fixture {
    pub root: tempfile::TempDir,
    pub definition: Definition,
}
impl Fixture {
    pub fn new(wait: bool) -> Self {
        let root = tempfile::tempdir().unwrap();
        let skill = root.path().join("demo-skill");
        std::fs::create_dir(&skill).unwrap();
        std::fs::write(skill.join("SKILL.md"),"---\nname: demo-skill\ndescription: Test bounded execution.\nmetadata:\n  version: \"0.1.0\"\n---\nComplete only the current entry.\n").unwrap();
        std::fs::write(root.path().join("input.schema.json"),serde_json::to_vec(&json!({"type":"object","properties":{"nonce":{"type":"string"},"hold":{"type":"boolean"}},"required":["nonce"],"additionalProperties":false})).unwrap()).unwrap();
        std::fs::write(root.path().join("result.schema.json"),serde_json::to_vec(&json!({"type":"object","properties":{"nonce":{"type":"string"}},"required":["nonce"],"additionalProperties":false})).unwrap()).unwrap();
        std::fs::write(root.path().join("resume.schema.json"),serde_json::to_vec(&json!({"type":"object","properties":{"confirm":{"type":"boolean"}},"required":["confirm"],"additionalProperties":false})).unwrap()).unwrap();
        let mut states = json!({"work":{"kind":"work","entry":"test.work","accepted":{"ready":if wait{"await"}else{"done"}},"rejected":"failed","failed":"failed"},"done":{"kind":"terminal","status":"completed","outcome":"ready","requires":[{"state":"work","slot":"result","selection":"latest"}]},"failed":{"kind":"terminal","status":"failed","outcome":"failure","requires":[]}});
        if wait {
            states["await"] = json!({"kind":"wait","signals":{"resume":{"schema":"resume.schema.json","target":"done"}},"timeout_seconds":60,"timeout_target":"partial"});
            states["partial"] = json!({"kind":"terminal","status":"completed","outcome":"insufficient","requires":[]});
        }
        let bundle = json!({"contract_version":1,"entries":{"test.work":{"instruction":"Copy the current nonce to result using artifact.write.","executor":"test.executor","skills":["demo-skill@0.1.0"],"tools":["artifact.read","artifact.write"],"input_schema":"task-input.schema.json","outputs":{"result":{"schema":"result.schema.json","required":true}},"gate":"test.gate","outcomes":["ready"],"requires":[]}},"flow":{"id":"test.flow","version":"1.0.0","initial":"work","input_schema":"input.schema.json","states":states,"limits":{"max_transitions":8,"max_visits_per_state":3,"max_runs":4}}});
        std::fs::write(
            root.path().join("flow.json"),
            serde_json::to_vec(&bundle).unwrap(),
        )
        .unwrap();
        let definition = Definition::load(
            root.path().join("flow.json"),
            &BTreeMap::from([("demo-skill@0.1.0".into(), skill)]),
        )
        .unwrap();
        Self { root, definition }
    }
    pub fn bindings(&self, kind: &str) -> HostBindings {
        HostBindings::default()
            .configuration_version("fixture@1")
            .executor("test.executor", Arc::new(CopyExecutor))
            .gate("test.gate", Arc::new(TestGate(kind.into())))
            .grant(
                "local-readonly",
                Grant {
                    definitions: vec![self.definition.digest.clone()],
                    tools: vec!["artifact.read".into(), "artifact.write".into()],
                    limits: ResourceLimits::default(),
                },
            )
    }
    pub async fn open(&self, kind: &str) -> Runtime {
        Runtime::open(
            RuntimeOptions::new(self.root.path().join("store")),
            vec![self.definition.clone()],
            self.bindings(kind),
        )
        .await
        .unwrap()
    }
    pub fn request(&self, key: &str, nonce: &str, hold: bool) -> StartRequest {
        StartRequest {
            definition_digest: self.definition.digest.clone(),
            inputs: json!({"nonce":nonce,"hold":hold}),
            grant_ref: "local-readonly".into(),
            request_key: key.into(),
            limits: ResourceLimits::default(),
        }
    }
}
struct CopyExecutor;
#[async_trait]
impl Executor for CopyExecutor {
    fn capabilities(&self) -> ExecutorCapabilities {
        ExecutorCapabilities::MANAGED
    }
    async fn execute(
        &self,
        inv: Invocation,
        tools: Arc<dyn ToolBridge>,
        cancel: CancellationToken,
    ) -> ExecutionOutcome {
        if inv.inputs["hold"] == true {
            cancel.cancelled().await;
            return ExecutionOutcome::Cancelled;
        }
        match tools
            .call(
                "artifact.write",
                json!({"slot":"result","value":{"nonce":inv.inputs["nonce"]}}),
            )
            .await
        {
            Ok(_) => ExecutionOutcome::Completed {
                usage: json!({"kind":"deterministic","model":null}),
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
struct TestGate(String);
#[async_trait]
impl DomainGate for TestGate {
    async fn check(&self, c: GateContext) -> CheckResult {
        match self.0.as_str() {
            "error" => CheckResult::unavailable("fixture unavailable"),
            "fail" => CheckResult::fail("bad", "result", "fixture rejected"),
            "invented" => CheckResult::pass("ready", vec!["imaginary".into()]),
            _ => {
                if c.output("result").unwrap()["nonce"] != c.inputs["nonce"] {
                    return CheckResult::fail("mismatch", "result", "wrong instance nonce");
                }
                CheckResult::pass("ready", vec![c.check_evidence_ref])
            }
        }
    }
}
pub async fn wait_for(
    rt: &Runtime,
    id: &str,
    predicate: impl Fn(&ExecutionSnapshot) -> bool,
) -> ExecutionSnapshot {
    for _ in 0..150 {
        let s = rt.get(id, &Caller::local()).await.unwrap();
        if predicate(&s) {
            return s;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out: {:?}", rt.get(id, &Caller::local()).await);
}
