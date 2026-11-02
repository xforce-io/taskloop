mod common;
use async_trait::async_trait;
use common::*;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use taskloop::*;
use tokio_util::sync::CancellationToken;
struct FailureExecutor {
    kind: &'static str,
    entered: Arc<tokio::sync::Notify>,
    confirm: Arc<AtomicBool>,
}
#[async_trait]
impl Executor for FailureExecutor {
    fn capabilities(&self) -> ExecutorCapabilities {
        ExecutorCapabilities::MANAGED
    }
    async fn execute(
        &self,
        _: Invocation,
        _: Arc<dyn ToolBridge>,
        cancel: CancellationToken,
    ) -> ExecutionOutcome {
        self.entered.notify_one();
        match self.kind {
            "timeout" => std::future::pending().await,
            "panic" => panic!("intentional adapter panic"),
            "cancel-unknown" => {
                cancel.cancelled().await;
                ExecutionOutcome::StopUnknown {
                    message: "stop not confirmed".into(),
                }
            }
            _ => ExecutionOutcome::StopUnknown {
                message: "stop not confirmed".into(),
            },
        }
    }
    async fn stop_or_confirm(&self, _: &str) -> StopState {
        if self.confirm.load(Ordering::SeqCst) {
            StopState::Confirmed
        } else {
            StopState::Unknown
        }
    }
}
async fn open(
    f: &Fixture,
    kind: &'static str,
    confirm: Arc<AtomicBool>,
    entered: Arc<tokio::sync::Notify>,
) -> Runtime {
    let mut b = f.bindings("pass");
    b.executors.insert(
        "test.executor".into(),
        Arc::new(FailureExecutor {
            kind,
            confirm,
            entered,
        }),
    );
    Runtime::open(
        RuntimeOptions::new(f.root.path().join("store")),
        vec![f.definition.clone()],
        b,
    )
    .await
    .unwrap()
}
#[tokio::test]
async fn internal_timeout_or_panic_must_not_become_user_cancellation() {
    for kind in ["timeout", "panic"] {
        let f = Fixture::new(false);
        let rt = open(
            &f,
            kind,
            Arc::new(AtomicBool::new(true)),
            Arc::new(tokio::sync::Notify::new()),
        )
        .await;
        let mut request = f.request(kind, "A", false);
        request.limits.max_active_ms = 20;
        let id = rt.start(request, &Caller::local()).await.unwrap();
        let s = wait_for(&rt, &id, |s| {
            s.status.terminal() || s.status == Status::Blocked
        })
        .await;
        assert_ne!(
            s.status,
            Status::Cancelled,
            "internal {kind} is not a user cancellation"
        );
        assert!(s.error.is_some());
        rt.shutdown().await.unwrap();
    }
}
#[tokio::test]
async fn shutdown_cannot_report_success_while_execution_stop_is_unknown() {
    let f = Fixture::new(false);
    let confirm = Arc::new(AtomicBool::new(false));
    let rt = open(
        &f,
        "unknown",
        confirm.clone(),
        Arc::new(tokio::sync::Notify::new()),
    )
    .await;
    let id = rt
        .start(f.request("unknown", "A", false), &Caller::local())
        .await
        .unwrap();
    wait_for(&rt, &id, |s| s.status == Status::Blocked).await;
    let (first, second) = tokio::join!(rt.shutdown(), rt.shutdown());
    assert_eq!(first.unwrap_err().code, "stop_unconfirmed");
    assert_eq!(second.unwrap_err().code, "stop_unconfirmed");
    confirm.store(true, Ordering::SeqCst);
    rt.shutdown().await.unwrap();
}
#[tokio::test]
async fn accepted_cancel_intent_survives_unknown_stop_until_confirmation() {
    let f = Fixture::new(false);
    let confirm = Arc::new(AtomicBool::new(false));
    let entered = Arc::new(tokio::sync::Notify::new());
    let rt = open(&f, "cancel-unknown", confirm.clone(), entered.clone()).await;
    let id = rt
        .start(f.request("unknown", "A", false), &Caller::local())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), entered.notified())
        .await
        .unwrap();
    rt.cancel(&id, "cancel-key", &Caller::local())
        .await
        .unwrap();
    let s = wait_for(&rt, &id, |s| {
        s.error
            .as_ref()
            .is_some_and(|e| e.code == "stop_unconfirmed")
    })
    .await;
    assert_eq!(s.status, Status::Cancelling);
    confirm.store(true, Ordering::SeqCst);
    assert_eq!(
        wait_for(&rt, &id, |s| s.status.terminal()).await.status,
        Status::Cancelled
    );
    assert_eq!(
        rt.cancel(&id, "cancel-key", &Caller::local())
            .await
            .unwrap()
            .status,
        Status::Cancelled
    );
    rt.shutdown().await.unwrap();
}

