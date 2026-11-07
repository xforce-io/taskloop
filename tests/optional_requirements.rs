mod common;
use common::*;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};
use taskloop::*;
use tokio_util::sync::CancellationToken;
fn load(f: &Fixture, optional: bool, selection: &str) -> Definition {
    let schema_path = f.root.path().join("result.schema.json");
    let mut schema: Value = serde_json::from_slice(&std::fs::read(&schema_path).unwrap()).unwrap();
    schema["properties"]["visit"] = json!({"type":"integer"});
    std::fs::write(&schema_path, serde_json::to_vec(&schema).unwrap()).unwrap();
    let p = f.root.path().join("flow.json");
    let mut b: Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
    b["entries"]["test.work"]["requires"] = json!([{"state":"work","slot":"result","selection":selection,"optional":optional,"include_attachments":true}]);
    b["entries"]["test.work"]["attachments"] =
        json!({"slot_prefix":"capture.","path_prefix":"captures/","max_files":2,"max_bytes":4096});
    b["entries"]["test.work"]["outcomes"] = json!(["ready", "finish"]);
    b["flow"]["states"]["work"]["accepted"]["finish"] = json!("done");
    b["flow"]["states"]["await"]["signals"]["resume"]["target"] = json!("work");
    std::fs::write(&p, serde_json::to_vec(&b).unwrap()).unwrap();
    Definition::load(
        &p,
        &BTreeMap::from([("demo-skill@0.1.0".into(), f.root.path().join("demo-skill"))]),
    )
    .unwrap()
}
#[derive(Default)]
struct Probe {
    seen: Mutex<Vec<(String, Vec<ArtifactRef>)>>,
    foreign: Mutex<Option<(String, String)>>,
}
#[async_trait::async_trait]
impl Executor for Probe {
    fn capabilities(&self) -> ExecutorCapabilities {
        ExecutorCapabilities::MANAGED
    }
    async fn execute(
        &self,
        i: Invocation,
        t: Arc<dyn ToolBridge>,
        _: CancellationToken,
    ) -> ExecutionOutcome {
        self.seen
            .lock()
            .unwrap()
            .push((i.execution_id.clone(), i.artifacts.clone()));
        for r in &i.artifacts {
            let value = t.call("artifact.read", json!({"id":r.id})).await.unwrap();
            assert_eq!(value["artifact"]["id"], r.id);
        }
        let foreign = self.foreign.lock().unwrap().clone();
        if let Some((owner, id)) = foreign
            && owner != i.execution_id
        {
            assert_eq!(
                t.call("artifact.read", json!({"id":id}))
                    .await
                    .unwrap_err()
                    .code,
                "capability_denied"
            );
        }
        assert_eq!(
            t.call("artifact.read", json!({"id":"undeclared"}))
                .await
                .unwrap_err()
                .code,
            "capability_denied"
        );
        t.call(
            "artifact.write",
            json!({"slot":"result","value":{"nonce":i.inputs["nonce"],"visit":i.visit}}),
        )
        .await
        .unwrap();
        ExecutionOutcome::Completed {
            usage: json!({"model_calls":0}),
        }
    }
    async fn stop_or_confirm(&self, _: &str) -> StopState {
        StopState::Confirmed
    }
}
#[tokio::test]
async fn missing_optional_history_then_latest_and_all_stay_scoped() {
    for selection in ["latest", "all"] {
        let mut f = Fixture::new(true);
        f.definition = load(&f, true, selection);
        let p = Arc::new(Probe::default());
        let mut b = f.bindings("pass");
        b.executors.insert("test.executor".into(), p.clone());
        let rt = Runtime::open(
            RuntimeOptions::new(f.root.path().join("store")),
            vec![f.definition.clone()],
            b,
        )
        .await
        .unwrap();
        let id = rt
            .start(f.request("first", "A", false), &Caller::local())
            .await
            .unwrap();
        let mut s = wait_for(&rt, &id, |s| s.status == Status::Waiting).await;
        let reference = s
            .accepted_artifacts
            .iter()
            .find(|x| x.slot == "result")
            .unwrap()
            .id
            .clone();
        *p.foreign.lock().unwrap() = Some((id.clone(), reference));
        let other = rt
            .start(f.request("other", "B", false), &Caller::local())
            .await
            .unwrap();
        wait_for(&rt, &other, |s| s.status == Status::Waiting).await;
        for n in 2..=3 {
            rt.signal(
                &id,
                SignalRequest {
                    event_name: "resume".into(),
                    payload: json!({"confirm":true}),
                    request_key: format!("visit-{n}"),
                    expected_revision: s.revision,
                },
                &Caller::local(),
            )
            .await
            .unwrap();
            let previous = s.revision;
            s = wait_for(&rt, &id, |x| {
                x.status == Status::Waiting && x.revision > previous
            })
            .await;
        }
        rt.shutdown().await.unwrap();
        let seen = p.seen.lock().unwrap();
        let own: Vec<_> = seen.iter().filter(|x| x.0 == id).collect();
        assert_eq!(own.len(), 3);
        assert!(own[0].1.is_empty());
        assert_eq!(own[1].1.len(), 1);
        assert_eq!(own[2].1.len(), if selection == "all" { 2 } else { 1 });
        assert_eq!(own[2].1.iter().map(|r| r.visit).max(), Some(2));
        if selection == "all" {
            assert!(own[2].1.iter().any(|r| r.visit == 1));
        }
    }
}
#[tokio::test]
async fn required_history_is_still_required_before_executor() {
    let mut f = Fixture::new(true);
    f.definition = load(&f, false, "latest");
    let p = Arc::new(Probe::default());
    let mut b = f.bindings("pass");
    b.executors.insert("test.executor".into(), p.clone());
    let rt = Runtime::open(
        RuntimeOptions::new(f.root.path().join("store")),
        vec![f.definition.clone()],
        b,
    )
    .await
    .unwrap();
    let id = rt
        .start(f.request("missing", "A", false), &Caller::local())
        .await
        .unwrap();
    let s = wait_for(&rt, &id, |s| {
        s.status.terminal() || s.status == Status::Blocked
    })
    .await;
    assert_eq!(s.error.unwrap().code, "input_missing");
    assert!(p.seen.lock().unwrap().is_empty());
    rt.shutdown().await.unwrap();
}
#[test]
fn false_preserves_old_digest_and_optional_terminal_is_rejected() {
    let f = Fixture::new(false);
    let p = f.root.path().join("flow.json");
    let mut b: Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
    let skills = BTreeMap::from([("demo-skill@0.1.0".into(), f.root.path().join("demo-skill"))]);
    b["flow"]["states"]["done"]["requires"][0]["optional"] = json!(false);
    std::fs::write(&p, serde_json::to_vec(&b).unwrap()).unwrap();
    assert_eq!(
        Definition::load(&p, &skills).unwrap().digest,
        f.definition.digest
    );
    b["flow"]["states"]["done"]["requires"][0]["optional"] = json!(true);
    std::fs::write(&p, serde_json::to_vec(&b).unwrap()).unwrap();
    assert_eq!(
        Definition::load(&p, &skills).unwrap_err().code,
        "definition_invalid"
    );
}
#[test]
fn malformed_optional_declarations_do_not_weaken_definition_validation() {
    for dependency in [
        json!({"state":"work","slot":"result","selection":"latest","optional":null}),
        json!({"state":"work","slot":"missing","selection":"latest","optional":true}),
        json!({"state":"missing","slot":"result","selection":"latest","optional":true}),
        json!({"state":"work","slot":"result","selection":"invalid","optional":true}),
    ] {
        let f = Fixture::new(false);
        let p = f.root.path().join("flow.json");
        let mut b: Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        b["entries"]["test.work"]["requires"] = json!([dependency]);
        std::fs::write(&p, serde_json::to_vec(&b).unwrap()).unwrap();
        assert!(
            Definition::load(
                &p,
                &BTreeMap::from([("demo-skill@0.1.0".into(), f.root.path().join("demo-skill"))])
            )
            .is_err()
        );
    }
}
