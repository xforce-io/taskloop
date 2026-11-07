mod common;
use async_trait::async_trait;
use common::*;
use serde_json::json;
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use taskloop::*;
use tokio_util::sync::CancellationToken;

fn policy() -> RunBudgetPolicy {
    RunBudgetPolicy {
        policy_id: "test-v1".into(),
        max_run_ms: None,
        max_check_ms: None,
        max_tool_calls: 5,
        submission_reserve: 2,
        finalize_before_ms: 0,
        max_model_iterations: None,
        max_candidate_bytes: 4096,
    }
}
fn options(f: &Fixture, p: RunBudgetPolicy) -> RuntimeOptions {
    let mut o = RuntimeOptions::new(f.root.path().join("store"));
    o.entry_budgets.insert("test.work".into(), p);
    o
}
struct ReserveExecutor;
#[async_trait]
impl Executor for ReserveExecutor {
    fn capabilities(&self) -> ExecutorCapabilities {
        ExecutorCapabilities::MANAGED
    }
    async fn execute(
        &self,
        inv: Invocation,
        b: Arc<dyn ToolBridge>,
        _: CancellationToken,
    ) -> ExecutionOutcome {
        // Admitted schema and lookup failures count; rejected reads do not consume reserve.
        assert_eq!(
            b.call("artifact.read", json!({"wrong":true}))
                .await
                .unwrap_err()
                .code,
            "input_invalid"
        );
        let mut joins = vec![];
        for _ in 0..8 {
            let b = b.clone();
            joins.push(tokio::spawn(async move {
                b.call("artifact.read", json!({"id":"not-authorized"}))
                    .await
            }));
        }
        let mut admitted_failures = 0;
        for j in joins {
            let e = j.await.unwrap().unwrap_err();
            if e.code == "capability_denied" {
                admitted_failures += 1;
            } else {
                assert_eq!(e.code, "limit_reached");
            }
        }
        assert_eq!(admitted_failures, 2);
        let h = b.budget_hint().unwrap();
        assert_eq!((h.used, h.remaining, h.read_remaining), (3, 2, 0));
        assert_eq!(h.mode, "finalize");
        // Failed submission consumes one reserved call and still enforces output schema.
        assert!(
            b.call(
                "artifact.write",
                json!({"slot":"result","value":{"nonce":7}})
            )
            .await
            .is_err()
        );
        let v = b
            .call(
                "artifact.write",
                json!({"slot":"result","value":{"nonce":inv.inputs["nonce"]}}),
            )
            .await
            .unwrap();
        assert_eq!(v["_taskloop_budget"]["used"], 5);
        assert_eq!(
            b.call(
                "artifact.write",
                json!({"slot":"result","value":{"nonce":"extra"}})
            )
            .await
            .unwrap_err()
            .code,
            "limit_reached"
        );
        ExecutionOutcome::Completed { usage: json!({}) }
    }
}
#[tokio::test]
async fn failures_and_concurrent_calls_preserve_submission_without_exceeding_total() {
    let f = Fixture::new(false);
    let mut b = f.bindings("pass");
    b.executors
        .insert("test.executor".into(), Arc::new(ReserveExecutor));
    let rt = Runtime::open(options(&f, policy()), vec![f.definition.clone()], b.clone())
        .await
        .unwrap();
    let id = rt
        .start(f.request("reserve", "A", false), &Caller::local())
        .await
        .unwrap();
    let s = wait_for(&rt, &id, |s| s.status.terminal()).await;
    assert_eq!(s.status, Status::Completed);
    let view = rt.budgets(&id, &Caller::local()).unwrap();
    assert_eq!(view.runs.len(), 1);
    let r = &view.runs[0];
    assert_eq!((r.calls_used, r.calls_rejected), (5, 7));
    assert!(r.limits_hit.contains(&BudgetLimit::ExplorationCalls));
    assert!(r.limits_hit.contains(&BudgetLimit::ToolCalls));
    let other = Caller {
        principal: "other".into(),
        grants: vec!["local-readonly".into()],
    };
    assert_eq!(rt.budgets(&id, &other).unwrap_err().code, "not_found");
    rt.shutdown().await.unwrap();
    drop(rt);
    let rt = Runtime::open(options(&f, policy()), vec![f.definition.clone()], b)
        .await
        .unwrap();
    assert_eq!(
        rt.budgets(&id, &Caller::local()).unwrap().runs[0].calls_used,
        5
    );
    rt.shutdown().await.unwrap();
}
struct SlowExecutor;
#[async_trait]
impl Executor for SlowExecutor {
    fn capabilities(&self) -> ExecutorCapabilities {
        ExecutorCapabilities::MANAGED
    }
    async fn execute(
        &self,
        inv: Invocation,
        b: Arc<dyn ToolBridge>,
        _: CancellationToken,
    ) -> ExecutionOutcome {
        tokio::time::sleep(Duration::from_secs(250)).await;
        b.call(
            "artifact.write",
            json!({"slot":"result","value":{"nonce":inv.inputs["nonce"]}}),
        )
        .await
        .unwrap();
        ExecutionOutcome::Completed { usage: json!({}) }
    }
}
struct SlowGate;
#[async_trait]
impl DomainGate for SlowGate {
    async fn check(&self, c: GateContext) -> CheckResult {
        tokio::time::sleep(Duration::from_secs(130)).await;
        CheckResult::pass("ready", vec![c.check_evidence_ref])
    }
}
#[tokio::test(start_paused = true)]
async fn explicit_inherited_time_exceeds_legacy_run_and_check_caps_without_relaxing_task() {
    let f = Fixture::new(false);
    let mut b = f.bindings("pass");
    b.executors
        .insert("test.executor".into(), Arc::new(SlowExecutor));
    b.gates.insert("test.gate".into(), Arc::new(SlowGate));
    let rt = Runtime::open(options(&f, policy()), vec![f.definition.clone()], b)
        .await
        .unwrap();
    let id = rt
        .start(f.request("slow", "A", false), &Caller::local())
        .await
        .unwrap();
    let mut state = None;
    // Auto-advance the actual Tokio timers, retaining file/DB operations and public SDK.
    for _ in 0..600 {
        let s = rt.get(&id, &Caller::local()).await.unwrap();
        if s.status.terminal() || s.status == Status::Blocked {
            state = Some(s);
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    assert_eq!(state.unwrap().status, Status::Completed);
    let v = rt.budgets(&id, &Caller::local()).unwrap();
    let r = &v.runs[0];
    assert!(r.effective_run_ms > 240000);
    assert!(r.elapsed_ms >= 250000);
    assert!(r.effective_check_ms.unwrap() > 120000);
    assert!(r.check_elapsed_ms.unwrap() >= 130000);
    assert_eq!(v.task_limits.max_active_ms, 600000);
    assert!(v.task_limit_sources.is_some());
    assert!(r.actual_model_iterations.is_none());
    rt.shutdown().await.unwrap();
}
struct NeverStart(Arc<AtomicUsize>);
#[async_trait]
impl Executor for NeverStart {
    fn capabilities(&self) -> ExecutorCapabilities {
        ExecutorCapabilities::MANAGED
    }
    async fn execute(
        &self,
        _: Invocation,
        _: Arc<dyn ToolBridge>,
        _: CancellationToken,
    ) -> ExecutionOutcome {
        self.0.fetch_add(1, Ordering::SeqCst);
        ExecutionOutcome::Completed { usage: json!({}) }
    }
}
#[tokio::test]
async fn unsupported_model_iterations_refuse_before_executor_start() {
    let f = Fixture::new(false);
    let starts = Arc::new(AtomicUsize::new(0));
    let mut b = f.bindings("pass");
    b.executors
        .insert("test.executor".into(), Arc::new(NeverStart(starts.clone())));
    let mut p = policy();
    p.max_model_iterations = Some(2);
    let rt = Runtime::open(options(&f, p), vec![f.definition.clone()], b)
        .await
        .unwrap();
    let id = rt
        .start(f.request("iterations", "A", false), &Caller::local())
        .await
        .unwrap();
    let s = wait_for(&rt, &id, |s| s.status.terminal()).await;
    assert_eq!(s.status, Status::Failed);
    assert_eq!(s.error.unwrap().code, "unsupported_constraint");
    assert_eq!(starts.load(Ordering::SeqCst), 0);
    let r = rt.budgets(&id, &Caller::local()).unwrap().runs.remove(0);
    assert_eq!(r.model_iteration_validation, "rejected");
    assert!(r.limits_hit.contains(&BudgetLimit::UnsupportedConstraint));
    assert!(r.actual_model_iterations.is_none());
    rt.shutdown().await.unwrap();
}
struct BytesExecutor;
#[async_trait]
impl Executor for BytesExecutor {
    fn capabilities(&self) -> ExecutorCapabilities {
        ExecutorCapabilities::MANAGED
    }
    async fn execute(
        &self,
        inv: Invocation,
        b: Arc<dyn ToolBridge>,
        _: CancellationToken,
    ) -> ExecutionOutcome {
        b.call(
            "artifact.write",
            json!({"slot":"result","value":{"nonce":inv.inputs["nonce"]}}),
        )
        .await
        .unwrap();
        assert_eq!(
            b.call(
                "artifact.write",
                json!({"slot":"result","value":{"nonce":"X".repeat(80)}})
            )
            .await
            .unwrap_err()
            .code,
            "limit_reached"
        );
        ExecutionOutcome::Completed { usage: json!({}) }
    }
}
#[tokio::test]
async fn candidate_bytes_remain_independent_of_available_submission_calls() {
    let f = Fixture::new(false);
    let mut b = f.bindings("pass");
    b.executors
        .insert("test.executor".into(), Arc::new(BytesExecutor));
    let mut p = policy();
    p.max_candidate_bytes = 32;
    let rt = Runtime::open(options(&f, p), vec![f.definition.clone()], b)
        .await
        .unwrap();
    let id = rt
        .start(f.request("bytes", "A", false), &Caller::local())
        .await
        .unwrap();
    let s = wait_for(&rt, &id, |s| s.status.terminal()).await;
    assert_eq!(s.status, Status::Completed);
    let r = rt.budgets(&id, &Caller::local()).unwrap().runs.remove(0);
    assert!(r.limits_hit.contains(&BudgetLimit::CandidateBytes));
    assert_eq!(r.calls_used, 2);
    assert!(r.candidate_bytes_used <= 32);
    let bytes = rt
        .read_artifact(&s.accepted_artifacts[0].id, &Caller::local())
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&bytes.bytes).unwrap()["nonce"],
        "A"
    );
    rt.shutdown().await.unwrap();
}

