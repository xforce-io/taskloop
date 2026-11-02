mod common;
use common::*;
use serde_json::{Value, json};
use taskloop::*;
#[tokio::test]
async fn completes_via_public_api_and_reads_accepted_bytes() {
    let f = Fixture::new(false);
    let rt = f.open("pass").await;
    let id = rt
        .start(f.request("one", "A", false), &Caller::local())
        .await
        .unwrap();
    let s = wait_for(&rt, &id, |s| s.status.terminal()).await;
    assert_eq!(s.status, Status::Completed);
    assert_eq!(s.inputs["nonce"], "A");
    assert_eq!(s.accepted_artifacts.len(), 1);
    let a = rt
        .read_artifact(&s.accepted_artifacts[0].id, &Caller::local())
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&a.bytes).unwrap()["nonce"],
        "A"
    );
    assert_eq!(a.reference.state_id, "work");
    rt.shutdown().await.unwrap();
}
#[tokio::test]
async fn waiting_signal_is_versioned_and_idempotent() {
    let f = Fixture::new(true);
    let rt = f.open("pass").await;
    let id = rt
        .start(f.request("one", "A", false), &Caller::local())
        .await
        .unwrap();
    let s = wait_for(&rt, &id, |s| s.status == Status::Waiting).await;
    let request = SignalRequest {
        event_name: "resume".into(),
        payload: json!({"confirm":true}),
        request_key: "continue".into(),
        expected_revision: s.revision,
    };
    let receipt = rt
        .signal(&id, request.clone(), &Caller::local())
        .await
        .unwrap();
    assert_eq!(
        rt.signal(&id, request, &Caller::local())
            .await
            .unwrap()
            .revision,
        receipt.revision
    );
    assert_eq!(
        rt.get(&id, &Caller::local()).await.unwrap().status,
        Status::Completed
    );
    rt.shutdown().await.unwrap();
}
#[tokio::test]
async fn independent_instances_and_cancel_do_not_mix() {
    let f = Fixture::new(false);
    let rt = f.open("pass").await;
    let a = rt
        .start(f.request("a", "A", true), &Caller::local())
        .await
        .unwrap();
    let b = rt
        .start(f.request("b", "B", false), &Caller::local())
        .await
        .unwrap();
    assert_ne!(a, b);
    let same = rt
        .start(f.request("a", "A", true), &Caller::local())
        .await
        .unwrap();
    assert_eq!(same, a);
    rt.cancel(&a, "stop-a", &Caller::local()).await.unwrap();
    let a = wait_for(&rt, &a, |s| s.status == Status::Cancelled).await;
    let b = wait_for(&rt, &b, |s| s.status == Status::Completed).await;
    assert!(a.accepted_artifacts.is_empty());
    let bytes = rt
        .read_artifact(&b.accepted_artifacts[0].id, &Caller::local())
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&bytes.bytes).unwrap()["nonce"],
        "B"
    );
    rt.shutdown().await.unwrap();
}
#[tokio::test]
async fn gate_error_blocks_and_false_evidence_never_passes() {
    for kind in ["error", "invented"] {
        let f = Fixture::new(false);
        let rt = f.open(kind).await;
        let id = rt
            .start(f.request("one", "A", false), &Caller::local())
            .await
            .unwrap();
        let s = wait_for(&rt, &id, |s| s.status == Status::Blocked).await;
        assert_eq!(s.current_state, "work");
        assert!(s.accepted_artifacts.is_empty());
        rt.shutdown().await.unwrap();
    }
}
#[tokio::test]
async fn gate_failure_is_distinct_from_unavailable() {
    let f = Fixture::new(false);
    let rt = f.open("fail").await;
    let id = rt
        .start(f.request("one", "A", false), &Caller::local())
        .await
        .unwrap();
    let s = wait_for(&rt, &id, |s| s.status.terminal()).await;
    assert_eq!(s.status, Status::Failed);
    assert_eq!(s.error.unwrap().code, "domain_check_failed");
    rt.shutdown().await.unwrap();
}
#[tokio::test]
async fn accepted_artifacts_and_waiting_survive_restart() {
    let f = Fixture::new(true);
    let id;
    let artifact;
    {
        let rt = f.open("pass").await;
        id = rt
            .start(f.request("one", "A", false), &Caller::local())
            .await
            .unwrap();
        let s = wait_for(&rt, &id, |s| s.status == Status::Waiting).await;
        artifact = s.accepted_artifacts[0].id.clone();
        rt.shutdown().await.unwrap();
    }
    let rt = f.open("pass").await;
    let s = rt.get(&id, &Caller::local()).await.unwrap();
    assert_eq!(s.status, Status::Waiting);
    assert!(
        !rt.read_artifact(&artifact, &Caller::local())
            .await
            .unwrap()
            .bytes
            .is_empty()
    );
    assert_eq!(
        rt.start(f.request("one", "A", false), &Caller::local())
            .await
            .unwrap(),
        id
    );
    rt.shutdown().await.unwrap();
}
#[tokio::test]
async fn owner_context_cannot_be_forged_by_request_payload() {
    let f = Fixture::new(false);
    let rt = f.open("pass").await;
    let id = rt
        .start(f.request("one", "A", true), &Caller::local())
        .await
        .unwrap();
    let other = Caller {
        principal: "other-owner".into(),
        grants: vec!["local-readonly".into()],
    };
    assert_eq!(rt.get(&id, &other).await.unwrap_err().code, "not_found");
    assert_eq!(
        rt.cancel(&id, "stop", &other).await.unwrap_err().code,
        "not_found"
    );
    rt.cancel(&id, "cleanup", &Caller::local()).await.unwrap();
    rt.shutdown().await.unwrap();
}

