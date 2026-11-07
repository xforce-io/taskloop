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
    Arc::new(MilkieExecutor::new(config).unwrap())
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
    assert_eq!(
        a.cleanup_observation(&run).await,
        ObservationCleanup::Unsupported
    );
}