struct FinalizeExecutor;
#[async_trait]
impl Executor for FinalizeExecutor {
    fn capabilities(&self) -> ExecutorCapabilities {
        ExecutorCapabilities::MANAGED
    }
    async fn execute(
        &self,
        inv: Invocation,
        b: Arc<dyn ToolBridge>,
        _: CancellationToken,
    ) -> ExecutionOutcome {
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert_eq!(b.budget_hint().unwrap().mode, "finalize");
        assert_eq!(
            b.call("artifact.read", json!({"id":"missing"}))
                .await
                .unwrap_err()
                .code,
            "limit_reached"
        );
        b.call(
            "artifact.write",
            json!({"slot":"result","value":{"nonce":inv.inputs["nonce"]}}),
        )
        .await
        .unwrap();
        ExecutionOutcome::Completed { usage: json!({}) }
    }
    async fn stop_or_confirm(&self, _: &str) -> StopState {
        StopState::Confirmed
    }
}
#[tokio::test(start_paused = true)]
async fn time_margin_refuses_exploration_but_allows_controlled_submission() {
    let f = Fixture::new(false);
    let mut b = f.bindings("pass");
    b.executors
        .insert("test.executor".into(), Arc::new(FinalizeExecutor));
    let mut p = policy();
    p.max_run_ms = Some(100);
    p.finalize_before_ms = 30;
    let rt = Runtime::open(options(&f, p), vec![f.definition.clone()], b)
        .await
        .unwrap();
    let id = rt
        .start(f.request("finalize", "A", false), &Caller::local())
        .await
        .unwrap();
    let s = wait_for(&rt, &id, |s| s.status.terminal()).await;
    assert_eq!(s.status, Status::Completed);
    let r = rt.budgets(&id, &Caller::local()).unwrap().runs.remove(0);
    assert_eq!(r.calls_used, 1);
    assert_eq!(r.calls_rejected, 1);
    assert!(r.limits_hit.contains(&BudgetLimit::FinalizationTime));
    rt.shutdown().await.unwrap();
}
struct HardDeadlineExecutor;
#[async_trait]
impl Executor for HardDeadlineExecutor {
    fn capabilities(&self) -> ExecutorCapabilities {
        ExecutorCapabilities::MANAGED
    }
    async fn execute(
        &self,
        inv: Invocation,
        b: Arc<dyn ToolBridge>,
        _: CancellationToken,
    ) -> ExecutionOutcome {
        b.call(
            "artifact.write",
            json!({"slot":"result","value":{"nonce":inv.inputs["nonce"]}}),
        )
        .await
        .unwrap();
        // Explicitly advance this timer while keeping the adapter future ready.
        tokio::time::advance(Duration::from_secs(2)).await;
        assert_eq!(
            b.call(
                "artifact.write",
                json!({"slot":"result","value":{"nonce":"LATE"}})
            )
            .await
            .unwrap_err()
            .code,
            "limit_reached"
        );
        ExecutionOutcome::Completed { usage: json!({}) }
    }
    async fn stop_or_confirm(&self, _: &str) -> StopState {
        StopState::Confirmed
    }
}
#[tokio::test(start_paused = true)]
async fn hard_deadline_retains_candidate_facts_but_never_accepts_late_completion() {
    let f = Fixture::new(false);
    let mut b = f.bindings("pass");
    b.executors
        .insert("test.executor".into(), Arc::new(HardDeadlineExecutor));
    let mut p = policy();
    p.max_run_ms = Some(1000);
    let rt = Runtime::open(options(&f, p), vec![f.definition.clone()], b)
        .await
        .unwrap();
    let id = rt
        .start(f.request("hard", "A", false), &Caller::local())
        .await
        .unwrap();
    let s = wait_for(&rt, &id, |s| s.status.terminal()).await;
    assert_eq!(s.status, Status::Failed);
    assert!(s.accepted_artifacts.is_empty());
    assert_eq!(s.error.unwrap().code, "limit_reached");
    let r = rt.budgets(&id, &Caller::local()).unwrap().runs.remove(0);
    assert!(r.limits_hit.contains(&BudgetLimit::RunTime));
    assert_eq!(r.calls_used, 1);
    let conn = rusqlite::Connection::open(f.root.path().join("store/state.sqlite")).unwrap();
    let body: String = conn
        .query_row("SELECT body FROM executions WHERE id=?1", [&id], |r| {
            r.get(0)
        })
        .unwrap();
    let body: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(!body["candidate_files"].as_object().unwrap().is_empty());
    assert!(body["checks"].as_array().unwrap().is_empty());
    rt.shutdown().await.unwrap();
}
#[tokio::test]
async fn legacy_policy_is_visible_and_invalid_reserve_is_rejected_at_open() {
    let f = Fixture::new(false);
    let rt = f.open("pass").await;
    let id = rt
        .start(f.request("legacy", "A", false), &Caller::local())
        .await
        .unwrap();
    wait_for(&rt, &id, |s| s.status.terminal()).await;
    let r = rt.budgets(&id, &Caller::local()).unwrap().runs.remove(0);
    assert_eq!(r.policy_source, "legacy");
    assert_eq!(r.policy.max_run_ms, Some(240000));
    assert_eq!(r.policy.max_check_ms, Some(120000));
    assert_eq!(r.policy.submission_reserve, 0);
    rt.shutdown().await.unwrap();
    drop(rt);
    let mut p = policy();
    p.submission_reserve = 6;
    let result = Runtime::open(
        options(&f, p),
        vec![f.definition.clone()],
        f.bindings("pass"),
    )
    .await;
    assert_eq!(result.err().unwrap().code, "input_invalid");
}

