use super::*;
use crate::{
    ExecutionOutcome, Executor, ExecutorCapabilities, HostBindings, Invocation, ToolBridge,
};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path, sync::Arc};
use tokio_util::sync::CancellationToken;

struct NeverExecute;
struct UnboundedExecutor;
#[async_trait]
impl Executor for UnboundedExecutor {
    fn capabilities(&self) -> ExecutorCapabilities {
        ExecutorCapabilities::MANAGED
    }
    async fn execute(
        &self,
        _: Invocation,
        _: Arc<dyn ToolBridge>,
        _: CancellationToken,
    ) -> ExecutionOutcome {
        panic!("unsafe adapters must be rejected before execution")
    }
}
struct CoreToolOverride;
#[async_trait]
impl crate::DomainTool for CoreToolOverride {
    fn spec(&self) -> crate::ToolSpec {
        crate::ToolSpec {
            id: "artifact.write".into(),
            description: "forbidden override".into(),
            input_schema: json!({}),
        }
    }
    async fn call(&self, _: crate::ToolContext, _: Value) -> crate::Result<crate::ToolOutput> {
        panic!("Core tool overrides must never run")
    }
}
struct CoreGateOverride;
#[async_trait]
impl crate::DomainGate for CoreGateOverride {
    async fn check(&self, _: crate::GateContext) -> crate::CheckResult {
        panic!("Core gate overrides must never run")
    }
}
#[test]
fn registration_rejects_unbounded_adapters_and_core_tool_or_gate_overrides() {
    let (registry, bindings) = fixture();
    let mut unbounded = bindings.clone();
    unbounded
        .executors
        .insert("milkie".into(), Arc::new(UnboundedExecutor));
    assert!(
        registry
            .clone()
            .validate_and_freeze(&unbounded)
            .unwrap_err()
            .message
            .contains("before decoding")
    );
    let tools = bindings.clone().tool(Arc::new(CoreToolOverride));
    assert!(
        registry
            .clone()
            .validate_and_freeze(&tools)
            .unwrap_err()
            .message
            .contains("cannot be overridden")
    );
    let gates = bindings.gate("_tl.schema@1", Arc::new(CoreGateOverride));
    assert!(
        registry
            .clone()
            .validate_and_freeze(&gates)
            .unwrap_err()
            .message
            .contains("cannot be overridden")
    );
}
#[async_trait]
impl Executor for NeverExecute {
    fn enforces_json_candidate_limit(&self) -> bool {
        true
    }
    fn capabilities(&self) -> ExecutorCapabilities {
        ExecutorCapabilities::MANAGED
    }
    async fn execute(
        &self,
        _: Invocation,
        _: Arc<dyn ToolBridge>,
        _: CancellationToken,
    ) -> ExecutionOutcome {
        panic!("registration/validation must never execute business work")
    }
}
pub(crate) fn fixture() -> (OrchestrationRegistry, HostBindings) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("docs/design/10-goal-dag-orchestration/contract/document-review-package");
    let package: Value =
        serde_json::from_slice(&std::fs::read(root.join("package.json")).unwrap()).unwrap();
    let mut registry = OrchestrationRegistry::new();
    for (id, resource) in package["schemas"].as_object().unwrap() {
        registry
            .register_schema(
                id,
                &std::fs::read(root.join(resource["path"].as_str().unwrap())).unwrap(),
            )
            .unwrap();
    }
    for (id, resource) in package["rule_sets"].as_object().unwrap() {
        registry
            .register_rules(
                id,
                &std::fs::read(root.join(resource["path"].as_str().unwrap())).unwrap(),
            )
            .unwrap();
    }
    for (id, resource) in package["skills"].as_object().unwrap() {
        let path = root.join(resource["path"].as_str().unwrap());
        let mut files = BTreeMap::new();
        crate::definition::collect_skill(&path, &path, &mut files).unwrap();
        registry
            .register_skill(
                id,
                crate::SkillPackage {
                    version: "1".into(),
                    sha256: crate::json::digest(&serde_json::to_vec(&files).unwrap()),
                    files,
                },
            )
            .unwrap();
    }
    for (id, entry) in package["entries"].as_object().unwrap() {
        registry
            .register_entry(
                id,
                serde_json::from_value(entry.clone()).unwrap(),
                serde_json::from_value(package["entry_budgets"][id].clone()).unwrap(),
            )
            .unwrap();
    }
    for value in package["capabilities"].as_object().unwrap().values() {
        registry
            .register_capability(serde_json::from_value(value.clone()).unwrap())
            .unwrap();
    }
    for (id, value) in package["checkers"].as_object().unwrap() {
        registry
            .register_checker(id, serde_json::from_value(value.clone()).unwrap())
            .unwrap();
    }
    for value in package["profiles"].as_object().unwrap().values() {
        registry
            .register_profile(serde_json::from_value(value.clone()).unwrap())
            .unwrap();
    }
    (
        registry,
        HostBindings::default().executor("milkie", Arc::new(NeverExecute)),
    )
}
#[test]
fn compiled_document_factory_registers_without_any_package_manifest_loader() {
    let (source, bindings) = fixture();
    let budgets = source
        .entries
        .iter()
        .map(|(id, entry)| (id.clone(), entry.budget.clone()))
        .collect();
    let original = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("docs/design/10-goal-dag-orchestration/contract/document-review-package");
    let directory = tempfile::tempdir().unwrap();
    for relative in [
        "schemas/goal-input.json",
        "schemas/goal-text.json",
        "schemas/text.json",
        "schemas/review-input.json",
        "schemas/report.json",
        "schemas/planner-input.json",
        "rules/document-quality.json",
        "skills/document-review/SKILL.md",
    ] {
        let target = directory.path().join(relative);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::copy(original.join(relative), target).unwrap();
    }
    assert!(!directory.path().join("package.json").exists());
    let mut registry = document_review_registry(directory.path(), "milkie", &budgets).unwrap();
    let receipt = registry.validate_and_freeze(&bindings).unwrap();
    assert_eq!(receipt.capabilities, vec!["review@1"]);
    assert!(receipt.profiles.contains_key("document-review@1"));
}
#[test]
fn example_registration_is_atomic_sealed_and_does_not_execute() {
    let (mut registry, bindings) = fixture();
    let receipt = registry.validate_and_freeze(&bindings).unwrap();
    assert_eq!(receipt.capabilities, vec!["review@1"]);
    assert_eq!(receipt.profiles["document-review@1"].len(), 64);
    assert_eq!(
        registry
            .validate_and_freeze(&bindings)
            .unwrap()
            .registry_digest,
        receipt.registry_digest
    );
    assert!(registry.register_schema("later", b"true").is_err());
    assert!(
        registry
            .validate_and_freeze(&bindings.clone().configuration_version("changed"))
            .is_err()
    );
}
#[test]
fn registry_refuses_invalid_member_checker_and_tool_contracts() {
    let (registry, bindings) = fixture();
    let mut changed = registry.clone();
    changed
        .entries
        .get_mut("review@1")
        .unwrap()
        .contract
        .tools
        .clear();
    assert!(changed.validate_and_freeze(&bindings).is_err());
    assert!(changed.frozen().is_err());
    let mut changed = registry.clone();
    let NodeMember::NodePorts { input_bindings, .. } = changed
        .capabilities
        .get_mut("review@1")
        .unwrap()
        .members
        .get_mut("reviewer")
        .unwrap()
    else {
        panic!()
    };
    input_bindings.remove("document");
    assert!(changed.validate_and_freeze(&bindings).is_err());
    let mut changed = registry.clone();
    changed
        .checkers
        .get_mut("quality@1")
        .unwrap()
        .goal_input_fields
        .push("secret".into());
    assert!(changed.validate_and_freeze(&bindings).is_err());
    let mut changed = registry.clone();
    changed
        .profiles
        .get_mut("document-review@1")
        .unwrap()
        .checks
        .clear();
    assert!(changed.validate_and_freeze(&bindings).is_err());
    let mut changed = registry;
    changed
        .profiles
        .get_mut("document-review@1")
        .unwrap()
        .goal_selectors
        .insert("/goal".into(), "text".into());
    assert!(changed.validate_and_freeze(&bindings).is_err());
}
#[test]
fn unrelated_resources_do_not_change_profile_digest_but_used_bytes_do() {
    let (registry, bindings) = fixture();
    let mut a = registry.clone();
    a.validate_and_freeze(&bindings).unwrap();
    let mut b = registry.clone();
    b.register_schema("unused", b"true").unwrap();
    b.validate_and_freeze(&bindings).unwrap();
    assert_eq!(
        a.profile_digest("document-review@1").unwrap(),
        b.profile_digest("document-review@1").unwrap()
    );
    assert_ne!(a.frozen().unwrap().digest, b.frozen().unwrap().digest);
    let mut c = registry;
    c.schemas.insert(
        "text".into(),
        super::schema::SchemaSource::new(
            b"{\"type\":\"string\",\"minLength\":1,\"maxLength\":64000}\n ",
        )
        .unwrap(),
    );
    c.validate_and_freeze(&bindings).unwrap();
    assert_ne!(
        a.profile_digest("document-review@1").unwrap(),
        c.profile_digest("document-review@1").unwrap()
    );
}
#[test]
fn schema_scope_is_preserved_and_data_keys_are_not_interpreted() {
    use super::schema::{SchemaSource, Schemas};
    let mut sources = BTreeMap::new();
    for (id, value) in [
        (
            "a",
            json!({"type":"object","required":["x"],"properties":{"x":{"type":"object","properties":{"v":{"$ref":"#/$defs/value"}}}},"$defs":{"value":{"type":"string"}}}),
        ),
        (
            "b",
            json!({"type":"object","properties":{"v":{"$ref":"#/$defs/value"}} ,"$defs":{"value":{"type":"integer"}}}),
        ),
        (
            "data",
            json!({"const":{"$id":"data-id","$ref":"https://must-not-be-fetched.invalid/","items":{"$dynamicRef":"not-a-schema"}}}),
        ),
    ] {
        sources.insert(
            id.into(),
            SchemaSource::new(&serde_json::to_vec(&value).unwrap()).unwrap(),
        );
    }
    let schemas = Schemas::compile(&sources).unwrap();
    assert!(!schemas.selector_matches("a", "/x", "b").unwrap());
    schemas.validate("data",&json!({"$id":"data-id","$ref":"https://must-not-be-fetched.invalid/","items":{"$dynamicRef":"not-a-schema"}})).unwrap();
    assert!(schemas.selected("a", "/missing").is_err());
    assert!(schemas.selected("a", "/x/v").is_err()); // v is optional
    sources.insert(
        "unsafe".into(),
        SchemaSource::new(br#"{"$ref":"https://outside.invalid/schema"}"#).unwrap(),
    );
    assert!(Schemas::compile(&sources).is_err());
}
#[test]
fn duplicate_json_and_explicit_null_defaults_are_not_lost() {
    let mut registry = OrchestrationRegistry::new();
    assert!(
        registry
            .register_schema("bad", br#"{"type":"string","type":"integer"}"#)
            .is_err()
    );
    let port:NodeInputPort=serde_json::from_value(json!({"schema_ref":"nullable","required":false,"source_policy":{"kind":"constant"},"default":null})).unwrap();
    assert_eq!(port.default, Some(Value::Null));
    assert!(registry.register_schema("_tl.fake@1", b"true").is_err());
}

fn options() -> OrchestrationOptions {
    OrchestrationOptions {
        grants: BTreeMap::new(),
        limits: GoalLimits {
            max_wall_ms: 1_000_000,
            max_active_ms: 5_000_000,
            max_runs: 20,
            max_transitions: 1000,
            max_tool_calls: 200,
            max_total_candidate_bytes: 10_000_000,
            max_concurrent_runs: 4,
        },
        max_admitted_goals: 8,
        max_spec_bytes: 1_048_576,
        max_plan_bytes: 4_194_304,
        max_nodes: 256,
        max_edges: 4096,
        max_workers_per_node: 32,
        max_json_candidate_bytes: 4_194_304,
    }
}
fn context(registry: &OrchestrationRegistry) -> PlanValidationContext {
    PlanValidationContext {
        spec_digest: "0".repeat(64),
        profile_ref: "document-review@1".into(),
        planning: GoalPlanning {
            mode: PlanningMode::Autonomous,
            max_revisions: 2,
            max_planning_ms: 200_000,
            template_ref: None,
        },
        limits: options().limits,
        allowed_capabilities: registry
            .frozen()
            .unwrap()
            .capabilities
            .keys()
            .cloned()
            .collect(),
        allowed_tools: vec!["artifact.write".into()],
        imports: BTreeMap::new(),
        runs_used: 1,
    }
}
fn example_plan(registry: &OrchestrationRegistry) -> Value {
    let mut value:Value=serde_json::from_str(include_str!("../../docs/design/10-goal-dag-orchestration/contract/document-review-package/plan.example.json")).unwrap();
    value["profile_digest"] = registry.profile_digest("document-review@1").unwrap().into();
    value
}
#[test]
fn valid_plan_is_checked_without_any_execution() {
    let (mut registry, bindings) = fixture();
    registry.validate_and_freeze(&bindings).unwrap();
    let result = registry
        .validate_plan(
            &serde_json::to_vec(&example_plan(&registry)).unwrap(),
            &context(&registry),
            &options(),
        )
        .unwrap();
    assert_eq!(result.topological_order(), &["review"]);
    assert_eq!(result.digest().len(), 64);
}
#[test]
fn malformed_cyclic_or_unauthorized_plans_are_rejected_with_paths() {
    let (mut registry, bindings) = fixture();
    registry.validate_and_freeze(&bindings).unwrap();
    let plan = example_plan(&registry);
    let context = context(&registry);
    let cases: [(fn(&mut Value), PlanIssueCategory); 10] = [
        (
            |p| p["nodes"]["review"]["depends_on"] = json!(["review"]),
            PlanIssueCategory::Cycle,
        ),
        (
            |p| p["nodes"]["review"]["depends_on"] = json!(["missing"]),
            PlanIssueCategory::Reference,
        ),
        (
            |p| p["nodes"]["review"]["capability_ref"] = "unregistered@1".into(),
            PlanIssueCategory::Reference,
        ),
        (
            |p| {
                p["nodes"]["review"]["inputs"]
                    .as_object_mut()
                    .unwrap()
                    .remove("question");
            },
            PlanIssueCategory::InputConnection,
        ),
        (
            |p| {
                p["nodes"]["review"]["inputs"]["question"] =
                    json!({"kind":"literal","value":"question"})
            },
            PlanIssueCategory::InputConnection,
        ),
        (
            |p| p["nodes"]["review"]["inputs"]["question"]["pointer"] = "/inputs/secret".into(),
            PlanIssueCategory::InputConnection,
        ),
        (
            |p| p["outputs"]["report"]["port"] = "missing".into(),
            PlanIssueCategory::Reference,
        ),
        (|p| p["checks"] = json!({}), PlanIssueCategory::Format),
        (
            |p| p["checks"]["quality"]["checker_ref"] = "other@1".into(),
            PlanIssueCategory::Reference,
        ),
        (
            |p| p["goal_spec_digest"] = "1".repeat(64).into(),
            PlanIssueCategory::BudgetAcceptance,
        ),
    ];
    for (mutate, category) in cases {
        let mut changed = plan.clone();
        mutate(&mut changed);
        let failure = registry
            .validate_plan(&serde_json::to_vec(&changed).unwrap(), &context, &options())
            .unwrap_err();
        assert!(
            failure.issues.iter().any(|i| i.category == category),
            "{failure:?}"
        );
        assert!(failure.issues.iter().all(|i| i.path.starts_with('/')));
    }
    let mut denied = context.clone();
    denied.allowed_tools.clear();
    assert!(
        registry
            .validate_plan(&serde_json::to_vec(&plan).unwrap(), &denied, &options())
            .unwrap_err()
            .issues
            .iter()
            .any(|i| i.category == PlanIssueCategory::CapabilityPermission)
    );
    denied = context.clone();
    denied.limits.max_runs = 2;
    assert!(
        registry
            .validate_plan(&serde_json::to_vec(&plan).unwrap(), &denied, &options())
            .unwrap_err()
            .issues
            .iter()
            .any(|i| i.category == PlanIssueCategory::BudgetAcceptance)
    );
    let mut orphan = plan.clone();
    orphan["nodes"]["orphan"] = orphan["nodes"]["review"].clone();
    assert!(
        registry
            .validate_plan(&serde_json::to_vec(&orphan).unwrap(), &context, &options())
            .unwrap_err()
            .issues
            .iter()
            .any(|i| i.message.contains("orphan"))
    );
    let bad = br#"{"protocol_version":1,"protocol_version":1}"#;
    assert_eq!(
        registry
            .validate_plan(bad, &context, &options())
            .unwrap_err()
            .issues[0]
            .category,
        PlanIssueCategory::Format
    );
}
fn checkpoint_fixture() -> (OrchestrationRegistry, HostBindings) {
    let (mut registry, bindings) = fixture();
    let capability:NodeContract=serde_json::from_value(json!({"protocol_version":1,"id":"checkpoint","version":"1","kind":"work","inputs":{"report":{"schema_ref":"report","required":true,"source_policy":{"kind":"artifact"}}},"outputs":{"evaluation":{"schema_ref":"_tl.evaluation-candidate@1"}},"gate":"_tl.schema@1","max_rule_check_ms":1000,"members":{"checker":{"entry_ref":"quality@1","input_mode":"check_request"}},"join":{"kind":"object-by-worker","outputs":{"evaluation":{"kind":"member","member_id":"checker","slot":"evaluation"}}},"evaluation":{"purpose":"checkpoint","checker_ref":"quality@1","target_ports":{"draft":"report"}}})).unwrap();
    registry.register_capability(capability).unwrap();
    let profile = registry.profiles.get_mut("document-review@1").unwrap();
    profile.capabilities.push("checkpoint@1".into());
    profile
        .outputs
        .insert("draft".into(), profile.outputs["report"].clone());
    let mut check = profile.checks[0].clone();
    check.id = "draft-check".into();
    check.phase = CheckPhase::NodeCheckpoint;
    check.target_outputs = vec!["draft".into()];
    check.before_outputs = Some(vec!["report".into()]);
    profile.checks.push(check);
    (registry, bindings)
}
fn checkpoint_plan(registry: &OrchestrationRegistry) -> Value {
    let mut plan = example_plan(registry);
    plan["nodes"]["draft"] = plan["nodes"]["review"].clone();
    plan["nodes"]["check"] = json!({"capability_ref":"checkpoint@1","depends_on":["draft"],"inputs":{"report":{"kind":"node","node_id":"draft","port":"report","pointer":""}}});
    plan["nodes"]["review"]["depends_on"] = json!(["check"]);
    plan["outputs"]["draft"] = json!({"node_id":"draft","port":"report"});
    plan["checks"]["draft-check"] = json!({"phase":"node_checkpoint","checker_ref":"quality@1","targets":["draft"],"evaluation_node_id":"check"});
    plan
}
#[test]
fn checkpoint_bindings_are_exact_bijective_and_protect_downstream_outputs() {
    let (mut registry, bindings) = checkpoint_fixture();
    registry.validate_and_freeze(&bindings).unwrap();
    let plan = checkpoint_plan(&registry);
    let context = context(&registry);
    registry
        .validate_plan(&serde_json::to_vec(&plan).unwrap(), &context, &options())
        .unwrap();
    let mut missing = plan.clone();
    missing["nodes"]["review"]["depends_on"] = json!([]);
    assert!(
        registry
            .validate_plan(&serde_json::to_vec(&missing).unwrap(), &context, &options())
            .unwrap_err()
            .issues
            .iter()
            .any(|i| i.message.contains("protected output"))
    );
    let mut unbound = plan.clone();
    unbound["checks"]["draft-check"]["evaluation_node_id"] = "review".into();
    assert!(
        registry
            .validate_plan(&serde_json::to_vec(&unbound).unwrap(), &context, &options())
            .is_err()
    );
    let mut wrong_target = plan.clone();
    wrong_target["nodes"]["check"]["inputs"]["report"]["node_id"] = "review".into();
    assert!(
        registry
            .validate_plan(
                &serde_json::to_vec(&wrong_target).unwrap(),
                &context,
                &options()
            )
            .is_err()
    );
    let mut extra = plan;
    extra["nodes"]["extra-check"] = extra["nodes"]["check"].clone();
    extra["nodes"]["review"]["depends_on"] = json!(["check", "extra-check"]);
    assert!(
        registry
            .validate_plan(&serde_json::to_vec(&extra).unwrap(), &context, &options())
            .unwrap_err()
            .issues
            .iter()
            .any(|i| i.message.contains("one-to-one"))
    );
}
#[test]
fn workflow_cannot_change_immutable_skeleton_or_connections() {
    let (mut registry, bindings) = fixture();
    let mut raw:Value=serde_json::from_str(include_str!("../../docs/design/10-goal-dag-orchestration/contract/document-review-package/plan.example.json")).unwrap();
    let plan: PlanProposal = serde_json::from_value(raw.clone()).unwrap();
    let template = WorkflowTemplate {
        id: "fixed-review".into(),
        version: "1".into(),
        nodes: BTreeMap::from([(
            "review".into(),
            TemplateNode {
                capabilities: vec!["review@1".into()],
                depends_on: vec![],
                inputs: plan.nodes["review"].inputs.clone(),
                fillable_inputs: vec![],
            },
        )]),
        outputs: plan.outputs,
        checks: plan.checks,
    };
    let template_digest = crate::json::digest(&serde_json::to_vec(&template).unwrap());
    registry.register_template(template).unwrap();
    registry
        .profiles
        .get_mut("document-review@1")
        .unwrap()
        .templates
        .push("fixed-review@1".into());
    registry.validate_and_freeze(&bindings).unwrap();
    let mut context = context(&registry);
    context.planning.mode = PlanningMode::Workflow;
    context.planning.template_ref = Some("fixed-review@1".into());
    raw["profile_digest"] = registry.profile_digest("document-review@1").unwrap().into();
    raw["planning_mode"] = "workflow".into();
    raw["template_digest"] = template_digest.into();
    registry
        .validate_plan(&serde_json::to_vec(&raw).unwrap(), &context, &options())
        .unwrap();
    raw["nodes"]["extra"] = raw["nodes"]["review"].clone();
    raw["nodes"]["review"]["depends_on"] = json!(["extra"]);
    assert!(
        registry
            .validate_plan(&serde_json::to_vec(&raw).unwrap(), &context, &options())
            .unwrap_err()
            .issues
            .iter()
            .any(|i| i.message.contains("skeleton"))
    );
}

#[test]
fn goal_request_authorization_normalization_and_deadlines_are_bounded() {
    let (mut registry, mut bindings) = fixture();
    registry.validate_and_freeze(&bindings).unwrap();
    let mut options = options();
    options.grants.insert(
        "goal-user".into(),
        GoalGrant {
            profile_digests: vec![registry.profile_digest("document-review@1").unwrap()],
            capabilities: vec!["review@1".into()],
            tools: vec!["artifact.write".into()],
            limits: options.limits.clone(),
        },
    );
    bindings.grants.insert(
        "goal-user".into(),
        crate::Grant {
            definitions: vec![],
            tools: vec!["artifact.write".into()],
            limits: crate::ResourceLimits {
                max_active_ms: 5_000_000,
                max_wall_ms: 1_000_000,
                max_transitions: 1000,
                max_runs: 20,
            },
        },
    );
    let caller = crate::Caller {
        principal: "alice".into(),
        grants: vec!["goal-user".into()],
    };
    let raw = json!({"protocol_version":1,"request_key":"request-1","grant_ref":"goal-user","spec":{"goal":"Review the document","inputs":{"document_text":"The document"},"profile_ref":"document-review@1","planning":{"mode":"autonomous","max_planning_ms":100_000},"limits":options.limits}});
    let frozen = registry.frozen().unwrap();
    let prepared = frozen
        .prepare_goal(
            &serde_json::to_vec(&raw).unwrap(),
            &caller,
            &bindings,
            &options,
            1000,
        )
        .unwrap();
    assert_eq!(prepared.request.spec.planning.max_revisions, 2);
    assert_eq!(prepared.wall_deadline_utc_ms, 1_001_000);
    assert_eq!(prepared.planning_deadline_utc_ms, 101_000);
    let mut explicit = raw.clone();
    explicit["spec"]["planning"]["max_revisions"] = 2.into();
    assert_eq!(
        prepared.request_fingerprint,
        frozen
            .prepare_goal(
                &serde_json::to_vec(&explicit).unwrap(),
                &caller,
                &bindings,
                &options,
                2000
            )
            .unwrap()
            .request_fingerprint
    );
    assert!(
        frozen
            .prepare_goal(
                &serde_json::to_vec(&raw).unwrap(),
                &crate::Caller {
                    principal: "mallory".into(),
                    grants: vec![]
                },
                &bindings,
                &options,
                1000
            )
            .is_err()
    );
    assert!(
        frozen
            .prepare_goal(
                &serde_json::to_vec(&raw).unwrap(),
                &caller,
                &bindings,
                &options,
                i64::MAX as u64
            )
            .is_err()
    );
    let mut small = options.clone();
    small.max_spec_bytes = 10;
    assert!(
        frozen
            .prepare_goal(
                &serde_json::to_vec(&raw).unwrap(),
                &caller,
                &bindings,
                &small,
                1000
            )
            .is_err()
    );
    let mut wrong = raw;
    wrong["spec"]["inputs"] = json!({"document_text":42});
    assert!(
        frozen
            .prepare_goal(
                &serde_json::to_vec(&wrong).unwrap(),
                &caller,
                &bindings,
                &options,
                1000
            )
            .is_err()
    );
}
#[test]
fn one_evaluation_path_checks_rules_evidence_and_verdict_then_runtime_issues_identity() {
    let (mut registry, bindings) = fixture();
    registry.validate_and_freeze(&bindings).unwrap();
    let f = registry.frozen().unwrap();
    let evidence = std::collections::BTreeSet::from(["artifact-1".to_string()]);
    let subject = CheckSubject::Artifacts {
        plan_digest: "0".repeat(64),
        targets: BTreeMap::from([(
            "report".into(),
            CheckTarget {
                artifact_id: "artifact-1".into(),
                sha256: "1".repeat(64),
                schema_digest: "2".repeat(64),
                value: json!({"summary":"reviewed"}),
            },
        )]),
    };
    let request = f
        .issue_check(
            "goal-example",
            "Review",
            &"0".repeat(64),
            &json!({"document_text":"The document","secret":"not visible"}),
            "quality",
            CheckInputPhase::GoalFinal,
            "quality@1",
            3,
            8,
            subject,
            evidence.clone(),
        )
        .unwrap();
    assert!(!request.input.goal_inputs.contains_key("secret"));
    let candidate = json!({"protocol_version":1,"verdict":"pass","results":request.input.rules.rules.iter().map(|r|json!({"id":r.id,"verdict":"pass","message":"verified","evidence_refs":["artifact-1"]})).collect::<Vec<_>>(),"evidence_refs":["artifact-1"],"violations":[]});
    let checked = f
        .check_evaluation(
            &request,
            &serde_json::to_vec(&candidate).unwrap(),
            4096,
            &evidence,
        )
        .unwrap();
    let receipt = request.receipt(checked, 9);
    assert_eq!(receipt.evaluation_id, request.input.nonce);
    assert_eq!(receipt.accepted_revision, 9);
    assert_eq!(receipt.input_digest, request.input_digest);
    for mutate in [
        |c: &mut Value| c["results"][0]["id"] = "invented".into(),
        |c: &mut Value| c["results"][0]["evidence_refs"] = json!([]),
        |c: &mut Value| c["results"][0]["evidence_refs"] = json!(["secret"]),
        |c: &mut Value| c["results"][0]["verdict"] = "fail".into(),
        |c: &mut Value| c["nonce"] = "forged".into(),
    ] {
        let mut bad = candidate.clone();
        mutate(&mut bad);
        assert!(
            f.check_evaluation(
                &request,
                &serde_json::to_vec(&bad).unwrap(),
                4096,
                &evidence
            )
            .is_err()
        );
    }
    assert!(
        f.check_evaluation(
            &request,
            &serde_json::to_vec(&candidate).unwrap(),
            4096,
            &Default::default()
        )
        .is_err()
    );
    assert!(
        f.check_evaluation(
            &request,
            &serde_json::to_vec(&candidate).unwrap(),
            5,
            &evidence
        )
        .is_err()
    );
    let mut duplicate = candidate.clone();
    let result = duplicate["results"][0].clone();
    duplicate["results"].as_array_mut().unwrap().push(result);
    assert!(
        f.check_evaluation(
            &request,
            &serde_json::to_vec(&duplicate).unwrap(),
            4096,
            &evidence
        )
        .is_err()
    );
}
#[cfg(unix)]
#[test]
fn file_resource_registration_reuses_no_escape_and_no_symlink_reads() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("schema.json"), b"true").unwrap();
    symlink(dir.path().join("schema.json"), dir.path().join("link.json")).unwrap();
    let mut registry = OrchestrationRegistry::new();
    registry
        .register_schema_file("safe", dir.path(), "schema.json")
        .unwrap();
    assert!(
        registry
            .register_schema_file("escape", dir.path(), "../schema.json")
            .is_err()
    );
    assert!(
        registry
            .register_schema_file("link", dir.path(), "link.json")
            .is_err()
    );
}

