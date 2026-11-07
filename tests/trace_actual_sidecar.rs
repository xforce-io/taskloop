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
        node.clone(),
        root.join("connection.json"),
        root.join("milkie"),
        MilkieStorage {
            config_dir: root.join("config"),
            session_dir: root.join("sessions"),
        },
    )
    .unwrap()
    .args([cli.clone().into_os_string()])
    .env("PATH", path);
    let executor = Arc::new(MilkieExecutor::new(config).unwrap());
    let mut bindings = f.bindings("pass");
    bindings
        .executors
        .insert("test.executor".into(), executor.clone());
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
            max_runs: 2,
            retention_ms: 8000,
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
    let running = rt
        .trace(
            &held,
            TraceQuery {
                limit: 1000,
                ..Default::default()
            },
            &Caller::local(),
        )
        .unwrap();
    let held_run = running
        .events
        .first()
        .expect("held phase must be observable")
        .event
        .run_id
        .clone();
    assert_eq!(
        executor.cleanup_observation(&held_run).await,
        ObservationCleanup::Deferred
    );
    assert_eq!(
        rt.get(&held, &Caller::local()).await.unwrap().status,
        Status::Running
    );
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
    // Two retained Runs fill max_runs. A third still succeeds without creating source observation.
    let third = rt
        .start(f.request("unobserved", "THIRD", false), &Caller::local())
        .await
        .unwrap();
    assert_eq!(
        wait_for(&rt, &third, |s| s.status.terminal()).await.status,
        Status::Completed
    );
    let suppressed = rt
        .trace(
            &third,
            TraceQuery {
                limit: 1000,
                ..Default::default()
            },
            &Caller::local(),
        )
        .unwrap();
    assert!(suppressed.events.is_empty());
    assert!(!suppressed.complete);
    assert!(suppressed.issues.iter().any(|i| i.starts_with("capacity:")));
    let mut cleaned = false;
    for _ in 0..500 {
        let a = rt
            .trace(
                &id,
                TraceQuery {
                    limit: 1000,
                    ..Default::default()
                },
                &Caller::local(),
            )
            .unwrap();
        let b = rt
            .trace(
                &held,
                TraceQuery {
                    limit: 1000,
                    ..Default::default()
                },
                &Caller::local(),
            )
            .unwrap();
        if a.source_cleanup.iter().any(|s| s.state == "purged")
            && b.source_cleanup.iter().any(|s| s.state == "purged")
        {
            cleaned = true;
            assert!(a.events.is_empty());
            assert!(b.events.is_empty());
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(cleaned, "source retention was not confirmed");
    assert_eq!(
        rt.read_trace_content(&id, &reference.id, &Caller::local())
            .unwrap_err()
            .code,
        "not_found"
    );
    assert!(
        rt.read_artifact(&s.accepted_artifacts[0].id, &Caller::local())
            .await
            .is_ok()
    );
    let mut purged = 0;
    let mut disabled = 0;
    for directory in std::fs::read_dir(root.join("milkie")).unwrap() {
        let directory = directory.unwrap().path();
        let owner: Value =
            serde_json::from_slice(&std::fs::read(directory.join("owner.json")).unwrap()).unwrap();
        // Cold read through Milkie's public SDK, not its internal SQLite/storage format.
        let output=tokio::process::Command::new(&node).arg("-e").arg("const {ExecutionClient}=require(process.argv[1]);const fs=require('node:fs');const c=new ExecutionClient({dataDir:process.argv[2],connection:JSON.parse(fs.readFileSync(process.argv[3],'utf8'))});console.log(JSON.stringify(c.observations(process.argv[4])));").arg("--").arg(cli.parent().unwrap().parent().unwrap().join("execution/ExecutionClient.js")).arg(directory.join("data")).arg(root.join("connection.json")).arg(owner["milkie_run_id"].as_str().unwrap()).output().await.unwrap();
        assert!(output.status.success(), "cold public SDK read failed");
        let observed: Value = serde_json::from_slice(&output.stdout).unwrap();
        if observed["status"]["fault"] == "expired" {
            purged += 1;
            assert!(observed["events"].as_array().unwrap().is_empty());
        } else {
            assert_eq!(observed["enabled"], false);
            disabled += 1;
        }
    }
    assert_eq!((purged, disabled), (2, 1));
    assert_eq!(
        std::fs::read(root.join("config/auth.json")).unwrap(),
        b"{\"fixture\":true}"
    );
    rt.shutdown().await.unwrap();
}
