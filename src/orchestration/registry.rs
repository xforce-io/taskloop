use super::{
    schema::{SchemaSource, Schemas, invalid},
    *,
};
use crate::{EntryContract, HostBindings, Result, RunBudgetPolicy, SkillPackage, json::digest};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TemplateNode {
    pub capabilities: Vec<String>,
    pub depends_on: Vec<String>,
    pub inputs: BTreeMap<String, PlanInput>,
    pub fillable_inputs: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowTemplate {
    pub id: String,
    pub version: String,
    pub nodes: BTreeMap<String, TemplateNode>,
    pub outputs: BTreeMap<String, PlanOutput>,
    pub checks: BTreeMap<String, PlanCheck>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistrationReceipt {
    pub registry_digest: String,
    pub profiles: BTreeMap<String, String>,
    pub capabilities: Vec<String>,
}
#[derive(Clone)]
pub(crate) struct LockedRules {
    pub value: RuleSet,
    pub digest: String,
}
#[derive(Default, Clone)]
pub struct OrchestrationRegistry {
    pub(super) schemas: BTreeMap<String, SchemaSource>,
    skills: BTreeMap<String, SkillPackage>,
    pub(super) entries: BTreeMap<String, RegisteredEntry>,
    pub(super) capabilities: BTreeMap<String, NodeContract>,
    pub(super) profiles: BTreeMap<String, OrchestrationProfile>,
    pub(super) checkers: BTreeMap<String, CheckerContract>,
    rules: BTreeMap<String, LockedRules>,
    templates: BTreeMap<String, WorkflowTemplate>,
    frozen: Option<Arc<FrozenRegistry>>,
}
pub(crate) struct FrozenRegistry {
    pub schemas: Arc<Schemas>,
    pub skills: BTreeMap<String, SkillPackage>,
    pub entries: BTreeMap<String, RegisteredEntry>,
    pub capabilities: BTreeMap<String, NodeContract>,
    pub profiles: BTreeMap<String, OrchestrationProfile>,
    pub checkers: BTreeMap<String, CheckerContract>,
    pub rules: BTreeMap<String, LockedRules>,
    pub templates: BTreeMap<String, WorkflowTemplate>,
    pub profile_digests: BTreeMap<String, String>,
    pub bindings_digest: String,
    pub digest: String,
}
impl OrchestrationRegistry {
    pub fn new() -> Self {
        Self::default()
    }
    fn writable(&self) -> Result<()> {
        if self.frozen.is_some() {
            Err(invalid("/registry", "registry is sealed"))
        } else {
            Ok(())
        }
    }
    pub fn register_schema(&mut self, id: impl Into<String>, bytes: &[u8]) -> Result<()> {
        self.writable()?;
        let id = id.into();
        resource_id(&id)?;
        if self.schemas.contains_key(&id) {
            return Err(invalid("/schemas", "duplicate schema id"));
        }
        self.schemas.insert(id, SchemaSource::new(bytes)?);
        Ok(())
    }
    pub fn register_schema_file(
        &mut self,
        id: impl Into<String>,
        root: &std::path::Path,
        relative: &str,
    ) -> Result<()> {
        let bytes = crate::definition::safe_read(root, relative, 1_048_576)?;
        self.register_schema(id, &bytes)
    }
    pub fn register_rules_file(
        &mut self,
        id: impl Into<String>,
        root: &std::path::Path,
        relative: &str,
    ) -> Result<String> {
        let bytes = crate::definition::safe_read(root, relative, 1_048_576)?;
        self.register_rules(id, &bytes)
    }
    pub fn register_skill_root(
        &mut self,
        id: impl Into<String>,
        root: &std::path::Path,
    ) -> Result<()> {
        if std::fs::symlink_metadata(root)?.file_type().is_symlink() {
            return Err(invalid("/skills", "Skill root symlink forbidden"));
        }
        let root = root.canonicalize()?;
        let mut files = BTreeMap::new();
        crate::definition::collect_skill(&root, &root, &mut files)?;
        let report = crate::skill::validate_skill_files(&root, &files)?;
        let version = report
            .declared_version
            .unwrap_or_else(|| "unversioned".into());
        self.register_skill(
            id,
            SkillPackage {
                sha256: report.sha256,
                files,
                version,
            },
        )
    }
    pub fn register_skill(&mut self, id: impl Into<String>, skill: SkillPackage) -> Result<()> {
        self.writable()?;
        let id = id.into();
        reference_id(&id)?;
        if self.skills.contains_key(&id) {
            return Err(invalid("/skills", "duplicate skill id"));
        }
        if digest(&serde_json::to_vec(&skill.files)?) != skill.sha256 {
            return Err(invalid("/skills", "skill bytes do not match digest"));
        }
        if skill.files.len() > 128
            || skill.files.values().any(|b| b.len() > 2 * 1024 * 1024)
            || skill.files.values().map(Vec::len).sum::<usize>() > 8 * 1024 * 1024
            || skill.files.keys().any(|key| {
                key.is_empty()
                    || key.contains('\\')
                    || key.contains('\0')
                    || std::path::Path::new(key)
                        .components()
                        .any(|c| !matches!(c, std::path::Component::Normal(_)))
            })
        {
            return Err(invalid(
                "/skills",
                "invalid Skill resource path or capacity",
            ));
        }
        let name = id.split('@').next().unwrap_or(&id);
        crate::skill::validate_skill_files(std::path::Path::new(name), &skill.files)?;
        self.skills.insert(id, skill);
        Ok(())
    }
    pub fn register_entry(
        &mut self,
        id: impl Into<String>,
        contract: EntryContract,
        budget: RunBudgetPolicy,
    ) -> Result<()> {
        self.writable()?;
        let id = id.into();
        reference_id(&id)?;
        if self.entries.contains_key(&id) {
            return Err(invalid("/entries", "duplicate entry id"));
        }
        self.entries
            .insert(id, RegisteredEntry { contract, budget });
        Ok(())
    }
    pub fn register_capability(&mut self, contract: NodeContract) -> Result<String> {
        self.writable()?;
        let id = format!("{}@{}", contract.id, contract.version);
        reference_id(&id)?;
        if self.capabilities.contains_key(&id) {
            return Err(invalid("/capabilities", "duplicate capability id"));
        }
        self.capabilities.insert(id.clone(), contract);
        Ok(id)
    }
    pub fn register_profile(&mut self, profile: OrchestrationProfile) -> Result<String> {
        self.writable()?;
        let id = format!("{}@{}", profile.id, profile.version);
        reference_id(&id)?;
        if self.profiles.contains_key(&id) {
            return Err(invalid("/profiles", "duplicate profile id"));
        }
        self.profiles.insert(id.clone(), profile);
        Ok(id)
    }
    pub fn register_checker(
        &mut self,
        id: impl Into<String>,
        checker: CheckerContract,
    ) -> Result<()> {
        self.writable()?;
        let id = id.into();
        reference_id(&id)?;
        if self.checkers.contains_key(&id) {
            return Err(invalid("/checkers", "duplicate checker id"));
        }
        self.checkers.insert(id, checker);
        Ok(())
    }
    pub fn register_rules(&mut self, id: impl Into<String>, bytes: &[u8]) -> Result<String> {
        self.writable()?;
        let id = id.into();
        resource_id(&id)?;
        if self.rules.contains_key(&id) {
            return Err(invalid("/rules", "duplicate rule id"));
        }
        if bytes.len() > 1_048_576 {
            return Err(invalid("/rules", "rules exceed 1 MiB"));
        }
        let value = serde_json::from_value(crate::parse_unique(bytes)?)?;
        let hash = digest(bytes);
        self.rules.insert(
            id,
            LockedRules {
                value,
                digest: hash.clone(),
            },
        );
        Ok(hash)
    }
    pub fn register_template(&mut self, mut template: WorkflowTemplate) -> Result<String> {
        self.writable()?;
        let id = format!("{}@{}", template.id, template.version);
        reference_id(&id)?;
        if self.templates.contains_key(&id) {
            return Err(invalid("/templates", "duplicate template id"));
        }
        for node in template.nodes.values_mut() {
            node.depends_on.sort();
        }
        self.templates.insert(id.clone(), template);
        Ok(id)
    }
    pub fn profile_digest(&self, id: &str) -> Result<String> {
        self.frozen()?
            .profile_digests
            .get(id)
            .cloned()
            .ok_or_else(|| invalid("/profile_ref", "unknown profile"))
    }
    pub(crate) fn frozen(&self) -> Result<Arc<FrozenRegistry>> {
        self.frozen
            .clone()
            .ok_or_else(|| invalid("/registry", "registry must be validated and sealed"))
    }
    pub fn validate_and_freeze(&mut self, bindings: &HostBindings) -> Result<RegistrationReceipt> {
        if let Some(f) = &self.frozen {
            if f.bindings_digest != bindings_digest(bindings)? {
                return Err(invalid("/bindings", "sealed bindings changed"));
            }
            return Ok(receipt(f));
        }
        if self.schemas.len() > 256
            || self.entries.len() > 256
            || self.capabilities.len() > 256
            || self.profiles.len() > 64
            || self.checkers.len() > 128
            || self.rules.len() > 128
        {
            return Err(invalid("/registry", "registry cardinality limit exceeded"));
        }
        let bytes = self.schemas.values().map(|s| s.bytes.len()).sum::<usize>()
            + self
                .skills
                .values()
                .flat_map(|s| s.files.values())
                .map(Vec::len)
                .sum::<usize>()
            + self
                .rules
                .values()
                .map(|r| {
                    serde_json::to_vec(&r.value)
                        .map(|v| v.len())
                        .unwrap_or(usize::MAX)
                })
                .sum::<usize>();
        if bytes > 16 * 1024 * 1024 {
            return Err(invalid("/resources", "resources exceed 16 MiB"));
        }
        if bindings
            .gates
            .keys()
            .any(|id| id.starts_with("_tl.") || id.starts_with("taskloop:"))
            || bindings.tools.keys().any(|id| builtin_tool(id))
        {
            return Err(invalid(
                "/bindings",
                "Core gate/tool bindings cannot be overridden",
            ));
        }
        let schemas = Arc::new(Schemas::compile(&self.schemas)?);
        for (id, entry) in &self.entries {
            schemas.contract("dag-entry", &serde_json::to_value(&entry.contract)?)?;
            let p = &entry.budget;
            p.validate(u64::MAX, crate::store::MAX_BLOB_BYTES)?;
            if [
                p.max_run_ms,
                p.max_check_ms,
                Some(p.max_tool_calls),
                p.max_model_iterations,
            ]
            .into_iter()
            .flatten()
            .any(|n| n > i64::MAX as u64)
            {
                return Err(invalid(
                    "/entry_budgets",
                    "Goal budget integers must fit SQLite counters",
                ));
            }
            if p.max_run_ms.is_none() || p.max_check_ms.is_none() {
                return Err(invalid(
                    &format!("/entries/{id}/budget"),
                    "Goal entry requires explicit run/check deadlines",
                ));
            }
            let executor = bindings
                .executors
                .get(&entry.contract.executor)
                .ok_or_else(|| invalid("/executor", "executor binding missing"))?;
            if !executor.capabilities().managed() {
                return Err(invalid(
                    "/executor",
                    "Goal requires managed execution and isolation",
                ));
            }
            if p.max_model_iterations.is_some_and(|n| {
                executor.budget_limits().model_iterations
                    != crate::ModelIterationSupport::RuntimeValidated
                    || executor
                        .budget_limits()
                        .max_model_iterations
                        .is_some_and(|maximum| n > maximum)
            }) {
                return Err(invalid(
                    "/executor",
                    "model iteration constraint unsupported",
                ));
            }
            schemas.source(&entry.contract.input_schema)?;
            for output in entry.contract.outputs.values() {
                schemas.source(&output.schema)?;
            }
            for skill in &entry.contract.skills {
                if !self.skills.contains_key(skill) {
                    return Err(invalid("/skills", "entry skill missing"));
                }
            }
            for tool in &entry.contract.tools {
                if !builtin_tool(tool) && !bindings.tools.contains_key(tool) {
                    return Err(invalid("/tools", format!("tool binding missing: {tool}")));
                }
            }
        }
        for rules in self.rules.values() {
            schemas.contract("rule-set", &serde_json::to_value(&rules.value)?)?;
            unique(rules.value.rules.iter().map(|r| r.id.as_str()), "/rules")?;
        }
        for checker in self.checkers.values() {
            let entry = self
                .entries
                .get(&checker.entry_ref)
                .ok_or_else(|| invalid("/checkers/entry_ref", "checker entry missing"))?;
            let slot = entry
                .contract
                .outputs
                .get(&checker.result_slot)
                .ok_or_else(|| invalid("/checkers/result_slot", "evaluation slot missing"))?;
            if !checker.fresh_context
                || !schemas.same(&entry.contract.input_schema, "_tl.check-input@1")?
                || !slot.required
                || !schemas.same(&slot.schema, "_tl.evaluation-candidate@1")?
                || !self.rules.contains_key(&checker.rules_ref)
            {
                return Err(invalid(
                    "/checkers",
                    "checker input/output/rules/isolation mismatch",
                ));
            }
            unique(
                checker.goal_input_fields.iter().map(String::as_str),
                "/checkers/goal_input_fields",
            )?;
        }
        for node in self.capabilities.values() {
            schemas.contract("node-contract", &serde_json::to_value(node)?)?;
            validate_node(node, &schemas, &self.entries, &self.checkers)?;
        }
        for profile in self.profiles.values() {
            if profile
                .allowed_tools
                .iter()
                .any(|t| !builtin_tool(t) && !bindings.tools.contains_key(t))
            {
                return Err(invalid("/allowed_tools", "Profile tool binding missing"));
            }
            validate_profile(
                profile,
                &schemas,
                &self.entries,
                &self.capabilities,
                &self.checkers,
                &self.rules,
                &self.templates,
            )?;
        }
        for template in self.templates.values() {
            if template.nodes.is_empty() || template.nodes.len() > 256 {
                return Err(invalid("/templates", "template must have 1..256 nodes"));
            }
            for node in template.nodes.values() {
                if node.capabilities.is_empty()
                    || node
                        .capabilities
                        .iter()
                        .any(|id| !self.capabilities.contains_key(id))
                {
                    return Err(invalid(
                        "/templates/capabilities",
                        "unknown template capability",
                    ));
                }
                unique(
                    node.fillable_inputs.iter().map(String::as_str),
                    "/templates/fillable_inputs",
                )?;
            }
        }
        let binding = bindings_digest(bindings)?;
        let definition = json!({"entries":self.entries,"capabilities":self.capabilities,"profiles":self.profiles,"checkers":self.checkers,"templates":self.templates});
        if serde_json::to_vec(&definition)?.len() > 1_048_576 {
            return Err(invalid("/registry", "typed definition exceeds 1 MiB"));
        }
        let mut frozen = FrozenRegistry {
            schemas,
            skills: self.skills.clone(),
            entries: self.entries.clone(),
            capabilities: self.capabilities.clone(),
            profiles: self.profiles.clone(),
            checkers: self.checkers.clone(),
            rules: self.rules.clone(),
            templates: self.templates.clone(),
            profile_digests: BTreeMap::new(),
            bindings_digest: binding.clone(),
            digest: String::new(),
        };
        for (id, profile) in &frozen.profiles {
            frozen
                .profile_digests
                .insert(id.clone(), profile_digest(profile, &frozen, bindings)?);
        }
        frozen.digest = digest(&serde_json::to_vec(
            &json!({"definitions":definition,"profiles":frozen.profile_digests,"bindings":binding,"schemas":frozen.schemas.sources.iter().map(|(k,v)|(k,&v.digest)).collect::<BTreeMap<_,_>>(),"skills":frozen.skills.iter().map(|(k,v)|(k,&v.sha256)).collect::<BTreeMap<_,_>>(),"rules":frozen.rules.iter().map(|(k,v)|(k,&v.digest)).collect::<BTreeMap<_,_>>()}),
        )?);
        let result = receipt(&frozen);
        self.frozen = Some(Arc::new(frozen));
        Ok(result)
    }
}
fn receipt(f: &FrozenRegistry) -> RegistrationReceipt {
    RegistrationReceipt {
        registry_digest: f.digest.clone(),
        profiles: f.profile_digests.clone(),
        capabilities: f.capabilities.keys().cloned().collect(),
    }
}
pub(crate) fn builtin_tool(id: &str) -> bool {
    matches!(id, "artifact.read" | "artifact.write")
}
pub(crate) fn bindings_digest(b: &HostBindings) -> Result<String> {
    Ok(digest(&serde_json::to_vec(
        &json!({"version":b.configuration_version,"grants":b.grants,"executors":b.executors.iter().map(|(k,e)|(k,e.identity(),{let c=e.capabilities();[c.bounded_entry,c.collect_artifacts,c.cancel,c.isolated_workspace,c.isolated_context]},e.budget_limits())).collect::<Vec<_>>(),"tools":b.tools.iter().map(|(k,t)|(k,t.spec())).collect::<Vec<_>>()}),
    )?))
}
pub(crate) fn reference_id(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > 160 || id.starts_with("_tl.") || id.starts_with("taskloop:") {
        Err(invalid("/id", "invalid or reserved reference"))
    } else {
        Ok(())
    }
}
fn resource_id(id: &str) -> Result<()> {
    reference_id(id)
}
pub(crate) fn unique<'a>(ids: impl Iterator<Item = &'a str>, path: &str) -> Result<()> {
    let mut set = BTreeSet::new();
    for id in ids {
        if !set.insert(id) {
            return Err(invalid(path, "duplicate id"));
        }
    }
    Ok(())
}

fn fixed_object(schemas: &Schemas, id: &str) -> Result<Value> {
    let value = schemas.resolve_in(id, &schemas.source(id)?.value)?;
    if value["type"] != "object" || !value["properties"].is_object() {
        return Err(invalid("/schema", "expected a fixed object schema"));
    }
    Ok(value)
}
pub(super) fn escaped(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}
fn validate_node(
    node: &NodeContract,
    schemas: &Schemas,
    entries: &BTreeMap<String, RegisteredEntry>,
    checkers: &BTreeMap<String, CheckerContract>,
) -> Result<()> {
    for port in node.inputs.values() {
        schemas.source(&port.schema_ref)?;
        if let Some(value) = &port.default {
            schemas.validate(&port.schema_ref, value)?;
        }
    }
    for port in node.outputs.values() {
        schemas.source(&port.schema_ref)?;
    }
    if node.kind == NodeKind::Input {
        let signal = node
            .signal_schema_ref
            .as_deref()
            .ok_or_else(|| invalid("/signal_schema_ref", "missing input signal schema"))?;
        if node.outputs.len() != 1
            || !node.outputs.contains_key("value")
            || !schemas.same(&node.outputs["value"].schema_ref, signal)?
        {
            return Err(invalid(
                "/outputs",
                "input node must expose exactly the signal as value",
            ));
        }
        return Ok(());
    }
    for member in node.members.values() {
        let entry = entries
            .get(member.entry_ref())
            .ok_or_else(|| invalid("/members/entry_ref", "unknown member entry"))?;
        match member {
            NodeMember::NodePorts { input_bindings, .. } => {
                if schemas.same(&entry.contract.input_schema, "_tl.check-input@1")? {
                    return Err(invalid(
                        "/members/input_mode",
                        "Core CheckInput requires Runtime-issued check_request mode",
                    ));
                }
                let (target, target_base) = schemas.selected(&entry.contract.input_schema, "")?;
                fixed_object(schemas, &entry.contract.input_schema)?;
                let props = target["properties"].as_object().expect("fixed object");
                let required = target["required"].as_array().cloned().unwrap_or_default();
                if required
                    .iter()
                    .any(|v| v.as_str().is_none_or(|k| !input_bindings.contains_key(k)))
                {
                    return Err(invalid(
                        "/members/input_bindings",
                        "required entry field is unbound",
                    ));
                }
                for (field, binding) in input_bindings {
                    let source = node
                        .inputs
                        .get(&binding.port)
                        .ok_or_else(|| invalid("/members/input_bindings", "unknown input port"))?;
                    let target_field = props
                        .get(field)
                        .ok_or_else(|| invalid("/members/input_bindings", "unknown entry field"))?;
                    if required.iter().any(|v| v == field)
                        && !source.required
                        && source.default.is_none()
                    {
                        return Err(invalid(
                            "/members/input_bindings",
                            "optional source cannot supply a required entry field",
                        ));
                    }
                    let (source_schema, base) =
                        schemas.selected(&source.schema_ref, &binding.pointer)?;
                    if !schemas.equivalent(&base, &source_schema, &target_base, target_field)? {
                        return Err(invalid(
                            "/members/input_bindings",
                            "member field schema mismatch",
                        ));
                    }
                }
            }
            NodeMember::CheckRequest { .. } => {
                if node.evaluation.is_none() {
                    return Err(invalid(
                        "/members/input_mode",
                        "check_request requires evaluation contract",
                    ));
                }
            }
        }
    }
    let join = node
        .join
        .as_ref()
        .ok_or_else(|| invalid("/join", "missing join"))?;
    if join.outputs.keys().ne(node.outputs.keys()) {
        return Err(invalid(
            "/join/outputs",
            "join must project exactly all public outputs",
        ));
    }
    for (port, projection) in &join.outputs {
        let output = &node.outputs[port].schema_ref;
        let slot_schema = |id: &str, slot: &str| -> Result<&str> {
            let member = node
                .members
                .get(id)
                .ok_or_else(|| invalid("/join/outputs", "unknown member"))?;
            let slot = entries[member.entry_ref()]
                .contract
                .outputs
                .get(slot)
                .ok_or_else(|| invalid("/join/outputs", "unknown member slot"))?;
            if !slot.required {
                return Err(invalid(
                    "/join/outputs",
                    "projection requires a required slot",
                ));
            }
            Ok(&slot.schema)
        };
        match projection {
            OutputProjection::Member { member_id, slot } => {
                if !schemas.same(output, slot_schema(member_id, slot)?)? {
                    return Err(invalid(
                        "/join/outputs",
                        "public/member schema digest mismatch",
                    ));
                }
            }
            OutputProjection::Members { slots } => {
                let public = fixed_object(schemas, output)?;
                let props = public["properties"].as_object().expect("fixed object");
                let required = public["required"].as_array().cloned().unwrap_or_default();
                if public["additionalProperties"] != false
                    || props.keys().ne(slots.keys())
                    || required.len() != slots.len()
                    || slots.keys().any(|id| !required.iter().any(|v| v == id))
                {
                    return Err(invalid(
                        "/join/outputs",
                        "members projection must be an exact required object",
                    ));
                }
                for (member, slot) in slots {
                    if !schemas.selector_matches(
                        output,
                        &format!("/{}", escaped(member)),
                        slot_schema(member, slot)?,
                    )? {
                        return Err(invalid(
                            "/join/outputs",
                            "members projection field schema mismatch",
                        ));
                    }
                }
            }
        }
    }
    if let Some(evaluation) = &node.evaluation {
        let checker = checkers
            .get(&evaluation.checker_ref)
            .ok_or_else(|| invalid("/evaluation/checker_ref", "unknown checker"))?;
        let (member_id, member) = node
            .members
            .first_key_value()
            .ok_or_else(|| invalid("/members", "evaluation member missing"))?;
        if node.members.len() != 1
            || !matches!(member, NodeMember::CheckRequest { .. })
            || member.entry_ref() != checker.entry_ref
        {
            return Err(invalid(
                "/evaluation",
                "evaluation requires exactly its checker entry in check_request mode",
            ));
        }
        let ports = evaluation.target_ports.values().collect::<BTreeSet<_>>();
        if ports.len() != evaluation.target_ports.len()
            || ports.len() != node.inputs.len()
            || node.inputs.iter().any(|(id, p)| {
                !ports.contains(id)
                    || !p.required
                    || !matches!(p.source_policy, InputSourcePolicy::Artifact)
            })
        {
            return Err(invalid(
                "/evaluation/target_ports",
                "evaluation targets must map one-to-one to required artifact inputs",
            ));
        }
        if node.outputs.len() != 1
            || !node.outputs.contains_key("evaluation")
            || !schemas.same(
                &node.outputs["evaluation"].schema_ref,
                "_tl.evaluation-candidate@1",
            )?
        {
            return Err(invalid(
                "/evaluation",
                "evaluation must expose only Core Evaluation",
            ));
        }
        if !matches!(join.outputs.get("evaluation"),Some(OutputProjection::Member{member_id:id,slot}) if id==member_id && slot==&checker.result_slot)
        {
            return Err(invalid(
                "/join/outputs/evaluation",
                "evaluation projection must select checker result slot",
            ));
        }
    }
    Ok(())
}
#[allow(clippy::too_many_arguments)]
fn validate_profile(
    profile: &OrchestrationProfile,
    schemas: &Schemas,
    entries: &BTreeMap<String, RegisteredEntry>,
    nodes: &BTreeMap<String, NodeContract>,
    checkers: &BTreeMap<String, CheckerContract>,
    rules: &BTreeMap<String, LockedRules>,
    templates: &BTreeMap<String, WorkflowTemplate>,
) -> Result<()> {
    schemas.contract("orchestration-profile", &serde_json::to_value(profile)?)?;
    schemas.source(&profile.goal_input_schema)?;
    for (pointer, target) in &profile.goal_selectors {
        let matches = if pointer == "/goal" {
            schemas.selector_matches("taskloop:goal-request:v1", "/spec/goal", target)?
        } else if let Some(pointer) = pointer.strip_prefix("/inputs") {
            if !pointer.is_empty() && !pointer.starts_with('/') {
                false
            } else {
                schemas.selector_matches(&profile.goal_input_schema, pointer, target)?
            }
        } else {
            false
        };
        if !matches {
            return Err(invalid(
                "/goal_selectors",
                "goal selector schema/path mismatch",
            ));
        }
    }
    for (source, selectors) in &profile.artifact_selectors {
        schemas.source(source)?;
        for (pointer, target) in selectors {
            if !schemas.selector_matches(source, pointer, target)? {
                return Err(invalid(
                    "/artifact_selectors",
                    "artifact selector schema/path mismatch",
                ));
            }
        }
    }
    for tool in &profile.allowed_tools {
        // All tools used by entries are resolved earlier; unbound, unused permission names are also rejected below at freeze.
        reference_id(tool)?;
    }
    for schema in &profile.planning_input_schemas {
        schemas.source(schema)?;
    }
    let planner = entries
        .get(&profile.planner_entry_ref)
        .ok_or_else(|| invalid("/planner_entry_ref", "unknown planner entry"))?;
    if planner.contract.outputs.len() != 1
        || !planner.contract.outputs.contains_key("proposal")
        || !schemas.same(
            &planner.contract.outputs["proposal"].schema,
            "_tl.planner-result@1",
        )?
    {
        return Err(invalid(
            "/planner_entry_ref",
            "planner must expose exactly Core PlannerResult as proposal",
        ));
    }
    let mut used_entries = BTreeSet::from([profile.planner_entry_ref.as_str()]);
    let mut used_checkers = BTreeSet::new();
    for id in &profile.capabilities {
        let node = nodes
            .get(id)
            .ok_or_else(|| invalid("/capabilities", "unknown capability"))?;
        for member in node.members.values() {
            used_entries.insert(member.entry_ref());
        }
        for input in node.inputs.values() {
            if let InputSourcePolicy::Goal { pointer } = &input.source_policy {
                let schema = profile.goal_selectors.get(pointer).ok_or_else(|| {
                    invalid("/goal_selectors", "goal source has no authorized selector")
                })?;
                if !schemas.same(schema, &input.schema_ref)? {
                    return Err(invalid(
                        "/goal_selectors",
                        "goal source schema digest mismatch",
                    ));
                }
            }
        }
        if let Some(e) = &node.evaluation {
            used_checkers.insert(e.checker_ref.as_str());
            for (alias, port) in &e.target_ports {
                let output = profile.outputs.get(alias).ok_or_else(|| {
                    invalid("/evaluation/target_ports", "target alias outside Profile")
                })?;
                if !schemas.same(&output.schema_ref, &node.inputs[port].schema_ref)? {
                    return Err(invalid(
                        "/evaluation/target_ports",
                        "target schema mismatch",
                    ));
                }
            }
        }
    }
    for output in profile.outputs.values() {
        schemas.source(&output.schema_ref)?;
        for id in &output.allowed_capabilities {
            if !profile.capabilities.contains(id) {
                return Err(invalid(
                    "/outputs/allowed_capabilities",
                    "producer outside Profile",
                ));
            }
            if !nodes[id].outputs.values().any(|p| {
                schemas
                    .same(&p.schema_ref, &output.schema_ref)
                    .unwrap_or(false)
            }) {
                return Err(invalid(
                    "/outputs/allowed_capabilities",
                    "producer has no output of declared schema",
                ));
            }
        }
    }
    if profile
        .final_outputs
        .iter()
        .any(|id| !profile.outputs.contains_key(id))
    {
        return Err(invalid("/final_outputs", "unknown final output"));
    }
    unique(profile.checks.iter().map(|r| r.id.as_str()), "/checks")?;
    for check in &profile.checks {
        let checker = checkers
            .get(&check.checker_ref)
            .ok_or_else(|| invalid("/checks/checker_ref", "unknown checker"))?;
        used_checkers.insert(check.checker_ref.as_str());
        if rules[&checker.rules_ref].digest != check.rules_digest {
            return Err(invalid("/checks/rules_digest", "rules digest mismatch"));
        }
        if check
            .target_outputs
            .iter()
            .any(|id| !profile.outputs.contains_key(id))
        {
            return Err(invalid("/checks/target_outputs", "unknown target output"));
        }
        if let Some(before) = &check.before_outputs {
            if before.iter().any(|id| !profile.outputs.contains_key(id)) {
                return Err(invalid(
                    "/checks/before_outputs",
                    "unknown protected output",
                ));
            }
        }
    }
    for id in used_checkers {
        let checker = &checkers[id];
        used_entries.insert(&checker.entry_ref);
        let input = fixed_object(schemas, &profile.goal_input_schema)?;
        if checker.goal_input_fields.iter().any(|field| {
            !input["properties"]
                .as_object()
                .is_some_and(|props| props.contains_key(field))
        }) {
            return Err(invalid(
                "/checkers/goal_input_fields",
                "checker requests undeclared Goal input field",
            ));
        }
    }
    for id in used_entries {
        if entries[id]
            .contract
            .tools
            .iter()
            .any(|t| !profile.allowed_tools.contains(t))
        {
            return Err(invalid(
                "/allowed_tools",
                "entry tool outside Profile authorization",
            ));
        }
    }
    for id in &profile.templates {
        let template = templates
            .get(id)
            .ok_or_else(|| invalid("/templates", "unknown workflow template"))?;
        if template
            .nodes
            .values()
            .flat_map(|n| &n.capabilities)
            .any(|id| !profile.capabilities.contains(id))
        {
            return Err(invalid(
                "/templates",
                "template must narrow Profile capabilities",
            ));
        }
    }
    Ok(())
}
fn profile_digest(
    profile: &OrchestrationProfile,
    f: &FrozenRegistry,
    bindings: &HostBindings,
) -> Result<String> {
    let mut schema_ids = BTreeSet::from([profile.goal_input_schema.clone()]);
    schema_ids.extend(profile.goal_selectors.values().cloned());
    for (source, selectors) in &profile.artifact_selectors {
        schema_ids.insert(source.clone());
        schema_ids.extend(selectors.values().cloned());
    }
    schema_ids.extend(profile.planning_input_schemas.iter().cloned());
    schema_ids.extend(profile.outputs.values().map(|v| v.schema_ref.clone()));
    let mut entry_ids = BTreeSet::from([profile.planner_entry_ref.clone()]);
    let mut checker_ids = profile
        .checks
        .iter()
        .map(|v| v.checker_ref.clone())
        .collect::<BTreeSet<_>>();
    let mut capabilities = BTreeMap::new();
    for id in &profile.capabilities {
        let n = &f.capabilities[id];
        capabilities.insert(id, n);
        schema_ids.extend(n.inputs.values().map(|v| v.schema_ref.clone()));
        schema_ids.extend(n.outputs.values().map(|v| v.schema_ref.clone()));
        schema_ids.extend(n.signal_schema_ref.iter().cloned());
        entry_ids.extend(n.members.values().map(|m| m.entry_ref().to_owned()));
        if let Some(e) = &n.evaluation {
            checker_ids.insert(e.checker_ref.clone());
        }
    }
    let mut checkers = BTreeMap::new();
    let mut rule_hashes = BTreeMap::new();
    for id in checker_ids {
        let c = &f.checkers[&id];
        checkers.insert(id, c);
        entry_ids.insert(c.entry_ref.clone());
        rule_hashes.insert(&c.rules_ref, &f.rules[&c.rules_ref].digest);
    }
    let mut entries = BTreeMap::new();
    let mut skills = BTreeMap::new();
    let mut tools = BTreeMap::new();
    let mut executors = BTreeMap::new();
    for id in entry_ids {
        let e = &f.entries[&id];
        entries.insert(id, e);
        schema_ids.insert(e.contract.input_schema.clone());
        schema_ids.extend(e.contract.outputs.values().map(|s| s.schema.clone()));
        for id in &e.contract.skills {
            skills.insert(id, &f.skills[id].sha256);
        }
        for id in &e.contract.tools {
            if !builtin_tool(id) {
                tools.insert(id, bindings.tools[id].spec());
            }
        }
        let ex = &bindings.executors[&e.contract.executor];
        let caps = ex.capabilities();
        executors.insert(&e.contract.executor,json!({"identity":ex.identity(),"budget":ex.budget_limits(),"capabilities":[caps.bounded_entry,caps.collect_artifacts,caps.cancel,caps.isolated_workspace,caps.isolated_context]}));
    }
    let mut schema_hashes = BTreeMap::new();
    let mut pending = schema_ids;
    while let Some(id) = pending.pop_first() {
        if schema_hashes.contains_key(&id) {
            continue;
        }
        schema_hashes.insert(id.clone(), f.schemas.source(&id)?.digest.clone());
        pending.extend(f.schemas.references(&id)?);
    }
    let templates = profile
        .templates
        .iter()
        .map(|id| (id, &f.templates[id]))
        .collect::<BTreeMap<_, _>>();
    Ok(digest(&serde_json::to_vec(
        &json!({"profile":profile,"capabilities":capabilities,"entries":entries,"schemas":schema_hashes,"skills":skills,"rules":rule_hashes,"checkers":checkers,"templates":templates,"tools":tools,"executors":executors,"configuration_version":bindings.configuration_version}),
    )?))
}