pub(crate) fn prepared_fixture() -> super::admission::PreparedGoal {
    let (mut registry, mut bindings) = fixture();
    bindings.grants.insert(
        "test".into(),
        crate::Grant {
            definitions: vec![],
            tools: vec!["artifact.write".into()],
            limits: crate::ResourceLimits {
                max_active_ms: 5_000_000,
                max_wall_ms: 1_000_000,
                max_runs: 20,
                max_transitions: 1000,
            },
        },
    );
    registry.validate_and_freeze(&bindings).unwrap();
    let mut options = options();
    options.grants.insert(
        "test".into(),
        GoalGrant {
            profile_digests: vec![registry.profile_digest("document-review@1").unwrap()],
            capabilities: vec!["review@1".into()],
            tools: vec!["artifact.write".into()],
            limits: options.limits.clone(),
        },
    );
    let raw = json!({"protocol_version":1,"request_key":"new-goal","grant_ref":"test","spec":{"goal":"Review","inputs":{"document_text":"Document"},"profile_ref":"document-review@1","planning":{"mode":"autonomous","max_planning_ms":100_000},"limits":options.limits}});
    registry
        .frozen()
        .unwrap()
        .prepare_goal(
            &serde_json::to_vec(&raw).unwrap(),
            &crate::Caller {
                principal: "alice".into(),
                grants: vec!["test".into()],
            },
            &bindings,
            &options,
            crate::runtime::now_ms(),
        )
        .unwrap()
}

#[test]
fn executor_schema_bundle_is_closed_and_preserves_reference_scope() {
    use super::schema::{SchemaSource, Schemas};
    let sources=BTreeMap::from([
        ("root".into(),SchemaSource::new(br##"{"type":"object","required":["value"],"properties":{"value":{"$ref":"leaf#/$defs/value"}},"additionalProperties":false}"##).unwrap()),
        ("leaf".into(),SchemaSource::new(br#"{"$defs":{"value":{"type":"string","const":"locked"}}}"#).unwrap()),
    ]);
    let schemas = Schemas::compile(&sources).unwrap();
    let bundle = schemas.bundle("root").unwrap();
    let validator = jsonschema::options()
        .with_draft(jsonschema::Draft::Draft202012)
        .build(&bundle)
        .unwrap();
    assert!(validator.is_valid(&json!({"value":"locked"})));
    assert!(!validator.is_valid(&json!({"value":42})));
    assert!(!validator.is_valid(&json!({"value":"different"})));
    let planner = schemas.bundle("_tl.planner-result@1").unwrap();
    assert!(
        jsonschema::options()
            .with_draft(jsonschema::Draft::Draft202012)
            .build(&planner)
            .is_ok()
    );
}
