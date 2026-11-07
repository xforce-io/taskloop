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

struct QueueBudgetExecutor {
    entered: Arc<tokio::sync::Notify>,
    b_calls: Arc<std::sync::atomic::AtomicUsize>,
}
#[async_trait]
impl Executor for QueueBudgetExecutor {
    fn capabilities(&self) -> ExecutorCapabilities {
        ExecutorCapabilities::MANAGED
    }
    async fn execute(
        &self,
        inv: Invocation,
        _: Arc<dyn ToolBridge>,
        cancel: CancellationToken,
    ) -> ExecutionOutcome {
        if inv.inputs["nonce"] == "A" {
            self.entered.notify_one();
            cancel.cancelled().await;
            ExecutionOutcome::Cancelled
        } else {
            self.b_calls.fetch_add(1, Ordering::SeqCst);
            ExecutionOutcome::Completed {
                usage: serde_json::json!({"kind":"deterministic"}),
            }
        }
    }
    async fn stop_or_confirm(&self, _: &str) -> StopState {
        StopState::Confirmed
    }
}
#[tokio::test]
async fn queued_expired_work_never_calls_the_executor() {
    let f = Fixture::new(false);
    let entered = Arc::new(tokio::sync::Notify::new());
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut bindings = f.bindings("pass");
    bindings.executors.insert(
        "test.executor".into(),
        Arc::new(QueueBudgetExecutor {
            entered: entered.clone(),
            b_calls: calls.clone(),
        }),
    );
    let rt = Runtime::open(
        RuntimeOptions::new(f.root.path().join("store")),
        vec![f.definition.clone()],
        bindings,
    )
    .await
    .unwrap();
    let a = rt
        .start(f.request("a", "A", false), &Caller::local())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), entered.notified())
        .await
        .unwrap();
    let mut request = f.request("b", "B", false);
    request.limits.max_wall_ms = 20;
    let b = rt.start(request, &Caller::local()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    rt.cancel(&a, "cancel", &Caller::local()).await.unwrap();
    let s = wait_for(&rt, &b, |s| {
        s.status == Status::Blocked || s.status.terminal()
    })
    .await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "expired queued work caused an external execution"
    );
    assert_eq!(s.status, Status::Blocked);
    assert_eq!(s.error.unwrap().code, "limit_reached");
    rt.shutdown().await.unwrap();
}
#[tokio::test]
async fn orphaned_cancel_after_commit_conflict_is_eventually_reconciled() {
    // White-box fault injection of the persisted state left by the reviewed CAS interleaving.
    // Application consumers never access this private store.
    let f = Fixture::new(false);
    let rt = f.open("pass").await;
    let id = rt
        .start(f.request("one", "A", false), &Caller::local())
        .await
        .unwrap();
    wait_for(&rt, &id, |s| s.status == Status::Completed).await;
    let connection = rusqlite::Connection::open(f.root.path().join("store/state.sqlite")).unwrap();
    let raw: String = connection
        .query_row("SELECT body FROM executions WHERE id=?1", [&id], |r| {
            r.get(0)
        })
        .unwrap();
    let mut body: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let run = body["attempts"][0]["run_id"].clone();
    body["status"] = serde_json::json!("cancelling");
    body["current_state"] = serde_json::json!("work");
    body["business_outcome"] = serde_json::Value::Null;
    body["progress"] = serde_json::json!({"phase":"executing"});
    body["attempts"] = serde_json::json!([]);
    body["stop_unknown"] = serde_json::json!(false);
    body["history"] = serde_json::json!([]);
    body["epoch"] = serde_json::json!(body["epoch"].as_u64().unwrap() + 1);
    body["active"] = serde_json::json!({"id":run,"executor":"test.executor","epoch":body["epoch"],"started":true,"started_ms":1});
    connection
        .execute(
            "UPDATE executions SET body=?1,revision=revision+1 WHERE id=?2",
            rusqlite::params![serde_json::to_string(&body).unwrap(), id],
        )
        .unwrap();
    drop(connection);
    let s = wait_for(&rt, &id, |s| s.status == Status::Cancelled).await;
    assert!(s.accepted_artifacts.is_empty());
    rt.shutdown().await.unwrap();
}

