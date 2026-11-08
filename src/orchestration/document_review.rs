//! The bundled compiled application factory. It reads named resources, never a CapabilityPackage.
use super::*;
use crate::definition::OutputSlot;
use crate::*;
use std::{collections::BTreeMap, path::Path};

pub fn document_review_registry(
    root: &Path,
    executor: &str,
    budgets: &BTreeMap<String, RunBudgetPolicy>,
) -> Result<OrchestrationRegistry> {
    let names = ["planner@1", "quality@1", "review@1"];
    if budgets.keys().map(String::as_str).ne(names) {
        return Err(Error::new(
            "input_invalid",
            "/entry_budgets",
            "document-review requires exactly planner@1, quality@1 and review@1 policies",
        ));
    }
    let mut registry = OrchestrationRegistry::new();
    for name in [
        "goal-input",
        "goal-text",
        "text",
        "review-input",
        "report",
        "planner-input",
    ] {
        registry.register_schema_file(name, root, &format!("schemas/{name}.json"))?;
    }
    registry.register_skill_root("document-review", &root.join("skills/document-review"))?;
    let rules_digest =
        registry.register_rules_file("document-quality", root, "rules/document-quality.json")?;
    for (id, input, output, schema, instruction, skills) in [
        (
            "planner@1",
            "planner-input",
            "proposal",
            "_tl.planner-result@1",
            "根据冻结目录、Profile和完整检查要求生成PlanProposal。必须回显goal_spec_digest和profile_digest，遵守planning_feedback，不增加目录外能力、权限或检查。用artifact.write向proposal槽提交PlannerResult信封。",
            vec![],
        ),
        (
            "review@1",
            "review-input",
            "report",
            "report",
            "按document-review方法审阅document，回答question，区分原文依据和判断。用artifact.write向report槽提交报告。",
            vec!["document-review".into()],
        ),
        (
            "quality@1",
            "_tl.check-input@1",
            "evaluation",
            "_tl.evaluation-candidate@1",
            "严格按CheckInput中的冻结规则核对目标报告，逐条覆盖规则并引用授权evidence_refs。仅用artifact.write向evaluation槽返回Evaluation候选，不生成请求身份。",
            vec![],
        ),
    ] {
        registry.register_entry(
            id,
            EntryContract {
                instruction: instruction.into(),
                executor: executor.into(),
                skills,
                tools: vec!["artifact.write".into()],
                input_schema: input.into(),
                requires: vec![],
                outputs: BTreeMap::from([(
                    output.into(),
                    OutputSlot {
                        schema: schema.into(),
                        required: true,
                    },
                )]),
                attachments: None,
                gate: "_tl.schema@1".into(),
                outcomes: vec!["completed".into()],
            },
            budgets[id].clone(),
        )?;
    }
    registry.register_capability(NodeContract {
        protocol_version: 1,
        id: "review".into(),
        version: "1".into(),
        kind: NodeKind::Work,
        inputs: BTreeMap::from([
            (
                "question".into(),
                NodeInputPort {
                    schema_ref: "goal-text".into(),
                    required: true,
                    source_policy: InputSourcePolicy::Goal {
                        pointer: "/goal".into(),
                    },
                    default: None,
                },
            ),
            (
                "document".into(),
                NodeInputPort {
                    schema_ref: "text".into(),
                    required: true,
                    source_policy: InputSourcePolicy::Goal {
                        pointer: "/inputs/document_text".into(),
                    },
                    default: None,
                },
            ),
        ]),
        outputs: BTreeMap::from([(
            "report".into(),
            NodeOutputPort {
                schema_ref: "report".into(),
            },
        )]),
        gate: "_tl.schema@1".into(),
        max_rule_check_ms: 1000,
        members: BTreeMap::from([(
            "reviewer".into(),
            NodeMember::NodePorts {
                entry_ref: "review@1".into(),
                input_bindings: BTreeMap::from([
                    (
                        "question".into(),
                        MemberInputBinding {
                            port: "question".into(),
                            pointer: "".into(),
                        },
                    ),
                    (
                        "document".into(),
                        MemberInputBinding {
                            port: "document".into(),
                            pointer: "".into(),
                        },
                    ),
                ]),
            },
        )]),
        join: Some(NodeJoin {
            kind: "object-by-worker".into(),
            outputs: BTreeMap::from([(
                "report".into(),
                OutputProjection::Member {
                    member_id: "reviewer".into(),
                    slot: "report".into(),
                },
            )]),
        }),
        signal_schema_ref: None,
        max_wait_ms: None,
        evaluation: None,
    })?;
    registry.register_checker(
        "quality@1",
        CheckerContract {
            entry_ref: "quality@1".into(),
            rules_ref: "document-quality".into(),
            result_slot: "evaluation".into(),
            fresh_context: true,
            goal_input_fields: vec!["document_text".into()],
        },
    )?;
    registry.register_profile(OrchestrationProfile {
        id: "document-review".into(),
        version: "1".into(),
        goal_input_schema: "goal-input".into(),
        goal_selectors: BTreeMap::from([
            ("/goal".into(), "goal-text".into()),
            ("/inputs/document_text".into(), "text".into()),
        ]),
        artifact_selectors: BTreeMap::new(),
        planner_entry_ref: "planner@1".into(),
        capabilities: vec!["review@1".into()],
        planning_input_schemas: vec![],
        templates: vec![],
        outputs: BTreeMap::from([(
            "report".into(),
            ProfileOutput {
                schema_ref: "report".into(),
                allowed_capabilities: vec!["review@1".into()],
            },
        )]),
        final_outputs: vec!["report".into()],
        checks: vec![CheckRequirement {
            id: "quality".into(),
            phase: CheckPhase::GoalFinal,
            checker_ref: "quality@1".into(),
            rules_digest,
            target_outputs: vec!["report".into()],
            before_outputs: None,
            required: true,
        }],
        allowed_tools: vec!["artifact.write".into()],
    })?;
    Ok(registry)
}
