//! Opt-in cross-repository integration. Real Milkie processes, fake native model protocol.
mod common;
use common::*;
use serde_json::Value;
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use taskloop::*;
struct Observer {
    count: AtomicUsize,
    leaked: AtomicBool,
}
#[async_trait::async_trait]
impl MilkieObserver for Observer {
    fn options(&self, _: &Invocation) -> MilkieObservationOptions {
        MilkieObservationOptions {
            traceparent: "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01".into(),
            max_events: 1000,
            max_bytes: 262144,
        }
    }
    async fn event(&self, _: &Invocation, event: Value) -> Result<()> {
        if event.to_string().contains("CROSS-LANGUAGE") {
            self.leaked.store(true, Ordering::SeqCst);
        }
        self.count.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}
#[tokio::test]
#[ignore = "requires exact upstream Milkie CLI and Node paths; never a real model success test"]
async fn real_milkie_preserves_artifact_schema_and_stop_boundaries() {
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
    let observer = Arc::new(Observer {
        count: AtomicUsize::new(0),
        leaked: AtomicBool::new(false),
    });
    let executor = Arc::new(
        MilkieExecutor::new(config)
            .unwrap()
            .with_observer(observer.clone()),
    );
    let mut bindings = f.bindings("pass");
    bindings.executors.insert("test.executor".into(), executor);
    let rt = Runtime::open(
        RuntimeOptions::new(root.join("store")),
        vec![f.definition.clone()],
        bindings,
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
    assert!(observer.count.load(Ordering::SeqCst) > 3);
    assert!(!observer.leaked.load(Ordering::SeqCst));
    rt.shutdown().await.unwrap();
}
