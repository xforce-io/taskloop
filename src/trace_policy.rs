//! Host-owned policy; never deserialized from Agent or business input.
use crate::{Error, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, sync::Arc};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TraceToolContent {
    pub input: Vec<String>,
    pub output: Vec<String>,
}
#[async_trait]
pub trait TraceContentFilter: Send + Sync {
    /// Receives only selected fields. Remove domain-specific secrets before returning.
    /// Runs off the business tool path; the future must yield and be cancellation-safe.
    /// Perform blocking work outside the async executor.
    async fn redact(&self, tool: &str, direction: &str, selected: Value) -> Result<Value>;
}
#[derive(Clone)]
pub struct TracePolicy {
    pub policy_id: String,
    pub max_total_events: u64,
    /// Retained serialized event and content bytes; excludes SQLite page overhead.
    pub max_total_bytes: u64,
    pub max_runs: u32,
    pub retention_ms: u64,
    pub max_content_bytes: u64,
    pub content_tools: BTreeMap<String, TraceToolContent>,
    pub filter_version: Option<String>,
    pub filter: Option<Arc<dyn TraceContentFilter>>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TracePolicySnapshot {
    pub version: u32,
    pub policy_id: String,
    pub content_mode: String,
    pub max_total_events: Option<u64>,
    pub max_total_bytes: Option<u64>,
    pub max_runs: Option<u32>,
    pub retention_ms: Option<u64>,
    pub max_content_bytes: u64,
    pub content_tools: BTreeMap<String, TraceToolContent>,
    pub filter_version: Option<String>,
    pub overflow: String,
    pub source_queue_events: u32,
    pub source_callback_ms: u32,
    pub source_drain_ms: u32,
    pub content_concurrency: u32,
    #[serde(default)]
    pub source_cleanup_timeout_ms: Option<u32>,
    #[serde(default)]
    pub source_cleanup_poll_ms: Option<u32>,
    #[serde(default)]
    pub source_cleanup_retry_ms: Option<u32>,
}
impl PartialEq for TraceToolContent {
    fn eq(&self, other: &Self) -> bool {
        self.input == other.input && self.output == other.output
    }
}
impl TracePolicy {
    pub(crate) fn validate(&self) -> Result<()> {
        let valid_id = |s: &str| {
            !s.is_empty()
                && s.len() <= 128
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-@".contains(&b))
        };
        if !valid_id(&self.policy_id)
            || !(2..=1_000_000_000).contains(&self.max_total_events)
            || !(4096..=1_099_511_627_776).contains(&self.max_total_bytes)
            || !(1..=10_000).contains(&self.max_runs)
            || self.retention_ms == 0
            || self.retention_ms > i64::MAX as u64 / 2
            || self.max_content_bytes > 67_108_864
            || self.content_tools.len() > 64
        {
            return Err(invalid());
        }
        if self.content_tools.is_empty() {
            if self.filter.is_some() || self.filter_version.is_some() {
                return Err(invalid());
            }
        } else if self.filter.is_none()
            || !self.filter_version.as_deref().is_some_and(valid_id)
            || self.max_content_bytes == 0
            || self.max_content_bytes > self.max_total_bytes
        {
            return Err(invalid());
        }
        for (tool, selection) in &self.content_tools {
            if tool.is_empty()
                || tool.len() > 128
                || selection.input.len() > 32
                || selection.output.len() > 32
                || selection.input.is_empty() && selection.output.is_empty()
            {
                return Err(invalid());
            }
            for pointer in selection.input.iter().chain(&selection.output) {
                if !valid_pointer(pointer) {
                    return Err(invalid());
                }
            }
        }
        if serde_json::to_vec(&self.snapshot())?.len() > 65536 {
            return Err(invalid());
        }
        Ok(())
    }
    pub(crate) fn snapshot(&self) -> TracePolicySnapshot {
        TracePolicySnapshot {
            version: 1,
            policy_id: self.policy_id.clone(),
            content_mode: if self.content_tools.is_empty() {
                "metadata_only"
            } else {
                "selected_tool_fields"
            }
            .into(),
            max_total_events: Some(self.max_total_events),
            max_total_bytes: Some(self.max_total_bytes),
            max_runs: Some(self.max_runs),
            retention_ms: Some(self.retention_ms),
            max_content_bytes: self.max_content_bytes,
            content_tools: self.content_tools.clone(),
            filter_version: self.filter_version.clone(),
            overflow: "drop_new; continue_business; report_incomplete".into(),
            source_queue_events: 256,
            source_callback_ms: 200,
            source_drain_ms: crate::milkie::OBSERVATION_TAIL_MS,
            content_concurrency: 8,
            source_cleanup_timeout_ms: Some(5000),
            source_cleanup_poll_ms: Some(500),
            source_cleanup_retry_ms: Some(1000),
        }
    }
    pub(crate) fn select(&self, tool: &str, direction: &str, value: &Value) -> Option<Value> {
        let selection = self.content_tools.get(tool)?;
        let fields = match direction {
            "input" => &selection.input,
            "output" => &selection.output,
            _ => return None,
        };
        let selected = fields
            .iter()
            .filter_map(|p| value.pointer(p).map(|v| (p.clone(), v.clone())))
            .collect::<serde_json::Map<_, _>>();
        if selected.is_empty() {
            None
        } else {
            Some(Value::Object(selected))
        }
    }
}
impl TracePolicySnapshot {
    pub(crate) fn legacy() -> Self {
        Self {
            version: 1,
            policy_id: "legacy-metadata-manual".into(),
            content_mode: "metadata_only".into(),
            max_total_events: None,
            max_total_bytes: None,
            max_runs: None,
            retention_ms: None,
            max_content_bytes: 0,
            content_tools: BTreeMap::new(),
            filter_version: None,
            overflow: "per_run_drop_new; continue_business; report_incomplete".into(),
            source_queue_events: 256,
            source_callback_ms: 200,
            source_drain_ms: crate::milkie::OBSERVATION_TAIL_MS,
            content_concurrency: 8,
            source_cleanup_timeout_ms: Some(5000),
            source_cleanup_poll_ms: Some(500),
            source_cleanup_retry_ms: Some(1000),
        }
    }
}
fn invalid() -> Error {
    Error::new(
        "input_invalid",
        "/trace/policy",
        "invalid explicit trace policy or content selection",
    )
}
fn sensitive(key: &str) -> bool {
    let normalized: String = key
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect();
    normalized == "env"
        || [
            "authorization",
            "password",
            "passwd",
            "secret",
            "apikey",
            "token",
            "cookie",
            "privatekey",
            "environment",
        ]
        .iter()
        .any(|s| normalized.contains(s))
}
fn valid_pointer(pointer: &str) -> bool {
    if !pointer.starts_with('/') || pointer.len() > 256 {
        return false;
    }
    for part in pointer[1..].split('/') {
        let mut chars = part.chars();
        while let Some(c) = chars.next() {
            if c == '~' && !matches!(chars.next(), Some('0' | '1')) {
                return false;
            }
        }
        let decoded = part.replace("~1", "/").replace("~0", "~");
        if sensitive(&decoded) {
            return false;
        }
    }
    true
}
pub(crate) fn redact_sensitive(value: Value) -> Value {
    fn walk(value: Value, depth: u32) -> Value {
        if depth >= 32 {
            return Value::String("[REDACTED_DEPTH]".into());
        }
        match value {
            Value::Object(map) => Value::Object(
                map.into_iter()
                    .map(|(key, value)| {
                        let v = if sensitive(&key) {
                            Value::String("[REDACTED]".into())
                        } else {
                            walk(value, depth + 1)
                        };
                        (key, v)
                    })
                    .collect(),
            ),
            Value::Array(values) => {
                Value::Array(values.into_iter().map(|v| walk(v, depth + 1)).collect())
            }
            v => v,
        }
    }
    walk(value, 0)
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceContentRef {
    pub id: String,
    pub run_id: String,
    pub span_id: String,
    pub tool: String,
    pub direction: String,
    pub sha256: String,
    pub bytes: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceContent {
    pub reference: TraceContentRef,
    pub value: Value,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn rejects_root_secrets_and_bad_escapes_but_handles_json_pointer() {
        for p in ["", "foo", "/auth/token", "/ENV/key", "/x~2y"] {
            assert!(!valid_pointer(p), "{p}");
        }
        for p in ["/result", "/a~1b/0", "/a~0b"] {
            assert!(valid_pointer(p), "{p}");
        }
    }
    #[test]
    fn redacts_nested_sensitive_fields() {
        let v = redact_sensitive(
            json!({"result":{"authorization":"SECRET","items":[{"password":"SECRET","safe":"kept"}]},"environment":{"X":"SECRET"}}),
        );
        assert!(!v.to_string().contains("SECRET"));
        assert!(v.to_string().contains("kept"));
    }
}
