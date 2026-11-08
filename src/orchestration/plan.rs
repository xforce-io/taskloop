use super::{
    registry::{FrozenRegistry, escaped},
    schema::invalid,
    *,
};
use crate::{Error, Result, json::digest};
use petgraph::{algo::toposort, graph::DiGraph};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Write,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrchestrationOptions {
    pub grants: BTreeMap<String, GoalGrant>,
    pub limits: GoalLimits,
    pub max_admitted_goals: usize,
    pub max_spec_bytes: usize,
    pub max_plan_bytes: usize,
    pub max_nodes: usize,
    pub max_edges: usize,
    pub max_workers_per_node: usize,
    pub max_json_candidate_bytes: usize,
}
impl OrchestrationOptions {
    pub fn validate(&self) -> Result<()> {
        for (name, value, cap) in [
            ("max_spec_bytes", self.max_spec_bytes, 1_048_576),
            ("max_plan_bytes", self.max_plan_bytes, 4_194_304),
            ("max_nodes", self.max_nodes, 256),
            ("max_edges", self.max_edges, 4096),
            ("max_workers_per_node", self.max_workers_per_node, 32),
            (
                "max_json_candidate_bytes",
                self.max_json_candidate_bytes,
                4_194_304,
            ),
        ] {
            if value == 0 || value > cap {
                return Err(invalid(
                    &format!("/{name}"),
                    format!("must be in 1..={cap}"),
                ));
            }
        }
        if self.max_admitted_goals == 0 {
            return Err(invalid("/max_admitted_goals", "must be positive"));
        }
        validate_limits(&self.limits)?;
        for grant in self.grants.values() {
            validate_limits(&grant.limits)?;
        }
        Ok(())
    }
    pub(crate) fn effective_limits(
        &self,
        request: &GoalLimits,
        grant: &GoalGrant,
    ) -> Result<GoalLimits> {
        validate_limits(request)?;
        Ok(request.intersect(&grant.limits).intersect(&self.limits))
    }
}
pub(crate) fn validate_limits(limits: &GoalLimits) -> Result<()> {
    let value = serde_json::to_value(limits)?;
    for (key, value) in value.as_object().expect("limits object") {
        if value.as_u64().is_none_or(|v| v == 0 || v > i64::MAX as u64) {
            return Err(invalid(
                &format!("/limits/{key}"),
                "expected positive SQLite-safe integer",
            ));
        }
    }
    Ok(())
}
/// A serialization sink that rejects excess before allocating the excess bytes.
pub(crate) fn bounded_json<T: Serialize>(value: &T, max: usize) -> Result<Vec<u8>> {
    struct Buffer {
        bytes: Vec<u8>,
        max: usize,
    }
    impl Write for Buffer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > self.max.saturating_sub(self.bytes.len()) {
                return Err(std::io::Error::other("JSON byte limit exceeded"));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut buffer = Buffer {
        bytes: Vec::new(),
        max,
    };
    serde_json::to_writer(&mut buffer, value)
        .map_err(|_| invalid("/", "normalized JSON exceeds byte limit"))?;
    Ok(buffer.bytes)
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlanIssueCategory {
    Format,
    Cycle,
    Reference,
    InputConnection,
    CapabilityPermission,
    BudgetAcceptance,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanIssue {
    pub category: PlanIssueCategory,
    pub path: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual: Option<Value>,
}
#[derive(Debug, Clone, Serialize, Deserialize, thiserror::Error)]
#[error("plan_invalid: {issues:?}")]
pub struct PlanValidationFailure {
    pub code: String,
    pub issues: Vec<PlanIssue>,
    pub truncated: bool,
}
impl PlanValidationFailure {
    fn new() -> Self {
        Self {
            code: "plan_invalid".into(),
            issues: Vec::new(),
            truncated: false,
        }
    }
    fn push(
        &mut self,
        category: PlanIssueCategory,
        path: impl Into<String>,
        message: impl Into<String>,
    ) {
        if self.issues.len() >= 100 {
            self.truncated = true;
            return;
        }
        self.issues.push(PlanIssue {
            category,
            path: path.into(),
            message: message.into(),
            expected: None,
            actual: None,
        });
    }
    fn mismatch(
        &mut self,
        category: PlanIssueCategory,
        path: impl Into<String>,
        expected: impl Serialize,
        actual: impl Serialize,
    ) {
        let length = self.issues.len();
        self.push(category, path, "locked contract mismatch");
        if self.issues.len() > length {
            let last = self.issues.last_mut().expect("new issue");
            last.expected = serde_json::to_value(expected).ok();
            last.actual = serde_json::to_value(actual).ok();
        }
    }
    fn from_error(category: PlanIssueCategory, error: Error) -> Self {
        let mut failure = Self::new();
        failure.push(category, error.path, error.message);
        failure
    }
}
/// Validation context contains previously admitted facts; a validated plan is never execution authority.
#[derive(Debug, Clone)]
pub struct PlanValidationContext {
    pub spec_digest: String,
    pub profile_ref: String,
    pub planning: GoalPlanning,
    pub limits: GoalLimits,
    pub allowed_capabilities: Vec<String>,
    pub allowed_tools: Vec<String>,
    /// Exact admitted import identities to their locked schema resources.
    pub imports: BTreeMap<String, String>,
    pub runs_used: u64,
}
#[derive(Debug, Clone, Serialize)]
pub struct ValidatedPlan {
    pub(crate) proposal: PlanProposal,
    pub(crate) digest: String,
    pub(crate) topological_order: Vec<String>,
}
impl ValidatedPlan {
    pub fn proposal(&self) -> &PlanProposal {
        &self.proposal
    }
    pub fn digest(&self) -> &str {
        &self.digest
    }
    pub fn topological_order(&self) -> &[String] {
        &self.topological_order
    }
}
impl OrchestrationRegistry {
    pub fn validate_plan(
        &self,
        bytes: &[u8],
        context: &PlanValidationContext,
        options: &OrchestrationOptions,
    ) -> std::result::Result<ValidatedPlan, PlanValidationFailure> {
        let frozen = self.frozen().map_err(|e| {
            PlanValidationFailure::from_error(PlanIssueCategory::CapabilityPermission, e)
        })?;
        frozen.validate_plan(bytes, context, options)
    }
}
impl FrozenRegistry {
    pub(crate) fn validate_plan(
        &self,
        bytes: &[u8],
        context: &PlanValidationContext,
        options: &OrchestrationOptions,
    ) -> std::result::Result<ValidatedPlan, PlanValidationFailure> {
        use PlanIssueCategory::*;
        options
            .validate()
            .map_err(|e| PlanValidationFailure::from_error(BudgetAcceptance, e))?;
        if bytes.len() > options.max_plan_bytes {
            return Err(PlanValidationFailure::from_error(
                Format,
                invalid("/", "plan exceeds byte limit"),
            ));
        }
        let value =
            crate::parse_unique(bytes).map_err(|e| PlanValidationFailure::from_error(Format, e))?;
        self.schemas
            .contract("plan-proposal", &value)
            .map_err(|e| PlanValidationFailure::from_error(Format, e))?;
        let mut proposal: PlanProposal = serde_json::from_value(value)
            .map_err(|e| PlanValidationFailure::from_error(Format, e.into()))?;
        for node in proposal.nodes.values_mut() {
            node.depends_on.sort();
        }
        let mut failure = PlanValidationFailure::new();
        let Some(profile) = self.profiles.get(&context.profile_ref) else {
            return Err(PlanValidationFailure::from_error(
                CapabilityPermission,
                invalid("/profile_ref", "unknown Profile"),
            ));
        };
        if proposal.goal_spec_digest != context.spec_digest {
            failure.mismatch(
                BudgetAcceptance,
                "/goal_spec_digest",
                &context.spec_digest,
                &proposal.goal_spec_digest,
            );
        }
        if proposal.profile_digest != self.profile_digests[&context.profile_ref] {
            failure.mismatch(
                CapabilityPermission,
                "/profile_digest",
                &self.profile_digests[&context.profile_ref],
                &proposal.profile_digest,
            );
        }
        if proposal.planning_mode != context.planning.mode {
            failure.mismatch(
                BudgetAcceptance,
                "/planning_mode",
                &context.planning.mode,
                &proposal.planning_mode,
            );
        }
        if proposal.nodes.len() > options.max_nodes {
            failure.push(Format, "/nodes", "node capacity exceeded");
        }
        let mut graph = DiGraph::<String, ()>::new();
        let indexes = proposal
            .nodes
            .keys()
            .map(|id| (id.clone(), graph.add_node(id.clone())))
            .collect::<BTreeMap<_, _>>();
        let mut edge_count = 0usize;
        let mut min_runs = 0u64;
        for (id, node) in &proposal.nodes {
            let path = format!("/nodes/{}", escaped(id));
            for dependency in &node.depends_on {
                if let Some(source) = indexes.get(dependency) {
                    graph.add_edge(*source, indexes[id], ());
                    edge_count += 1;
                } else {
                    failure.push(
                        Reference,
                        format!("{path}/depends_on"),
                        format!("unknown node {dependency}"),
                    );
                }
            }
            let Some(contract) = self.capabilities.get(&node.capability_ref) else {
                failure.push(
                    Reference,
                    format!("{path}/capability_ref"),
                    "unknown capability",
                );
                continue;
            };
            if !profile.capabilities.contains(&node.capability_ref)
                || !context.allowed_capabilities.contains(&node.capability_ref)
            {
                failure.push(
                    CapabilityPermission,
                    format!("{path}/capability_ref"),
                    "capability outside Profile/Grant",
                );
            }
            if contract.members.len() > options.max_workers_per_node {
                failure.push(
                    Format,
                    format!("{path}/members"),
                    "member capacity exceeded",
                );
            }
            min_runs = min_runs.saturating_add(contract.members.len() as u64);
            for member in contract.members.values() {
                if self.entries[member.entry_ref()]
                    .contract
                    .tools
                    .iter()
                    .any(|t| !context.allowed_tools.contains(t))
                {
                    failure.push(
                        CapabilityPermission,
                        format!("{path}/capability_ref"),
                        "member tool outside GoalGrant",
                    );
                }
            }
            for (port, input) in &contract.inputs {
                if input.required && !node.inputs.contains_key(port) && input.default.is_none() {
                    failure.push(
                        InputConnection,
                        format!("{path}/inputs/{}", escaped(port)),
                        "required input is unbound",
                    );
                }
            }
            for (port, binding) in &node.inputs {
                let path = format!("{path}/inputs/{}", escaped(port));
                let Some(input) = contract.inputs.get(port) else {
                    failure.push(Reference, path, "unknown input port");
                    continue;
                };
                let source = match (&input.source_policy, binding) {
                    (
                        InputSourcePolicy::Goal { pointer: required },
                        PlanInput::Goal { pointer },
                    ) if required == pointer => profile.goal_selectors.get(pointer).cloned(),
                    (
                        InputSourcePolicy::Artifact,
                        PlanInput::Node {
                            node_id,
                            port,
                            pointer,
                        },
                    ) => {
                        if !node.depends_on.contains(node_id) {
                            failure.push(
                                InputConnection,
                                &path,
                                "node data source must be an explicit dependency",
                            );
                        }
                        let source = proposal
                            .nodes
                            .get(node_id)
                            .and_then(|n| self.capabilities.get(&n.capability_ref))
                            .and_then(|c| c.outputs.get(port));
                        match source {
                            Some(source) => self
                                .artifact_selector(profile, &source.schema_ref, pointer)
                                .ok(),
                            None => {
                                failure.push(Reference, &path, "unknown source node/public port");
                                None
                            }
                        }
                    }
                    (
                        InputSourcePolicy::Artifact,
                        PlanInput::Import {
                            artifact_id,
                            pointer,
                        },
                    ) => context
                        .imports
                        .get(artifact_id)
                        .and_then(|schema| self.artifact_selector(profile, schema, pointer).ok()),
                    (InputSourcePolicy::Constant, PlanInput::Literal { value }) => {
                        if let Err(error) = self.schemas.validate(&input.schema_ref, value) {
                            failure.push(InputConnection, &path, error.message);
                        }
                        Some(input.schema_ref.clone())
                    }
                    _ => {
                        failure.push(
                            InputConnection,
                            &path,
                            "input violates frozen source_policy",
                        );
                        None
                    }
                };
                match source {
                    Some(source) => {
                        if !self
                            .schemas
                            .same(&source, &input.schema_ref)
                            .unwrap_or(false)
                        {
                            failure.mismatch(
                                InputConnection,
                                path,
                                self.schemas
                                    .source(&input.schema_ref)
                                    .ok()
                                    .map(|s| s.digest.clone()),
                                self.schemas.source(&source).ok().map(|s| s.digest.clone()),
                            );
                        }
                    }
                    None => failure.push(InputConnection, path, "source or selector not admitted"),
                }
            }
        }
        if edge_count > options.max_edges {
            failure.push(Format, "/nodes", "edge capacity exceeded");
        }
        let topological = match toposort(&graph, None) {
            Ok(order) => order
                .into_iter()
                .map(|n| graph[n].clone())
                .collect::<Vec<_>>(),
            Err(node) => {
                failure.push(
                    Cycle,
                    format!("/nodes/{}/depends_on", escaped(&graph[node.node_id()])),
                    "dependency graph contains a cycle",
                );
                Vec::new()
            }
        };
        if proposal.outputs.keys().ne(profile.outputs.keys()) {
            failure.mismatch(
                BudgetAcceptance,
                "/outputs",
                profile.outputs.keys().collect::<Vec<_>>(),
                proposal.outputs.keys().collect::<Vec<_>>(),
            );
        }
        for (alias, output) in &proposal.outputs {
            let path = format!("/outputs/{}", escaped(alias));
            let Some(expected) = profile.outputs.get(alias) else {
                failure.push(Reference, path, "unknown output alias");
                continue;
            };
            let Some(node) = proposal.nodes.get(&output.node_id) else {
                failure.push(Reference, path, "unknown output producer");
                continue;
            };
            let actual = self
                .capabilities
                .get(&node.capability_ref)
                .and_then(|c| c.outputs.get(&output.port));
            if !expected.allowed_capabilities.contains(&node.capability_ref) {
                failure.push(
                    CapabilityPermission,
                    &path,
                    "output producer outside Profile output contract",
                );
            }
            match actual {
                None => failure.push(Reference, path, "unknown public output port"),
                Some(actual) => {
                    if !self
                        .schemas
                        .same(&expected.schema_ref, &actual.schema_ref)
                        .unwrap_or(false)
                    {
                        failure.push(InputConnection, path, "output schema digest mismatch");
                    }
                }
            }
        }
        let required = profile
            .checks
            .iter()
            .filter(|r| r.phase != CheckPhase::Plan)
            .map(|r| (&r.id, r))
            .collect::<BTreeMap<_, _>>();
        if proposal.checks.keys().ne(required.keys().copied()) {
            failure.mismatch(
                BudgetAcceptance,
                "/checks",
                required.keys().collect::<Vec<_>>(),
                proposal.checks.keys().collect::<Vec<_>>(),
            );
        }
        let mut bound_checkpoints = BTreeSet::new();
        for (id, check) in &proposal.checks {
            let path = format!("/checks/{}", escaped(id));
            let Some(requirement) = required.get(id) else {
                failure.push(Reference, path, "undeclared executable check");
                continue;
            };
            if check.phase != requirement.phase
                || check.checker_ref != requirement.checker_ref
                || check.targets.iter().collect::<BTreeSet<_>>()
                    != requirement.target_outputs.iter().collect::<BTreeSet<_>>()
            {
                failure.push(
                    Reference,
                    &path,
                    "check phase/checker/targets differ from Profile",
                );
            }
            if requirement.phase == CheckPhase::GoalFinal {
                min_runs = min_runs.saturating_add(1);
                if check.evaluation_node_id.is_some() {
                    failure.push(
                        Reference,
                        &path,
                        "goal_final cannot map to a DAG evaluation node",
                    );
                }
                let entry = &self.entries[&self.checkers[&requirement.checker_ref].entry_ref];
                if entry
                    .contract
                    .tools
                    .iter()
                    .any(|t| !context.allowed_tools.contains(t))
                {
                    failure.push(
                        CapabilityPermission,
                        &path,
                        "final checker tool outside GoalGrant",
                    );
                }
                continue;
            }
            let Some(node_id) = &check.evaluation_node_id else {
                failure.push(Reference, path, "checkpoint requires evaluation_node_id");
                continue;
            };
            if !bound_checkpoints.insert(node_id.clone()) {
                failure.push(
                    Reference,
                    &path,
                    "checkpoint evaluation mapped more than once",
                );
            }
            let Some(node) = proposal.nodes.get(node_id) else {
                failure.push(Reference, path, "unknown checkpoint node");
                continue;
            };
            let evaluation = self
                .capabilities
                .get(&node.capability_ref)
                .and_then(|n| n.evaluation.as_ref());
            let Some(evaluation) = evaluation.filter(|e| {
                e.purpose == EvaluationPurpose::Checkpoint
                    && e.checker_ref == requirement.checker_ref
            }) else {
                failure.push(
                    Reference,
                    path,
                    "checkpoint must use its exact checker evaluation capability",
                );
                continue;
            };
            if evaluation.target_ports.keys().collect::<BTreeSet<_>>()
                != requirement.target_outputs.iter().collect::<BTreeSet<_>>()
            {
                failure.push(Reference, &path, "evaluation target aliases mismatch");
            }
            self.validate_evaluation_bindings(&proposal, node, evaluation, &path, &mut failure);
            for alias in requirement.before_outputs.as_deref().unwrap_or_default() {
                if proposal
                    .outputs
                    .get(alias)
                    .and_then(|o| proposal.nodes.get(&o.node_id))
                    .is_none_or(|n| !n.depends_on.contains(node_id))
                {
                    failure.push(
                        InputConnection,
                        &path,
                        format!("protected output {alias} requires explicit checkpoint dependency"),
                    );
                }
            }
        }
        let checkpoints = proposal
            .nodes
            .iter()
            .filter_map(|(id, n)| {
                self.capabilities
                    .get(&n.capability_ref)
                    .and_then(|c| c.evaluation.as_ref())
                    .filter(|e| e.purpose == EvaluationPurpose::Checkpoint)
                    .map(|_| id.clone())
            })
            .collect::<BTreeSet<_>>();
        if checkpoints != bound_checkpoints {
            failure.push(
                Reference,
                "/checks",
                "all checkpoint nodes must map one-to-one to required checkpoints",
            );
        }
        for (id, node) in &proposal.nodes {
            if let Some(e) = self
                .capabilities
                .get(&node.capability_ref)
                .and_then(|c| c.evaluation.as_ref())
                .filter(|e| e.purpose == EvaluationPurpose::Data)
            {
                self.validate_evaluation_bindings(
                    &proposal,
                    node,
                    e,
                    &format!("/nodes/{}", escaped(id)),
                    &mut failure,
                );
            }
        }
        // Walk backwards from all declared outputs and mandatory checkpoint nodes.
        let mut pending = proposal
            .outputs
            .values()
            .map(|o| o.node_id.clone())
            .chain(bound_checkpoints)
            .collect::<BTreeSet<_>>();
        let mut relevant = BTreeSet::new();
        while let Some(id) = pending.pop_first() {
            if !relevant.insert(id.clone()) {
                continue;
            }
            if let Some(n) = proposal.nodes.get(&id) {
                pending.extend(n.depends_on.iter().cloned());
            }
        }
        for id in proposal.nodes.keys() {
            if !relevant.contains(id) {
                failure.push(
                    Reference,
                    format!("/nodes/{}", escaped(id)),
                    "orphan work does not contribute to an output/check",
                );
            }
        }
        if context
            .runs_used
            .checked_add(min_runs)
            .is_none_or(|n| n > context.limits.max_runs)
        {
            failure.mismatch(
                BudgetAcceptance,
                "/limits/max_runs",
                context.limits.max_runs,
                context.runs_used.saturating_add(min_runs),
            );
        }
        self.validate_template(&proposal, context, profile, &mut failure);
        if !failure.issues.is_empty() {
            return Err(failure);
        }
        Ok(ValidatedPlan {
            digest: digest(
                &bounded_json(&proposal, options.max_plan_bytes)
                    .map_err(|e| PlanValidationFailure::from_error(Format, e))?,
            ),
            proposal,
            topological_order: topological,
        })
    }
    fn artifact_selector(
        &self,
        profile: &OrchestrationProfile,
        source: &str,
        pointer: &str,
    ) -> Result<String> {
        if pointer.is_empty() {
            self.schemas.source(source)?;
            return Ok(source.to_string());
        }
        profile
            .artifact_selectors
            .get(source)
            .and_then(|s| s.get(pointer))
            .cloned()
            .ok_or_else(|| invalid("/pointer", "artifact selector not registered in Profile"))
    }
    fn validate_evaluation_bindings(
        &self,
        proposal: &PlanProposal,
        node: &PlanNode,
        evaluation: &NodeEvaluation,
        path: &str,
        failure: &mut PlanValidationFailure,
    ) {
        for (alias, port) in &evaluation.target_ports {
            let binding = node.inputs.get(port);
            let output = proposal.outputs.get(alias);
            if !matches!((binding,output),(Some(PlanInput::Node{node_id,port,pointer}),Some(output)) if node_id==&output.node_id&&port==&output.port&&pointer.is_empty()&&node.depends_on.contains(node_id))
            {
                failure.push(
                    PlanIssueCategory::InputConnection,
                    path,
                    format!(
                        "evaluation target {alias} must bind the exact aliased public artifact"
                    ),
                );
            }
        }
    }
    fn validate_template(
        &self,
        proposal: &PlanProposal,
        context: &PlanValidationContext,
        profile: &OrchestrationProfile,
        failure: &mut PlanValidationFailure,
    ) {
        use PlanIssueCategory::BudgetAcceptance;
        if context.planning.mode == PlanningMode::Autonomous {
            if proposal.template_digest.is_some() || context.planning.template_ref.is_some() {
                failure.push(
                    BudgetAcceptance,
                    "/template_digest",
                    "autonomous mode has no template",
                );
            }
            return;
        }
        let Some(id) = &context.planning.template_ref else {
            failure.push(
                BudgetAcceptance,
                "/template_ref",
                "workflow requires template",
            );
            return;
        };
        let Some(template) = self
            .templates
            .get(id)
            .filter(|_| profile.templates.contains(id))
        else {
            failure.push(
                BudgetAcceptance,
                "/template_ref",
                "template outside Profile",
            );
            return;
        };
        let expected = digest(&serde_json::to_vec(template).expect("serializable template"));
        if proposal.template_digest.as_deref() != Some(&expected) {
            failure.mismatch(
                BudgetAcceptance,
                "/template_digest",
                expected,
                &proposal.template_digest,
            );
        }
        if proposal.nodes.keys().ne(template.nodes.keys()) {
            failure.push(
                BudgetAcceptance,
                "/nodes",
                "workflow cannot add/remove skeleton nodes",
            );
        }
        for (id, fixed) in &template.nodes {
            let Some(actual) = proposal.nodes.get(id) else {
                continue;
            };
            if !fixed.capabilities.contains(&actual.capability_ref)
                || fixed.depends_on.iter().collect::<BTreeSet<_>>()
                    != actual.depends_on.iter().collect::<BTreeSet<_>>()
            {
                failure.push(
                    BudgetAcceptance,
                    format!("/nodes/{}", escaped(id)),
                    "workflow capability/dependencies differ from template",
                );
            }
            for (port, binding) in &fixed.inputs {
                if !fixed.fillable_inputs.contains(port)
                    && serde_json::to_value(actual.inputs.get(port)).ok()
                        != serde_json::to_value(Some(binding)).ok()
                {
                    failure.push(
                        BudgetAcceptance,
                        format!("/nodes/{}/inputs/{}", escaped(id), escaped(port)),
                        "template binding is immutable",
                    );
                }
            }
            for port in actual.inputs.keys() {
                if !fixed.inputs.contains_key(port) && !fixed.fillable_inputs.contains(port) {
                    failure.push(
                        BudgetAcceptance,
                        format!("/nodes/{}/inputs/{}", escaped(id), escaped(port)),
                        "field is not fillable in template",
                    );
                }
            }
        }
        if serde_json::to_value(&proposal.outputs).ok()
            != serde_json::to_value(&template.outputs).ok()
            || serde_json::to_value(&proposal.checks).ok()
                != serde_json::to_value(&template.checks).ok()
        {
            failure.push(
                BudgetAcceptance,
                "/outputs",
                "workflow outputs/check connections differ from template",
            );
        }
    }
}
