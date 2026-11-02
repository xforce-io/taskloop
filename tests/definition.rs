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
