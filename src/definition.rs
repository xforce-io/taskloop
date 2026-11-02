use crate::{
    Error, Result,
    json::{digest, has_ref},
    parse_unique,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Component, Path},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bundle {
    pub contract_version: u32,
    pub entries: BTreeMap<String, EntryContract>,
    pub flow: Flow,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntryContract {
    pub instruction: String,
    pub executor: String,
    pub skills: Vec<String>,
    pub tools: Vec<String>,
    pub input_schema: String,
    #[serde(default)]
    pub requires: Vec<Requirement>,
    pub outputs: BTreeMap<String, OutputSlot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachments: Option<AttachmentPolicy>,
    pub gate: String,
    pub outcomes: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Requirement {
    pub state: String,
    pub slot: String,
    pub selection: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub include_attachments: bool,
}
fn is_false(value: &bool) -> bool {
    !*value
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputSlot {
    pub schema: String,
    pub required: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttachmentPolicy {
    pub slot_prefix: String,
    pub path_prefix: String,
    pub max_files: u64,
    pub max_bytes: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Flow {
    pub id: String,
    pub version: String,
    pub input_schema: String,
    pub initial: String,
    pub states: BTreeMap<String, State>,
    pub limits: FlowLimits,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FlowLimits {
    pub max_transitions: u64,
    pub max_visits_per_state: u64,
    pub max_runs: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
pub enum State {
    Work {
        entry: String,
        accepted: BTreeMap<String, String>,
        rejected: String,
        failed: String,
    },
    Wait {
        signals: BTreeMap<String, SignalSpec>,
        timeout_seconds: u64,
        timeout_target: String,
    },
    Terminal {
        status: String,
        outcome: String,
        requires: Vec<Requirement>,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignalSpec {
    pub schema: String,
    pub target: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillPackage {
    pub version: String,
    pub files: BTreeMap<String, Vec<u8>>,
    pub sha256: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Definition {
    pub bundle: Bundle,
    pub schemas: BTreeMap<String, Value>,
    pub skills: BTreeMap<String, SkillPackage>,
    pub digest: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationReport {
    pub valid: bool,
    pub definition_digest: String,
    pub states: usize,
    pub entries: usize,
    pub skills: usize,
}
impl Definition {
    /// Static validation: no model, tool callback, installation script or network access.
    pub fn load(
        bundle_path: impl AsRef<Path>,
        skill_roots: &BTreeMap<String, PathBuf>,
    ) -> Result<Self> {
        let file = bundle_path.as_ref();
        let root = file
            .parent()
            .ok_or_else(|| Error::new("definition_invalid", "/", "missing resource root"))?
            .canonicalize()?;
        let raw = safe_read(
            &root,
            file.file_name().and_then(|n| n.to_str()).unwrap_or(""),
            4 * 1024 * 1024,
        )?;
        let value = parse_unique(&raw)?;
        validate_schema(
            &serde_json::from_str(include_str!("../schemas/bundle.schema.json"))?,
            &value,
            "/bundle",
        )?;
        let bundle: Bundle = serde_json::from_value(value)
            .map_err(|e| Error::new("definition_invalid", "/bundle", e.to_string()))?;
        let mut names = BTreeSet::from([bundle.flow.input_schema.clone()]);
        for entry in bundle.entries.values() {
            names.insert(entry.input_schema.clone());
            for o in entry.outputs.values() {
                names.insert(o.schema.clone());
            }
        }
        for state in bundle.flow.states.values() {
            if let State::Wait { signals, .. } = state {
                for s in signals.values() {
                    names.insert(s.schema.clone());
                }
            }
        }
        let mut schemas = BTreeMap::new();
        for name in names {
            let value = if name == "task-input.schema.json" {
                serde_json::from_str(include_str!("../schemas/task-input.schema.json"))?
            } else {
                parse_unique(&safe_read(&root, &name, 1024 * 1024)?)?
            };
            if has_ref(&value) {
                return Err(Error::new(
                    "definition_invalid",
                    format!("/schemas/{name}"),
                    "$ref is unsupported; supply complete local schemas",
                ));
            }
            jsonschema::validator_for(&value).map_err(|e| {
                Error::new(
                    "definition_invalid",
                    format!("/schemas/{name}"),
                    e.to_string(),
                )
            })?;
            schemas.insert(name, value);
        }
        let mut skills = BTreeMap::new();
        for entry in bundle.entries.values() {
            for id in &entry.skills {
                if skills.contains_key(id) {
                    continue;
                }
                let path = skill_roots.get(id).ok_or_else(|| {
                    Error::new(
                        "binding_missing",
                        format!("/skills/{id}"),
                        "skill root is not registered",
                    )
                })?;
                let skill_report = crate::validate_skill(path)?;
                let canonical = path.canonicalize()?;
                let mut files = BTreeMap::new();
                collect_skill(&canonical, &canonical, &mut files)?;
                if !files.contains_key("SKILL.md") {
                    return Err(Error::new(
                        "definition_invalid",
                        format!("/skills/{id}"),
                        "SKILL.md missing",
                    ));
                }
                let version = id
                    .rsplit_once('@')
                    .map(|(_, v)| v.to_string())
                    .ok_or_else(|| {
                        Error::new("definition_invalid", id, "skill id must include @version")
                    })?;
                if skill_report
                    .declared_version
                    .as_ref()
                    .is_some_and(|v| v != &version)
                {
                    return Err(Error::new(
                        "definition_invalid",
                        format!("/skills/{id}/version"),
                        "declared Skill version does not match binding",
                    ));
                }
                let content = serde_json::to_vec(&files)?;
                skills.insert(
                    id.clone(),
                    SkillPackage {
                        version,
                        files,
                        sha256: digest(&content),
                    },
                );
            }
        }
        let mut definition = Self {
            bundle,
            schemas,
            skills,
            digest: String::new(),
        };
        definition.check_graph()?;
        definition.digest = digest(&serde_json::to_vec(&definition)?);
        Ok(definition)
    }
    pub fn report(&self) -> ValidationReport {
        ValidationReport {
            valid: true,
            definition_digest: self.digest.clone(),
            states: self.bundle.flow.states.len(),
            entries: self.bundle.entries.len(),
            skills: self.skills.len(),
        }
    }
    pub(crate) fn verify_integrity(&self) -> Result<()> {
        let mut copy = self.clone();
        copy.digest.clear();
        if digest(&serde_json::to_vec(&copy)?) != self.digest {
            return Err(Error::new(
                "definition_invalid",
                "/definition",
                "locked definition integrity mismatch",
            ));
        }
        Ok(())
    }
    pub(crate) fn verify(&self) -> Result<()> {
        self.verify_integrity()?;
        validate_schema(
            &serde_json::from_str(include_str!("../schemas/bundle.schema.json"))?,
            &serde_json::to_value(&self.bundle)?,
            "/bundle",
        )?;
        self.check_graph()?;
        let mut names = BTreeSet::from([self.bundle.flow.input_schema.as_str()]);
        for (id, entry) in &self.bundle.entries {
            names.insert(&entry.input_schema);
            names.extend(entry.outputs.values().map(|o| o.schema.as_str()));
            for skill in &entry.skills {
                let package = self.skills.get(skill).ok_or_else(|| {
                    Error::new(
                        "definition_invalid",
                        format!("/entries/{id}/skills"),
                        "locked skill missing",
                    )
                })?;
                if digest(&serde_json::to_vec(&package.files)?) != package.sha256
                    || !package.files.contains_key("SKILL.md")
                {
                    return Err(Error::new(
                        "definition_invalid",
                        format!("/skills/{skill}"),
                        "locked skill integrity mismatch",
                    ));
                }
            }
        }
        for state in self.bundle.flow.states.values() {
            if let State::Wait { signals, .. } = state {
                names.extend(signals.values().map(|s| s.schema.as_str()));
            }
        }
        for name in names {
            let value = self.schemas.get(name).ok_or_else(|| {
                Error::new(
                    "definition_invalid",
                    format!("/schemas/{name}"),
                    "required schema missing",
                )
            })?;
            if has_ref(value) {
                return Err(Error::new(
                    "definition_invalid",
                    format!("/schemas/{name}"),
                    "$ref unsupported",
                ));
            }
            jsonschema::validator_for(value).map_err(|e| {
                Error::new(
                    "definition_invalid",
                    format!("/schemas/{name}"),
                    e.to_string(),
                )
            })?;
        }
        if let Some(schema) = self.schemas.get("task-input.schema.json")
            && schema
                != &serde_json::from_str::<Value>(include_str!(
                    "../schemas/task-input.schema.json"
                ))?
        {
            return Err(Error::new(
                "definition_invalid",
                "/schemas/task-input.schema.json",
                "reserved input schema cannot be replaced",
            ));
        }
        Ok(())
    }
    pub fn validate_value(&self, schema: &str, value: &Value) -> Result<()> {
        let s = self.schemas.get(schema).ok_or_else(|| {
            Error::new(
                "definition_invalid",
                format!("/schemas/{schema}"),
                "schema not locked",
            )
        })?;
        validate_schema(s, value, schema)
    }
    fn check_graph(&self) -> Result<()> {
        let flow = &self.bundle.flow;
        if !flow.states.contains_key(&flow.initial) {
            return Err(Error::new(
                "definition_invalid",
                "/flow/initial",
                "unknown initial state",
            ));
        }
        let mut edges = BTreeMap::<String, BTreeSet<String>>::new();
        for (id, s) in &flow.states {
            let mut targets = BTreeSet::new();
            match s {
                State::Work {
                    entry,
                    accepted,
                    rejected,
                    failed,
                } => {
                    let e = self.bundle.entries.get(entry).ok_or_else(|| {
                        Error::new(
                            "definition_invalid",
                            format!("/flow/states/{id}/entry"),
                            "unknown entry",
                        )
                    })?;
                    if accepted.keys().cloned().collect::<BTreeSet<_>>()
                        != e.outcomes.iter().cloned().collect()
                    {
                        return Err(Error::new(
                            "definition_invalid",
                            format!("/flow/states/{id}/accepted"),
                            "outcomes and accepted branches must match",
                        ));
                    }
                    targets.extend(accepted.values().cloned());
                    targets.insert(rejected.clone());
                    targets.insert(failed.clone());
                }
                State::Wait {
                    signals,
                    timeout_target,
                    ..
                } => {
                    for (event, spec) in signals {
                        if event.starts_with("work.") || event.starts_with("runtime.") {
                            return Err(Error::new(
                                "definition_invalid",
                                format!("/flow/states/{id}/signals/{event}"),
                                "reserved event",
                            ));
                        }
                        targets.insert(spec.target.clone());
                    }
                    targets.insert(timeout_target.clone());
                }
                State::Terminal { requires, .. } => {
                    self.check_requirements(requires, &format!("/flow/states/{id}/requires"))?
                }
            }
            for t in &targets {
                if !flow.states.contains_key(t) {
                    return Err(Error::new(
                        "definition_invalid",
                        format!("/flow/states/{id}"),
                        format!("unknown target {t}"),
                    ));
                }
            }
            edges.insert(id.clone(), targets);
        }
        for (id, e) in &self.bundle.entries {
            self.check_requirements(&e.requires, &format!("/entries/{id}/requires"))?;
        }
        let reachable = walk(&flow.initial, &edges);
        if reachable.len() != flow.states.len() {
            return Err(Error::new(
                "definition_invalid",
                "/flow/states",
                "unreachable state",
            ));
        }
        let terminals: BTreeSet<_> = flow
            .states
            .iter()
            .filter_map(|(id, s)| matches!(s, State::Terminal { .. }).then_some(id.clone()))
            .collect();
        for id in flow.states.keys() {
            if walk(id, &edges).is_disjoint(&terminals) {
                return Err(Error::new(
                    "definition_invalid",
                    format!("/flow/states/{id}"),
                    "no path to a terminal",
                ));
            }
        }
        Ok(())
    }
    fn check_requirements(&self, requires: &[Requirement], path: &str) -> Result<()> {
        for r in requires {
            let Some(State::Work { entry, .. }) = self.bundle.flow.states.get(&r.state) else {
                return Err(Error::new(
                    "definition_invalid",
                    path,
                    "requirement producer must be a work state",
                ));
            };
            if !self
                .bundle
                .entries
                .get(entry)
                .ok_or_else(|| Error::new("definition_invalid", path, "producer entry not found"))?
                .outputs
                .contains_key(&r.slot)
            {
                return Err(Error::new(
                    "definition_invalid",
                    path,
                    format!("unknown output slot {}", r.slot),
                ));
            }
            if r.include_attachments && self.bundle.entries[entry].attachments.is_none() {
                return Err(Error::new(
                    "definition_invalid",
                    path,
                    "attachment dependency requires an attachment-producing entry",
                ));
            }
        }
        Ok(())
    }
}
use std::path::PathBuf;
fn walk(start: &str, edges: &BTreeMap<String, BTreeSet<String>>) -> BTreeSet<String> {
    let mut seen = BTreeSet::new();
    let mut stack = vec![start.to_string()];
    while let Some(s) = stack.pop() {
        if seen.insert(s.clone())
            && let Some(ts) = edges.get(&s)
        {
            stack.extend(ts.iter().cloned());
        }
    }
    seen
}
pub(crate) fn collect_skill(
    root: &Path,
    path: &Path,
    out: &mut BTreeMap<String, Vec<u8>>,
) -> Result<()> {
    if path
        .strip_prefix(root)
        .map(|p| p.components().count())
        .unwrap_or(99)
        > 16
    {
        return Err(Error::new(
            "definition_invalid",
            "/skills",
            "resource directory nesting exceeds 16",
        ));
    }
    for e in std::fs::read_dir(path)? {
        let e = e?;
        let p = e.path();
        let t = e.file_type()?;
        if t.is_symlink() {
            return Err(Error::new(
                "definition_invalid",
                p.display().to_string(),
                "skill symlink forbidden",
            ));
        }
        if t.is_dir() {
            collect_skill(root, &p, out)?;
        } else if t.is_file() {
            let rel = p.strip_prefix(root).unwrap().to_string_lossy().to_string();
            let bytes = safe_read(root, &rel, 2 * 1024 * 1024)?;
            out.insert(rel, bytes);
        }
    }
    if out.len() > 128 || out.values().map(Vec::len).sum::<usize>() > 8 * 1024 * 1024 {
        return Err(Error::new(
            "definition_invalid",
            "/skills",
            "method resources exceed 128 files or 8 MiB",
        ));
    }
    Ok(())
}
pub(crate) fn validate_schema(schema: &Value, value: &Value, path: &str) -> Result<()> {
    let v = jsonschema::validator_for(schema)
        .map_err(|e| Error::new("definition_invalid", path, e.to_string()))?;
    if let Err(e) = v.validate(value) {
        return Err(Error::new(
            "input_invalid",
            format!("{path}{}", e.instance_path()),
            e.to_string(),
        ));
    }
    Ok(())
}
pub(crate) fn safe_read(root: &Path, relative: &str, max: u64) -> Result<Vec<u8>> {
    use std::io::Read;
    if relative.is_empty()
        || Path::new(relative)
            .components()
            .any(|x| !matches!(x, Component::Normal(_)))
    {
        return Err(Error::new(
            "artifact_invalid",
            relative,
            "relative normal path required",
        ));
    }
    let root_fd = std::fs::File::open(root)?;
    #[cfg(unix)]
    let file = {
        use std::{
            ffi::CString,
            os::fd::{AsRawFd, FromRawFd},
        };
        let mut current = root_fd;
        let components: Vec<_> = Path::new(relative).components().collect();
        for (i, c) in components.iter().enumerate() {
            let name = CString::new(c.as_os_str().as_encoded_bytes())
                .map_err(|_| Error::new("artifact_invalid", relative, "NUL in path"))?;
            let last = i + 1 == components.len();
            let flags = libc::O_RDONLY
                | libc::O_NOFOLLOW
                | libc::O_CLOEXEC
                | if last { 0 } else { libc::O_DIRECTORY };
            let fd = unsafe { libc::openat(current.as_raw_fd(), name.as_ptr(), flags) };
            if fd < 0 {
                return Err(Error::new(
                    "artifact_invalid",
                    relative,
                    std::io::Error::last_os_error().to_string(),
                ));
            }
            current = unsafe { std::fs::File::from_raw_fd(fd) };
        }
        current
    };
    #[cfg(not(unix))]
    let file = {
        let _ = root_fd;
        return Err(Error::new(
            "capability_denied",
            relative,
            "safe file collection requires Unix in v0.1",
        ));
    };
    let meta = file.metadata()?;
    if !meta.is_file() || meta.len() > max {
        return Err(Error::new(
            "artifact_invalid",
            relative,
            "not a file or exceeds size limit",
        ));
    }
    let mut raw = Vec::new();
    file.take(max + 1).read_to_end(&mut raw)?;
    if raw.len() as u64 > max {
        return Err(Error::new(
            "artifact_invalid",
            relative,
            "file grew past size limit",
        ));
    }
    Ok(raw)
}
