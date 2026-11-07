use crate::{
    ArtifactReceipt, ArtifactRef, CheckResult, EntryContract, Grant, Result, SignalFact,
    SkillPackage,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub id: String,
    pub description: String,
    pub input_schema: Value,
}
#[derive(Debug, Clone)]
pub struct ArtifactData {
    pub reference: ArtifactRef,
    pub bytes: Arc<[u8]>,
}
#[derive(Debug, Clone)]
pub struct Candidate {
    pub slot: String,
    pub bytes: Arc<[u8]>,
}
impl Candidate {
    pub fn json(slot: impl Into<String>, value: &Value) -> Result<Self> {
        Ok(Self {
            slot: slot.into(),
            bytes: serde_json::to_vec(value)?.into(),
        })
    }
    pub fn bytes(slot: impl Into<String>, bytes: impl Into<Arc<[u8]>>) -> Self {
        Self {
            slot: slot.into(),
            bytes: bytes.into(),
        }
    }
    pub fn value(&self) -> Result<Value> {
        crate::parse_unique(&self.bytes)
    }
    pub fn sha256(&self) -> String {
        crate::json::digest(&self.bytes)
    }
}
#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub payload: Value,
    pub files: Vec<Candidate>,
}
impl ToolOutput {
    pub fn json(payload: Value) -> Self {
        Self {
            payload,
            files: vec![],
        }
    }
}
#[derive(Clone)]
pub struct ToolContext {
    pub inputs: Value,
    pub state_id: String,
    pub visit: u64,
    pub accepted: Vec<ArtifactData>,
    pub signal_facts: Vec<SignalFact>,
    pub(crate) cancellation: CancellationToken,
}
impl ToolContext {
    pub fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }
    pub fn artifacts(&self, state: &str, slot: &str) -> Vec<&ArtifactData> {
        self.accepted
            .iter()
            .filter(|a| a.reference.state_id == state && a.reference.slot == slot)
            .collect()
    }
    pub fn latest(&self, state: &str, slot: &str) -> Option<&ArtifactData> {
        self.artifacts(state, slot)
            .into_iter()
            .max_by_key(|a| a.reference.visit)
    }
}
#[derive(Clone)]
pub struct GateContext {
    pub inputs: Value,
    pub state_id: String,
    pub visit: u64,
    pub candidates: BTreeMap<String, Candidate>,
    pub accepted: Vec<ArtifactData>,
    pub tool_facts: Vec<ArtifactReceipt>,
    pub signal_facts: Vec<SignalFact>,
    pub check_evidence_ref: String,
    pub(crate) cancellation: CancellationToken,
}
impl GateContext {
    pub fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }
    pub async fn cancelled(&self) {
        self.cancellation.cancelled().await;
    }

    pub fn output(&self, slot: &str) -> Result<Value> {
        self.candidates
            .get(slot)
            .ok_or_else(|| {
                crate::Error::new("artifact_invalid", slot, "required candidate missing")
            })?
            .value()
    }
    pub fn artifacts(&self, state: &str, slot: &str) -> Vec<&ArtifactData> {
        self.accepted
            .iter()
            .filter(|a| a.reference.state_id == state && a.reference.slot == slot)
            .collect()
    }
    pub fn latest(&self, state: &str, slot: &str) -> Option<&ArtifactData> {
        self.artifacts(state, slot)
            .into_iter()
            .max_by_key(|a| a.reference.visit)
    }
}
#[async_trait]
pub trait DomainTool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    async fn call(&self, context: ToolContext, args: Value) -> Result<ToolOutput>;
}
#[async_trait]
pub trait DomainGate: Send + Sync {
    async fn check(&self, context: GateContext) -> CheckResult;
}
#[derive(Debug, Clone)]
pub struct Invocation {
    /// Exact input envelope already validated against EntryContract.input_schema.
    pub stage_input: Value,
    pub execution_id: String,
    pub run_id: String,
    pub state_id: String,
    pub visit: u64,
    pub entry: EntryContract,
    pub skills: BTreeMap<String, SkillPackage>,
    pub output_schemas: BTreeMap<String, Value>,
    pub inputs: Value,
    pub artifacts: Vec<ArtifactRef>,
    pub signal_facts: Vec<SignalFact>,
    pub work_dir: PathBuf,
    pub timeout_ms: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ExecutionOutcome {
    Completed { usage: Value },
    Failed { code: String, message: String },
    Cancelled,
    StopUnknown { message: String },
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopState {
    Confirmed,
    Unknown,
}
#[derive(Debug, Clone, Copy)]
pub struct ExecutorCapabilities {
    pub bounded_entry: bool,
    pub collect_artifacts: bool,
    pub cancel: bool,
    pub isolated_workspace: bool,
    pub isolated_context: bool,
}
impl ExecutorCapabilities {
    pub const MANAGED: Self = Self {
        bounded_entry: true,
        collect_artifacts: true,
        cancel: true,
        isolated_workspace: true,
        isolated_context: true,
    };
    pub fn managed(self) -> bool {
        self.bounded_entry
            && self.collect_artifacts
            && self.cancel
            && self.isolated_workspace
            && self.isolated_context
    }
}
#[async_trait]
pub trait ToolBridge: Send + Sync {
    fn specs(&self) -> Vec<ToolSpec>;
    async fn call(&self, id: &str, args: Value) -> Result<Value>;
    async fn call_with_metadata(
        &self,
        id: &str,
        args: Value,
        _metadata: crate::ToolCallMetadata,
    ) -> Result<Value> {
        self.call(id, args).await
    }
}
#[async_trait]
pub trait Executor: Send + Sync {
    fn identity(&self) -> Value {
        serde_json::json!({"implementation":std::any::type_name::<Self>()})
    }

    fn capabilities(&self) -> ExecutorCapabilities;
    async fn execute(
        &self,
        invocation: Invocation,
        tools: Arc<dyn ToolBridge>,
        cancel: CancellationToken,
    ) -> ExecutionOutcome;
    /// Only a trusted adapter may confirm that its opaque run has no live execution left.
    async fn stop_or_confirm(&self, _run_id: &str) -> StopState {
        StopState::Unknown
    }
}
#[derive(Clone, Default)]
pub struct HostBindings {
    /// Version of external, immutable policy/provider configuration supplied by the host.
    pub configuration_version: String,
    pub executors: BTreeMap<String, Arc<dyn Executor>>,
    pub tools: BTreeMap<String, Arc<dyn DomainTool>>,
    pub gates: BTreeMap<String, Arc<dyn DomainGate>>,
    pub grants: BTreeMap<String, Grant>,
}
impl HostBindings {
    pub fn configuration_version(mut self, version: impl Into<String>) -> Self {
        self.configuration_version = version.into();
        self
    }

    pub fn executor(mut self, id: impl Into<String>, value: Arc<dyn Executor>) -> Self {
        self.executors.insert(id.into(), value);
        self
    }
    pub fn tool(mut self, value: Arc<dyn DomainTool>) -> Self {
        self.tools.insert(value.spec().id, value);
        self
    }
    pub fn gate(mut self, id: impl Into<String>, value: Arc<dyn DomainGate>) -> Self {
        self.gates.insert(id.into(), value);
        self
    }
    pub fn grant(mut self, id: impl Into<String>, value: Grant) -> Self {
        self.grants.insert(id.into(), value);
        self
    }
}
