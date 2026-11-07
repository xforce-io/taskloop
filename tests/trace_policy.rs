mod common;
use common::*;
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use taskloop::*;
struct Filter {
    slow: bool,
}
#[async_trait::async_trait]
impl TraceContentFilter for Filter {
    async fn redact(&self, _: &str, _: &str, value: Value) -> Result<Value> {
        if self.slow {
            tokio::time::sleep(Duration::from_secs(20)).await;
        }
        Ok(serde_json::from_str(&value.to_string().replace("SYNTHETIC-KEY", "[MASKED]")).unwrap())
    }
}
fn policy(content: bool) -> TracePolicy {
    TracePolicy {
        policy_id: "test-v1".into(),
        max_total_events: 1000,
        max_total_bytes: 1048576,
        max_runs: 16,
        retention_ms: 60000,
        max_content_bytes: 4096,
        content_tools: if content {
            BTreeMap::from([(
                "artifact.write".into(),
                TraceToolContent {
                    input: vec!["/value".into()],
                    output: vec![],
                },
            )])
        } else {
            BTreeMap::new()
        },
        filter_version: content.then(|| "filter-v1".into()),
        filter: content.then(|| Arc::new(Filter { slow: false }) as Arc<dyn TraceContentFilter>),
    }
}
fn options() -> TraceOptions {
    TraceOptions {
        max_events_per_run: 1000,
        max_bytes_per_run: 1048576,
        source_max_events: 1000,
        source_max_bytes: 262144,
    }
}
fn q() -> TraceQuery {
    TraceQuery {
        limit: 1000,
        ..Default::default()
    }
}
async fn open(f: &Fixture, p: TracePolicy) -> Runtime {
    Runtime::open_with_trace_policy(
        RuntimeOptions::new(f.root.path().join("store")),
        vec![f.definition.clone()],
        f.bindings("pass"),
        options(),
        p,
    )
    .await
    .unwrap()
}
async fn completed(rt: &Runtime, f: &Fixture, key: &str) -> String {
    let id = rt
        .start(
            f.request(key, "PUBLIC SYNTHETIC-KEY", false),
            &Caller::local(),
        )
        .await
        .unwrap();
    assert_eq!(
        wait_for(rt, &id, |s| s.status.terminal()).await.status,
        Status::Completed
    );
    for _ in 0..100 {
        if !rt
            .trace(&id, q(), &Caller::local())
            .unwrap()
            .issues
            .iter()
            .any(|s| s.starts_with("content_pending:"))
        {
            return id;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("content collectors did not settle");
}
fn reference(page: &TracePage) -> TraceContentRef {
    serde_json::from_value(
        page.events
            .iter()
            .find(|e| e.event.kind == "tool.content" && e.event.data["direction"] == "input")
            .unwrap()
            .event
            .data["content"]
            .clone(),
    )
    .unwrap()
}
#[tokio::test]
async fn metadata_mode_has_no_content_and_exposes_actual_policy() {
    let f = Fixture::new(false);
    let rt = open(&f, policy(false)).await;
    let id = rt
        .start(
            f.request(
                "safe",
                "PUBLIC SYNTHETIC-KEY ENV_VAR=HIDDEN FULL_PROMPT=HIDDEN",
                false,
            ),
            &Caller::local(),
        )
        .await
        .unwrap();
    assert_eq!(
        wait_for(&rt, &id, |s| s.status.terminal()).await.status,
        Status::Completed
    );
    let page = rt.trace(&id, q(), &Caller::local()).unwrap();
    let s = page.policy.as_ref().unwrap();
    assert_eq!(s.content_mode, "metadata_only");
    assert_eq!(s.retention_ms, Some(60000));
    assert_eq!(s.source_queue_events, 256);
    let export = rt.export_trace(&id, q(), &Caller::local()).unwrap();
    assert!(!String::from_utf8(export).unwrap().contains("SYNTHETIC-KEY"));
    assert!(
        page.events
            .iter()
            .all(|e| e.event.data["input_content"].is_null())
    );
    let db = rusqlite::Connection::open(f.root.path().join("store/trace.sqlite")).unwrap();
    assert_eq!(
        db.query_row("SELECT count(*) FROM trace_content", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM trace_events WHERE body LIKE '%SYNTHETIC-KEY%'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    rt.shutdown().await.unwrap();
}
#[tokio::test]
async fn selected_content_is_filtered_and_bound_to_instance_owner() {
    let f = Fixture::new(false);
    let rt = open(&f, policy(true)).await;
    let a = completed(&rt, &f, "a").await;
    let b = completed(&rt, &f, "b").await;
    let page = rt.trace(&a, q(), &Caller::local()).unwrap();
    let reference = reference(&page);
    let content = rt
        .read_trace_content(&a, &reference.id, &Caller::local())
        .unwrap();
    assert_eq!(content.value, json!({"/value":{"nonce":"PUBLIC [MASKED]"}}));
    assert_eq!(
        rt.read_trace_content(&b, &reference.id, &Caller::local())
            .unwrap_err()
            .code,
        "not_found"
    );
    assert_eq!(
        rt.read_trace_content(
            &a,
            &reference.id,
            &Caller {
                principal: "other".into(),
                grants: vec![]
            }
        )
        .unwrap_err()
        .code,
        "not_found"
    );
    assert!(
        !String::from_utf8(rt.export_trace(&a, q(), &Caller::local()).unwrap())
            .unwrap()
            .contains("SYNTHETIC-KEY")
    );
    let s = rt.get(&a, &Caller::local()).await.unwrap();
    let artifact = rt
        .read_artifact(&s.accepted_artifacts[0].id, &Caller::local())
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&artifact.bytes).unwrap()["nonce"],
        "PUBLIC SYNTHETIC-KEY"
    );
    rt.shutdown().await.unwrap();
    drop(rt);
    let rt = open(&f, policy(true)).await;
    assert_eq!(
        rt.read_trace_content(&a, &reference.id, &Caller::local())
            .unwrap()
            .value,
        content.value
    );
    rt.shutdown().await.unwrap();
}
#[tokio::test]
async fn expiry_removes_only_trace_and_does_not_extend_on_reopen() {
    let f = Fixture::new(false);
    let mut p = policy(true);
    p.retention_ms = 500;
    let rt = open(&f, p.clone()).await;
    let a = completed(&rt, &f, "a").await;
    let first = rt.trace(&a, q(), &Caller::local()).unwrap();
    let reference = reference(&first);
    rt.shutdown().await.unwrap();
    drop(rt);
    tokio::time::sleep(Duration::from_millis(550)).await;
    let rt = open(&f, p).await;
    rt.maintain_trace().unwrap();
    let page = rt.trace(&a, q(), &Caller::local()).unwrap();
    assert!(page.events.is_empty());
    assert!(!page.complete);
    assert!(!page.expired_runs.is_empty());
    assert_eq!(
        rt.read_trace_content(&a, &reference.id, &Caller::local())
            .unwrap_err()
            .code,
        "not_found"
    );
    let snapshot = rt.get(&a, &Caller::local()).await.unwrap();
    assert_eq!(snapshot.status, Status::Completed);
    assert_eq!(snapshot.accepted_artifacts.len(), 1);
    assert!(
        rt.read_artifact(&snapshot.accepted_artifacts[0].id, &Caller::local())
            .await
            .is_ok()
    );
    rt.shutdown().await.unwrap();
}
#[tokio::test]
async fn content_timeout_and_global_quota_are_diagnostic_only() {
    for slow in [true, false] {
        let f = Fixture::new(false);
        let mut p = policy(true);
        if slow {
            p.filter = Some(Arc::new(Filter { slow: true }));
        } else {
            p.max_total_events = 2;
        }
        let rt = open(&f, p).await;
        let started = std::time::Instant::now();
        let id = completed(&rt, &f, "fault").await;
        assert!(started.elapsed() < Duration::from_secs(3));
        let page = rt.trace(&id, q(), &Caller::local()).unwrap();
        assert!(!page.complete);
        assert!(
            page.issues.iter().any(|i| i.starts_with(if slow {
                "content_filter:"
            } else {
                "capacity:"
            })),
            "{page:#?}"
        );
        if !slow {
            assert_eq!(page.events.len(), 2);
        }
        rt.shutdown().await.unwrap();
    }
}
#[tokio::test]
async fn invalid_policy_and_future_schema_do_not_start_execution() {
    let f = Fixture::new(false);
    let mut p = policy(true);
    p.content_tools.get_mut("artifact.write").unwrap().input = vec!["/environment/token".into()];
    let result = Runtime::open_with_trace_policy(
        RuntimeOptions::new(f.root.path().join("store")),
        vec![f.definition.clone()],
        f.bindings("pass"),
        options(),
        p,
    )
    .await;
    assert_eq!(result.err().unwrap().code, "input_invalid");
    let dir = f.root.path().join("store");
    let db = rusqlite::Connection::open(dir.join("trace.sqlite")).unwrap();
    db.pragma_update(None, "user_version", 99).unwrap();
    drop(db);
    let result = Runtime::open_with_trace_policy(
        RuntimeOptions::new(dir),
        vec![f.definition.clone()],
        f.bindings("pass"),
        options(),
        policy(false),
    )
    .await;
    assert_eq!(result.err().unwrap().code, "storage_incompatible");
}

#[tokio::test]
async fn content_write_failure_cannot_fail_business_or_leak_filter_error() {
    let f = Fixture::new(false);
    let rt = open(&f, policy(true)).await;
    let db = rusqlite::Connection::open(f.root.path().join("store/trace.sqlite")).unwrap();
    db.execute_batch("CREATE TRIGGER fail_content BEFORE INSERT ON trace_content BEGIN SELECT RAISE(ABORT,'SYNTHETIC-KEY'); END;").unwrap();
    let id = completed(&rt, &f, "write-fault").await;
    let page = rt.trace(&id, q(), &Caller::local()).unwrap();
    assert!(
        page.issues
            .iter()
            .any(|e| e.starts_with("content_storage:"))
    );
    assert!(!page.complete);
    assert!(
        !String::from_utf8(rt.export_trace(&id, q(), &Caller::local()).unwrap())
            .unwrap()
            .contains("SYNTHETIC-KEY")
    );
    rt.shutdown().await.unwrap();
}
#[tokio::test]
async fn run_count_and_total_byte_limits_are_observable() {
    for count_limit in [true, false] {
        let f = Fixture::new(false);
        let mut p = policy(false);
        if count_limit {
            p.max_runs = 1;
        } else {
            p.max_total_bytes = 4096;
        }
        let rt = open(&f, p).await;
        let a = completed(&rt, &f, "a").await;
        let b = completed(&rt, &f, "b").await;
        let page = rt.trace(&b, q(), &Caller::local()).unwrap();
        assert!(!page.complete);
        assert!(
            page.issues.iter().any(|e| e.starts_with("capacity:")),
            "{page:#?}"
        );
        let db = rusqlite::Connection::open(f.root.path().join("store/trace.sqlite")).unwrap();
        if count_limit {
            assert_eq!(
                db.query_row("SELECT runs FROM trace_global WHERE id=1", [], |r| r
                    .get::<_, i64>(0))
                    .unwrap(),
                1
            );
            assert!(
                !rt.trace(&a, q(), &Caller::local())
                    .unwrap()
                    .events
                    .is_empty()
            );
        } else {
            assert!(
                db.query_row(
                    "SELECT bytes+content_bytes FROM trace_global WHERE id=1",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap()
                    <= 4096
            );
        }
        rt.shutdown().await.unwrap();
    }
}

struct CleanupExecutor {
    label: &'static str,
    allow: Arc<std::sync::atomic::AtomicBool>,
    calls: Arc<std::sync::atomic::AtomicUsize>,
    cleanups: Arc<std::sync::atomic::AtomicUsize>,
    block: bool,
    startup_delay_ms: u64,
    not_started_no_source: bool,
    hold: std::sync::atomic::AtomicBool,
    release: tokio::sync::Notify,
}
#[async_trait::async_trait]
impl Executor for CleanupExecutor {
    fn capabilities(&self) -> ExecutorCapabilities {
        ExecutorCapabilities::MANAGED
    }
    fn identity(&self) -> Value {
        json!({"fixture":self.label})
    }
    async fn execute(
        &self,
        inv: Invocation,
        tools: Arc<dyn ToolBridge>,
        _: tokio_util::sync::CancellationToken,
    ) -> ExecutionOutcome {
        tokio::time::sleep(Duration::from_millis(self.startup_delay_ms)).await;
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        tools
            .call(
                "artifact.write",
                json!({"slot":"result","value":{"nonce":inv.inputs["nonce"]}}),
            )
            .await
            .unwrap();
        if self.hold.load(std::sync::atomic::Ordering::SeqCst) {
            self.release.notified().await;
        }
        ExecutionOutcome::Completed {
            usage: json!({"fixture":true}),
        }
    }
    async fn cleanup_observation(&self, _: &str) -> ObservationCleanup {
        self.cleanups
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self.not_started_no_source && self.calls.load(std::sync::atomic::Ordering::SeqCst) == 0 {
            return ObservationCleanup::NotApplicable;
        }
        if self.block {
            std::future::pending::<()>().await;
        }
        if self.allow.load(std::sync::atomic::Ordering::SeqCst) {
            ObservationCleanup::Purged
        } else {
            ObservationCleanup::Deferred
        }
    }
}
fn cleanup_executor(label: &'static str) -> Arc<CleanupExecutor> {
    Arc::new(CleanupExecutor {
        label,
        allow: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        cleanups: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        block: false,
        startup_delay_ms: 0,
        not_started_no_source: false,
        hold: std::sync::atomic::AtomicBool::new(false),
        release: tokio::sync::Notify::new(),
    })
}
async fn open_cleanup(f: &Fixture, executor: Arc<CleanupExecutor>) -> Runtime {
    let mut p = policy(false);
    p.retention_ms = 150;
    p.max_runs = 1;
    let bindings = f.bindings("pass").executor("test.executor", executor);
    Runtime::open_with_trace_policy(
        RuntimeOptions::new(f.root.path().join("store")),
        vec![f.definition.clone()],
        bindings,
        options(),
        p,
    )
    .await
    .unwrap()
}
async fn cleanup_state(rt: &Runtime, id: &str, expected: &str) -> TracePage {
    for _ in 0..200 {
        let page = rt.trace(id, q(), &Caller::local()).unwrap();
        if page.source_cleanup.iter().any(|s| s.state == expected) {
            return page;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("expected source cleanup state {expected}")
}
#[tokio::test]
async fn pending_source_cleanup_survives_restart_and_is_not_evicted_for_capacity() {
    use std::sync::atomic::Ordering;
    let f = Fixture::new(false);
    let executor = cleanup_executor("stable");
    let rt = open_cleanup(&f, executor.clone()).await;
    let a = completed(&rt, &f, "a").await;
    rt.shutdown().await.unwrap();
    drop(rt);
    tokio::time::sleep(Duration::from_millis(180)).await;
    let rt = open_cleanup(&f, executor.clone()).await;
    cleanup_state(&rt, &a, "deferred").await;
    assert_eq!(
        executor.calls.load(Ordering::SeqCst),
        1,
        "reopen must not replay execution"
    );
    let b = completed(&rt, &f, "b").await;
    assert!(
        rt.trace(&b, q(), &Caller::local())
            .unwrap()
            .events
            .is_empty()
    );
    assert!(
        rt.trace(&a, q(), &Caller::local())
            .unwrap()
            .source_cleanup
            .iter()
            .any(|s| s.state == "deferred")
    );
    executor.allow.store(true, Ordering::SeqCst);
    cleanup_state(&rt, &a, "purged").await;
    // Keep C active while checking capacity; completed Trace is intentionally
    // eligible for expiry after only 150ms on this fixture's retention policy.
    executor.hold.store(true, Ordering::SeqCst);
    let c = rt
        .start(f.request("c", "c", false), &Caller::local())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(4), async {
        while rt
            .trace(&c, q(), &Caller::local())
            .unwrap()
            .events
            .is_empty()
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("purged source must release trace capacity for active C");
    executor.release.notify_one();
    wait_for(&rt, &c, |s| s.status.terminal()).await;
    assert_eq!(executor.calls.load(Ordering::SeqCst), 3);
    rt.shutdown().await.unwrap();
}
#[tokio::test]
async fn changed_executor_binding_cannot_claim_old_source_was_cleaned() {
    use std::sync::atomic::Ordering;
    let f = Fixture::new(false);
    let rt = open_cleanup(&f, cleanup_executor("old")).await;
    let a = completed(&rt, &f, "a").await;
    rt.shutdown().await.unwrap();
    drop(rt);
    tokio::time::sleep(Duration::from_millis(180)).await;
    let replacement = cleanup_executor("replacement");
    replacement.allow.store(true, Ordering::SeqCst);
    let rt = open_cleanup(&f, replacement.clone()).await;
    let page = cleanup_state(&rt, &a, "binding_changed").await;
    assert!(!page.complete);
    assert_eq!(replacement.cleanups.load(Ordering::SeqCst), 0);
    rt.shutdown().await.unwrap();
}
#[tokio::test]
async fn blocked_source_cleanup_does_not_block_shutdown_or_change_business_state() {
    use std::sync::atomic::Ordering;
    let f = Fixture::new(false);
    let mut executor = cleanup_executor("blocked");
    Arc::get_mut(&mut executor).unwrap().block = true;
    let rt = open_cleanup(&f, executor.clone()).await;
    let a = completed(&rt, &f, "a").await;
    for _ in 0..100 {
        if executor.cleanups.load(Ordering::SeqCst) > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(executor.cleanups.load(Ordering::SeqCst) > 0);
    let before = std::time::Instant::now();
    rt.shutdown().await.unwrap();
    assert!(before.elapsed() < Duration::from_secs(1));
    assert_eq!(
        rt.get(&a, &Caller::local()).await.unwrap().status,
        Status::Completed
    );
}

#[tokio::test]
async fn retention_cleanup_waits_until_framework_cannot_start_the_run() {
    use std::sync::atomic::Ordering;
    let f = Fixture::new(false);
    let mut executor = cleanup_executor("delayed");
    Arc::get_mut(&mut executor).unwrap().startup_delay_ms = 1200;
    Arc::get_mut(&mut executor).unwrap().not_started_no_source = true;
    executor.allow.store(true, Ordering::SeqCst);
    let rt = open_cleanup(&f, executor.clone()).await;
    let id = rt
        .start(
            f.request("delayed", "PUBLIC SYNTHETIC-KEY", false),
            &Caller::local(),
        )
        .await
        .unwrap();
    wait_for(&rt, &id, |s| s.status == Status::Running).await;
    tokio::time::sleep(Duration::from_millis(650)).await;
    let before = executor.cleanups.load(Ordering::SeqCst);
    assert_eq!(
        wait_for(&rt, &id, |s| s.status.terminal()).await.status,
        Status::Completed
    );
    let page = rt.trace(&id, q(), &Caller::local()).unwrap();
    rt.shutdown().await.unwrap();
    assert_eq!(
        before, 0,
        "collector must not ask an executor to retire a run still starting"
    );
    assert!(
        !page
            .source_cleanup
            .iter()
            .any(|s| s.state == "not_applicable")
    );
}

#[tokio::test]
async fn slow_content_redaction_cannot_consume_the_business_execution_deadline() {
    // The filter needs a permit that only post-tool business progress supplies.
    // Waiting for it in the tool path therefore cannot succeed. This causal
    // assertion avoids attributing a busy runner's >100ms IO to filter waiting.
    struct AfterTool {
        progress: tokio::sync::Notify,
        filtered: std::sync::atomic::AtomicUsize,
    }
    #[async_trait::async_trait]
    impl TraceContentFilter for AfterTool {
        async fn redact(&self, _: &str, _: &str, value: Value) -> Result<Value> {
            self.progress.notified().await;
            self.filtered
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(value)
        }
    }
    struct BusinessProgress(Arc<AfterTool>);
    #[async_trait::async_trait]
    impl Executor for BusinessProgress {
        fn capabilities(&self) -> ExecutorCapabilities {
            ExecutorCapabilities::MANAGED
        }
        async fn execute(
            &self,
            inv: Invocation,
            tools: Arc<dyn ToolBridge>,
            _: tokio_util::sync::CancellationToken,
        ) -> ExecutionOutcome {
            tools
                .call(
                    "artifact.write",
                    json!({"slot":"result","value":{"nonce":inv.inputs["nonce"]}}),
                )
                .await
                .unwrap();
            self.0.progress.notify_one();
            ExecutionOutcome::Completed {
                usage: json!({"model_calls":0}),
            }
        }
        async fn stop_or_confirm(&self, _: &str) -> StopState {
            StopState::Confirmed
        }
    }
    for direction in ["disabled", "input", "output"] {
        let capture = direction != "disabled";
        let f = Fixture::new(false);
        let filter = Arc::new(AfterTool {
            progress: tokio::sync::Notify::new(),
            filtered: std::sync::atomic::AtomicUsize::new(0),
        });
        let mut p = policy(capture);
        if capture {
            p.filter = Some(filter.clone());
            if direction == "output" {
                let selection = p.content_tools.get_mut("artifact.write").unwrap();
                selection.input.clear();
                selection.output = vec!["/written".into()];
            }
        }
        let mut bindings = f.bindings("pass");
        bindings.executors.insert(
            "test.executor".into(),
            Arc::new(BusinessProgress(filter.clone())),
        );
        let rt = Runtime::open_with_trace_policy(
            RuntimeOptions::new(f.root.path().join("store")),
            vec![f.definition.clone()],
            bindings,
            options(),
            p,
        )
        .await
        .unwrap();
        let mut request = f.request("content-deadline", "BUSINESS", false);
        request.limits.max_active_ms = 5000;
        let id = rt.start(request, &Caller::local()).await.unwrap();
        let result = wait_for(&rt, &id, |s| {
            s.status.terminal() || s.status == Status::Blocked
        })
        .await;
        rt.shutdown().await.unwrap();
        assert_eq!(
            result.status,
            Status::Completed,
            "direction={direction}: {result:?}"
        );
        assert_eq!(result.accepted_artifacts.len(), 1);
        assert_eq!(
            filter.filtered.load(std::sync::atomic::Ordering::SeqCst),
            usize::from(capture),
            "direction={direction}; filter could not finish using post-tool progress"
        );
        let page = rt.trace(&id, q(), &Caller::local()).unwrap();
        assert!(
            !page.issues.iter().any(|s| s.starts_with("content_filter:")),
            "{page:?}"
        );
    }
}

struct HeldFilter {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}
#[async_trait::async_trait]
impl TraceContentFilter for HeldFilter {
    async fn redact(&self, _: &str, _: &str, value: Value) -> Result<Value> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(value)
    }
}
#[tokio::test]
async fn content_pending_is_visible_and_shutdown_drains_before_clean_reopen() {
    let f = Fixture::new(false);
    let filter = Arc::new(HeldFilter {
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let mut policy = policy(true);
    policy.filter = Some(filter.clone());
    let rt = open(&f, policy.clone()).await;
    let id = rt
        .start(f.request("content-tail", "SAFE", false), &Caller::local())
        .await
        .unwrap();
    filter.entered.notified().await;
    assert_eq!(
        wait_for(&rt, &id, |s| s.status.terminal()).await.status,
        Status::Completed
    );
    let pending = rt.trace(&id, q(), &Caller::local()).unwrap();
    assert!(
        pending
            .issues
            .iter()
            .any(|s| s.starts_with("content_pending:"))
    );
    let mut closing = tokio::spawn(async move {
        rt.shutdown().await.unwrap();
        rt
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(10), &mut closing)
            .await
            .is_err(),
        "shutdown ignored an active content collector"
    );
    filter.release.notify_one();
    let rt = closing.await.unwrap();
    let page = rt.trace(&id, q(), &Caller::local()).unwrap();
    assert!(
        !page
            .issues
            .iter()
            .any(|s| s.starts_with("content_pending:"))
    );
    let content = reference(&page);
    assert_eq!(
        rt.read_trace_content(&id, &content.id, &Caller::local())
            .unwrap()
            .value["/value"]["nonce"],
        "SAFE"
    );
    let started = page
        .events
        .iter()
        .find(|e| e.event.kind == "tool.started")
        .unwrap();
    assert_eq!(started.event.span_id, content.span_id);
    drop(rt);
    let rt = open(&f, policy).await;
    let page = rt.trace(&id, q(), &Caller::local()).unwrap();
    assert!(!page.issues.iter().any(|s| s.starts_with("host_unflushed:")));
    assert_eq!(
        serde_json::to_value(reference(&page)).unwrap(),
        serde_json::to_value(content).unwrap()
    );
    rt.shutdown().await.unwrap();
}