#[tokio::test]
async fn entry_input_schema_is_checked_before_executor_runs() {
    use serde_json::{Value, json};
    let mut f = Fixture::new(false);
    let p = f.root.path().join("flow.json");
    let mut bundle: Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
    bundle["entries"]["test.work"]["input_schema"] = json!("forbidden-input.schema.json");
    std::fs::write(&p, serde_json::to_vec(&bundle).unwrap()).unwrap();
    std::fs::write(f.root.path().join("forbidden-input.schema.json"), b"false").unwrap();
    f.definition = Definition::load(
        &p,
        &std::collections::BTreeMap::from([(
            "demo-skill@0.1.0".into(),
            f.root.path().join("demo-skill"),
        )]),
    )
    .unwrap();
    let rt = f.open("pass").await;
    let id = rt
        .start(f.request("forbidden", "A", false), &Caller::local())
        .await
        .unwrap();
    let s = wait_for(&rt, &id, |s| {
        s.status.terminal() || s.status == Status::Blocked
    })
    .await;
    assert_eq!(s.status, Status::Blocked);
    assert_eq!(s.error.unwrap().code, "input_invalid");
    assert!(s.accepted_artifacts.is_empty());
    rt.shutdown().await.unwrap();
}

struct StartRaceExecutor {
    reported_cancelled: Arc<Vec<AtomicBool>>,
    violations: Arc<std::sync::atomic::AtomicUsize>,
}
#[async_trait]
impl Executor for StartRaceExecutor {
    fn capabilities(&self) -> ExecutorCapabilities {
        ExecutorCapabilities::MANAGED
    }
    async fn execute(
        &self,
        inv: Invocation,
        _: Arc<dyn ToolBridge>,
        cancel: CancellationToken,
    ) -> ExecutionOutcome {
        let index = inv.inputs["nonce"]
            .as_str()
            .unwrap()
            .parse::<usize>()
            .unwrap();
        if self.reported_cancelled[index].load(Ordering::SeqCst) {
            self.violations.fetch_add(1, Ordering::SeqCst);
        }
        cancel.cancelled().await;
        ExecutionOutcome::Cancelled
    }
    async fn stop_or_confirm(&self, _: &str) -> StopState {
        StopState::Confirmed
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn confirmed_cancellation_never_precedes_a_new_executor_start() {
    let f = Fixture::new(false);
    let reported = Arc::new((0..64).map(|_| AtomicBool::new(false)).collect::<Vec<_>>());
    let violations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut b = f.bindings("pass");
    b.executors.insert(
        "test.executor".into(),
        Arc::new(StartRaceExecutor {
            reported_cancelled: reported.clone(),
            violations: violations.clone(),
        }),
    );
    let rt = Runtime::open(
        RuntimeOptions::new(f.root.path().join("store")),
        vec![f.definition.clone()],
        b,
    )
    .await
    .unwrap();
    for index in 0..64 {
        let text = index.to_string();
        let id = rt
            .start(f.request(&text, &text, false), &Caller::local())
            .await
            .unwrap();
        tokio::task::yield_now().await;
        let s = rt.cancel(&id, "cancel", &Caller::local()).await.unwrap();
        if s.status == Status::Cancelled {
            reported[index].store(true, Ordering::SeqCst);
        }
        wait_for(&rt, &id, |s| s.status == Status::Cancelled).await;
    }
    assert_eq!(
        violations.load(Ordering::SeqCst),
        0,
        "an executor started after cancellation was confirmed"
    );
    rt.shutdown().await.unwrap();
    assert_eq!(
        rt.start(f.request("closed", "A", false), &Caller::local())
            .await
            .unwrap_err()
            .code,
        "runtime_closed"
    );
}
