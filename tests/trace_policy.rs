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
    id
}
fn reference(page: &TracePage) -> TraceContentRef {
    serde_json::from_value(
        page.events
            .iter()
            .find(|e| e.event.kind == "tool.started")
            .unwrap()
            .event
            .data["input_content"]
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
