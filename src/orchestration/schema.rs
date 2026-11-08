use crate::{Error, Result, json::digest, parse_unique};
use serde_json::Value;
use std::{collections::BTreeMap, sync::Arc};

const CONTRACTS: &[(&str, &str)] = &[
    (
        "goal-request",
        include_str!(
            "../../docs/design/10-goal-dag-orchestration/contract/goal-request.schema.json"
        ),
    ),
    (
        "plan-proposal",
        include_str!(
            "../../docs/design/10-goal-dag-orchestration/contract/plan-proposal.schema.json"
        ),
    ),
    (
        "planner-result",
        include_str!(
            "../../docs/design/10-goal-dag-orchestration/contract/planner-result.schema.json"
        ),
    ),
    (
        "evaluation-candidate",
        include_str!(
            "../../docs/design/10-goal-dag-orchestration/contract/evaluation-candidate.schema.json"
        ),
    ),
    (
        "check-requirement",
        include_str!(
            "../../docs/design/10-goal-dag-orchestration/contract/check-requirement.schema.json"
        ),
    ),
    (
        "check-requirements",
        include_str!(
            "../../docs/design/10-goal-dag-orchestration/contract/check-requirements.schema.json"
        ),
    ),
    (
        "node-contract",
        include_str!(
            "../../docs/design/10-goal-dag-orchestration/contract/node-contract.schema.json"
        ),
    ),
    (
        "dag-entry",
        include_str!("../../docs/design/10-goal-dag-orchestration/contract/dag-entry.schema.json"),
    ),
    (
        "rule-set",
        include_str!("../../docs/design/10-goal-dag-orchestration/contract/rule-set.schema.json"),
    ),
    (
        "check-input",
        include_str!(
            "../../docs/design/10-goal-dag-orchestration/contract/check-input.schema.json"
        ),
    ),
    (
        "orchestration-profile",
        include_str!(
            "../../docs/design/10-goal-dag-orchestration/contract/orchestration-profile.schema.json"
        ),
    ),
];
#[derive(Clone)]
pub(crate) struct SchemaSource {
    pub bytes: Arc<[u8]>,
    pub value: Value,
    pub digest: String,
}
impl SchemaSource {
    pub fn new(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > 1_048_576 {
            return Err(invalid("/schemas", "schema resource exceeds 1 MiB"));
        }
        let value = parse_unique(bytes)?;
        if !value.is_object() && !value.is_boolean() {
            return Err(invalid("/schemas", "expected JSON Schema"));
        }
        Ok(Self {
            bytes: bytes.to_vec().into(),
            value,
            digest: digest(bytes),
        })
    }
}
pub(crate) struct Schemas {
    pub sources: BTreeMap<String, SchemaSource>,
    validators: BTreeMap<String, Arc<jsonschema::Validator>>,
    contracts: BTreeMap<String, Arc<jsonschema::Validator>>,
}
pub(crate) fn invalid(path: &str, message: impl Into<String>) -> Error {
    Error::new("input_invalid", path, message)
}
impl Schemas {
    pub fn compile(application: &BTreeMap<String, SchemaSource>) -> Result<Self> {
        let mut sources = application.clone();
        let mut contract_ids = BTreeMap::new();
        for (name, text) in CONTRACTS {
            let source = SchemaSource::new(text.as_bytes())?;
            let id = source.value["$id"]
                .as_str()
                .ok_or_else(|| invalid("/schemas", "core schema id missing"))?
                .to_owned();
            sources.insert(id.clone(), source.clone());
            contract_ids.insert(name.to_string(), id);
            let alias = format!("_tl.{name}@1");
            sources.insert(alias, source);
        }
        let mut registry = jsonschema::Registry::new();
        for (id, source) in &sources {
            let document = rewrite_refs(&source.value, &sources)?;
            registry = registry
                .add(uri(id), jsonschema::Resource::from_contents(document))
                .map_err(|e| invalid("/schemas", e.to_string()))?;
        }
        let registry = registry
            .prepare()
            .map_err(|e| invalid("/schemas", e.to_string()))?;
        let mut validators = BTreeMap::new();
        for (id, source) in &sources {
            let document = rewrite_refs(&source.value, &sources)?;
            let validator = jsonschema::options()
                .with_draft(jsonschema::Draft::Draft202012)
                .with_base_uri(uri(id))
                .with_registry(&registry)
                .build(&document)
                .map_err(|e| invalid(&format!("/schemas/{id}"), e.to_string()))?;
            validators.insert(id.clone(), Arc::new(validator));
        }
        let contracts = contract_ids
            .into_iter()
            .map(|(name, id)| (name, validators[&id].clone()))
            .collect();
        Ok(Self {
            sources,
            validators,
            contracts,
        })
    }
    pub fn contract(&self, name: &str, value: &Value) -> Result<()> {
        check(
            self.contracts
                .get(name)
                .ok_or_else(|| invalid("/schema", "unknown core contract"))?,
            value,
            "",
        )
    }
    pub fn validate(&self, name: &str, value: &Value) -> Result<()> {
        check(
            self.validators
                .get(name)
                .ok_or_else(|| invalid(&format!("/schemas/{name}"), "unknown schema"))?,
            value,
            "/value",
        )
    }
    pub fn source(&self, name: &str) -> Result<&SchemaSource> {
        self.sources
            .get(name)
            .ok_or_else(|| invalid("/schema_ref", format!("unknown schema {name}")))
    }
    pub fn same(&self, a: &str, b: &str) -> Result<bool> {
        Ok(self.source(a)?.digest == self.source(b)?.digest)
    }
    pub fn resolve_in(&self, name: &str, schema: &Value) -> Result<Value> {
        self.resolve_with_base(name, schema).map(|v| v.0)
    }
    fn resolve_with_base(&self, name: &str, schema: &Value) -> Result<(Value, String)> {
        let mut base = name.to_string();
        let mut value = schema.clone();
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..128 {
            let Some(reference) = value.get("$ref").and_then(Value::as_str).map(str::to_owned)
            else {
                return Ok((value, base));
            };
            if value.as_object().is_some_and(|m| {
                m.keys().any(|k| {
                    !matches!(
                        k.as_str(),
                        "$ref" | "$id" | "$schema" | "title" | "description" | "$comment"
                    )
                })
            }) {
                return Err(invalid("/schema", "selector $ref siblings are unsupported"));
            }
            let (target, pointer) = reference.split_once('#').unwrap_or((&reference, ""));
            if !target.is_empty() {
                base = target.to_string();
            }
            if !seen.insert((base.clone(), pointer.to_string())) {
                return Err(invalid("/schema", "cyclic selector reference"));
            }
            let root = &self.source(&base)?.value;
            value = if pointer.is_empty() {
                root.clone()
            } else {
                root.pointer(pointer)
                    .cloned()
                    .ok_or_else(|| invalid("/schema", "invalid schema pointer"))?
            };
        }
        Err(invalid("/schema", "selector reference depth exceeded"))
    }
    pub(super) fn selected(&self, name: &str, pointer: &str) -> Result<(Value, String)> {
        let mut schema = self.source(name)?.value.clone();
        let mut base = name.to_string();
        if pointer.is_empty() {
            return self.resolve_with_base(&base, &schema);
        }
        if !pointer.starts_with('/') {
            return Err(invalid(
                "/pointer",
                "JSON Pointer must be empty or start with /",
            ));
        }
        for encoded in pointer[1..].split('/') {
            let mut token = String::new();
            let mut chars = encoded.chars();
            while let Some(c) = chars.next() {
                if c == '~' {
                    match chars.next() {
                        Some('0') => token.push('~'),
                        Some('1') => token.push('/'),
                        _ => return Err(invalid("/pointer", "invalid escape")),
                    }
                } else {
                    token.push(c)
                }
            }
            let resolved = self.resolve_with_base(&base, &schema)?;
            schema = resolved.0;
            base = resolved.1;
            if schema["type"] != "object"
                || !schema["required"]
                    .as_array()
                    .is_some_and(|r| r.iter().any(|v| v == &token))
            {
                return Err(invalid(
                    "/pointer",
                    "selector must traverse required object properties",
                ));
            }
            schema = schema["properties"]
                .get(&token)
                .cloned()
                .ok_or_else(|| invalid("/pointer", "unknown property"))?;
        }
        self.resolve_with_base(&base, &schema)
    }
    pub fn selector(&self, name: &str, pointer: &str) -> Result<Value> {
        self.selected(name, pointer).map(|s| s.0)
    }
    /// Compare a statically provable selector to a locked resource, retaining $ref scope.
    pub fn selector_matches(&self, name: &str, pointer: &str, target: &str) -> Result<bool> {
        let (selected, base) = self.selected(name, pointer)?;
        self.equivalent(&base, &selected, target, &self.source(target)?.value)
    }
    pub fn equivalent(&self, a_base: &str, a: &Value, b_base: &str, b: &Value) -> Result<bool> {
        let (a, a_base) = self.resolve_with_base(a_base, a)?;
        let (b, b_base) = self.resolve_with_base(b_base, b)?;
        Ok(self.signature(&a_base, &a)? == self.signature(&b_base, &b)?)
    }
    fn signature(&self, base: &str, value: &Value) -> Result<Value> {
        map_schema(value, &mut |node| {
            node.remove("$id");
            node.remove("$schema");
            if let Some(reference) = node.get("$ref").and_then(Value::as_str) {
                let (id, fragment) = reference.split_once('#').unwrap_or((reference, ""));
                let id = if id.is_empty() { base } else { id };
                let hash = &self.source(id)?.digest;
                node.insert(
                    "$ref".into(),
                    Value::String(format!("sha256:{hash}#{fragment}")),
                );
            }
            Ok(())
        })
    }
    pub fn references(&self, name: &str) -> Result<std::collections::BTreeSet<String>> {
        let mut refs = std::collections::BTreeSet::new();
        map_schema(&self.source(name)?.value, &mut |node| {
            if let Some(r) = node.get("$ref").and_then(Value::as_str) {
                let id = r.split('#').next().unwrap_or_default();
                if !id.is_empty() {
                    self.source(id)?;
                    refs.insert(id.to_string());
                }
            }
            Ok(())
        })?;
        Ok(refs)
    }
}
fn check(validator: &jsonschema::Validator, value: &Value, prefix: &str) -> Result<()> {
    if let Some(error) = validator.iter_errors(value).next() {
        return Err(invalid(
            &format!("{prefix}{}", error.instance_path()),
            error.to_string(),
        ));
    }
    Ok(())
}
fn uri(id: &str) -> String {
    if id.starts_with("taskloop:") {
        id.to_string()
    } else {
        format!("https://taskloop.invalid/schema/{}", digest(id.as_bytes()))
    }
}
fn rewrite_refs(value: &Value, sources: &BTreeMap<String, SchemaSource>) -> Result<Value> {
    fn visit(value: &Value, sources: &BTreeMap<String, SchemaSource>, root: bool) -> Result<Value> {
        let Some(map) = value.as_object() else {
            return Ok(value.clone());
        };
        if map.contains_key("$dynamicRef") {
            return Err(invalid("/schemas", "$dynamicRef is unsupported"));
        }
        if !root && map.contains_key("$id") {
            return Err(invalid("/schemas", "nested schema $id is unsupported"));
        }
        if map
            .get("$vocabulary")
            .and_then(Value::as_object)
            .is_some_and(|m| {
                m.iter().any(|(k, v)| {
                    v == true && !k.starts_with("https://json-schema.org/draft/2020-12/vocab/")
                })
            })
        {
            return Err(invalid("/schemas", "unsupported required vocabulary"));
        }
        if map
            .get("$schema")
            .is_some_and(|v| v != "https://json-schema.org/draft/2020-12/schema")
        {
            return Err(invalid("/schemas", "only JSON Schema 2020-12 is supported"));
        }
        let mut out = map.clone();
        out.remove("$id");
        for (key, v) in map {
            match key.as_str() {
                "$ref" => {
                    let reference = v
                        .as_str()
                        .ok_or_else(|| invalid("/schemas/$ref", "expected string"))?;
                    if reference.starts_with('#') {
                        continue;
                    }
                    let (name, fragment) = reference.split_once('#').unwrap_or((reference, ""));
                    if !sources.contains_key(name) {
                        return Err(invalid(
                            "/schemas/$ref",
                            format!("unlocked reference {name}"),
                        ));
                    }
                    out.insert(
                        key.clone(),
                        Value::String(format!(
                            "{}{}{}",
                            uri(name),
                            if fragment.is_empty() { "" } else { "#" },
                            fragment
                        )),
                    );
                }
                "properties" | "patternProperties" | "$defs" | "definitions"
                | "dependentSchemas" => {
                    let Some(children) = v.as_object() else {
                        continue;
                    };
                    let mut changed = children.clone();
                    for (name, child) in children {
                        changed.insert(name.clone(), visit(child, sources, false)?);
                    }
                    out.insert(key.clone(), Value::Object(changed));
                }
                "allOf" | "anyOf" | "oneOf" | "prefixItems" => {
                    if let Some(children) = v.as_array() {
                        out.insert(
                            key.clone(),
                            Value::Array(
                                children
                                    .iter()
                                    .map(|c| visit(c, sources, false))
                                    .collect::<Result<_>>()?,
                            ),
                        );
                    }
                }
                "items"
                | "contains"
                | "not"
                | "if"
                | "then"
                | "else"
                | "additionalProperties"
                | "unevaluatedProperties"
                | "unevaluatedItems"
                | "propertyNames"
                | "contentSchema" => {
                    out.insert(key.clone(), visit(v, sources, false)?);
                }
                _ => {} // const/default/examples and property names are data, not schema keywords.
            }
        }
        Ok(Value::Object(out))
    }
    visit(value, sources, true)
}