struct BudgetConsumedExecutor;
#[async_trait]
impl Executor for BudgetConsumedExecutor {
    fn capabilities(&self) -> ExecutorCapabilities {
        ExecutorCapabilities::MANAGED
    }
    async fn execute(
        &self,
        inv: Invocation,
        bridge: Arc<dyn ToolBridge>,
        _: CancellationToken,
    ) -> ExecutionOutcome {
        let _ = bridge
            .call(
                "artifact.write",
                serde_json::json!({"slot":"result","value":{"nonce":inv.inputs["nonce"]}}),
            )
            .await;
        // A ready future may finish synchronous cleanup after its timer expired.
        std::thread::sleep(Duration::from_millis(50));
        ExecutionOutcome::Completed {
            usage: serde_json::json!({"kind":"deterministic"}),
        }
    }
    async fn stop_or_confirm(&self, _: &str) -> StopState {
        StopState::Confirmed
    }
}
struct CountingGate(Arc<std::sync::atomic::AtomicUsize>);
#[async_trait]
impl DomainGate for CountingGate {
    async fn check(&self, c: GateContext) -> CheckResult {
        self.0.fetch_add(1, Ordering::SeqCst);
        CheckResult::pass("ready", vec![c.check_evidence_ref])
    }
}
#[tokio::test]
async fn exhausted_budget_does_not_start_domain_gate() {
    let f = Fixture::new(false);
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut bindings = f.bindings("pass");
    bindings
        .executors
        .insert("test.executor".into(), Arc::new(BudgetConsumedExecutor));
    bindings
        .gates
        .insert("test.gate".into(), Arc::new(CountingGate(calls.clone())));
    let rt = Runtime::open(
        RuntimeOptions::new(f.root.path().join("store")),
        vec![f.definition.clone()],
        bindings,
    )
    .await
    .unwrap();
    let mut request = f.request("budget", "A", false);
    request.limits.max_active_ms = 20;
    let id = rt.start(request, &Caller::local()).await.unwrap();
    let s = wait_for(&rt, &id, |s| {
        s.status.terminal() || s.status == Status::Blocked
    })
    .await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "a domain gate started after execution consumed its budget"
    );
    assert_eq!(s.status, Status::Failed);
    assert_eq!(s.error.unwrap().code, "limit_reached");
    rt.shutdown().await.unwrap();
}

