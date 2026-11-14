mod common;
use common::*;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use taskloop::*;
use tokio_util::sync::CancellationToken;

fn adapter(f: &Fixture, mode: &str) -> Arc<MilkieExecutor> {
    adapter_search(f, mode, MilkieWebSearchMode::Disabled)
}
fn adapter_search(f: &Fixture, mode: &str, search: MilkieWebSearchMode) -> Arc<MilkieExecutor> {
    let root = f.root.path();
    for dir in ["config", "sessions", "milkie"] {
        std::fs::create_dir_all(root.join(dir)).unwrap();
    }
    std::fs::write(root.join("connection.json"),b"{\"contractVersion\":1,\"fields\":{\"transport\":\"agent-cli\",\"runtime\":\"codex\",\"model\":\"fixture\"}}").unwrap();
    let config = MilkieConfig::new(
        "/usr/bin/env",
        root.join("connection.json"),
        root.join("milkie"),
        MilkieStorage {
            config_dir: root.join("config"),
            session_dir: root.join("sessions"),
        },
    )
    .unwrap()
    .args([
        "python3".into(),
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/milkie_sidecar.py")
            .into_os_string(),
    ])
    .env("MILKIE_FIXTURE_MODE", mode)
    .env(
        "MILKIE_FIXTURE_STARTED_FILE",
        root.join("started.log").into_os_string(),
    );
    let config = config.env(
        "MILKIE_FIXTURE_HANDSHAKE_FILE",
        root.join("handshake.log").into_os_string(),
    );
    let config = config.env(
        "MILKIE_FIXTURE_OBSERVATION_FILE",
        root.join("observation-reading.marker").into_os_string(),
    );
    Arc::new(MilkieExecutor::new(config.web_search(search)).unwrap())
}
async fn runtime(f: &Fixture, mode: &str, gate: &str) -> Runtime {
    let mut bindings = f.bindings(gate);
    bindings
        .executors
        .insert("test.executor".into(), adapter(f, mode));
    Runtime::open(
        RuntimeOptions::new(f.root.path().join("store")),
        vec![f.definition.clone()],
        bindings,
    )
    .await
    .unwrap()
}
#[tokio::test]
async fn runtime_acceptance_and_gate_rejection_stay_in_taskloop() {
    for gate in ["pass", "fail"] {
        let f = Fixture::new(false);
        let rt = runtime(&f, "normal", gate).await;
        let id = rt
            .start(f.request("milkie", "CURRENT", false), &Caller::local())
            .await
            .unwrap();
        let s = wait_for(&rt, &id, |s| s.status.terminal()).await;
        assert_eq!(
            s.status,
            if gate == "pass" {
                Status::Completed
            } else {
                Status::Failed
            }
        );
        if gate == "pass" {
            let a = rt
                .read_artifact(&s.accepted_artifacts[0].id, &Caller::local())
                .await
                .unwrap();
            assert_eq!(
                serde_json::from_slice::<Value>(&a.bytes).unwrap()["nonce"],
                "CURRENT"
            );
        } else {
            assert!(s.accepted_artifacts.is_empty());
        }
        rt.shutdown().await.unwrap();
    }
}
#[tokio::test]
async fn empty_chat_wrong_context_and_bad_handshake_cannot_submit() {
    for mode in ["empty", "wrong_context", "bad_version", "harness_exit"] {
        let f = Fixture::new(false);
        let rt = runtime(&f, mode, "pass").await;
        let id = rt
            .start(f.request(mode, "A", false), &Caller::local())
            .await
            .unwrap();
        let s = wait_for(&rt, &id, |s| s.status.terminal()).await;
        assert_eq!(s.status, Status::Failed, "{mode}: {s:?}");
        assert!(s.accepted_artifacts.is_empty());
        rt.shutdown().await.unwrap();
    }
}
#[tokio::test]
async fn two_tasks_cancel_and_late_callback_do_not_mix() {
    let f = Fixture::new(false);
    let rt = runtime(&f, "late", "pass").await;
    let a = rt
        .start(f.request("held", "A", true), &Caller::local())
        .await
        .unwrap();
    let b = rt
        .start(f.request("completed", "B", false), &Caller::local())
        .await
        .unwrap();
    for _ in 0..100 {
        if f.root.path().join("started.log").exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    rt.cancel(&a, "cancel-held", &Caller::local())
        .await
        .unwrap();
    let ready = wait_for(&rt, &b, |s| s.status.terminal()).await;
    assert_eq!(ready.status, Status::Completed);
    let stopped = wait_for(&rt, &a, |s| s.status.terminal()).await;
    assert_eq!(stopped.status, Status::Cancelled);
    assert!(stopped.accepted_artifacts.is_empty());
    let bytes = rt
        .read_artifact(&ready.accepted_artifacts[0].id, &Caller::local())
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&bytes.bytes).unwrap()["nonce"],
        "B"
    );
    rt.shutdown().await.unwrap();
}
#[derive(Default)]
struct Spy(AtomicUsize);
#[async_trait::async_trait]
impl ToolBridge for Spy {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            id: "artifact.write".into(),
            description: "Write candidate".into(),
            input_schema: json!({"type":"object","properties":{"slot":{"type":"string"},"value":{}},"required":["slot","value"],"additionalProperties":false}),
        }]
    }
    async fn call(&self, _: &str, _: Value) -> Result<Value> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(json!({"written":true}))
    }
}
fn invocation(f: &Fixture, hold: bool, timeout_ms: u64) -> Invocation {
    let work = f.root.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    Invocation {
        stage_input: json!({}),
        execution_id: "test-execution".into(),
        run_id: uuid::Uuid::new_v4().to_string(),
        state_id: "work".into(),
        visit: 1,
        entry: f.definition.bundle.entries["test.work"].clone(),
        skills: f.definition.skills.clone(),
        output_schemas: BTreeMap::from([(
            "result".into(),
            f.definition.schemas["result.schema.json"].clone(),
        )]),
        inputs: json!({"nonce":"DIRECT","hold":hold}),
        artifacts: vec![],
        signal_facts: vec![],
        work_dir: work,
        timeout_ms,
        budget: None,
    }
}
#[tokio::test]
async fn duplicate_calls_and_fragmented_frames_preserve_one_host_effect() {
    for mode in ["duplicate", "fragment"] {
        let f = Fixture::new(false);
        let a = adapter(&f, mode);
        let spy = Arc::new(Spy::default());
        let inv = invocation(&f, false, 10000);
        let run = inv.run_id.clone();
        assert!(matches!(
            a.execute(inv, spy.clone(), CancellationToken::new()).await,
            ExecutionOutcome::Completed { .. }
        ));
        assert_eq!(spy.0.load(Ordering::SeqCst), 1);
        assert_eq!(a.stop_or_confirm(&run).await, StopState::Confirmed);
    }
}
#[tokio::test]
async fn dropped_execute_future_keeps_a_stop_handle() {
    let f = Fixture::new(false);
    let a = adapter(&f, "normal");
    let inv = invocation(&f, true, 10000);
    let run = inv.run_id.clone();
    let a2 = a.clone();
    let task = tokio::spawn(async move {
        a2.execute(inv, Arc::new(Spy::default()), CancellationToken::new())
            .await
    });
    for _ in 0..100 {
        if f.root.path().join("started.log").exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(f.root.path().join("started.log").exists());
    task.abort();
    let _ = task.await;
    assert_eq!(a.stop_or_confirm(&run).await, StopState::Confirmed);
}
#[tokio::test]
async fn timeout_and_unconfirmed_native_stop_are_separate() {
    for mode in ["normal", "stop_unknown"] {
        let f = Fixture::new(false);
        let a = adapter(&f, mode);
        let inv = invocation(&f, true, 250);
        let run = inv.run_id.clone();
        let result = a
            .execute(inv, Arc::new(Spy::default()), CancellationToken::new())
            .await;
        if mode == "normal" {
            assert!(
                matches!(result, ExecutionOutcome::Failed { ref code, .. } if code == "limit_reached")
            );
            assert_eq!(a.stop_or_confirm(&run).await, StopState::Confirmed);
        } else {
            assert!(matches!(result, ExecutionOutcome::StopUnknown { .. }));
            assert_eq!(a.stop_or_confirm(&run).await, StopState::Unknown);
        }
    }
}
#[tokio::test]
async fn sidecar_exit_without_native_proof_is_unknown_and_can_be_queried_again() {
    let f = Fixture::new(false);
    let a = adapter(&f, "crash");
    let inv = invocation(&f, false, 10000);
    let run = inv.run_id.clone();
    let spy = Arc::new(Spy::default());
    assert!(matches!(
        a.execute(inv, spy.clone(), CancellationToken::new()).await,
        ExecutionOutcome::StopUnknown { .. }
    ));
    assert_eq!(spy.0.load(Ordering::SeqCst), 0);
    assert_eq!(a.stop_or_confirm(&run).await, StopState::Confirmed); // public recovery query, never a second start
}

#[tokio::test]
async fn concurrent_owned_runs_can_stop_one_without_stopping_the_other() {
    let f = Fixture::new(false);
    let executor = adapter(&f, "normal");
    let held = invocation(&f, true, 10000);
    let held_id = held.run_id.clone();
    let mut active = invocation(&f, false, 10000);
    active.inputs["nonce"] = json!("SECOND");
    let other_id = active.run_id.clone();
    let exec = executor.clone();
    let task = tokio::spawn(async move {
        exec.execute(held, Arc::new(Spy::default()), CancellationToken::new())
            .await
    });
    for _ in 0..100 {
        if f.root.path().join("started.log").exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let other = executor
        .execute(active, Arc::new(Spy::default()), CancellationToken::new())
        .await;
    assert!(matches!(other, ExecutionOutcome::Completed { .. }));
    assert_eq!(
        executor.stop_or_confirm(&held_id).await,
        StopState::Confirmed
    );
    assert!(matches!(task.await.unwrap(), ExecutionOutcome::Cancelled));
    assert_eq!(
        executor.stop_or_confirm(&other_id).await,
        StopState::Confirmed
    );
}

struct BlockedObserver;
#[async_trait::async_trait]
impl MilkieObserver for BlockedObserver {
    fn options(&self, _: &Invocation) -> MilkieObservationOptions {
        MilkieObservationOptions {
            traceparent: "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01".into(),
            max_events: 100,
            max_bytes: 65536,
        }
    }
    async fn event(&self, _: &Invocation, _: Value) -> Result<()> {
        std::future::pending().await
    }
}
#[tokio::test]
async fn blocked_observation_delivery_cannot_block_tools_or_native_stop() {
    let f = Fixture::new(false);
    let base = adapter(&f, "normal");
    // Construct a separate registered adapter from the same immutable configuration.
    drop(base);
    let root = f.root.path();
    let config = MilkieConfig::new(
        "/usr/bin/env",
        root.join("connection.json"),
        root.join("milkie"),
        MilkieStorage {
            config_dir: root.join("config"),
            session_dir: root.join("sessions"),
        },
    )
    .unwrap()
    .args([
        "python3".into(),
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/milkie_sidecar.py")
            .into_os_string(),
    ]);
    let executor = MilkieExecutor::new(config)
        .unwrap()
        .with_observer(Arc::new(BlockedObserver));
    let inv = invocation(&f, false, 3000);
    let id = inv.run_id.clone();
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        executor.execute(inv, Arc::new(Spy::default()), CancellationToken::new()),
    )
    .await
    .expect("observer blocked execution");
    assert!(matches!(result, ExecutionOutcome::Completed { .. }));
    assert_eq!(executor.stop_or_confirm(&id).await, StopState::Confirmed);
}

#[tokio::test]
async fn native_stop_and_unconfirmed_host_effects_remain_distinct() {
    let f = Fixture::new(false);
    let executor = adapter(&f, "pending_effect");
    let inv = invocation(&f, false, 3000);
    let run = inv.run_id.clone();
    let result = executor
        .execute(inv, Arc::new(Spy::default()), CancellationToken::new())
        .await;
    assert!(
        matches!(result,ExecutionOutcome::StopUnknown { ref message } if message.contains("Native stop confirmed") && message.contains("host tool effects"))
    );
    assert_eq!(executor.stop_or_confirm(&run).await, StopState::Unknown);
}

#[tokio::test]
async fn reused_rpc_identity_cannot_cross_tool_results() {
    let f = Fixture::new(false);
    let executor = adapter(&f, "duplicate_wire");
    let inv = invocation(&f, false, 3000);
    let spy = Arc::new(Spy::default());
    assert!(matches!(
        executor
            .execute(inv, spy.clone(), CancellationToken::new())
            .await,
        ExecutionOutcome::StopUnknown { .. }
    ));
    assert!(spy.0.load(Ordering::SeqCst) <= 1);
}

#[tokio::test]
async fn old_source_without_purge_capability_is_explicitly_unsupported() {
    let f = Fixture::new(false);
    let a = adapter(&f, "normal");
    let inv = invocation(&f, false, 10000);
    let run = inv.run_id.clone();
    assert!(matches!(
        a.execute(inv, Arc::new(Spy::default()), CancellationToken::new())
            .await,
        ExecutionOutcome::Completed { .. }
    ));
    a.finish_observations().await.unwrap();
    assert_eq!(
        a.cleanup_observation(&run).await,
        ObservationCleanup::Unsupported
    );
}

#[tokio::test]
async fn cleanup_during_handshake_cannot_claim_no_source_will_start() {
    let f = Fixture::new(false);
    let a = adapter(&f, "slow_handshake");
    let inv = invocation(&f, false, 10000);
    let run = inv.run_id.clone();
    let executor = a.clone();
    let task = tokio::spawn(async move {
        executor
            .execute(inv, Arc::new(Spy::default()), CancellationToken::new())
            .await
    });
    for _ in 0..100 {
        if f.root.path().join("handshake.log").exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(f.root.path().join("handshake.log").exists());
    let cleanup = a.cleanup_observation(&run).await;
    let result = task.await.unwrap();
    assert!(matches!(result, ExecutionOutcome::Completed { .. }));
    assert_eq!(cleanup, ObservationCleanup::Deferred);
}

struct ReplayObserver(Arc<tokio::sync::Notify>);
#[async_trait::async_trait]
impl MilkieObserver for ReplayObserver {
    fn options(&self, _: &Invocation) -> MilkieObservationOptions {
        MilkieObservationOptions {
            traceparent: "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01".into(),
            max_events: 1000,
            max_bytes: 262144,
        }
    }
    async fn event(&self, _: &Invocation, _: Value) -> Result<()> {
        Ok(())
    }
    async fn status(&self, _: &Invocation, status: Value) -> Result<()> {
        if status["kind"] == "adapter.summary" {
            self.0.notify_one();
        }
        Ok(())
    }
}
#[tokio::test]
async fn observation_replay_cannot_hold_an_already_stopped_execution_result() {
    let f = Fixture::new(false);
    let executor = adapter(&f, "slow_observation");
    let inv = invocation(&f, false, 5000);
    let run = inv.run_id.clone();
    let summary = Arc::new(tokio::sync::Notify::new());
    let observer = Arc::new(ReplayObserver(summary.clone()));
    let e = executor.clone();
    let mut job = tokio::spawn(async move {
        e.execute_observed(
            inv,
            Arc::new(Spy::default()),
            CancellationToken::new(),
            Some(observer),
        )
        .await
    });
    // The peer marks this only after native completion and zero pending tools were checked.
    for _ in 0..200 {
        if f.root.path().join("observation-reading.marker").exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(f.root.path().join("observation-reading.marker").exists());
    let result = tokio::time::timeout(Duration::from_millis(300), &mut job).await;
    let delivered_before_observation = result.is_ok();
    let result = match result {
        Ok(v) => v.unwrap(),
        Err(_) => job.await.unwrap(),
    };
    tokio::time::timeout(Duration::from_secs(8), summary.notified())
        .await
        .unwrap();
    assert!(matches!(result, ExecutionOutcome::Completed { .. }));
    assert_eq!(executor.stop_or_confirm(&run).await, StopState::Confirmed);
    executor.finish_observations().await.unwrap();
    assert!(
        delivered_before_observation,
        "completed native result was held behind observation-only replay"
    );
}

#[tokio::test]
async fn observation_only_wait_cannot_turn_a_completed_runtime_phase_into_timeout() {
    let f = Fixture::new(false);
    let base = Arc::try_unwrap(adapter(&f, "slow_observation"))
        .ok()
        .unwrap();
    let observer = Arc::new(ReplayObserver(Arc::new(tokio::sync::Notify::new())));
    let mut bindings = f.bindings("pass");
    bindings.executors.insert(
        "test.executor".into(),
        Arc::new(base.with_observer(observer)),
    );
    let rt = Runtime::open(
        RuntimeOptions::new(f.root.path().join("store")),
        vec![f.definition.clone()],
        bindings,
    )
    .await
    .unwrap();
    let mut request = f.request("observer-budget", "A", false);
    request.limits.max_active_ms = 1000;
    let id = rt.start(request, &Caller::local()).await.unwrap();
    let result = wait_for(&rt, &id, |s| {
        s.status.terminal() || s.status == Status::Blocked
    })
    .await;
    assert_eq!(
        result.status,
        Status::Completed,
        "observation wait changed the business result: {result:?}"
    );
    assert_eq!(result.accepted_artifacts.len(), 1);
    rt.shutdown().await.unwrap();
}

struct FaultObserver(Arc<std::sync::atomic::AtomicBool>);
#[async_trait::async_trait]
impl MilkieObserver for FaultObserver {
    fn options(&self, inv: &Invocation) -> MilkieObservationOptions {
        ReplayObserver(Arc::new(tokio::sync::Notify::new())).options(inv)
    }
    async fn event(&self, _: &Invocation, _: Value) -> Result<()> {
        Ok(())
    }
    async fn status(&self, _: &Invocation, status: Value) -> Result<()> {
        if status["adapter_delivery_failed"] == true {
            self.0.store(true, Ordering::SeqCst);
        }
        Ok(())
    }
}
#[tokio::test]
async fn malformed_observation_tail_is_diagnostic_without_changing_business_acceptance() {
    let f = Fixture::new(false);
    let base = Arc::try_unwrap(adapter(&f, "bad_observation"))
        .ok()
        .unwrap();
    let bad = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let observer = Arc::new(FaultObserver(bad.clone()));
    let mut bindings = f.bindings("pass");
    bindings.executors.insert(
        "test.executor".into(),
        Arc::new(base.with_observer(observer)),
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
            f.request("malformed-observation", "A", false),
            &Caller::local(),
        )
        .await
        .unwrap();
    let result = wait_for(&rt, &id, |s| s.status.terminal()).await;
    assert_eq!(result.status, Status::Completed);
    assert_eq!(result.accepted_artifacts.len(), 1);
    rt.shutdown().await.unwrap();
    assert!(bad.load(Ordering::SeqCst));
}

#[tokio::test]
async fn observation_tail_never_reopens_host_tools_after_native_stop_proof() {
    let f = Fixture::new(false);
    let executor = adapter(&f, "late_after_finish");
    let inv = invocation(&f, false, 5000);
    let spy = Arc::new(Spy::default());
    let observer = Arc::new(ReplayObserver(Arc::new(tokio::sync::Notify::new())));
    let outcome = executor
        .execute_observed(inv, spy.clone(), CancellationToken::new(), Some(observer))
        .await;
    assert!(matches!(outcome, ExecutionOutcome::Completed { .. }));
    executor.finish_observations().await.unwrap();
    assert_eq!(
        spy.0.load(Ordering::SeqCst),
        1,
        "a late sidecar request acquired a second host effect after native completion"
    );
}

#[tokio::test]
async fn retention_waits_for_observation_tail_after_business_completion() {
    let f = Fixture::new(false);
    let executor = adapter(&f, "slow_observation");
    let inv = invocation(&f, false, 5000);
    let run = inv.run_id.clone();
    let observer = Arc::new(ReplayObserver(Arc::new(tokio::sync::Notify::new())));
    let outcome = executor
        .execute_observed(
            inv,
            Arc::new(Spy::default()),
            CancellationToken::new(),
            Some(observer),
        )
        .await;
    assert!(matches!(outcome, ExecutionOutcome::Completed { .. }));
    let cleanup = executor.cleanup_observation(&run).await;
    executor.finish_observations().await.unwrap();
    assert_eq!(cleanup, ObservationCleanup::Deferred);
}

#[derive(Default)]
struct TailObserver {
    sequences: std::sync::Mutex<std::collections::BTreeSet<u64>>,
    summary: std::sync::Mutex<Value>,
    fail_terminal_once: std::sync::atomic::AtomicBool,
    delay_all: bool,
    timeout_terminal: bool,
    panic_terminal: bool,
    callback_delay_ms: u64,
    fail_sequence: u64,
}
#[async_trait::async_trait]
impl MilkieObserver for TailObserver {
    fn options(&self, inv: &Invocation) -> MilkieObservationOptions {
        ReplayObserver(Arc::new(tokio::sync::Notify::new())).options(inv)
    }
    async fn event(&self, _: &Invocation, e: Value) -> Result<()> {
        if self.callback_delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(self.callback_delay_ms)).await;
        }
        if self.delay_all {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        if ((self.fail_sequence == 0 && e["kind"] == "execution.finished")
            || e["sequence"] == self.fail_sequence)
            && self.fail_terminal_once.swap(false, Ordering::SeqCst)
        {
            if self.timeout_terminal {
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            assert!(!self.panic_terminal, "controlled observer panic");
            return Err(Error::new(
                "diagnostic_callback",
                "/observer",
                "controlled callback rejection",
            ));
        }
        assert!(
            self.sequences
                .lock()
                .unwrap()
                .insert(e["sequence"].as_u64().unwrap()),
            "duplicate observer effect"
        );
        Ok(())
    }
    async fn status(&self, _: &Invocation, s: Value) -> Result<()> {
        if s["kind"] == "adapter.summary" {
            *self.summary.lock().unwrap() = s;
        }
        Ok(())
    }
}
#[tokio::test]
async fn delayed_and_failed_tail_recovers_with_contiguous_ack_and_one_tool_effect() {
    for (
        mode,
        callback_error,
        timeout_terminal,
        panic_terminal,
        callback_delay_ms,
        fail_sequence,
    ) in [
        ("tail_normal", false, false, false, 0, 0),
        ("tail_delayed", false, false, false, 0, 0),
        ("tail_retry", false, false, false, 0, 0),
        ("tail_normal", true, false, false, 0, 0),
        ("tail_normal", true, true, false, 0, 0),
        ("tail_normal", true, false, true, 0, 0),
        ("tail_normal", true, false, false, 10, 0),
        ("tail_normal", true, true, false, 10, 0),
        ("tail_normal", true, false, false, 10, 50),
    ] {
        let f = Fixture::new(false);
        let executor = adapter(&f, mode);
        let observer = Arc::new(TailObserver {
            fail_terminal_once: std::sync::atomic::AtomicBool::new(callback_error),
            timeout_terminal,
            panic_terminal,
            callback_delay_ms,
            fail_sequence,
            ..Default::default()
        });
        let spy = Arc::new(Spy::default());
        let result = executor
            .execute_observed(
                invocation(&f, false, 5000),
                spy.clone(),
                CancellationToken::new(),
                Some(observer.clone()),
            )
            .await;
        assert!(matches!(result, ExecutionOutcome::Completed { .. }));
        tokio::time::timeout(Duration::from_secs(8), executor.finish_observations())
            .await
            .unwrap()
            .unwrap();
        let summary = observer.summary.lock().unwrap().clone();
        assert_eq!(
            summary["adapter_delivery_failed"], false,
            "mode={mode}: {summary}"
        );
        assert_eq!(summary["delivered_sequence"], 126);
        assert_eq!(summary["source_last_sequence"], 126);
        assert_eq!(observer.sequences.lock().unwrap().len(), 126);
        assert_eq!(spy.0.load(Ordering::SeqCst), 1);
        if callback_error {
            assert!(
                summary["delivery_failures"]
                    .as_array()
                    .unwrap()
                    .contains(&json!(if timeout_terminal {
                        "callback_timeout"
                    } else {
                        "callback_error"
                    }))
            );
        }
        if mode == "tail_retry" {
            assert!(summary["replay_attempts"].as_u64().unwrap() > 1);
        }
    }
}
#[tokio::test]
async fn unrecoverable_tail_has_bounded_typed_diagnostics_without_changing_business() {
    for (mode, reason) in [
        ("tail_timeout", "replay_timeout"),
        ("tail_protocol", "protocol_invalid"),
        ("tail_missing", "source_incomplete"),
        ("tail_overflow", "queue_overflow"),
    ] {
        let f = Fixture::new(false);
        let executor = adapter(&f, mode);
        let observer = Arc::new(TailObserver {
            delay_all: mode == "tail_overflow",
            ..Default::default()
        });
        let spy = Arc::new(Spy::default());
        let start = std::time::Instant::now();
        let result = executor
            .execute_observed(
                invocation(&f, false, 5000),
                spy.clone(),
                CancellationToken::new(),
                Some(observer.clone()),
            )
            .await;
        assert!(matches!(result, ExecutionOutcome::Completed { .. }));
        tokio::time::timeout(Duration::from_secs(8), executor.finish_observations())
            .await
            .unwrap()
            .unwrap();
        assert!(start.elapsed() < Duration::from_secs(9));
        let s = observer.summary.lock().unwrap().clone();
        assert_eq!(s["adapter_delivery_failed"], true, "{mode}: {s}");
        assert!(
            s["delivery_failures"]
                .as_array()
                .unwrap()
                .contains(&json!(reason)),
            "{mode}: {s}"
        );
        assert!(s["replay_attempts"].as_u64().unwrap() <= 3);
        assert_eq!(spy.0.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn recovered_tail_is_complete_in_public_trace_with_export_and_closed_execution_span() {
    let f = Fixture::new(false);
    let executor = adapter(&f, "tail_delayed");
    let mut bindings = f.bindings("pass");
    bindings.executors.insert("test.executor".into(), executor);
    let rt = Runtime::open_with_trace(
        RuntimeOptions::new(f.root.path().join("store")),
        vec![f.definition.clone()],
        bindings,
        TraceOptions {
            max_events_per_run: 1000,
            max_bytes_per_run: 1048576,
            source_max_events: 1000,
            source_max_bytes: 262144,
        },
    )
    .await
    .unwrap();
    let id = rt
        .start(f.request("tail-public", "PUBLIC", false), &Caller::local())
        .await
        .unwrap();
    assert_eq!(
        wait_for(&rt, &id, |s| s.status.terminal()).await.status,
        Status::Completed
    );
    let mut page = None;
    for _ in 0..300 {
        let p = rt
            .trace(
                &id,
                TraceQuery {
                    limit: 1000,
                    ..Default::default()
                },
                &Caller::local(),
            )
            .unwrap();
        if p.complete {
            page = Some(p);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let p = page.expect("recovered tail never became complete");
    let native: Vec<_> = p
        .events
        .iter()
        .filter(|e| e.event.source_id.starts_with("milkie:"))
        .collect();
    assert_eq!(native.len(), 126);
    assert!(p.open_spans.is_empty());
    assert!(p.issues.is_empty());
    let s = p
        .events
        .iter()
        .find(|e| e.event.kind == "source.status" && e.event.data["kind"] == "adapter.summary")
        .unwrap();
    assert_eq!(s.event.data["delivered_sequence"], 126);
    assert_eq!(s.event.data["source_last_sequence"], 126);
    let exported = rt
        .export_trace(
            &id,
            TraceQuery {
                limit: 1000,
                ..Default::default()
            },
            &Caller::local(),
        )
        .unwrap();
    let exported: TracePage = serde_json::from_slice(&exported).unwrap();
    assert_eq!(
        p.events.iter().map(|e| &e.event.id).collect::<Vec<_>>(),
        exported
            .events
            .iter()
            .map(|e| &e.event.id)
            .collect::<Vec<_>>()
    );
    rt.shutdown().await.unwrap();
}

#[tokio::test]
async fn rejected_prestart_handshake_does_not_invent_a_missing_native_tail() {
    let f = Fixture::new(false);
    let executor = adapter(&f, "bad_version");
    let observer = Arc::new(TailObserver::default());
    let spy = Arc::new(Spy::default());
    let outcome = executor
        .execute_observed(
            invocation(&f, false, 5000),
            spy.clone(),
            CancellationToken::new(),
            Some(observer.clone()),
        )
        .await;
    assert!(matches!(outcome, ExecutionOutcome::Failed { .. }));
    executor.finish_observations().await.unwrap();
    let s = observer.summary.lock().unwrap();
    assert_eq!(s["adapter_delivery_failed"], false);
    assert_eq!(s["replay_attempts"], 0);
    assert!(s["source_last_sequence"].is_null());
    assert_eq!(spy.0.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn native_stop_with_unknown_host_effects_can_have_a_complete_tail_but_never_reopen_tools() {
    let f = Fixture::new(false);
    let executor = adapter(&f, "tail_pending_effect");
    let observer = Arc::new(TailObserver::default());
    let spy = Arc::new(Spy::default());
    let inv = invocation(&f, false, 5000);
    let run = inv.run_id.clone();
    let result = executor
        .execute_observed(
            inv,
            spy.clone(),
            CancellationToken::new(),
            Some(observer.clone()),
        )
        .await;
    assert!(matches!(result, ExecutionOutcome::StopUnknown { .. }));
    tokio::time::timeout(Duration::from_secs(8), executor.finish_observations())
        .await
        .unwrap()
        .unwrap();
    let summary = observer.summary.lock().unwrap().clone();
    assert_eq!(summary["adapter_delivery_failed"], false, "{summary}");
    assert_eq!(summary["delivered_sequence"], 126);
    assert_eq!(spy.0.load(Ordering::SeqCst), 1);
    assert_eq!(executor.stop_or_confirm(&run).await, StopState::Unknown);
}

#[tokio::test]
async fn native_search_mode_is_explicit_negotiated_and_part_of_binding_identity() {
    for (search, expected) in [
        (MilkieWebSearchMode::Disabled, None),
        (MilkieWebSearchMode::Cached, Some("cached")),
        (MilkieWebSearchMode::Live, Some("live")),
    ] {
        let f = Fixture::new(false);
        let a = adapter_search(&f, "normal", search);
        let mut b = f.bindings("pass");
        b.executors.insert("test.executor".into(), a.clone());
        let rt = Runtime::open(
            RuntimeOptions::new(f.root.path().join("store")),
            vec![f.definition.clone()],
            b,
        )
        .await
        .unwrap();
        let id = rt
            .start(f.request("mode", "MODE", false), &Caller::local())
            .await
            .unwrap();
        let done = wait_for(&rt, &id, |s| s.status.terminal()).await;
        assert_eq!(done.status, Status::Completed, "{done:?}");
        let config = f.root.path().join("milkie");
        let files = walk_fixture_records(&config);
        assert_eq!(files.len(), 1);
        let record: Value = serde_json::from_slice(&std::fs::read(&files[0]).unwrap()).unwrap();
        assert_eq!(record["constraints"]["webSearch"].as_str(), expected);
        assert_eq!(record["stopped"], true);
        rt.shutdown().await.unwrap();
    }
    let f = Fixture::new(false);
    assert_eq!(
        adapter(&f, "normal").identity(),
        adapter_search(&f, "normal", MilkieWebSearchMode::Disabled).identity()
    );
    assert_ne!(
        adapter(&f, "normal").identity(),
        adapter_search(&f, "normal", MilkieWebSearchMode::Live).identity()
    );
    assert_ne!(
        adapter_search(&f, "normal", MilkieWebSearchMode::Cached).identity(),
        adapter_search(&f, "normal", MilkieWebSearchMode::Live).identity()
    );
}
fn walk_fixture_records(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut result = Vec::new();
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries {
            let path = entry.unwrap().path();
            if path.is_dir() {
                result.extend(walk_fixture_records(&path));
            } else if path.file_name().is_some_and(|n| n == "fixture-record.json") {
                result.push(path);
            }
        }
    }
    result
}
#[tokio::test]
async fn native_search_missing_capability_is_rejected_before_context_creation() {
    let f = Fixture::new(false);
    let a = adapter_search(&f, "no_search_capability", MilkieWebSearchMode::Live);
    let result = a
        .execute(
            invocation(&f, false, 10000),
            Arc::new(Spy::default()),
            CancellationToken::new(),
        )
        .await;
    assert!(matches!(result,ExecutionOutcome::Failed{ref code,..}if code=="capability_denied"));
    assert!(walk_fixture_records(&f.root.path().join("milkie")).is_empty());
    assert!(!f.root.path().join("started.log").exists());
}

#[tokio::test]
async fn native_search_receipt_is_bound_to_completed_run_and_survives_adapter_reopen() {
    for mode in ["normal", "search_wrong_context"] {
        let f = Fixture::new(false);
        let a = adapter_search(&f, mode, MilkieWebSearchMode::Live);
        let inv = invocation(&f, false, 10000);
        let run = inv.run_id.clone();
        assert!(a.native_web_searches(&run).await.is_err());
        let outcome = a
            .execute(inv, Arc::new(Spy::default()), CancellationToken::new())
            .await;
        assert!(
            matches!(outcome, ExecutionOutcome::Completed { .. }),
            "{outcome:?}"
        );
        let reopened = adapter_search(&f, mode, MilkieWebSearchMode::Live);
        let receipt = reopened.native_web_searches(&run).await;
        if mode == "normal" {
            let page = receipt.unwrap();
            assert_eq!(page["complete"], true);
            assert_eq!(
                page["items"][0]["results"][0]["url"],
                "https://example.com/source"
            );
            assert!(reopened.native_web_searches("unowned-run").await.is_err());
            assert!(adapter(&f, mode).native_web_searches(&run).await.is_err());
            assert!(
                adapter_search(&f, mode, MilkieWebSearchMode::Cached)
                    .native_web_searches(&run)
                    .await
                    .is_err()
            );
        } else {
            assert_eq!(receipt.unwrap_err().code, "protocol_invalid");
        }
    }
}