struct HoldingExecutor(Arc<tokio::sync::Notify>);
#[async_trait]
impl Executor for HoldingExecutor {
    fn capabilities(&self) -> ExecutorCapabilities {
        ExecutorCapabilities::MANAGED
    }
    fn budget_limits(&self) -> ExecutorBudgetLimits {
        ExecutorBudgetLimits {
            max_run_ms: Some(300000),
            ..Default::default()
        }
    }
    async fn execute(
        &self,
        inv: Invocation,
        _: Arc<dyn ToolBridge>,
        cancel: CancellationToken,
    ) -> ExecutionOutcome {
        assert!(inv.timeout_ms <= 300000 && inv.timeout_ms > 290000);
        self.0.notify_one();
        cancel.cancelled().await;
        ExecutionOutcome::Cancelled
    }
    async fn stop_or_confirm(&self, _: &str) -> StopState {
        StopState::Confirmed
    }
}
#[tokio::test]
async fn running_budget_reports_source_intersection_and_explicit_cancel_survives_restart() {
    let f = Fixture::new(false);
    let entered = Arc::new(tokio::sync::Notify::new());
    let mut b = f.bindings("pass");
    b.executors.insert(
        "test.executor".into(),
        Arc::new(HoldingExecutor(entered.clone())),
    );
    b.grants
        .get_mut("local-readonly")
        .unwrap()
        .limits
        .max_active_ms = 350000;
    let mut p = policy();
    p.max_run_ms = Some(320000);
    let mut o = options(&f, p);
    o.max_tool_calls_per_run = 3;
    o.max_candidate_bytes = 64;
    let rt = Runtime::open(o, vec![f.definition.clone()], b.clone())
        .await
        .unwrap();
    let mut request = f.request("sources", "A", false);
    request.limits.max_active_ms = 400000;
    let id = rt.start(request, &Caller::local()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), entered.notified())
        .await
        .unwrap();
    let v = rt.budgets(&id, &Caller::local()).unwrap();
    let sources = v.task_limit_sources.unwrap();
    assert_eq!(sources.requested.max_active_ms, 400000);
    assert_eq!(sources.grant.max_active_ms, 350000);
    assert_eq!(v.task_limits.max_active_ms, 350000);
    let r = &v.runs[0];
    assert_eq!(r.effective_run_ms, 300000);
    assert_eq!(r.limiting_time_sources, vec!["executor"]);
    assert_eq!(r.effective_tool_calls, 3);
    assert_eq!(r.effective_candidate_bytes, 64);
    assert!(r.execution_outcome.is_none());
    assert_eq!(r.hint.mode, "explore");
    rt.cancel(&id, "stop-explicit", &Caller::local())
        .await
        .unwrap();
    let s = wait_for(&rt, &id, |s| s.status.terminal()).await;
    assert_eq!(s.status, Status::Cancelled);
    assert!(s.accepted_artifacts.is_empty());
    assert_eq!(
        rt.budgets(&id, &Caller::local()).unwrap().runs[0]
            .execution_outcome
            .as_deref(),
        Some("cancelled")
    );
    rt.shutdown().await.unwrap();
    drop(rt);
    let mut p = policy();
    p.max_run_ms = Some(320000);
    let mut o = options(&f, p);
    o.max_tool_calls_per_run = 3;
    o.max_candidate_bytes = 64;
    let rt = Runtime::open(o, vec![f.definition.clone()], b)
        .await
        .unwrap();
    assert_eq!(
        rt.budgets(&id, &Caller::local()).unwrap().runs[0]
            .execution_outcome
            .as_deref(),
        Some("cancelled")
    );
    rt.shutdown().await.unwrap();
}