struct ConcurrentStopExecutor {
    entered: Arc<tokio::sync::Notify>,
    enabled: Arc<AtomicBool>,
    stop_entered: Arc<tokio::sync::Notify>,
    barrier: Arc<tokio::sync::Barrier>,
}
#[async_trait]
impl Executor for ConcurrentStopExecutor {
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
        cancel.cancelled().await;
        ExecutionOutcome::StopUnknown {
            message: "awaiting explicit stop confirmation".into(),
        }
    }
    async fn stop_or_confirm(&self, _: &str) -> StopState {
        if !self.enabled.load(Ordering::SeqCst) {
            return StopState::Unknown;
        }
        self.stop_entered.notify_one();
        self.barrier.wait().await;
        StopState::Confirmed
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_stop_confirmations_do_not_leak_internal_revision_conflicts() {
    let f = Fixture::new(false);
    let entered = Arc::new(tokio::sync::Notify::new());
    let enabled = Arc::new(AtomicBool::new(false));
    let stop_entered = Arc::new(tokio::sync::Notify::new());
    let mut bindings = f.bindings("pass");
    bindings.executors.insert(
        "test.executor".into(),
        Arc::new(ConcurrentStopExecutor {
            entered: entered.clone(),
            enabled: enabled.clone(),
            stop_entered: stop_entered.clone(),
            barrier: Arc::new(tokio::sync::Barrier::new(3)),
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
        .start(f.request("one", "A", false), &Caller::local())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), entered.notified())
        .await
        .unwrap();
    rt.cancel(&id, "initial", &Caller::local()).await.unwrap();
    wait_for(&rt, &id, |s| {
        s.error
            .as_ref()
            .is_some_and(|e| e.code == "stop_unconfirmed")
    })
    .await;
    enabled.store(true, Ordering::SeqCst);
    tokio::time::timeout(Duration::from_secs(3), stop_entered.notified())
        .await
        .unwrap();
    let caller = Caller::local();
    let (a, b) = tokio::join!(
        rt.cancel(&id, "confirm-a", &caller),
        rt.cancel(&id, "confirm-b", &caller)
    );
    assert!(
        a.is_ok(),
        "accepted cancel must not fail after another stop commit: {a:?}"
    );
    assert!(
        b.is_ok(),
        "accepted cancel must not fail after another stop commit: {b:?}"
    );
    assert_eq!(
        wait_for(&rt, &id, |s| s.status == Status::Cancelled)
            .await
            .status,
        Status::Cancelled
    );
    rt.shutdown().await.unwrap();
}

#[tokio::test]
async fn unsupported_candidate_limit_is_rejected_before_store_or_executor() {
    for limit in [128 * 1024 * 1024, 64 * 1024 * 1024 + 1, 0] {
        let f = Fixture::new(false);
        let store = f.root.path().join("unsupported-store");
        let mut options = RuntimeOptions::new(&store);
        options.max_candidate_bytes = limit;
        match Runtime::open(options, vec![f.definition.clone()], f.bindings("pass")).await {
            Ok(rt) => {
                rt.shutdown().await.unwrap();
                panic!("unsupported candidate limit {limit} was accepted");
            }
            Err(e) => {
                assert_eq!(e.code, "input_invalid");
                assert_eq!(e.path, "/max_candidate_bytes");
                assert!(
                    !store.exists(),
                    "invalid configuration must not open a store"
                );
            }
        }
    }
}

#[tokio::test]
async fn supported_candidate_limit_endpoints_open_successfully() {
    for limit in [1, 64 * 1024 * 1024] {
        let f = Fixture::new(false);
        let mut options = RuntimeOptions::new(f.root.path().join("store"));
        options.max_candidate_bytes = limit;
        let rt = Runtime::open(options, vec![f.definition.clone()], f.bindings("pass"))
            .await
            .unwrap();
        rt.shutdown().await.unwrap();
    }
}

struct BrokenBlobExecutor {
    blobs: std::path::PathBuf,
    ignore_error: bool,
    retry: bool,
    domain: bool,
}
#[async_trait]
impl Executor for BrokenBlobExecutor {
    fn capabilities(&self) -> ExecutorCapabilities {
        ExecutorCapabilities::MANAGED
    }
    async fn execute(
        &self,
        inv: Invocation,
        bridge: Arc<dyn ToolBridge>,
        _: CancellationToken,
    ) -> ExecutionOutcome {
        // Only the fresh fixture blob directory is damaged; SQLite stays writable.
        std::fs::remove_dir(&self.blobs).unwrap();
        std::fs::write(&self.blobs, b"not a directory").unwrap();
        let error = bridge
            .call(
                if self.domain {
                    "test.persist"
                } else {
                    "artifact.write"
                },
                if self.domain {
                    serde_json::json!({})
                } else {
                    serde_json::json!({"slot":"result","value":{"nonce":inv.inputs["nonce"]}})
                },
            )
            .await
            .unwrap_err();
        assert_eq!(error.code, "storage_error");
        if self.retry {
            std::fs::remove_file(&self.blobs).unwrap();
            std::fs::create_dir(&self.blobs).unwrap();
            bridge
                .call(
                    "artifact.write",
                    serde_json::json!({"slot":"result","value":{"nonce":inv.inputs["nonce"]}}),
                )
                .await
                .unwrap();
        }
        if self.ignore_error {
            ExecutionOutcome::Completed {
                usage: serde_json::json!({}),
            }
        } else {
            ExecutionOutcome::Failed {
                code: "adapter_failed".into(),
                message: error.to_string(),
            }
        }
    }
}
async fn blob_failure_case(ignore_error: bool, retry: bool, domain: bool) {
    let f = if domain {
        persistence_tool_fixture()
    } else {
        Fixture::new(false)
    };
    let mut b = f.bindings("pass");
    b.executors.insert(
        "test.executor".into(),
        Arc::new(BrokenBlobExecutor {
            blobs: f.root.path().join("store/blobs"),
            ignore_error,
            retry,
            domain,
        }),
    );
    if domain {
        register_persistence_tool(&mut b);
    }
    let rt = Runtime::open(
        RuntimeOptions::new(f.root.path().join("store")),
        vec![f.definition.clone()],
        b,
    )
    .await
    .unwrap();
    let id = rt
        .start(f.request("blob-io", "A", false), &Caller::local())
        .await
        .unwrap();
    let result = wait_for(&rt, &id, |s| {
        s.status.terminal() || s.status == Status::Blocked
    })
    .await;
    assert_eq!(result.status, Status::Blocked);
    assert_eq!(result.current_state, "work");
    assert_eq!(result.error.unwrap().code, "storage_error");
    assert!(result.accepted_artifacts.is_empty());
    rt.shutdown().await.unwrap();
}
#[tokio::test]
async fn bridge_storage_failure_cannot_be_reclassified_as_business_failure() {
    blob_failure_case(false, false, false).await;
}
#[tokio::test]
async fn bridge_storage_failure_cannot_be_ignored_as_completed() {
    blob_failure_case(true, false, false).await;
}

#[tokio::test]
async fn successful_retry_cannot_erase_an_earlier_storage_failure() {
    blob_failure_case(true, true, false).await;
}
#[tokio::test]
async fn domain_output_persistence_failure_stays_blocked() {
    blob_failure_case(true, false, true).await;
}
struct PersistenceTool;
#[async_trait]
impl DomainTool for PersistenceTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            id: "test.persist".into(),
            description: "Test trusted output persistence.".into(),
            input_schema: serde_json::json!({"type":"object","additionalProperties":false}),
        }
    }
    async fn call(&self, c: ToolContext, _: serde_json::Value) -> Result<ToolOutput> {
        Ok(ToolOutput {
            payload: serde_json::json!({"stored":true}),
            files: vec![Candidate::json(
                "result",
                &serde_json::json!({"nonce":c.inputs["nonce"]}),
            )?],
        })
    }
}
fn persistence_tool_fixture() -> Fixture {
    let mut f = Fixture::new(false);
    let path = f.root.path().join("flow.json");
    let mut bundle: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    bundle["entries"]["test.work"]["tools"] =
        serde_json::json!(["artifact.read", "artifact.write", "test.persist"]);
    std::fs::write(&path, serde_json::to_vec(&bundle).unwrap()).unwrap();
    f.definition = Definition::load(
        &path,
        &std::collections::BTreeMap::from([(
            "demo-skill@0.1.0".into(),
            f.root.path().join("demo-skill"),
        )]),
    )
    .unwrap();
    f
}
fn register_persistence_tool(b: &mut HostBindings) {
    b.tools
        .insert("test.persist".into(), Arc::new(PersistenceTool));
    b.grants
        .get_mut("local-readonly")
        .unwrap()
        .tools
        .push("test.persist".into());
}
struct ReceiptFailureExecutor(std::path::PathBuf);
#[async_trait]
impl Executor for ReceiptFailureExecutor {
    fn capabilities(&self) -> ExecutorCapabilities {
        ExecutorCapabilities::MANAGED
    }
    async fn execute(
        &self,
        _: Invocation,
        bridge: Arc<dyn ToolBridge>,
        _: CancellationToken,
    ) -> ExecutionOutcome {
        // Controlled white-box fault: only receipt-bearing updates fail, so the
        // later Runtime blocked record can still be committed.
        let connection = rusqlite::Connection::open(&self.0).unwrap();
        connection.execute_batch("CREATE TRIGGER reject_receipts BEFORE UPDATE ON executions WHEN json_array_length(NEW.body, '$.receipts') > 0 BEGIN SELECT RAISE(ABORT, 'injected receipt storage failure'); END;").unwrap();
        let error = bridge
            .call("test.persist", serde_json::json!({}))
            .await
            .unwrap_err();
        assert_eq!(error.code, "storage_error");
        ExecutionOutcome::Completed {
            usage: serde_json::json!({}),
        }
    }
}
#[tokio::test]
async fn receipt_storage_failure_cannot_accept_already_written_candidates() {
    let f = persistence_tool_fixture();
    let mut b = f.bindings("pass");
    register_persistence_tool(&mut b);
    b.executors.insert(
        "test.executor".into(),
        Arc::new(ReceiptFailureExecutor(
            f.root.path().join("store/state.sqlite"),
        )),
    );
    let rt = Runtime::open(
        RuntimeOptions::new(f.root.path().join("store")),
        vec![f.definition.clone()],
        b,
    )
    .await
    .unwrap();
    let id = rt
        .start(f.request("receipt-io", "A", false), &Caller::local())
        .await
        .unwrap();
    let result = wait_for(&rt, &id, |s| {
        s.status.terminal() || s.status == Status::Blocked
    })
    .await;
    assert_eq!(result.status, Status::Blocked);
    assert_eq!(result.current_state, "work");
    assert_eq!(result.error.unwrap().code, "storage_error");
    assert!(result.accepted_artifacts.is_empty());
    rt.shutdown().await.unwrap();
}

