//! Real OS process crash, using deterministic execution to isolate persistence semantics.
mod common;
use async_trait::async_trait;
use common::Fixture;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use taskloop::*;
use tokio_util::sync::CancellationToken;
struct Recoverable {
    marker: Option<std::path::PathBuf>,
    confirmed: Arc<AtomicBool>,
    starts: Arc<AtomicUsize>,
}
#[async_trait]
impl Executor for Recoverable {
    fn capabilities(&self) -> ExecutorCapabilities {
        ExecutorCapabilities::MANAGED
    }
    fn identity(&self) -> Value {
        json!({"adapter":"crash-test@1"})
    }
    async fn execute(
        &self,
        inv: Invocation,
        tools: Arc<dyn ToolBridge>,
        _: CancellationToken,
    ) -> ExecutionOutcome {
        let call = self.starts.fetch_add(1, Ordering::SeqCst) + 1;
        let nonce = if self.marker.is_some() && call == 2 {
            json!("UNACCEPTED-BEFORE-CRASH")
        } else {
            inv.inputs["nonce"].clone()
        };
        tools
            .call(
                "artifact.write",
                json!({"slot":"result","value":{"nonce":nonce}}),
            )
            .await
            .unwrap();
        if let Some(marker) = &self.marker
            && call == 2
        {
            std::fs::write(
                marker,
                serde_json::to_vec(&json!({"execution":inv.execution_id,"run":inv.run_id}))
                    .unwrap(),
            )
            .unwrap();
            std::future::pending::<()>().await;
        }
        ExecutionOutcome::Completed {
            usage: json!({"kind":"deterministic-crash-test"}),
        }
    }
    async fn stop_or_confirm(&self, _: &str) -> StopState {
        if self.confirmed.load(Ordering::SeqCst) {
            StopState::Confirmed
        } else {
            StopState::Unknown
        }
    }
}
struct Gate;
#[async_trait]
impl DomainGate for Gate {
    async fn check(&self, c: GateContext) -> CheckResult {
        if c.output("result").unwrap()["nonce"] == c.inputs["nonce"] {
            CheckResult::pass("ready", vec![c.check_evidence_ref])
        } else {
            CheckResult::fail("mismatch", "result", "wrong execution")
        }
    }
}
fn bindings(definition: &Definition, executor: Arc<Recoverable>) -> HostBindings {
    HostBindings::default()
        .configuration_version("crash-test@1")
        .executor("test.executor", executor)
        .gate("test.gate", Arc::new(Gate))
        .grant(
            "local-readonly",
            Grant {
                definitions: vec![definition.digest.clone()],
                tools: vec!["artifact.read".into(), "artifact.write".into()],
                limits: ResourceLimits::default(),
            },
        )
}
#[tokio::test]
async fn crash_child() {
    let Some(root) = std::env::var_os("TASKLOOP_CRASH_CHILD_ROOT").map(std::path::PathBuf::from)
    else {
        return;
    };
    let definition = Definition::load(
        root.join("flow.json"),
        &BTreeMap::from([("demo-skill@0.1.0".into(), root.join("demo-skill"))]),
    )
    .unwrap();
    let executor = Arc::new(Recoverable {
        marker: Some(root.join("entered.json")),
        confirmed: Arc::new(AtomicBool::new(false)),
        starts: Arc::new(AtomicUsize::new(0)),
    });
    let rt = Runtime::open(
        RuntimeOptions::new(root.join("store")),
        vec![definition.clone()],
        bindings(&definition, executor),
    )
    .await
    .unwrap();
    let id = rt
        .start(
            StartRequest {
                definition_digest: definition.digest,
                inputs: json!({"nonce":"BEFORE-CRASH","hold":false}),
                grant_ref: "local-readonly".into(),
                request_key: "before-crash".into(),
                limits: ResourceLimits::default(),
            },
            &Caller::local(),
        )
        .await
        .unwrap();
    loop {
        if root.join("request-cancel").exists() {
            let snapshot = rt
                .cancel(&id, "before-crash-cancel", &Caller::local())
                .await
                .unwrap();
            assert_eq!(snapshot.status, Status::Cancelling);
            std::fs::write(root.join("cancel-receipt"), b"accepted").unwrap();
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    std::future::pending::<()>().await;
}
async fn run_crash_case(cancel_before_kill: bool) {
    let mut f = Fixture::new(false);
    let flow_path = f.root.path().join("flow.json");
    let mut bundle: Value = serde_json::from_slice(&std::fs::read(&flow_path).unwrap()).unwrap();
    bundle["flow"]["states"]["work"]["accepted"]["ready"] = json!("after_initial");
    bundle["flow"]["states"]["after_initial"] = json!({"kind":"work","entry":"test.work","accepted":{"ready":"done"},"rejected":"failed","failed":"failed"});
    std::fs::write(&flow_path, serde_json::to_vec(&bundle).unwrap()).unwrap();
    f.definition = Definition::load(
        &flow_path,
        &BTreeMap::from([("demo-skill@0.1.0".into(), f.root.path().join("demo-skill"))]),
    )
    .unwrap();
    let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "crash_child", "--nocapture"])
        .env("TASKLOOP_CRASH_CHILD_ROOT", f.root.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let marker = f.root.path().join("entered.json");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !marker.exists() {
        assert!(
            child.try_wait().unwrap().is_none(),
            "child terminated before persisted candidate"
        );
        assert!(
            tokio::time::Instant::now() < deadline,
            "child never entered"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let entered: Value = serde_json::from_slice(&std::fs::read(marker).unwrap()).unwrap();
    let a = entered["execution"].as_str().unwrap();
    if cancel_before_kill {
        std::fs::write(f.root.path().join("request-cancel"), b"cancel").unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !f.root.path().join("cancel-receipt").exists() {
            assert!(child.try_wait().unwrap().is_none());
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    child.kill().await.unwrap();
    assert!(!child.wait().await.unwrap().success());
    let blobs: Vec<_> = std::fs::read_dir(f.root.path().join("store/blobs"))
        .unwrap()
        .map(|e| std::fs::read(e.unwrap().path()).unwrap())
        .collect();
    assert!(blobs.iter().any(|b| {
        serde_json::from_slice::<Value>(b).is_ok_and(|v| v["nonce"] == "UNACCEPTED-BEFORE-CRASH")
    }));
    let confirmed = Arc::new(AtomicBool::new(false));
    let starts = Arc::new(AtomicUsize::new(0));
    let executor = Arc::new(Recoverable {
        marker: None,
        confirmed: confirmed.clone(),
        starts: starts.clone(),
    });
    let rt = Runtime::open(
        RuntimeOptions::new(f.root.path().join("store")),
        vec![f.definition.clone()],
        bindings(&f.definition, executor),
    )
    .await
    .unwrap();
    let recovered = rt.get(a, &Caller::local()).await.unwrap();
    assert_eq!(
        recovered.status,
        if cancel_before_kill {
            Status::Cancelling
        } else {
            Status::Blocked
        }
    );
    assert_eq!(recovered.error.unwrap().code, "interrupted");
    assert_eq!(recovered.accepted_artifacts.len(), 1);
    let saved = rt
        .read_artifact(&recovered.accepted_artifacts[0].id, &Caller::local())
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&saved.bytes).unwrap()["nonce"],
        "BEFORE-CRASH"
    );
    let b = rt
        .start(
            f.request("after-crash", "AFTER-CRASH", false),
            &Caller::local(),
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(
        starts.load(Ordering::SeqCst),
        0,
        "unknown execution must hold worker capacity"
    );
    confirmed.store(true, Ordering::SeqCst);
    if !cancel_before_kill {
        rt.cancel(a, "confirmed-stop", &Caller::local())
            .await
            .unwrap();
    }
    let final_b = common::wait_for(&rt, &b, |s| s.status.terminal()).await;
    assert_eq!(final_b.status, Status::Completed);
    assert_eq!(
        starts.load(Ordering::SeqCst),
        2,
        "only B's two phases may run, A must never replay"
    );
    assert_eq!(
        rt.get(a, &Caller::local()).await.unwrap().status,
        Status::Cancelled
    );
    rt.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn killed_host_never_replays_unknown_run_and_retains_candidate_bytes() {
    run_crash_case(false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn accepted_cancel_survives_host_crash_and_finishes_without_new_cancel() {
    run_crash_case(true).await;
}
