use super::{admission::PreparedGoal, check::IssuedCheck, control::PermissionFacts, *};
use crate::{ArtifactRef, Error, RunBudgetSnapshot, SignalFact};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GoalNodeStatus {
    Pending,
    Ready,
    Running,
    Checking,
    WaitingInput,
    Succeeded,
    Failed,
    Cancelled,
    DependencyFailed,
    StopUnknown,
    Interrupted,
}
impl GoalNodeStatus {
    pub(crate) fn terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded
                | Self::Failed
                | Self::Cancelled
                | Self::DependencyFailed
                | Self::Interrupted
        )
    }
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GoalMemberStatus {
    Pending,
    Reserved,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    Suspended,
    StopUnknown,
    Interrupted,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GoalRunPhase {
    Reserved,
    Suspended,
    Started,
    ExecutionReturned,
    Checking,
    Settled,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum GoalRunRole {
    Planner {
        attempt: u64,
    },
    PlanCheck {
        check_id: String,
        proposal_digest: String,
    },
    NodeMember {
        node_id: String,
        member_id: String,
    },
    FinalCheck {
        check_id: String,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalArtifact {
    pub protocol_version: u32,
    pub artifact: ArtifactRef,
    pub goal_id: String,
    pub node_id: Option<String>,
    pub member_id: Option<String>,
    pub plan_digest: Option<String>,
    pub schema_ref: String,
    pub schema_digest: String,
    pub producer_kind: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalMemberSnapshot {
    pub status: GoalMemberStatus,
    pub run_id: Option<String>,
    pub outputs: BTreeMap<String, ArtifactRef>,
    pub error: Option<Error>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalNodeSnapshot {
    pub capability_ref: String,
    pub status: GoalNodeStatus,
    pub input_digest: Option<String>,
    pub members: BTreeMap<String, GoalMemberSnapshot>,
    pub outputs: BTreeMap<String, ArtifactRef>,
    pub error: Option<Error>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalRunSnapshot {
    pub run_id: String,
    pub role: GoalRunRole,
    pub entry_ref: String,
    pub epoch: u64,
    pub phase: GoalRunPhase,
    pub started: bool,
    pub native_stop_confirmed: bool,
    pub stop_unknown: bool,
    pub interrupted: bool,
    pub budget: Option<RunBudgetSnapshot>,
    pub error: Option<Error>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalWaiting {
    pub wait_id: String,
    pub nonce: String,
    pub node_id: Option<String>,
    pub schema_ref: String,
    pub payload_schema_digest: String,
    pub expires_at_utc_ms: u64,
    pub consumed: bool,
    pub reason: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalSnapshot {
    pub protocol_version: u32,
    pub goal_id: String,
    pub revision: u64,
    pub spec: GoalSpec,
    pub spec_digest: String,
    pub profile_digest: String,
    pub plan_digest: Option<String>,
    pub status: GoalStatus,
    pub stage: GoalStage,
    pub outcome: Option<GoalOutcome>,
    pub terminal_intent: Option<GoalTerminalIntent>,
    pub blockers: BTreeSet<GoalBlocker>,
    pub admitted_at_utc_ms: u64,
    pub wall_deadline_utc_ms: u64,
    pub planning_deadline_utc_ms: u64,
    pub nodes: BTreeMap<String, GoalNodeSnapshot>,
    pub runs: BTreeMap<String, GoalRunSnapshot>,
    pub waiting: Vec<GoalWaiting>,
    pub artifacts: Vec<GoalArtifact>,
    pub evaluations: Vec<GoalEvaluationSummary>,
    pub budget: GoalBudget,
    pub limit_sources: GoalLimitSources,
    pub deadline_expired: bool,
    pub stop: GoalStopSummary,
    pub limits: GoalLimits,
    pub error: Option<Error>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalEvaluationSummary {
    pub evaluation_id: Option<String>,
    pub check_id: String,
    pub phase: CheckInputPhase,
    pub status: String,
    pub verdict: Option<EvaluationVerdict>,
    pub receipt_id: Option<String>,
    pub error: Option<Error>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockedPlan {
    pub protocol_version: u32,
    pub goal_id: String,
    pub plan_digest: String,
    pub proposal: PlanProposal,
    pub accepted_checks: Vec<String>,
    pub locked_at_utc_ms: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalReceipt {
    pub protocol_version: u32,
    pub goal_id: String,
    pub revision: u64,
    pub replayed: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalSignalRequest {
    pub wait_id: String,
    pub nonce: String,
    pub payload: Value,
    pub request_key: String,
    pub expected_revision: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalCommandReceipt {
    pub protocol_version: u32,
    pub goal_id: String,
    pub request_key: String,
    pub kind: String,
    pub applied_revision: u64,
    pub accepted: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct GoalNodeRecord {
    pub view: GoalNodeSnapshot,
    pub input_sha: Option<String>,
    pub input_artifact_ids: Vec<String>,
    pub check_request_id: Option<String>,
    pub checking_started_at_ms: Option<u64>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct GoalRunRequest {
    pub request_id: String,
    pub role: GoalRunRole,
    pub entry_ref: String,
    pub epoch: u64,
    pub input_sha: String,
    pub input_digest: String,
    pub input_artifact_ids: Vec<String>,
    pub check_request_id: Option<String>,
    pub run_id: Option<String>,
    pub cancelled: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct GoalRunRecord {
    pub view: GoalRunSnapshot,
    pub request_id: String,
    pub input_sha: String,
    pub executor: String,
    pub executor_binding: String,
    pub candidates: BTreeMap<String, String>,
    pub started_at_utc_ms: Option<u64>,
    pub returned_at_utc_ms: Option<u64>,
    pub usage_sha: Option<String>,
    pub business_disposed: bool,
    pub settlement_sequence: u64,
    pub active_ms: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct GoalCheckRecord {
    pub view: GoalEvaluationSummary,
    pub input_sha: String,
    pub input_digest: String,
    pub producer_run_id: Option<String>,
    pub candidate_sha: Option<String>,
    pub receipt_sha: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ProposalRecord {
    pub attempt: u64,
    pub candidate_sha: String,
    pub proposal_digest: Option<String>,
    pub failure: Option<PlanValidationFailure>,
    pub check_ids: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct GoalRecord {
    pub prepared: PreparedGoal,
    pub raw_request_sha: String,
    pub status: GoalStatus,
    pub stage: GoalStage,
    pub outcome: Option<GoalOutcome>,
    pub terminal_intent: Option<GoalTerminalIntent>,
    pub pause_at_utc_ms: Option<u64>,
    pub blockers: BTreeSet<GoalBlocker>,
    pub epoch: u64,
    pub binding_fingerprint: String,
    pub plan: Option<LockedPlan>,
    pub proposals: Vec<ProposalRecord>,
    pub planning_attempts: u64,
    pub nodes: BTreeMap<String, GoalNodeRecord>,
    pub requests: BTreeMap<String, GoalRunRequest>,
    pub runs: BTreeMap<String, GoalRunRecord>,
    pub waiting: BTreeMap<String, GoalWaiting>,
    pub checks: BTreeMap<String, GoalCheckRecord>,
    pub unissued_checks: BTreeMap<String, GoalEvaluationSummary>,
    pub artifacts: BTreeMap<String, GoalArtifact>,
    pub signals: Vec<SignalFact>,
    #[serde(skip)]
    pub budget: GoalBudget,
    pub error: Option<Error>,
}
impl GoalRecord {
    pub(crate) fn new(
        prepared: PreparedGoal,
        raw_request_sha: String,
        binding_fingerprint: String,
    ) -> Self {
        Self {
            prepared,
            raw_request_sha,
            status: GoalStatus::Running,
            stage: GoalStage::Planning,
            outcome: None,
            terminal_intent: None,
            pause_at_utc_ms: None,
            blockers: BTreeSet::new(),
            epoch: 1,
            binding_fingerprint,
            plan: None,
            proposals: Vec::new(),
            planning_attempts: 0,
            nodes: BTreeMap::new(),
            requests: BTreeMap::new(),
            runs: BTreeMap::new(),
            waiting: BTreeMap::new(),
            checks: BTreeMap::new(),
            unissued_checks: BTreeMap::new(),
            artifacts: BTreeMap::new(),
            signals: Vec::new(),
            budget: GoalBudget {
                transitions_used: 1,
                ..GoalBudget::default()
            },
            error: None,
        }
    }
    pub(crate) fn permissions(&self, now_ms: u64, clock_trusted: bool) -> PermissionFacts {
        PermissionFacts {
            status: self.status,
            stage: self.stage,
            terminal: self.terminal_intent.is_some(),
            blocked: !self.blockers.is_empty(),
            clock_trusted,
            pause_at_ms: self.pause_at_utc_ms,
            epoch: self.epoch,
            now_ms,
            wall_deadline_ms: self.prepared.wall_deadline_utc_ms,
            planning_deadline_ms: self.prepared.planning_deadline_utc_ms,
        }
    }
    pub(crate) fn snapshot(&self, id: &str, revision: u64) -> GoalSnapshot {
        let now = crate::runtime::now_ms();
        GoalSnapshot {
            protocol_version: 1,
            goal_id: id.into(),
            revision,
            spec: self.prepared.request.spec.clone(),
            spec_digest: self.prepared.spec_digest.clone(),
            profile_digest: self.prepared.profile_digest.clone(),
            plan_digest: self.plan.as_ref().map(|p| p.plan_digest.clone()),
            status: self.status,
            stage: self.stage,
            outcome: self.outcome,
            terminal_intent: self.terminal_intent.clone(),
            blockers: self.blockers.clone(),
            admitted_at_utc_ms: self.prepared.admitted_at_utc_ms,
            wall_deadline_utc_ms: self.prepared.wall_deadline_utc_ms,
            planning_deadline_utc_ms: self.prepared.planning_deadline_utc_ms,
            nodes: self
                .nodes
                .iter()
                .map(|(id, n)| (id.clone(), n.view.clone()))
                .collect(),
            runs: self
                .runs
                .iter()
                .map(|(id, r)| (id.clone(), r.view.clone()))
                .collect(),
            waiting: self
                .waiting
                .values()
                .filter(|w| !w.consumed)
                .cloned()
                .collect(),
            artifacts: self.artifacts.values().cloned().collect(),
            evaluations: self
                .checks
                .values()
                .map(|c| c.view.clone())
                .chain(self.unissued_checks.values().cloned())
                .collect(),
            budget: self.budget.clone(),
            limit_sources: self.prepared.limit_sources.clone(),
            deadline_expired: self.status != GoalStatus::Finished
                && (now >= self.prepared.wall_deadline_utc_ms
                    || matches!(self.stage, GoalStage::Planning | GoalStage::PlanChecking)
                        && now >= self.prepared.planning_deadline_utc_ms
                    || self
                        .waiting
                        .values()
                        .any(|w| !w.consumed && now >= w.expires_at_utc_ms)),
            stop: GoalStopSummary {
                pause_at_utc_ms: self.pause_at_utc_ms,
                terminal_at_utc_ms: self.terminal_intent.as_ref().map(|i| i.at_utc_ms),
                pending_runs: self
                    .runs
                    .iter()
                    .filter(|(_, r)| r.view.phase != GoalRunPhase::Settled)
                    .map(|(id, _)| id.clone())
                    .collect(),
                stop_unknown_runs: self
                    .runs
                    .iter()
                    .filter(|(_, r)| r.view.stop_unknown)
                    .map(|(id, _)| id.clone())
                    .collect(),
            },
            limits: self.prepared.effective_limits.clone(),
            error: self.error.clone(),
        }
    }
}
#[derive(Default)]
pub(crate) struct GoalCommitEffects {
    pub artifacts: Vec<GoalArtifact>,
    pub checks: Vec<(IssuedCheck, Option<EvaluationReceipt>)>,
    pub plan: Option<LockedPlan>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalStopSummary {
    pub pause_at_utc_ms: Option<u64>,
    pub terminal_at_utc_ms: Option<u64>,
    pub pending_runs: Vec<String>,
    pub stop_unknown_runs: Vec<String>,
}
