use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Trusted host identity context. Never deserialize this from an untrusted business HTTP request.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Caller {
    pub principal: String,
    pub grants: Vec<String>,
}
impl Caller {
    pub fn local() -> Self {
        Self {
            principal: "local-user".into(),
            grants: vec!["local-readonly".into()],
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceLimits {
    pub max_active_ms: u64,
    pub max_wall_ms: u64,
    pub max_transitions: u64,
    pub max_runs: u64,
}
impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            max_active_ms: 600_000,
            max_wall_ms: 86_400_000,
            max_transitions: 30,
            max_runs: 24,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartRequest {
    pub definition_digest: String,
    pub inputs: Value,
    pub grant_ref: String,
    pub request_key: String,
    #[serde(default)]
    pub limits: ResourceLimits,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignalRequest {
    pub event_name: String,
    pub payload: Value,
    pub request_key: String,
    pub expected_revision: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Created,
    Running,
    Waiting,
    Blocked,
    Cancelling,
    Completed,
    Failed,
    Cancelled,
}
impl Status {
    pub fn terminal(&self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactRef {
    pub id: String,
    pub sha256: String,
    pub slot: String,
    pub state_id: String,
    pub visit: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WaitingSignal {
    pub name: String,
    pub payload_schema_digest: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionSnapshot {
    pub protocol_version: u32,
    pub execution_id: String,
    pub definition_digest: String,
    pub inputs: Value,
    pub status: Status,
    pub current_state: String,
    pub visit: u64,
    pub revision: u64,
    pub business_outcome: Option<String>,
    pub progress: Value,
    pub accepted_artifacts: Vec<ArtifactRef>,
    pub waiting_signals: Vec<WaitingSignal>,
    pub error: Option<crate::Error>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignalReceipt {
    pub request_key: String,
    pub revision: u64,
    pub accepted: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Violation {
    pub code: String,
    pub subject: String,
    pub message: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Pass,
    Fail,
    Error,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckResult {
    pub verdict: Verdict,
    pub outcome: Option<String>,
    pub evidence_refs: Vec<String>,
    pub violations: Vec<Violation>,
}
impl CheckResult {
    pub fn pass(outcome: impl Into<String>, evidence_refs: Vec<String>) -> Self {
        Self {
            verdict: Verdict::Pass,
            outcome: Some(outcome.into()),
            evidence_refs,
            violations: vec![],
        }
    }
    pub fn fail(
        code: impl Into<String>,
        subject: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            verdict: Verdict::Fail,
            outcome: None,
            evidence_refs: vec![],
            violations: vec![Violation {
                code: code.into(),
                subject: subject.into(),
                message: message.into(),
            }],
        }
    }
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self {
            verdict: Verdict::Error,
            outcome: None,
            evidence_refs: vec![],
            violations: vec![Violation {
                code: "check_unavailable".into(),
                subject: "gate".into(),
                message: message.into(),
            }],
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactReceipt {
    pub protocol_version: u32,
    pub id: String,
    pub execution_id: String,
    pub run_id: String,
    pub tool_id: String,
    pub occurred_at: String,
    pub request_sha256: String,
    pub result_sha256: String,
    pub payload: Value,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignalFact {
    pub id: String,
    pub event_name: String,
    pub payload: Value,
    pub received_at: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Grant {
    pub definitions: Vec<String>,
    pub tools: Vec<String>,
    pub limits: ResourceLimits,
}
