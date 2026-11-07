//! Opt-in cross-repository integration. Real Milkie processes, fake native model protocol.
mod common;
use common::*;
use serde_json::Value;
use std::collections::BTreeMap;
use std::{path::PathBuf, sync::Arc, time::Duration};
use taskloop::*;
struct SelectedFilter;
#[async_trait::async_trait]
impl TraceContentFilter for SelectedFilter {
    async fn redact(&self, _: &str, _: &str, selected: Value) -> Result<Value> {
        Ok(selected)
    }
}
#[tokio::test]
#[ignore = "requires exact upstream Milkie CLI and Node paths; never a real model success test"]
async fn real_milkie_trace_public_query_and_cancel() {
    let node = PathBuf::from(
        std::env::var_os("MILKIE_NODE").expect("MILKIE_NODE must be an absolute Node path"),
    );
    let cli = PathBuf::from(
        std::env::var_os("MILKIE_CLI")
            .expect("MILKIE_CLI must be the fixed upstream dist/cli/index.js"),
    );
    let f = Fixture::new(false);
    let root = f.root.path();
    for d in ["config", "sessions", "bin", "milkie"] {
        std::fs::create_dir(root.join(d)).unwrap();
    }
    std::fs::write(root.join("config/auth.json"), b"{\"fixture\":true}").unwrap();
    let fixture =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/agent_cli_workflow.cjs");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fixture, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink(&fixture, root.join("bin/pi")).unwrap();
    }
    std::fs::write(root.join("connection.json"),br#"{"contractVersion":1,"fields":{"transport":"agent-cli","runtime":"pi","model":"fixture"}}"#).unwrap();
    let path = std::env::join_paths(
        [root.join("bin"), node.parent().unwrap().to_path_buf()]
            .into_iter()
            .chain(std::env::split_paths(
                &std::env::var_os("PATH").unwrap_or_default(),
            )),
    )
    .unwrap();
    let config = MilkieConfig::new(
        node,
        root.join("connection.json"),
        root.join("milkie"),
        MilkieStorage {
            config_dir: root.join("config"),
            session_dir: root.join("sessions"),
        },
    )
    .unwrap()
    .args([cli.into_os_string()])
    .env("PATH", path);
    let executor = Arc::new(MilkieExecutor::new(config).unwrap());
    let mut bindings = f.bindings("pass");
    bindings.executors.insert("test.executor".into(), executor);
    let rt = Runtime::open_with_trace_policy(
        RuntimeOptions::new(root.join("store")),
        vec![f.definition.clone()],
        bindings,
        TraceOptions {
            max_events_per_run: 2000,
            max_bytes_per_run: 1048576,
            source_max_events: 1000,
            source_max_bytes: 262144,
        },
        TracePolicy {
            policy_id: "cross-language-v1".into(),
            max_total_events: 10000,
            max_total_bytes: 4194304,
            max_runs: 16,
            retention_ms: 60000,
            max_content_bytes: 4096,
            content_tools: BTreeMap::from([(
                "artifact.write".into(),
                TraceToolContent {
                    input: vec!["/value".into()],
                    output: vec![],
                },
            )]),
            filter_version: Some("selected-fixture-v1".into()),
            filter: Some(Arc::new(SelectedFilter)),
        },
    )
    .await
    .unwrap();
    let id = rt
        .start(
            f.request("accepted", "CROSS-LANGUAGE", false),
            &Caller::local(),
        )
        .await
        .unwrap();
    let mut result = None;
    for _ in 0..300 {
        let snapshot = rt.get(&id, &Caller::local()).await.unwrap();
        if snapshot.status.terminal() {
            result = Some(snapshot);
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    let s = result.expect("real Milkie did not finish");
    assert_eq!(s.status, Status::Completed, "{s:?}");
    let bytes = rt
        .read_artifact(&s.accepted_artifacts[0].id, &Caller::local())
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&bytes.bytes).unwrap()["nonce"],
        "CROSS-LANGUAGE"
    );
    let held = rt
        .start(f.request("cancelled", "SECOND", true), &Caller::local())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(700)).await;
    rt.cancel(&held, "stop-held", &Caller::local())
        .await
        .unwrap();
    let stopped = wait_for(&rt, &held, |s| s.status.terminal()).await;
    assert_eq!(stopped.status, Status::Cancelled);
    assert!(stopped.accepted_artifacts.is_empty());
    tokio::time::sleep(Duration::from_millis(100)).await;
    let page = rt
        .trace(
            &id,
            TraceQuery {
                limit: 1000,
                ..Default::default()
            },
            &Caller::local(),
        )
        .unwrap();
    assert!(page.complete, "{page:#?}");
    for kind in [
        "phase.started",
        "execution.started",
        "tool.started",
        "check.started",
        "check.finished",
        "execution.finished",
        "phase.finished",
    ] {
        assert!(
            page.events.iter().any(|e| e.event.kind == kind),
            "missing {kind}"
        );
    }
    let tool = page
        .events
        .iter()
        .find(|e| e.event.kind == "tool.started")
        .unwrap();
    assert!(page.events.iter().any(|e| e.event.kind == "tool.requested"
        && Some(&e.event.span_id) == tool.event.parent_span_id.as_ref()));
    let text = String::from_utf8(
        rt.export_trace(
            &id,
            TraceQuery {
                limit: 1000,
                ..Default::default()
            },
            &Caller::local(),
        )
        .unwrap(),
    )
    .unwrap();
    assert!(!text.contains("CROSS-LANGUAGE"));
    assert!(
        page.events
            .iter()
            .filter(|e| e.event.source_id.starts_with("milkie:"))
            .all(|e| e.event.milkie_run_id.is_some() && e.event.milkie_context_id.is_some())
    );
    let reference: TraceContentRef =
        serde_json::from_value(tool.event.data["input_content"].clone()).unwrap();
    let content = rt
        .read_trace_content(&id, &reference.id, &Caller::local())
        .unwrap();
    assert_eq!(content.value["/value"]["nonce"], "CROSS-LANGUAGE");
    assert_eq!(
        rt.read_trace_content(&held, &reference.id, &Caller::local())
            .unwrap_err()
            .code,
        "not_found"
    );
    let cancelled = rt
        .trace(
            &held,
            TraceQuery {
                limit: 1000,
                ..Default::default()
            },
            &Caller::local(),
        )
        .unwrap();
    assert!(
        cancelled
            .events
            .iter()
            .any(|e| e.event.kind == "execution.finished" && e.event.data["status"] == "cancelled")
    );
    rt.shutdown().await.unwrap();
}
