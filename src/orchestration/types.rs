use crate::{EntryContract, RunBudgetPolicy};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlanningMode {
    Autonomous,
    Workflow,
}
fn revisions() -> u64 {
    2
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalPlanning {
    pub mode: PlanningMode,
    #[serde(default = "revisions")]
    pub max_revisions: u64,
    pub max_planning_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template_ref: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GoalLimits {
    pub max_wall_ms: u64,
    pub max_active_ms: u64,
    pub max_runs: u64,
    pub max_transitions: u64,
    pub max_tool_calls: u64,
    pub max_total_candidate_bytes: u64,
    pub max_concurrent_runs: u64,
}
impl GoalLimits {
    pub(crate) fn intersect(&self, b: &Self) -> Self {
        Self {
            max_wall_ms: self.max_wall_ms.min(b.max_wall_ms),
            max_active_ms: self.max_active_ms.min(b.max_active_ms),
            max_runs: self.max_runs.min(b.max_runs),
            max_transitions: self.max_transitions.min(b.max_transitions),
            max_tool_calls: self.max_tool_calls.min(b.max_tool_calls),
            max_total_candidate_bytes: self
                .max_total_candidate_bytes
                .min(b.max_total_candidate_bytes),
            max_concurrent_runs: self.max_concurrent_runs.min(b.max_concurrent_runs),
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalImport {
    pub artifact_id: String,
    pub sha256: String,
    pub schema_digest: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalSpec {
    pub goal: String,
    pub inputs: Value,
    pub profile_ref: String,
    pub planning: GoalPlanning,
    pub limits: GoalLimits,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_goal_id: Option<String>,
    #[serde(default)]
    pub imports: Vec<GoalImport>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalRequest {
    pub protocol_version: u32,
    pub request_key: String,
    pub grant_ref: String,
    pub spec: GoalSpec,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalGrant {
    pub profile_digests: Vec<String>,
    pub capabilities: Vec<String>,
    pub tools: Vec<String>,
    pub limits: GoalLimits,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum InputSourcePolicy {
    Goal { pointer: String },
    Artifact,
    Constant,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeInputPort {
    pub schema_ref: String,
    pub required: bool,
    pub source_policy: InputSourcePolicy,
    #[serde(
        default,
        deserialize_with = "some_json",
        skip_serializing_if = "Option::is_none"
    )]
    pub default: Option<Value>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeOutputPort {
    pub schema_ref: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemberInputBinding {
    pub port: String,
    pub pointer: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "input_mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum NodeMember {
    NodePorts {
        entry_ref: String,
        input_bindings: BTreeMap<String, MemberInputBinding>,
    },
    CheckRequest {
        entry_ref: String,
    },
}
impl NodeMember {
    pub fn entry_ref(&self) -> &str {
        match self {
            Self::NodePorts { entry_ref, .. } | Self::CheckRequest { entry_ref } => entry_ref,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum OutputProjection {
    Member { member_id: String, slot: String },
    Members { slots: BTreeMap<String, String> },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeJoin {
    pub kind: String,
    pub outputs: BTreeMap<String, OutputProjection>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EvaluationPurpose {
    Checkpoint,
    Data,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeEvaluation {
    pub purpose: EvaluationPurpose,
    pub checker_ref: String,
    pub target_ports: BTreeMap<String, String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    Work,
    Input,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeContract {
    pub protocol_version: u32,
    pub id: String,
    pub version: String,
    pub kind: NodeKind,
    pub inputs: BTreeMap<String, NodeInputPort>,
    pub outputs: BTreeMap<String, NodeOutputPort>,
    pub gate: String,
    pub max_rule_check_ms: u64,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub members: BTreeMap<String, NodeMember>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub join: Option<NodeJoin>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal_schema_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_wait_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evaluation: Option<NodeEvaluation>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileOutput {
    pub schema_ref: String,
    pub allowed_capabilities: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CheckPhase {
    Plan,
    NodeCheckpoint,
    GoalFinal,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckRequirement {
    pub id: String,
    pub phase: CheckPhase,
    pub checker_ref: String,
    pub rules_digest: String,
    pub target_outputs: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_outputs: Option<Vec<String>>,
    pub required: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrchestrationProfile {
    pub id: String,
    pub version: String,
    pub goal_input_schema: String,
    pub goal_selectors: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub artifact_selectors: BTreeMap<String, BTreeMap<String, String>>,
    pub planner_entry_ref: String,
    pub capabilities: Vec<String>,
    pub planning_input_schemas: Vec<String>,
    pub templates: Vec<String>,
    pub outputs: BTreeMap<String, ProfileOutput>,
    pub final_outputs: Vec<String>,
    pub checks: Vec<CheckRequirement>,
    pub allowed_tools: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckerContract {
    pub entry_ref: String,
    pub rules_ref: String,
    pub result_slot: String,
    pub fresh_context: bool,
    pub goal_input_fields: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub id: String,
    pub instruction: String,
    pub pass_condition: String,
    pub evidence_required: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleSet {
    pub protocol_version: u32,
    pub id: String,
    pub version: String,
    pub rules: Vec<Rule>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PlanInput {
    Goal {
        pointer: String,
    },
    Node {
        node_id: String,
        port: String,
        pointer: String,
    },
    Import {
        artifact_id: String,
        pointer: String,
    },
    Literal {
        value: Value,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanNode {
    pub capability_ref: String,
    pub depends_on: Vec<String>,
    pub inputs: BTreeMap<String, PlanInput>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanOutput {
    pub node_id: String,
    pub port: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanCheck {
    pub phase: CheckPhase,
    pub checker_ref: String,
    pub targets: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evaluation_node_id: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanProposal {
    pub protocol_version: u32,
    pub goal_spec_digest: String,
    pub profile_digest: String,
    pub planning_mode: PlanningMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template_digest: Option<String>,
    pub nodes: BTreeMap<String, PlanNode>,
    pub outputs: BTreeMap<String, PlanOutput>,
    pub checks: BTreeMap<String, PlanCheck>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PlannerResult {
    Proposal {
        protocol_version: u32,
        proposal: PlanProposal,
    },
    NeedsInput {
        protocol_version: u32,
        input_schema_ref: String,
        reason: String,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EvaluationVerdict {
    Pass,
    Fail,
    Error,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationItem {
    pub id: String,
    pub verdict: EvaluationVerdict,
    pub message: String,
    pub evidence_refs: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationViolation {
    pub code: String,
    pub subject: String,
    pub message: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationCandidate {
    pub protocol_version: u32,
    pub verdict: EvaluationVerdict,
    pub results: Vec<EvaluationItem>,
    pub evidence_refs: Vec<String>,
    pub violations: Vec<EvaluationViolation>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegisteredEntry {
    pub contract: EntryContract,
    pub budget: RunBudgetPolicy,
}

fn some_json<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<Option<Value>, D::Error> {
    Value::deserialize(d).map(Some)
}
