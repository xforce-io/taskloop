mod common;
use std::collections::BTreeMap;
use taskloop::{Definition, parse_unique};
fn root() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("docs/design/2-minimal-runtime-contract/contract")
}
fn skills() -> BTreeMap<String, std::path::PathBuf> {
    BTreeMap::from([
        (
            "inquiry-deep-research@0.1.0".into(),
            root().join("skills/inquiry-deep-research"),
        ),
        (
            "document-review@0.1.0".into(),
            root().join("skills/document-review"),
        ),
    ])
}
#[test]
fn two_businesses_load_through_same_core() {
    for name in ["research.bundle.json", "document-review.bundle.json"] {
        let d = Definition::load(root().join(name), &skills()).unwrap();
        assert!(d.report().valid);
        assert_eq!(d.digest.len(), 64);
    }
}
#[test]
fn rejects_duplicate_keys_recursively() {
    assert!(parse_unique(br#"{"a":{"x":1,"x":2}}"#).is_err());
    assert!(parse_unique(br#"{"a":1} trailing"#).is_err());
}
#[test]
fn reports_unknown_state_without_executing() {
    let temp = tempfile::tempdir().unwrap();
    for e in std::fs::read_dir(root()).unwrap() {
        let e = e.unwrap();
        if e.path().extension().is_some_and(|x| x == "json") {
            std::fs::copy(e.path(), temp.path().join(e.file_name())).unwrap();
        }
    }
    let p = temp.path().join("research.bundle.json");
    let mut v: serde_json::Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
    v["flow"]["states"]["catalogue"]["accepted"]["need_more"] = serde_json::json!("missing_state");
    std::fs::write(&p, serde_json::to_vec(&v).unwrap()).unwrap();
    let e = Definition::load(&p, &skills()).unwrap_err();
    assert_eq!(e.code, "definition_invalid");
    assert!(e.path.contains("catalogue"));
    assert!(e.message.contains("missing_state"));
}
#[test]
fn locked_method_bytes_survive_source_change() {
    let temp = tempfile::tempdir().unwrap();
    let destination = temp.path().join("inquiry-deep-research");
    fn copy_tree(from: &std::path::Path, to: &std::path::Path) {
        std::fs::create_dir_all(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                copy_tree(&entry.path(), &to.join(entry.file_name()));
            } else {
                std::fs::copy(entry.path(), to.join(entry.file_name())).unwrap();
            }
        }
    }
    copy_tree(&root().join("skills/inquiry-deep-research"), &destination);
    let mut registered = skills();
    registered.insert("inquiry-deep-research@0.1.0".into(), destination.clone());
    let d = Definition::load(root().join("research.bundle.json"), &registered).unwrap();
    let original = d.skills["inquiry-deep-research@0.1.0"].files["SKILL.md"].clone();
    let mut changed = original.clone();
    changed.extend_from_slice(b"\nChanged after loading.\n");
    std::fs::write(destination.join("SKILL.md"), &changed).unwrap();
    let reloaded = Definition::load(root().join("research.bundle.json"), &registered).unwrap();
    assert_eq!(
        d.skills["inquiry-deep-research@0.1.0"].files["SKILL.md"],
        original
    );
    assert_eq!(
        reloaded.skills["inquiry-deep-research@0.1.0"].files["SKILL.md"],
        changed
    );
    assert_ne!(d.digest, reloaded.digest);
}
#[test]
fn rejects_missing_skill_registration() {
    let e = Definition::load(root().join("research.bundle.json"), &BTreeMap::new()).unwrap_err();
    assert_eq!(e.code, "binding_missing");
}

struct PausedExecutor;
#[async_trait::async_trait]
impl taskloop::Executor for PausedExecutor {
    fn capabilities(&self) -> taskloop::ExecutorCapabilities {
        taskloop::ExecutorCapabilities::MANAGED
    }
    async fn execute(
        &self,
        _: taskloop::Invocation,
        _: std::sync::Arc<dyn taskloop::ToolBridge>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> taskloop::ExecutionOutcome {
        cancel.cancelled().await;
        taskloop::ExecutionOutcome::Cancelled
    }
}
struct UnusedTool(String);
#[async_trait::async_trait]
impl taskloop::DomainTool for UnusedTool {
    fn spec(&self) -> taskloop::ToolSpec {
        taskloop::ToolSpec {
            id: self.0.clone(),
            description: "Unused read-only capability in definition-start test.".into(),
            input_schema: serde_json::json!({"type":"object"}),
        }
    }
    async fn call(
        &self,
        _: taskloop::ToolContext,
        _: serde_json::Value,
    ) -> taskloop::Result<taskloop::ToolOutput> {
        panic!("start-only test must not invoke domain capabilities")
    }
}
struct UnusedGate;
#[async_trait::async_trait]
impl taskloop::DomainGate for UnusedGate {
    async fn check(&self, _: taskloop::GateContext) -> taskloop::CheckResult {
        panic!("start-only test must not evaluate domain outputs")
    }
}
#[tokio::test]
async fn two_businesses_start_through_public_api_with_distinct_inputs() {
    use serde_json::json;
    use std::sync::Arc;
    use taskloop::*;
    let definitions = ["research.bundle.json", "document-review.bundle.json"]
        .map(|name| Definition::load(root().join(name), &skills()).unwrap());
    let mut bindings = HostBindings::default().configuration_version("two-domains-start@1");
    let mut tools = std::collections::BTreeSet::new();
    for d in &definitions {
        for e in d.bundle.entries.values() {
            bindings
                .executors
                .insert(e.executor.clone(), Arc::new(PausedExecutor));
            bindings.gates.insert(e.gate.clone(), Arc::new(UnusedGate));
            for name in &e.tools {
                tools.insert(name.clone());
                if !matches!(name.as_str(), "artifact.read" | "artifact.write") {
                    bindings
                        .tools
                        .insert(name.clone(), Arc::new(UnusedTool(name.clone())));
                }
            }
        }
    }
    bindings.grants.insert(
        "local-readonly".into(),
        Grant {
            definitions: definitions.iter().map(|d| d.digest.clone()).collect(),
            tools: tools.into_iter().collect(),
            limits: ResourceLimits::default(),
        },
    );
    let temp = tempfile::tempdir().unwrap();
    let rt = Runtime::open(
        RuntimeOptions::new(temp.path().join("store")),
        definitions.to_vec(),
        bindings,
    )
    .await
    .unwrap();
    let inputs = [
        json!({"question":"Architecture boundaries?","seed_urls":[],"scope":"public read-only"}),
        json!({"document_id":"doc-42","criteria":["clarity"]}),
    ];
    let mut ids = Vec::new();
    for (d, input) in definitions.iter().zip(inputs) {
        let id = rt
            .start(
                StartRequest {
                    definition_digest: d.digest.clone(),
                    inputs: input.clone(),
                    grant_ref: "local-readonly".into(),
                    request_key: d.bundle.flow.id.clone(),
                    limits: ResourceLimits::default(),
                },
                &Caller::local(),
            )
            .await
            .unwrap();
        let snapshot = rt.get(&id, &Caller::local()).await.unwrap();
        assert_eq!(snapshot.inputs, input);
        assert_eq!(snapshot.current_state, d.bundle.flow.initial);
        ids.push(id);
    }
    assert_ne!(ids[0], ids[1]);
    for id in ids {
        rt.cancel(&id, "cleanup", &Caller::local()).await.unwrap();
    }
    rt.shutdown().await.unwrap();
}

#[test]
fn invalid_output_slots_fail_static_loading() {
    for name in ["bad/slot".to_string(), String::new(), "a".repeat(161)] {
        let f = common::Fixture::new(false);
        let p = f.root.path().join("flow.json");
        let mut v: serde_json::Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        let slots = v["entries"]["test.work"]["outputs"]
            .as_object_mut()
            .unwrap();
        let output = slots.remove("result").unwrap();
        slots.insert(name, output);
        v["flow"]["states"]["done"]["requires"] = serde_json::json!([]);
        std::fs::write(&p, serde_json::to_vec(&v).unwrap()).unwrap();
        assert!(
            Definition::load(
                &p,
                &BTreeMap::from([("demo-skill@0.1.0".into(), f.root.path().join("demo-skill"))])
            )
            .is_err()
        );
    }
}
#[test]
fn definition_filename_without_parent_is_supported() {
    if let Some(root) =
        std::env::var_os("TASKLOOP_RELATIVE_DEFINITION_ROOT").map(std::path::PathBuf::from)
    {
        assert!(
            Definition::load(
                "flow.json",
                &BTreeMap::from([("demo-skill@0.1.0".into(), root.join("demo-skill"))])
            )
            .unwrap()
            .report()
            .valid
        );
        return;
    }
    let f = common::Fixture::new(false);
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "definition_filename_without_parent_is_supported",
            "--nocapture",
        ])
        .current_dir(f.root.path())
        .env("TASKLOOP_RELATIVE_DEFINITION_ROOT", f.root.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn fifo_paths_are_rejected_without_blocking_static_loading() {
    if let Some(root) = std::env::var_os("TASKLOOP_FIFO_ROOT").map(std::path::PathBuf::from) {
        let kind = std::env::var("TASKLOOP_FIFO_KIND").unwrap();
        if kind == "skill" {
            assert!(taskloop::validate_skill(root.join("demo-skill")).is_err());
        } else {
            let path = if kind == "root" {
                root.join("root-fifo/flow.json")
            } else {
                root.join("flow.json")
            };
            assert!(
                Definition::load(
                    path,
                    &BTreeMap::from([("demo-skill@0.1.0".into(), root.join("demo-skill"))])
                )
                .is_err()
            );
        }
        return;
    }
    use std::{
        os::unix::ffi::OsStrExt,
        time::{Duration, Instant},
    };
    let mut failures = Vec::new();
    for kind in ["definition", "schema", "root", "skill"] {
        let f = common::Fixture::new(false);
        let path = f.root.path().join(match kind {
            "definition" => "flow.json",
            "schema" => "input.schema.json",
            "root" => "root-fifo",
            _ => "demo-skill/pipe",
        });
        if path.exists() {
            std::fs::remove_file(&path).unwrap();
        }
        let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "fifo_paths_are_rejected_without_blocking_static_loading",
                "--nocapture",
            ])
            .env("TASKLOOP_FIFO_ROOT", f.root.path())
            .env("TASKLOOP_FIFO_KIND", kind)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                if !status.success() {
                    failures.push(format!("{kind}: FIFO was not rejected"));
                }
                break;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                failures.push(format!("{kind}: loader blocked on FIFO"));
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("; "));
}

#[test]
fn skill_validation_and_locking_must_use_matching_package_bytes() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    let f = common::Fixture::new(false);
    let package = f.root.path().join("demo-skill");
    let valid = "---\nname: demo-skill\ndescription: Atomic package validation.\nmetadata:\n  version: \"0.1.0\"\n---\nMethod A.\n";
    let other = valid.replace("0.1.0", "0.2.0");
    std::fs::write(package.join("SKILL.md"), valid).unwrap();
    std::fs::write(package.join("asset.bin"), vec![7; 256 * 1024]).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let writer_stop = stop.clone();
    let target = package.join("SKILL.md");
    let staging = f.root.path().join("replacement.md");
    let good = valid.to_string();
    let bad = other.clone();
    let writer = std::thread::spawn(move || {
        let mut alternate = false;
        while !writer_stop.load(Ordering::SeqCst) {
            std::fs::write(&staging, if alternate { &good } else { &bad }).unwrap();
            std::fs::rename(&staging, &target).unwrap();
            alternate = !alternate;
            std::thread::sleep(std::time::Duration::from_micros(200));
        }
    });
    let mut mismatched = false;
    for _ in 0..32 {
        if let Ok(d) = Definition::load(
            f.root.path().join("flow.json"),
            &BTreeMap::from([("demo-skill@0.1.0".into(), package.clone())]),
        ) {
            let locked = &d.skills["demo-skill@0.1.0"].files["SKILL.md"];
            if locked == other.as_bytes() {
                mismatched = true;
                break;
            }
        }
    }
    stop.store(true, Ordering::SeqCst);
    writer.join().unwrap();
    assert!(
        !mismatched,
        "accepted a 0.1.0 binding with locked Skill bytes declaring 0.2.0"
    );
}
