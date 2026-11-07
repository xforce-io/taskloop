mod common;
use common::*;
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use taskloop::*;
use tokio_util::sync::CancellationToken;

fn options() -> TraceOptions {
    TraceOptions {
        max_events_per_run: 1000,
        max_bytes_per_run: 1048576,
        source_max_events: 1000,
        source_max_bytes: 262144,
    }
}
fn query() -> TraceQuery {
    TraceQuery {
        limit: 1000,
        ..Default::default()
    }
}
struct Observed;
#[async_trait::async_trait]
impl Executor for Observed {
    fn capabilities(&self) -> ExecutorCapabilities {
        ExecutorCapabilities::MANAGED
    }
    fn internal_observation(&self) -> bool {
        true
    }
    async fn execute(
        &self,
        _: Invocation,
        _: Arc<dyn ToolBridge>,
        _: CancellationToken,
    ) -> ExecutionOutcome {
        panic!("Runtime must use observed entry")
    }
    async fn execute_observed(
        &self,
        inv: Invocation,
        tools: Arc<dyn ToolBridge>,
        cancel: CancellationToken,
        observer: Option<Arc<dyn MilkieObserver>>,
    ) -> ExecutionOutcome {
        let observer = observer.unwrap();
        let parent = observer.options(&inv).traceparent;
        let trace = &parent[3..35];
        let source = uuid::Uuid::new_v4().to_string();
        let event = |seq: u64, kind: &str, span: &str, up: &str, data: Value| json!({"version":1,"id":format!("{source}:{seq}"),"sourceId":source,"sequence":seq,"runId":source,"contextId":"fixture-context","timestamp":1000-seq,"monotonicNs":seq.to_string(),"traceId":trace,"spanId":span,"parentSpanId":up,"kind":kind,"data":data,"native":{"sessionId":"native-session"}});
        let agent = "1111111111111111";
        let tool = "2222222222222222";
        observer
            .event(
                &inv,
                event(
                    1,
                    "execution.started",
                    agent,
                    &parent[36..52],
                    json!({"runtime":"fixture","capabilities":{"modelRequests":"unavailable"}}),
                ),
            )
            .await
            .unwrap();
        if inv.inputs["hold"] == true {
            cancel.cancelled().await;
            return ExecutionOutcome::StopUnknown {
                message: "fixture disconnected".into(),
            };
        }
        let requested = event(
            2,
            "tool.requested",
            tool,
            agent,
            json!({"callId":"call-1","name":"artifact.write","input":{"bytes":100,"sha256":"a".repeat(64)}}),
        );
        observer.event(&inv, requested.clone()).await.unwrap();
        observer.event(&inv, requested).await.unwrap();
        tools
            .call_with_metadata(
                "artifact.write",
                json!({"slot":"result","value":{"nonce":inv.inputs["nonce"]}}),
                ToolCallMetadata {
                    call_id: "call-1".into(),
                    source_run_id: source.clone(),
                    native_call_id: Some("native-call".into()),
                    traceparent: Some(format!("00-{trace}-{tool}-01")),
                },
            )
            .await
            .unwrap();
        // Delivery/finish arrive out of source order and wall time deliberately runs backwards.
        observer
            .event(
                &inv,
                event(
                    4,
                    "execution.finished",
                    agent,
                    &parent[36..52],
                    json!({"status":"succeeded","stopped":true}),
                ),
            )
            .await
            .unwrap();
        observer
            .event(
                &inv,
                event(
                    3,
                    "tool.delivery",
                    tool,
                    agent,
                    json!({"callId":"call-1","outcome":"delivered"}),
                ),
            )
            .await
            .unwrap();
        observer
            .status(
                &inv,
                json!({"state":"finished","complete":true,"lastSequence":4,"dropped":0}),
            )
            .await
            .unwrap();
        observer
            .status(
                &inv,
                json!({"kind":"adapter.summary","adapter_delivery_failed":false}),
            )
            .await
            .unwrap();
        ExecutionOutcome::Completed {
            usage: json!({"fixture":true}),
        }
    }
}
async fn open(f: &Fixture, capacity: u64) -> Runtime {
    let mut bindings = f.bindings("pass");
    bindings
        .executors
        .insert("test.executor".into(), Arc::new(Observed));
    let mut opts = options();
    opts.max_events_per_run = capacity;
    Runtime::open_with_trace(
        RuntimeOptions::new(f.root.path().join("store")),
        vec![f.definition.clone()],
        bindings,
        opts,
    )
    .await
    .unwrap()
}
async fn settled(rt: &Runtime, id: &str) -> TracePage {
    for _ in 0..100 {
        let p = rt.trace(id, query(), &Caller::local()).unwrap();
        if p.events.iter().any(|e| e.event.kind == "phase.finished") {
            return p;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("phase did not finish")
}
#[tokio::test]
async fn sdk_four_layers_paging_mapping_authorization_and_restart() {
    let f = Fixture::new(false);
    let rt = open(&f, 1000).await;
    let id = rt
        .start(
            f.request("trace", "SECRET-CONTENT", false),
            &Caller::local(),
        )
        .await
        .unwrap();
    wait_for(&rt, &id, |s| s.status.terminal()).await;
    let p = settled(&rt, &id).await;
    assert!(p.complete, "{p:#?}");
    assert!(p.open_spans.is_empty());
    for kind in [
        "phase.started",
        "execution.started",
        "tool.started",
        "check.started",
        "phase.finished",
    ] {
        assert!(p.events.iter().any(|e| e.event.kind == kind), "{kind}");
    }
    let native: Vec<_> = p
        .events
        .iter()
        .filter(|e| e.event.source_id.starts_with("milkie:"))
        .collect();
    assert_eq!(native.len(), 4);
    assert_eq!(
        native
            .iter()
            .map(|e| e.event.source_sequence)
            .collect::<Vec<_>>(),
        vec![1, 2, 4, 3]
    );
    assert!(
        native
            .iter()
            .all(|e| e.event.milkie_context_id.as_deref() == Some("fixture-context"))
    );
    let actual = p
        .events
        .iter()
        .find(|e| e.event.kind == "tool.started")
        .unwrap();
    assert_eq!(
        actual.event.parent_span_id.as_deref(),
        Some("2222222222222222")
    );
    let run = p.events[0].event.run_id.clone();
    let mut after = 0;
    let mut ids = vec![];
    loop {
        let page = rt
            .trace(
                &id,
                TraceQuery {
                    run_id: Some(run.clone()),
                    after,
                    limit: 2,
                },
                &Caller::local(),
            )
            .unwrap();
        if page.events.is_empty() {
            break;
        }
        assert!(page.next > after);
        after = page.next;
        ids.extend(page.events.iter().map(|e| e.event.id.clone()));
    }
    assert_eq!(
        ids,
        p.events
            .iter()
            .map(|e| e.event.id.clone())
            .collect::<Vec<_>>()
    );
    let bytes = rt.export_trace(&id, query(), &Caller::local()).unwrap();
    assert!(
        !String::from_utf8(bytes.clone())
            .unwrap()
            .contains("SECRET-CONTENT")
    );
    let page: TracePage = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(page.next, p.next);
    let stranger = Caller {
        principal: "other".into(),
        grants: vec![],
    };
    assert_eq!(
        rt.trace(&id, query(), &stranger).unwrap_err().code,
        "not_found"
    );
    assert_eq!(
        rt.export_trace(&id, query(), &stranger).unwrap_err().code,
        "not_found"
    );
    assert_eq!(
        rt.trace(
            &id,
            TraceQuery {
                limit: 0,
                ..Default::default()
            },
            &Caller::local()
        )
        .unwrap_err()
        .code,
        "input_invalid"
    );
    assert!(
        !rt.trace(
            &id,
            TraceQuery {
                run_id: Some("unknown".into()),
                ..query()
            },
            &Caller::local()
        )
        .unwrap()
        .complete
    );
    let other = rt
        .start(
            f.request("another", "OTHER-SECRET", false),
            &Caller::local(),
        )
        .await
        .unwrap();
    wait_for(&rt, &other, |s| s.status.terminal()).await;
    let second = settled(&rt, &other).await;
    assert!(second.complete);
    assert!(second.events.iter().all(|e| e.event.execution_id == other));
    assert!(second.events.iter().all(|e| e.event.run_id != run));
    assert_eq!(
        rt.trace(&id, query(), &Caller::local())
            .unwrap()
            .events
            .len(),
        p.events.len()
    );
    rt.shutdown().await.unwrap();
    drop(rt);
    let rt = open(&f, 1000).await;
    let reopened = rt.trace(&id, query(), &Caller::local()).unwrap();
    assert_eq!(reopened.events.len(), p.events.len());
    assert_eq!(reopened.next, p.next);
    assert!(reopened.complete);
    rt.shutdown().await.unwrap();
}
#[tokio::test]
async fn running_disconnection_and_capacity_do_not_change_business_result() {
    let f = Fixture::new(false);
    let rt = open(&f, 1000).await;
    let id = rt
        .start(f.request("held", "SECRET", true), &Caller::local())
        .await
        .unwrap();
    for _ in 0..100 {
        let p = rt.trace(&id, query(), &Caller::local()).unwrap();
        if p.events.iter().any(|e| e.event.kind == "execution.started") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let p = rt.trace(&id, query(), &Caller::local()).unwrap();
    assert!(!p.complete);
    assert!(p.open_spans.iter().any(|e| e.kind == "execution.started"));
    rt.cancel(&id, "stop", &Caller::local()).await.unwrap();
    let p = settled(&rt, &id).await;
    assert!(!p.complete);
    assert!(p.open_spans.iter().any(|e| e.kind == "execution.started"));
    assert_eq!(
        rt.get(&id, &Caller::local()).await.unwrap().status,
        Status::Cancelling
    );
    assert_eq!(rt.shutdown().await.unwrap_err().code, "stop_unconfirmed");
    let f = Fixture::new(false);
    let rt = open(&f, 2).await;
    let id = rt
        .start(f.request("capacity", "SECRET", false), &Caller::local())
        .await
        .unwrap();
    assert_eq!(
        wait_for(&rt, &id, |s| s.status.terminal()).await.status,
        Status::Completed
    );
    tokio::time::sleep(Duration::from_millis(20)).await;
    let p = rt.trace(&id, query(), &Caller::local()).unwrap();
    assert_eq!(p.events.len(), 2);
    assert!(!p.complete);
    assert!(p.issues.iter().any(|e| e.starts_with("capacity:")));
    rt.shutdown().await.unwrap();
}
#[tokio::test]
async fn old_runtime_explicitly_reports_disabled() {
    let f = Fixture::new(false);
    let rt = f.open("pass").await;
    let id = rt
        .start(f.request("old", "X", false), &Caller::local())
        .await
        .unwrap();
    let p = rt.trace(&id, query(), &Caller::local()).unwrap();
    assert!(!p.enabled);
    assert!(!p.complete);
    assert_eq!(p.issues, vec!["disabled"]);
    rt.shutdown().await.unwrap();
}
