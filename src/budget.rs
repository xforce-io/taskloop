//! Execution policy facts. Observation loss never changes these limits.
use crate::{Error, ExecutionOutcome, ResourceLimits, Result};
use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use tokio::time::Instant;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunBudgetPolicy {
    pub policy_id: String,
    pub max_run_ms: Option<u64>,
    pub max_check_ms: Option<u64>,
    pub max_tool_calls: u64,
    pub submission_reserve: u64,
    pub finalize_before_ms: u64,
    pub max_model_iterations: Option<u64>,
    pub max_candidate_bytes: u64,
}
impl RunBudgetPolicy {
    pub(crate) fn validate(&self, host_calls: u64, host_bytes: u64) -> Result<()> {
        if self.policy_id.is_empty()
            || self.policy_id.len() > 128
            || !self
                .policy_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-@".contains(&b))
            || self.max_run_ms == Some(0)
            || self.max_check_ms == Some(0)
            || self.max_model_iterations == Some(0)
            || self.max_run_ms.is_some_and(|n| {
                std::time::Instant::now()
                    .checked_add(std::time::Duration::from_millis(n))
                    .is_none()
            })
            || self.max_check_ms.is_some_and(|n| {
                std::time::Instant::now()
                    .checked_add(std::time::Duration::from_millis(n))
                    .is_none()
            })
            || self.max_tool_calls == 0
            || self.submission_reserve > self.max_tool_calls.min(host_calls)
            || self.max_candidate_bytes == 0
            || self.max_candidate_bytes > crate::store::MAX_BLOB_BYTES
            || host_bytes == 0
        {
            return Err(Error::new(
                "input_invalid",
                "/entry_budgets",
                "invalid explicit execution policy",
            ));
        }
        Ok(())
    }
    pub(crate) fn legacy(calls: u64, bytes: u64) -> Self {
        Self {
            policy_id: "legacy-v1".into(),
            max_run_ms: Some(240_000),
            max_check_ms: Some(120_000),
            max_tool_calls: calls,
            submission_reserve: 0,
            finalize_before_ms: 0,
            max_model_iterations: None,
            max_candidate_bytes: bytes,
        }
    }
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ModelIterationSupport {
    Unsupported,
    RuntimeValidated,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutorBudgetLimits {
    pub max_run_ms: Option<u64>,
    pub model_iterations: ModelIterationSupport,
    pub max_model_iterations: Option<u64>,
    pub max_frame_bytes: Option<u64>,
}
impl Default for ExecutorBudgetLimits {
    fn default() -> Self {
        Self {
            max_run_ms: None,
            model_iterations: ModelIterationSupport::Unsupported,
            max_model_iterations: None,
            max_frame_bytes: None,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskLimitSources {
    pub requested: ResourceLimits,
    pub grant: ResourceLimits,
    pub definition_max_runs: u64,
    pub definition_max_transitions: u64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetLimit {
    ExplorationCalls,
    ToolCalls,
    RunTime,
    TaskWallTime,
    CandidateBytes,
    ModelIterations,
    UnsupportedConstraint,
    FinalizationTime,
    CheckTime,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BudgetHint {
    pub limit: u64,
    pub used: u64,
    pub remaining: u64,
    pub read_remaining: u64,
    pub submission_reserve: u64,
    pub remaining_ms: u64,
    pub mode: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunBudgetSnapshot {
    pub version: u32,
    pub run_id: String,
    pub state_id: String,
    pub entry_id: String,
    pub policy_source: String,
    pub policy: RunBudgetPolicy,
    pub task_active_remaining_ms: u64,
    pub task_wall_remaining_ms: u64,
    pub host_tool_limit: u64,
    pub host_candidate_limit: u64,
    pub executor_limits: ExecutorBudgetLimits,
    pub effective_run_ms: u64,
    pub preparation_ms: u64,
    pub limiting_time_sources: Vec<String>,
    pub deadline_ms: u64,
    pub effective_tool_calls: u64,
    pub effective_candidate_bytes: u64,
    pub calls_used: u64,
    pub calls_rejected: u64,
    pub candidate_bytes_used: u64,
    pub elapsed_ms: u64,
    pub hint: BudgetHint,
    pub limits_hit: Vec<BudgetLimit>,
    pub execution_outcome: Option<String>,
    pub model_iteration_validation: String,
    pub actual_model_iterations: Option<u64>,
    pub effective_check_ms: Option<u64>,
    pub check_task_active_remaining_ms: Option<u64>,
    pub check_task_wall_remaining_ms: Option<u64>,
    pub check_elapsed_ms: Option<u64>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BudgetView {
    pub task_limits: ResourceLimits,
    pub task_limit_sources: Option<TaskLimitSources>,
    pub runs: Vec<RunBudgetSnapshot>,
}
pub(crate) struct BudgetState {
    data: Mutex<RunBudgetSnapshot>,
    started: Instant,
    pub deadline: Instant,
    explicit: bool,
}
impl BudgetState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        run: &str,
        state: &str,
        entry: &str,
        policy: RunBudgetPolicy,
        explicit: bool,
        active: u64,
        wall: u64,
        prep_ms: u64,
        host_calls: u64,
        host_bytes: u64,
        limits: ExecutorBudgetLimits,
    ) -> Result<Self> {
        let stage = policy.max_run_ms.map(|n| n.saturating_sub(prep_ms));
        let candidates = [
            ("task_active", Some(active)),
            ("task_wall", Some(wall)),
            ("entry", stage),
            ("executor", limits.max_run_ms),
        ];
        let effective = candidates.iter().filter_map(|(_, n)| *n).min().unwrap_or(0);
        let limiting_time_sources = candidates
            .iter()
            .filter(|(_, n)| *n == Some(effective))
            .map(|(s, _)| s.to_string())
            .collect();
        let started = Instant::now();
        let deadline = started
            .checked_add(std::time::Duration::from_millis(effective))
            .ok_or_else(|| {
                Error::new(
                    "unsupported_constraint",
                    "/budget/time",
                    "deadline cannot be represented",
                )
            })?;
        let call_limit = policy.max_tool_calls.min(host_calls);
        let bytes = policy.max_candidate_bytes.min(host_bytes);
        let hint = BudgetHint {
            limit: call_limit,
            used: 0,
            remaining: call_limit,
            read_remaining: call_limit.saturating_sub(policy.submission_reserve),
            submission_reserve: policy.submission_reserve.min(call_limit),
            remaining_ms: effective,
            mode: "explore".into(),
        };
        let validation = if policy.max_model_iterations.is_none() {
            "not_requested"
        } else {
            "pending"
        };
        Ok(Self {
            started,
            deadline,
            explicit,
            data: Mutex::new(RunBudgetSnapshot {
                version: 1,
                run_id: run.into(),
                state_id: state.into(),
                entry_id: entry.into(),
                policy_source: if explicit { "entry" } else { "legacy" }.into(),
                policy: policy.clone(),
                task_active_remaining_ms: active,
                task_wall_remaining_ms: wall,
                host_tool_limit: host_calls,
                host_candidate_limit: host_bytes,
                executor_limits: limits,
                effective_run_ms: effective,
                preparation_ms: prep_ms,
                limiting_time_sources,
                deadline_ms: crate::runtime::now_ms().saturating_add(effective),
                effective_tool_calls: call_limit,
                effective_candidate_bytes: bytes,
                calls_used: 0,
                calls_rejected: 0,
                candidate_bytes_used: 0,
                elapsed_ms: 0,
                hint,
                limits_hit: vec![],
                execution_outcome: None,
                model_iteration_validation: validation.into(),
                actual_model_iterations: None,
                effective_check_ms: None,
                check_task_active_remaining_ms: None,
                check_task_wall_remaining_ms: None,
                check_elapsed_ms: None,
            }),
        })
    }
    fn update(&self, d: &mut RunBudgetSnapshot) {
        if d.execution_outcome.is_none() {
            d.elapsed_ms = self.started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        }
        let remaining = d.effective_run_ms.saturating_sub(d.elapsed_ms);
        let left = d.effective_tool_calls.saturating_sub(d.calls_used);
        let reads = d
            .effective_tool_calls
            .saturating_sub(d.policy.submission_reserve)
            .saturating_sub(d.calls_used);
        let mode = if d.execution_outcome.is_some() {
            "ended"
        } else if remaining == 0 || left == 0 {
            "exhausted"
        } else if reads == 0 || remaining <= d.policy.finalize_before_ms {
            "finalize"
        } else {
            "explore"
        };
        d.hint = BudgetHint {
            limit: d.effective_tool_calls,
            used: d.calls_used,
            remaining: left,
            read_remaining: reads,
            submission_reserve: d.policy.submission_reserve.min(d.effective_tool_calls),
            remaining_ms: remaining,
            mode: mode.into(),
        };
    }
    fn hit(d: &mut RunBudgetSnapshot, limit: BudgetLimit) {
        if !d.limits_hit.contains(&limit) {
            d.limits_hit.push(limit);
        }
    }
    pub fn snapshot(&self) -> RunBudgetSnapshot {
        let mut d = self.data.lock().unwrap_or_else(|e| e.into_inner());
        self.update(&mut d);
        d.clone()
    }
    pub fn explicit(&self) -> bool {
        self.explicit
    }
    pub fn hint(&self) -> Option<BudgetHint> {
        self.explicit.then(|| self.snapshot().hint)
    }
    pub fn guard_time(&self) -> Result<()> {
        if self
            .data
            .lock()
            .map_err(Error::io)?
            .execution_outcome
            .is_some()
        {
            return Err(Error::new(
                "stale_attempt",
                "/budget",
                "executor invocation already ended",
            ));
        }
        if Instant::now() >= self.deadline {
            let mut d = self.data.lock().unwrap_or_else(|e| e.into_inner());
            Self::hit(&mut d, BudgetLimit::RunTime);
            return Err(Error::new(
                "limit_reached",
                "/budget/time",
                "execution deadline reached",
            ));
        }
        Ok(())
    }
    pub(crate) fn admit_tool_with(
        &self,
        id: &str,
        admit: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        let mut d = self.data.lock().map_err(Error::io)?;
        self.update(&mut d);
        let limit = if d.hint.remaining_ms == 0 {
            Some(BudgetLimit::RunTime)
        } else if d.calls_used >= d.effective_tool_calls {
            Some(BudgetLimit::ToolCalls)
        } else if id != "artifact.write"
            && d.calls_used
                >= d.effective_tool_calls
                    .saturating_sub(d.policy.submission_reserve)
        {
            Some(BudgetLimit::ExplorationCalls)
        } else if id != "artifact.write" && d.hint.remaining_ms <= d.policy.finalize_before_ms {
            Some(BudgetLimit::FinalizationTime)
        } else {
            None
        };
        if let Some(limit) = limit {
            d.calls_rejected = d.calls_rejected.saturating_add(1);
            Self::hit(&mut d, limit);
            return Err(Error::new(
                "limit_reached",
                "/budget/tools",
                "tool refused by execution budget; use admitted submission path while budget remains",
            ));
        }
        if d.execution_outcome.is_some() {
            return Err(Error::new(
                "stale_attempt",
                "/budget",
                "executor invocation already ended",
            ));
        }
        if let Err(error) = admit() {
            d.calls_rejected = d.calls_rejected.saturating_add(1);
            return Err(error);
        }
        d.calls_used += 1;
        self.update(&mut d);
        Ok(())
    }
    pub(crate) fn admit_bytes_with(
        &self,
        bytes: u64,
        retained: u64,
        admit: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        self.guard_time()?;
        let mut d = self.data.lock().map_err(Error::io)?;
        if retained > d.effective_candidate_bytes
            || d.candidate_bytes_used.saturating_add(bytes) > d.effective_candidate_bytes
        {
            Self::hit(&mut d, BudgetLimit::CandidateBytes);
            return Err(Error::new(
                "limit_reached",
                "/budget/candidates",
                "candidate retained or cumulative byte limit reached",
            ));
        }
        if d.execution_outcome.is_some() {
            return Err(Error::new(
                "stale_attempt",
                "/budget",
                "executor invocation already ended",
            ));
        }
        admit()?;
        d.candidate_bytes_used += bytes;
        Ok(())
    }
    pub fn mark(&self, limit: BudgetLimit) {
        let mut d = self.data.lock().unwrap_or_else(|e| e.into_inner());
        Self::hit(&mut d, limit);
    }
    pub fn model_constraint_supported(&self) -> bool {
        let d = self.data.lock().unwrap_or_else(|e| e.into_inner());
        d.policy.max_model_iterations.is_none_or(|n| {
            d.executor_limits.model_iterations != ModelIterationSupport::Unsupported
                && d.executor_limits
                    .max_model_iterations
                    .is_none_or(|max| n <= max)
        })
    }
    pub fn finish(&self, outcome: &ExecutionOutcome) {
        let mut d = self.data.lock().unwrap_or_else(|e| e.into_inner());
        self.update(&mut d);
        let name = match outcome {
            ExecutionOutcome::Completed { usage } => {
                if d.policy.max_model_iterations.is_some()
                    && usage["budget_constraints"]["model_iterations_validated"] == true
                {
                    d.model_iteration_validation = "accepted".into();
                }
                "completed"
            }
            ExecutionOutcome::Failed { code, .. } => {
                if code == "model_iteration_limit" {
                    Self::hit(&mut d, BudgetLimit::ModelIterations);
                    d.model_iteration_validation = "accepted".into();
                } else if code == "unsupported_constraint" {
                    Self::hit(&mut d, BudgetLimit::UnsupportedConstraint);
                    d.model_iteration_validation = "rejected".into();
                }
                "failed"
            }
            ExecutionOutcome::Cancelled => "cancelled",
            ExecutionOutcome::StopUnknown { .. } => "stop_unknown",
        };
        d.execution_outcome = Some(name.into());
        self.update(&mut d);
    }
    pub fn check_start(&self, active: u64, wall: u64) -> u64 {
        let mut d = self.data.lock().unwrap_or_else(|e| e.into_inner());
        let limit = active
            .min(wall)
            .min(d.policy.max_check_ms.unwrap_or(u64::MAX));
        d.effective_check_ms = Some(limit);
        d.check_task_active_remaining_ms = Some(active);
        d.check_task_wall_remaining_ms = Some(wall);
        limit
    }
    pub fn check_end(&self, elapsed: u64) {
        self.data
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .check_elapsed_ms = Some(elapsed);
    }
}