struct StopCommitFailureExecutor {
    database: std::path::PathBuf,
    returned: Arc<tokio::sync::Notify>,
    confirm: Arc<AtomicBool>,
    b_starts: Arc<std::sync::atomic::AtomicUsize>,
}
#[async_trait]
impl Executor for StopCommitFailureExecutor {
    fn capabilities(&self) -> ExecutorCapabilities {
        ExecutorCapabilities::MANAGED
    }
    async fn execute(
        &self,
        inv: Invocation,
        bridge: Arc<dyn ToolBridge>,
        _: CancellationToken,
    ) -> ExecutionOutcome {
        if inv.inputs["nonce"] == "A" {
            // Row-specific SQLite fault after started=true was committed. Both
            // the StopUnknown save and its fallback fail while B remains writable.
            let connection = rusqlite::Connection::open(&self.database).unwrap();
            connection
                .execute_batch("CREATE TABLE stop_fault_target(id TEXT PRIMARY KEY);")
                .unwrap();
            connection
                .execute(
                    "INSERT INTO stop_fault_target(id) VALUES(?1)",
                    [&inv.execution_id],
                )
                .unwrap();
            connection.execute_batch("CREATE TRIGGER reject_stop_commit BEFORE UPDATE ON executions WHEN NEW.id IN (SELECT id FROM stop_fault_target) BEGIN SELECT RAISE(ABORT, 'injected stop commit failure'); END;").unwrap();
            self.returned.notify_one();
            ExecutionOutcome::StopUnknown {
                message: "external execution stop remains unknown".into(),
            }
        } else {
            self.b_starts.fetch_add(1, Ordering::SeqCst);
            bridge
                .call(
                    "artifact.write",
                    serde_json::json!({"slot":"result","value":{"nonce":inv.inputs["nonce"]}}),
                )
                .await
                .unwrap();
            ExecutionOutcome::Completed {
                usage: serde_json::json!({}),
            }
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
async fn stop_commit_failure_case(confirm_before_recovery: bool) {
    let f = Fixture::new(false);
    let returned = Arc::new(tokio::sync::Notify::new());
    let confirm = Arc::new(AtomicBool::new(false));
    let b_starts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let database = f.root.path().join("store/state.sqlite");
    let mut b = f.bindings("pass");
    b.executors.insert(
        "test.executor".into(),
        Arc::new(StopCommitFailureExecutor {
            database: database.clone(),
            returned: returned.clone(),
            confirm: confirm.clone(),
            b_starts: b_starts.clone(),
        }),
    );
    let rt = Runtime::open(
        RuntimeOptions::new(f.root.path().join("store")),
        vec![f.definition.clone()],
        b,
    )
    .await
    .unwrap();
    let a = rt
        .start(f.request("stop-fault-A", "A", false), &Caller::local())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), returned.notified())
        .await
        .unwrap();
    let b = rt
        .start(f.request("stop-fault-B", "B", false), &Caller::local())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(600)).await;
    let connection = rusqlite::Connection::open(&database).unwrap();
    let raw: String = connection
        .query_row("SELECT body FROM executions WHERE id=?1", [&a], |r| {
            r.get(0)
        })
        .unwrap();
    let record: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(record["status"], "running");
    assert_eq!(record["active"]["started"], true);
    assert_eq!(record["stop_unknown"], false);
    assert_eq!(
        b_starts.load(Ordering::SeqCst),
        0,
        "failed stop writes must retain capacity"
    );
    if confirm_before_recovery {
        confirm.store(true, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(
            b_starts.load(Ordering::SeqCst),
            0,
            "stop confirmation must also be committed before releasing capacity"
        );
    }
    connection
        .execute_batch("DROP TRIGGER reject_stop_commit;")
        .unwrap();
    if !confirm_before_recovery {
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(
            b_starts.load(Ordering::SeqCst),
            0,
            "storage recovery alone cannot confirm the old execution stopped"
        );
        let blocked = wait_for(&rt, &a, |s| s.status == Status::Blocked).await;
        assert_eq!(blocked.current_state, "work");
        assert_eq!(blocked.error.unwrap().code, "interrupted");
        assert!(blocked.accepted_artifacts.is_empty());
        confirm.store(true, Ordering::SeqCst);
    }
    assert_eq!(
        wait_for(&rt, &b, |s| s.status.terminal()).await.status,
        Status::Completed
    );
    assert_eq!(b_starts.load(Ordering::SeqCst), 1);
    assert_eq!(
        rt.get(&a, &Caller::local()).await.unwrap().status,
        Status::Blocked
    );
    rt.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_unknown_stop_commit_must_not_release_worker_capacity() {
    stop_commit_failure_case(false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn confirmed_stop_must_commit_before_worker_capacity_is_released() {
    stop_commit_failure_case(true).await;
}
