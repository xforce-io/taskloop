use crate::{Error, Result, definition::collect_skill, json::digest};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, path::Path};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillValidationReport {
    pub name: String,
    pub description: String,
    pub declared_version: Option<String>,
    pub sha256: String,
    pub files: usize,
}
/// Checks a normal Skill package without requiring a Playbook, executing scripts or calling a model.
pub fn validate_skill(root: impl AsRef<Path>) -> Result<SkillValidationReport> {
    let root = root.as_ref().canonicalize()?;
    let mut files = BTreeMap::new();
    collect_skill(&root, &root, &mut files)?;
    validate_skill_files(&root, &files)
}

/// Validates the same immutable byte map that Definition will lock.
pub(crate) fn validate_skill_files(
    root: &Path,
    files: &BTreeMap<String, Vec<u8>>,
) -> Result<SkillValidationReport> {
    let text = files
        .get("SKILL.md")
        .ok_or_else(|| Error::new("definition_invalid", "/SKILL.md", "required file missing"))?;
    let text = std::str::from_utf8(text)
        .map_err(|_| Error::new("definition_invalid", "/SKILL.md", "SKILL.md must be UTF-8"))?;
    let normalized = text.replace("\r\n", "\n");
    let Some(rest) = normalized.strip_prefix("---\n") else {
        return Err(Error::new(
            "definition_invalid",
            "/SKILL.md",
            "YAML frontmatter required",
        ));
    };
    let end = rest
        .find("\n---\n")
        .or_else(|| rest.strip_suffix("\n---").map(str::len))
        .ok_or_else(|| {
            Error::new(
                "definition_invalid",
                "/SKILL.md",
                "frontmatter closing delimiter missing",
            )
        })?;
    let meta: Value = serde_yaml_ng::from_str(&rest[..end])
        .map_err(|e| Error::new("definition_invalid", "/SKILL.md/frontmatter", e.to_string()))?;
    let name = meta.get("name").and_then(Value::as_str).ok_or_else(|| {
        Error::new(
            "definition_invalid",
            "/SKILL.md/name",
            "string name required",
        )
    })?;
    if name.is_empty()
        || name.chars().count() > 64
        || name.starts_with('-')
        || name.ends_with('-')
        || name.contains("--")
        || !name
            .chars()
            .all(|c| c == '-' || (c.is_alphanumeric() && !c.is_uppercase()))
    {
        return Err(Error::new(
            "definition_invalid",
            "/SKILL.md/name",
            "use 1–64 lowercase letters, digits and single hyphens",
        ));
    }
    if root.file_name().and_then(|x| x.to_str()) != Some(name) {
        return Err(Error::new(
            "definition_invalid",
            "/SKILL.md/name",
            "name must match package directory",
        ));
    }
    let description = meta
        .get("description")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            Error::new(
                "definition_invalid",
                "/SKILL.md/description",
                "description string required",
            )
        })?;
    if description.trim().is_empty() || description.chars().count() > 1024 {
        return Err(Error::new(
            "definition_invalid",
            "/SKILL.md/description",
            "description must be nonempty and at most 1024 characters",
        ));
    }
    for field in ["license", "allowed-tools"] {
        if meta.get(field).is_some_and(|value| !value.is_string()) {
            return Err(Error::new(
                "definition_invalid",
                format!("/SKILL.md/{field}"),
                "optional Skill field must be a string",
            ));
        }
    }
    if let Some(compat) = meta.get("compatibility")
        && compat
            .as_str()
            .is_none_or(|s| s.trim().is_empty() || s.chars().count() > 500)
    {
        return Err(Error::new(
            "definition_invalid",
            "/SKILL.md/compatibility",
            "compatibility must be a string of at most 500 characters",
        ));
    }
    if let Some(m) = meta.get("metadata")
        && m.as_object()
            .is_none_or(|m| m.values().any(|v| !v.is_string()))
    {
        return Err(Error::new(
            "definition_invalid",
            "/SKILL.md/metadata",
            "metadata must map strings to strings",
        ));
    }
    Ok(SkillValidationReport {
        name: name.into(),
        description: description.into(),
        declared_version: meta
            .pointer("/metadata/version")
            .and_then(Value::as_str)
            .map(str::to_string),
        sha256: digest(&serde_json::to_vec(files)?),
        files: files.len(),
    })
}
