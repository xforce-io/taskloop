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
    pub schema_ref: Option<String>,
    pub schema_digest: Option<String>,
    pub attachment_policy_digest: Option<String>,
    pub accepted: bool,
    pub producer_run_id: Option<String>,
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
    /// Derived from the original proposal attempt; never changes its execution outcome.
    #[serde(default)]
    pub plan_validation_failure: Option<PlanValidationFailure>,
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
/// Point-in-time scheduling projection. Host counts disclose no other owner's identities.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct GoalProgress {
    pub pending_requests: u64,
    pub queue_reason: Option<String>,
    pub goal_occupied_slots: u64,
    pub host_occupied_slots: u64,
    pub host_stop_unknown_slots: u64,
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
    #[serde(default)]
    pub progress: GoalProgress,
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
    pub protocol_version: u32,
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
    pub checking_nonce: Option<String>,
    pub checking_epoch: Option<u64>,
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
    pub allocation: GoalBudgetAmount,
    pub run_allocation_ms: u64,
    pub check_allocation_ms: u64,
    pub tool_receipt_shas: Vec<String>,
    #[serde(default)]
    pub execution_outcome: Option<crate::ExecutionOutcome>,
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
    pub validated_proposal_sha: Option<String>,
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
        let pending_requests = self
            .requests
            .values()
            .filter(|r| !r.cancelled && r.run_id.is_none())
            .count()
            + self
                .runs
                .values()
                .filter(|r| !r.view.started && r.view.phase == GoalRunPhase::Suspended)
                .count();
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
                .map(|(id, r)| {
                    let mut view = r.view.clone();
                    if let GoalRunRole::Planner { attempt } = &view.role {
                        view.plan_validation_failure = self
                            .proposals
                            .iter()
                            .find(|p| p.attempt == *attempt)
                            .and_then(|p| p.failure.clone());
                    }
                    (id.clone(), view)
                })
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
            progress: GoalProgress {
                pending_requests: pending_requests as u64,
                queue_reason: (pending_requests > 0
                    && matches!(self.status, GoalStatus::Running | GoalStatus::Waiting)
                    && self.pause_at_utc_ms.is_none()
                    && self.terminal_intent.is_none()
                    && self.blockers.is_empty())
                .then(|| "run_admission_pending".into()),
                ..Default::default()
            },
            stop: GoalStopSummary {
                pause_at_utc_ms: self.pause_at_utc_ms,
                terminal_at_utc_ms: self.terminal_intent.as_ref().map(|i| i.at_utc_ms),
                pending_runs: self
                    .runs
                    .iter()
                    .filter(|(_, r)| {
                        r.view.started && r.view.phase != GoalRunPhase::Settled
                            || !r.view.started && r.view.phase == GoalRunPhase::Reserved
                    })
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
    pub business_edges: Option<u64>,
    pub finish_proofs_valid: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalStopSummary {
    pub pause_at_utc_ms: Option<u64>,
    pub terminal_at_utc_ms: Option<u64>,
    pub pending_runs: Vec<String>,
    pub stop_unknown_runs: Vec<String>,
}

impl GoalRunRole {
    pub(crate) fn order_key(&self) -> (u8, String, String) {
        match self {
            Self::Planner { attempt } => (0, format!("{attempt:03}"), String::new()),
            Self::PlanCheck { check_id, .. } => (1, check_id.clone(), String::new()),
            Self::NodeMember { node_id, member_id } => (2, node_id.clone(), member_id.clone()),
            Self::FinalCheck { check_id } => (3, check_id.clone(), String::new()),
        }
    }
}

pub(crate) fn run_scope_valid(
    run: &GoalRunRecord,
    request: &GoalRunRequest,
    node: Option<&GoalNodeRecord>,
    check: Option<&GoalCheckRecord>,
    stage: GoalStage,
    planning_attempt: u64,
    latest_proposal_digest: Option<&str>,
) -> bool {
    if request.cancelled
        || request.run_id.as_deref() != Some(&run.view.run_id)
        || request.input_sha != run.input_sha
        || request.input_digest != run.input_sha
        || request.epoch != run.view.epoch
        || request.entry_ref != run.view.entry_ref
        || request.role != run.view.role
    {
        return false;
    }
    match &run.view.role {
        GoalRunRole::Planner { attempt } => {
            stage == GoalStage::Planning && *attempt == planning_attempt
        }
        GoalRunRole::PlanCheck {
            check_id,
            proposal_digest,
        } => {
            stage == GoalStage::PlanChecking
                && latest_proposal_digest == Some(proposal_digest.as_str())
                && check.is_some_and(|c| {
                    c.view.check_id == *check_id
                        && c.view.phase == CheckInputPhase::Plan
                        && c.view.status == "issued"
                        && c.producer_run_id.as_deref() == Some(&run.view.run_id)
                        && c.input_sha == run.input_sha
                })
        }
        GoalRunRole::NodeMember { member_id, .. } => {
            stage == GoalStage::Executing
                && node.is_some_and(|n| {
                    matches!(
                        n.view.status,
                        GoalNodeStatus::Running | GoalNodeStatus::Checking
                    ) && n
                        .view
                        .members
                        .get(member_id)
                        .is_some_and(|m| m.run_id.as_deref() == Some(&run.view.run_id))
                })
        }
        GoalRunRole::FinalCheck { check_id } => {
            stage == GoalStage::GoalChecking
                && check.is_some_and(|c| {
                    c.view.check_id == *check_id
                        && c.view.phase == CheckInputPhase::GoalFinal
                        && c.view.status == "issued"
                        && c.producer_run_id.as_deref() == Some(&run.view.run_id)
                        && c.input_sha == run.input_sha
                })
        }
    }
}
impl GoalRecord {
    pub(crate) fn result_authority(&self, id: &str, binding: &str) -> control::ResultAuthority {
        let Some(run) = self.runs.get(id) else {
            return control::ResultAuthority {
                original_source_valid: false,
                ownership_and_digests_valid: false,
                epoch: 0,
                predecessor_valid: false,
            };
        };
        let node = match &run.view.role {
            GoalRunRole::NodeMember { node_id, .. } => self.nodes.get(node_id),
            _ => None,
        };
        let request = self.requests.get(&run.request_id);
        let check = request
            .and_then(|r| r.check_request_id.as_ref())
            .and_then(|id| self.checks.get(id));
        control::ResultAuthority {
            original_source_valid: run.executor_binding == binding
                && run.view.started
                && !run.view.stop_unknown,
            ownership_and_digests_valid: request.is_some_and(|request| {
                run_scope_valid(
                    run,
                    request,
                    node,
                    check,
                    self.stage,
                    self.planning_attempts,
                    self.proposals
                        .last()
                        .and_then(|p| p.proposal_digest.as_deref()),
                )
            }),
            epoch: run.view.epoch,
            predecessor_valid: !run.business_disposed && run.view.phase != GoalRunPhase::Settled,
        }
    }
}