#[tokio::test]
async fn waiting_deadline_reaches_explicit_partial_outcome() {
    let mut f = Fixture::new(true);
    let path = f.root.path().join("flow.json");
    let mut bundle: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    bundle["flow"]["states"]["await"]["timeout_seconds"] = json!(1);
    std::fs::write(&path, serde_json::to_vec(&bundle).unwrap()).unwrap();
    f.definition = Definition::load(
        &path,
        &std::collections::BTreeMap::from([(
            "demo-skill@0.1.0".into(),
            f.root.path().join("demo-skill"),
        )]),
    )
    .unwrap();
    let rt = f.open("pass").await;
    let id = rt
        .start(f.request("expires", "A", false), &Caller::local())
        .await
        .unwrap();
    let s = wait_for(&rt, &id, |s| s.status.terminal()).await;
    assert_eq!(s.status, Status::Completed);
    assert_eq!(s.business_outcome.as_deref(), Some("insufficient"));
    assert_eq!(s.current_state, "partial");
    assert_eq!(s.accepted_artifacts.len(), 1);
    rt.shutdown().await.unwrap();
}
struct LateExecutor {
    entered: std::sync::Arc<tokio::sync::Notify>,
    denied: std::sync::Arc<std::sync::atomic::AtomicBool>,
}
#[async_trait::async_trait]
impl Executor for LateExecutor {
    fn capabilities(&self) -> ExecutorCapabilities {
        ExecutorCapabilities::MANAGED
    }
    async fn execute(
        &self,
        _: Invocation,
        tools: std::sync::Arc<dyn ToolBridge>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> ExecutionOutcome {
        self.entered.notify_one();
        cancel.cancelled().await;
        let late = tools
            .call(
                "artifact.write",
                json!({"slot":"result","value":{"nonce":"TOO-LATE"}}),
            )
            .await;
        self.denied.store(
            late.is_err_and(|e| e.code == "stale_attempt"),
            std::sync::atomic::Ordering::SeqCst,
        );
        ExecutionOutcome::Completed {
            usage: json!({"kind":"deterministic"}),
        }
    }
}
#[tokio::test]
async fn cancelled_attempt_cannot_submit_late_output_even_if_it_reports_completed() {
    let f = Fixture::new(false);
    let entered = std::sync::Arc::new(tokio::sync::Notify::new());
    let denied = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut bindings = f.bindings("pass");
    bindings.executors.insert(
        "test.executor".into(),
        std::sync::Arc::new(LateExecutor {
            entered: entered.clone(),
            denied: denied.clone(),
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
        .start(f.request("late", "A", false), &Caller::local())
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(3), entered.notified())
        .await
        .unwrap();
    rt.cancel(&id, "cancel", &Caller::local()).await.unwrap();
    let s = wait_for(&rt, &id, |s| s.status == Status::Cancelled).await;
    assert!(denied.load(std::sync::atomic::Ordering::SeqCst));
    assert!(s.accepted_artifacts.is_empty());
    assert_eq!(s.current_state, "work");
    rt.shutdown().await.unwrap();
}

#[tokio::test]
async fn competing_signals_and_changed_idempotency_payload_cannot_double_advance() {
    let f = Fixture::new(true);
    let rt = f.open("pass").await;
    let id = rt
        .start(f.request("one", "A", false), &Caller::local())
        .await
        .unwrap();
    assert_eq!(
        rt.start(f.request("one", "DIFFERENT", false), &Caller::local())
            .await
            .unwrap_err()
            .code,
        "request_conflict"
    );
    let s = wait_for(&rt, &id, |s| s.status == Status::Waiting).await;
    let first = SignalRequest {
        event_name: "resume".into(),
        payload: json!({"confirm":true}),
        request_key: "first".into(),
        expected_revision: s.revision,
    };
    let second = SignalRequest {
        request_key: "second".into(),
        ..first.clone()
    };
    let caller = Caller::local();
    let (a, b) = tokio::join!(
        rt.signal(&id, first.clone(), &caller),
        rt.signal(&id, second, &caller)
    );
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
    let accepted = a.or(b).unwrap();
    assert_eq!(
        rt.get(&id, &caller).await.unwrap().revision,
        accepted.revision
    );
    let mut changed = first.clone();
    changed.payload = json!({"confirm":false});
    assert_eq!(
        rt.signal(&id, changed, &caller).await.unwrap_err().code,
        "request_conflict"
    );
    assert_eq!(
        rt.signal(&id, first, &caller).await.unwrap().revision,
        accepted.revision
    );
    rt.shutdown().await.unwrap();
}
struct InvalidSubmissionExecutor(std::sync::Arc<std::sync::atomic::AtomicUsize>);
#[async_trait::async_trait]
impl Executor for InvalidSubmissionExecutor {
    fn capabilities(&self) -> ExecutorCapabilities {
        ExecutorCapabilities::MANAGED
    }
    async fn execute(
        &self,
        inv: Invocation,
        bridge: std::sync::Arc<dyn ToolBridge>,
        _: tokio_util::sync::CancellationToken,
    ) -> ExecutionOutcome {
        for (tool, args) in [
            (
                "artifact.write",
                json!({"slot":"undeclared","value":{"nonce":"x"}}),
            ),
            (
                "artifact.write",
                json!({"slot":"result","value":{"nonce":42}}),
            ),
            ("unregistered.tool", json!({})),
        ] {
            assert!(bridge.call(tool, args).await.is_err());
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        bridge
            .call(
                "artifact.write",
                json!({"slot":"result","value":{"nonce":inv.inputs["nonce"]}}),
            )
            .await
            .unwrap();
        ExecutionOutcome::Completed {
            usage: json!({"kind":"deterministic"}),
        }
    }
}
#[tokio::test]
async fn bridge_rejects_undeclared_slots_bad_schema_and_tools_before_acceptance() {
    let f = Fixture::new(false);
    let denials = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut bindings = f.bindings("pass");
    bindings.executors.insert(
        "test.executor".into(),
        std::sync::Arc::new(InvalidSubmissionExecutor(denials.clone())),
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
    let s = wait_for(&rt, &id, |s| s.status.terminal()).await;
    assert_eq!(s.status, Status::Completed);
    assert_eq!(denials.load(std::sync::atomic::Ordering::SeqCst), 3);
    assert_eq!(s.accepted_artifacts.len(), 1);
    let output = rt
        .read_artifact(&s.accepted_artifacts[0].id, &Caller::local())
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&output.bytes).unwrap()["nonce"],
        "A"
    );
    rt.shutdown().await.unwrap();
}