/// Walk only Schema-valued keywords. const/default/examples remain opaque JSON data.
fn map_schema(
    value: &Value,
    f: &mut impl FnMut(&mut serde_json::Map<String, Value>) -> Result<()>,
) -> Result<Value> {
    let Some(original) = value.as_object() else {
        return Ok(value.clone());
    };
    let mut out = original.clone();
    f(&mut out)?;
    for (key, child) in original {
        let mapped = match key.as_str() {
            "properties" | "patternProperties" | "$defs" | "definitions" | "dependentSchemas" => {
                if let Some(children) = child.as_object() {
                    let mut children = children.clone();
                    for value in children.values_mut() {
                        *value = map_schema(value, f)?;
                    }
                    Some(Value::Object(children))
                } else {
                    None
                }
            }
            "allOf" | "anyOf" | "oneOf" | "prefixItems" => child
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .map(|v| map_schema(v, f))
                        .collect::<Result<Vec<_>>>()
                })
                .transpose()?
                .map(Value::Array),
            "items"
            | "contains"
            | "not"
            | "if"
            | "then"
            | "else"
            | "additionalProperties"
            | "unevaluatedProperties"
            | "unevaluatedItems"
            | "propertyNames"
            | "contentSchema" => Some(map_schema(child, f)?),
            _ => None,
        };
        if let Some(mapped) = mapped {
            out.insert(key.clone(), mapped);
        }
    }
    Ok(Value::Object(out))
}