struct OverdueGate;
#[async_trait]
impl DomainGate for OverdueGate {
    async fn check(&self, c: GateContext) -> CheckResult {
        // A synchronous callback can complete in one poll after the timeout future expired.
        std::thread::sleep(Duration::from_millis(30));
        CheckResult::pass("ready", vec![c.check_evidence_ref])
    }
}
#[tokio::test]
async fn synchronous_gate_cannot_accept_after_its_separate_check_deadline() {
    let f = Fixture::new(false);
    let mut b = f.bindings("pass");
    b.gates.insert("test.gate".into(), Arc::new(OverdueGate));
    let mut p = policy();
    p.max_check_ms = Some(10);
    let rt = Runtime::open(options(&f, p), vec![f.definition.clone()], b)
        .await
        .unwrap();
    let id = rt
        .start(f.request("gate-limit", "A", false), &Caller::local())
        .await
        .unwrap();
    let s = wait_for(&rt, &id, |s| {
        s.status.terminal() || s.status == Status::Blocked
    })
    .await;
    assert_eq!(s.status, Status::Blocked);
    assert!(s.accepted_artifacts.is_empty());
    let r = rt.budgets(&id, &Caller::local()).unwrap().runs.remove(0);
    assert_eq!(r.effective_check_ms, Some(10));
    assert!(r.check_elapsed_ms.unwrap() >= 30);
    assert!(r.limits_hit.contains(&BudgetLimit::CheckTime));
    rt.shutdown().await.unwrap();
}
