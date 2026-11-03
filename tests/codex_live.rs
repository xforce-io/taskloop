//! Opt-in real provider tests; no fixtures may be substituted for these claims.
mod common;
use async_trait::async_trait;
use common::*;
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use taskloop::*;
struct MethodGate {
    expected: String,
}
#[async_trait]
impl DomainGate for MethodGate {
    async fn check(&self, c: GateContext) -> CheckResult {
        match c.output("result") {
            Ok(v) if v["nonce"] == self.expected => {
                CheckResult::pass("ready", vec![c.check_evidence_ref])
            }
            _ => CheckResult::fail(
                "method_missing",
                "result",
                "Agent did not use the locked method nonce",
            ),
        }
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires local Codex 0.166.0 login and real model access"]
async fn real_agent_reads_locked_method_and_submits_only_through_host_tools() {
    let mut f = Fixture::new(false);
    let nonce = format!("METHOD-{}", uuid::Uuid::new_v4());
    let skill = f.root.path().join("demo-skill");
    let method = format!(
        "---\nname: demo-skill\ndescription: Test locked method delivery.\nmetadata:\n  version: \"0.1.0\"\n---\nWhen writing the current result, nonce MUST be {nonce}. Do not copy the nonce supplied in business inputs. This exact value exists only in this locked method. Use the host artifact.write tool then finish.\n"
    );
    std::fs::write(skill.join("SKILL.md"), method).unwrap();
    f.definition = Definition::load(
        f.root.path().join("flow.json"),
        &BTreeMap::from([("demo-skill@0.1.0".into(), skill)]),
    )
    .unwrap();
    let mut bindings = f.bindings("pass");
    bindings.executors.insert(
        "test.executor".into(),
        Arc::new(AuditedExecutor {
            native: CodexExecutor::new("gpt-6.1-sol").unwrap(),
            runs: Arc::new(std::sync::Mutex::new(Vec::new())),
            denials: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }),
    );
    bindings.gates.insert(
        "test.gate".into(),
        Arc::new(MethodGate {
            expected: nonce.clone(),
        }),
    );
    let rt = Runtime::open(
        RuntimeOptions::new(f.root.path().join("store")),
        vec![f.definition.clone()],
        bindings,
    )
    .await
    .unwrap();
    let id = rt
        .start(
            f.request("live-method", "NOT-THE-METHOD", false),
            &Caller::local(),
        )
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(240);
    let snapshot = loop {
        let s = rt.get(&id, &Caller::local()).await.unwrap();
        if s.status.terminal() || s.status == Status::Blocked {
            break s;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "real provider deadline; last snapshot={s:?}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    assert_eq!(snapshot.status, Status::Completed, "{snapshot:?}");
    let artifact = rt
        .read_artifact(&snapshot.accepted_artifacts[0].id, &Caller::local())
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&artifact.bytes).unwrap()["nonce"],
        nonce
    );
    println!(
        "{}",
        json!({"test":"real_method_delivery","status":"pass","model":"gpt-6.1-sol","execution_id":id,"artifact_sha256":artifact.reference.sha256,"codex_version":"0.166.0"})
    );
    rt.shutdown().await.unwrap();
}

struct Barrier {
    entered: Arc<tokio::sync::Notify>,
    calls: Arc<std::sync::Mutex<Vec<String>>>,
}
#[async_trait]
impl DomainTool for Barrier {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            id: "test.barrier".into(),
            description:
                "Call once before writing result. The host releases the current instance when safe."
                    .into(),
            input_schema: json!({"type":"object","properties":{},"additionalProperties":false}),
        }
    }
    async fn call(&self, context: ToolContext, _: Value) -> Result<ToolOutput> {
        self.calls
            .lock()
            .unwrap()
            .push(context.inputs["nonce"].as_str().unwrap().into());
        if context.inputs["hold"] == true {
            self.entered.notify_one();
            while !context.is_cancelled() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            return Err(Error::new("cancelled", "/tool", "cancelled instance"));
        }
        Ok(ToolOutput::json(json!({"nonce":context.inputs["nonce"]})))
    }
}
struct AuditedExecutor {
    native: CodexExecutor,
    runs: Arc<std::sync::Mutex<Vec<String>>>,
    denials: Arc<std::sync::atomic::AtomicUsize>,
}
struct AuditBridge {
    inner: Arc<dyn ToolBridge>,
    denials: Arc<std::sync::atomic::AtomicUsize>,
}
#[async_trait]
impl ToolBridge for AuditBridge {
    fn specs(&self) -> Vec<ToolSpec> {
        self.inner.specs()
    }
    async fn call(&self, id: &str, args: Value) -> Result<Value> {
        let result = self.inner.call(id, args).await;
        if result
            .as_ref()
            .is_err_and(|e| e.code == "capability_denied")
        {
            self.denials
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        result
    }
}
#[async_trait]
impl Executor for AuditedExecutor {
    fn capabilities(&self) -> ExecutorCapabilities {
        self.native.capabilities()
    }
    fn identity(&self) -> Value {
        self.native.identity()
    }
    async fn execute(
        &self,
        invocation: Invocation,
        bridge: Arc<dyn ToolBridge>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> ExecutionOutcome {
        let run_id = invocation.run_id.clone();
        self.runs.lock().unwrap().push(run_id.clone());
        let started = std::time::Instant::now();
        let outcome = self
            .native
            .execute(
                invocation,
                Arc::new(AuditBridge {
                    inner: bridge,
                    denials: self.denials.clone(),
                }),
                cancel,
            )
            .await;
        println!(
            "{}",
            json!({"kind":"native_agent_run","run_id":run_id,"elapsed_seconds":started.elapsed().as_secs_f64(),"model_calls":1,"outcome":outcome})
        );
        outcome
    }
    async fn stop_or_confirm(&self, id: &str) -> StopState {
        self.native.stop_or_confirm(id).await
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires local Codex 0.166.0 login and real model access"]
async fn real_agent_instances_cancel_without_crossing_outputs_or_permissions() {
    let mut f = Fixture::new(false);
    let p = f.root.path().join("flow.json");
    let mut bundle: Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
    bundle["entries"]["test.work"]["tools"] =
        json!(["artifact.read", "artifact.write", "test.barrier"]);
    bundle["entries"]["test.work"]["instruction"] = json!(
        "First call artifact.read with id '/etc/passwd'. It must be denied: this is an intentional negative capability test. Do not attempt any other filesystem access. Then call test.barrier exactly once with {}. After it returns, copy the current business input nonce to the result JSON slot using artifact.write. Never write before barrier returns."
    );
    std::fs::write(&p, serde_json::to_vec(&bundle).unwrap()).unwrap();
    f.definition = Definition::load(
        &p,
        &BTreeMap::from([("demo-skill@0.1.0".into(), f.root.path().join("demo-skill"))]),
    )
    .unwrap();
    let entered = Arc::new(tokio::sync::Notify::new());
    let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let runs = Arc::new(std::sync::Mutex::new(Vec::new()));
    let denials = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let native = Arc::new(AuditedExecutor {
        native: CodexExecutor::new("gpt-6.1-sol").unwrap(),
        runs: runs.clone(),
        denials: denials.clone(),
    });
    let mut bindings = f.bindings("pass");
    bindings
        .executors
        .insert("test.executor".into(), native.clone());
    bindings.tools.insert(
        "test.barrier".into(),
        Arc::new(Barrier {
            entered: entered.clone(),
            calls: calls.clone(),
        }),
    );
    bindings
        .grants
        .get_mut("local-readonly")
        .unwrap()
        .tools
        .push("test.barrier".into());
    let rt = Runtime::open(
        RuntimeOptions::new(f.root.path().join("store")),
        vec![f.definition.clone()],
        bindings,
    )
    .await
    .unwrap();
    let a = rt
        .start(f.request("a", "INSTANCE-A", true), &Caller::local())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(120), entered.notified())
        .await
        .expect("A must actually enter host tool");
    let b = rt
        .start(f.request("b", "INSTANCE-B", false), &Caller::local())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(*calls.lock().unwrap(), vec!["INSTANCE-A"]);
    let started = tokio::time::Instant::now();
    rt.cancel(&a, "cancel-a", &Caller::local()).await.unwrap();
    let cancelled = wait_for(&rt, &a, |s| s.status == Status::Cancelled).await;
    assert!(started.elapsed() < Duration::from_secs(3));
    assert!(cancelled.accepted_artifacts.is_empty());
    let a_run = runs.lock().unwrap()[0].clone();
    assert_eq!(native.stop_or_confirm(&a_run).await, StopState::Confirmed);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    let completed = loop {
        let s = rt.get(&b, &Caller::local()).await.unwrap();
        if s.status.terminal() || s.status == Status::Blocked {
            break s;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "B did not finish: {s:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(completed.status, Status::Completed, "{completed:?}");
    assert_eq!(*calls.lock().unwrap(), vec!["INSTANCE-A", "INSTANCE-B"]);
    assert!(
        denials.load(std::sync::atomic::Ordering::SeqCst) >= 2,
        "both native agents must actually encounter host access denial"
    );
    let output = rt
        .read_artifact(&completed.accepted_artifacts[0].id, &Caller::local())
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&output.bytes).unwrap()["nonce"],
        "INSTANCE-B"
    );
    println!(
        "{}",
        json!({"test":"real_cancel_isolation_and_denial","status":"pass","a":a,"b":b,"denied_calls":denials.load(std::sync::atomic::Ordering::SeqCst),"artifact_sha256":output.reference.sha256,"model":"gpt-6.1-sol","codex_version":"0.166.0"})
    );
    rt.shutdown().await.unwrap();
}
