use super::{
    admission::PreparedGoal, goal_budget::BudgetAdmission, plan::bounded_json, record::*,
    registry::FrozenRegistry, *,
};
use crate::{
    json::digest,
    runtime::Inner,
    store::{
        CommandReceipt,
        goals::{GoalCommit, GoalRow, GoalWriteGuard},
    },
    *,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

pub(crate) struct GoalCore {
    pub inner: Arc<Inner>,
    pub registry: Arc<FrozenRegistry>,
    pub options: OrchestrationOptions,
    pub clock: Arc<GoalClock>,
    pub binding: String,
    pub coordinator: Mutex<()>,
    pub root_traces: Mutex<BTreeMap<String, Arc<crate::trace::RunTrace>>>,
}
#[derive(Clone)]
pub struct Goals {
    pub(crate) core: Arc<GoalCore>,
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum CommandResponse {
    Accepted { receipt: GoalCommandReceipt },
    Rejected { error: Error },
}
#[derive(Clone, Copy)]
pub(crate) enum GoalControlCommand {
    Cancel,
    Stop,
    Resume,
}
impl GoalControlCommand {
    fn name(self) -> &'static str {
        match self {
            Self::Cancel => "cancel",
            Self::Stop => "stop",
            Self::Resume => "resume",
        }
    }
}
impl GoalCore {
    pub fn new(
        inner: Arc<Inner>,
        registry: Arc<FrozenRegistry>,
        options: OrchestrationOptions,
        binding: String,
    ) -> Result<Arc<Self>> {
        options.validate()?;
        let clock = Arc::new(GoalClock::new(inner.store.goal_clock_highwater()?));
        Ok(Arc::new(Self {
            inner,
            registry,
            options,
            clock,
            binding,
            coordinator: Mutex::new(()),
            root_traces: Mutex::new(BTreeMap::new()),
        }))
    }
    pub fn authorized(&self, id: &str, caller: &Caller) -> Result<GoalRow> {
        let row = self.inner.store.goal_load(id)?;
        if row.owner != caller.principal {
            return Err(Error::new("not_found", "/goal", "Goal not found"));
        }
        Ok(row)
    }
    fn open(&self) -> Result<()> {
        if self.inner.stop.is_cancelled() {
            Err(Error::new(
                "runtime_closed",
                "/runtime",
                "Runtime is closed",
            ))
        } else {
            Ok(())
        }
    }
    pub fn submit(&self, bytes: &[u8], caller: &Caller) -> Result<GoalReceipt> {
        self.open()?;
        let _coordinator = self.coordinator.lock().map_err(Error::io)?;
        if bytes.len() > 4_194_304 {
            return Err(Error::new(
                "input_invalid",
                "/",
                "Goal request exceeds 4 MiB",
            ));
        }
        let value = crate::parse_unique(bytes)?;
        self.registry.schemas.contract("goal-request", &value)?;
        let normalized: GoalRequest = serde_json::from_value(value)?;
        let fp = digest(&bounded_json(&normalized, 4_194_304)?);
        if let Some(prior) =
            self.inner
                .store
                .goal_find(&caller.principal, &normalized.request_key, &fp)?
        {
            return Ok(GoalReceipt {
                protocol_version: 1,
                goal_id: prior.id,
                revision: 0,
                replayed: true,
            });
        }
        let observed = self.inner.store.observe_goal_clock(&self.clock)?;
        if !observed.trusted {
            return Err(Error::new(
                "clock_untrusted",
                "/clock",
                "restore the host clock before admitting a Goal",
            ));
        }
        let prepared = self.registry.prepare_goal(
            bytes,
            caller,
            &self.inner.bindings,
            &self.options,
            observed.utc_ms,
        )?;
        self.validate_imports(&prepared, caller)?;
        let id = format!("goal-{}", uuid::Uuid::new_v4());
        let raw_sha = self.inner.store.write_blob(bytes)?;
        let mut record = GoalRecord::new(prepared, raw_sha, self.binding.clone());
        let profile = &self.registry.profiles[&record.prepared.request.spec.profile_ref];
        for check in &profile.checks {
            record.unissued_checks.insert(
                check.id.clone(),
                GoalEvaluationSummary {
                    evaluation_id: None,
                    check_id: check.id.clone(),
                    phase: (&check.phase).into(),
                    status: "not_issued".into(),
                    verdict: None,
                    receipt_id: None,
                    error: None,
                },
            );
        }
        let final_allocations = profile
            .checks
            .iter()
            .filter(|c| c.phase == CheckPhase::GoalFinal)
            .map(|c| {
                Ok((
                    c.id.clone(),
                    GoalBudgetAmount::for_entry(
                        &self.registry.entries[&self.registry.checkers[&c.checker_ref].entry_ref]
                            .budget,
                    )?,
                ))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;
        record
            .budget
            .protect_final(final_allocations, &record.prepared.effective_limits)?;
        self.prepare_planner_request(&mut record)?;
        let (row, created) = self.inner.store.goal_create(
            &id,
            &caller.principal,
            &record,
            self.options.max_admitted_goals,
            &self.clock,
        )?;
        if created {
            self.trace_admitted(&row);
        }
        self.inner.notify.notify_one();
        Ok(GoalReceipt {
            protocol_version: 1,
            goal_id: row.id,
            revision: 0,
            replayed: !created,
        })
    }
    fn validate_imports(&self, prepared: &PreparedGoal, caller: &Caller) -> Result<()> {
        let mut seen = BTreeSet::new();
        for import in &prepared.request.spec.imports {
            if !seen.insert(&import.artifact_id) {
                return Err(Error::new(
                    "input_invalid",
                    "/spec/imports",
                    "duplicate imported artifact",
                ));
            }
            let artifact = self
                .inner
                .store
                .goal_artifact(&import.artifact_id, &caller.principal)?;
            if artifact.artifact.sha256 != import.sha256
                || artifact.schema_digest.as_deref() != Some(import.schema_digest.as_str())
                || !artifact.accepted
            {
                return Err(Error::new(
                    "input_invalid",
                    "/spec/imports",
                    "import digest differs from accepted version",
                ));
            }
            if !self
                .registry
                .schemas
                .sources
                .values()
                .any(|s| s.digest == import.schema_digest)
            {
                return Err(Error::new(
                    "input_invalid",
                    "/spec/imports",
                    "import schema is not frozen in this registry",
                ));
            }
            self.inner.store.read_blob(&import.sha256)?;
        }
        Ok(())
    }
    pub(crate) fn prepare_planner_request(&self, record: &mut GoalRecord) -> Result<()> {
        if record.requests.values().any(|r| {
            matches!(r.role, GoalRunRole::Planner { .. })
                && !r.cancelled
                && r.run_id.as_ref().is_none_or(|id| {
                    record
                        .runs
                        .get(id)
                        .is_some_and(|run| !run.business_disposed)
                })
        }) {
            return Ok(());
        }
        let spec = &record.prepared.request.spec;
        let attempt = record
            .planning_attempts
            .checked_add(1)
            .ok_or_else(|| Error::io("planning attempt overflow"))?;
        if attempt > 1 + spec.planning.max_revisions {
            return Err(Error::new(
                "planning_exhausted",
                "/planning",
                "planner attempt budget exhausted",
            ));
        }
        let profile = &self.registry.profiles[&spec.profile_ref];
        let previous = record.proposals.last();
        let errors = previous
            .and_then(|p| p.failure.as_ref())
            .map(|f| f.issues.clone())
            .unwrap_or_default();
        let check_results = previous
            .map(|p| {
                p.check_ids
                    .iter()
                    .filter_map(|id| record.checks.get(id).map(|c| c.view.clone()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let catalog = profile
            .capabilities
            .iter()
            .filter(|id| record.prepared.allowed_capabilities.contains(id))
            .map(|id| self.registry.capabilities[id].clone())
            .collect::<Vec<_>>();
        let mut input = json!({"goal":spec.goal,"inputs":spec.inputs,"goal_spec_digest":record.prepared.spec_digest,"profile_digest":record.prepared.profile_digest,"planning_mode":spec.planning.mode,"effective_limits":record.prepared.effective_limits,"imports":spec.imports,"capability_catalog":catalog,"profile":profile,"planning_feedback":{"attempt":attempt,"previous_candidate_sha":previous.map(|p|&p.candidate_sha),"errors":errors,"check_results":check_results}});
        if let Some(id) = &spec.planning.template_ref {
            let template = &self.registry.templates[id];
            input["template"] = serde_json::to_value(template)?;
            input["template_digest"] = digest(&serde_json::to_vec(template)?).into();
        }
        let entry = &self.registry.entries[&profile.planner_entry_ref];
        self.registry
            .schemas
            .validate(&entry.contract.input_schema, &input)?;
        let bytes = bounded_json(&input, self.options.max_plan_bytes)?;
        let sha = self.inner.store.write_blob(&bytes)?;
        let id = format!("request-{}", uuid::Uuid::new_v4());
        record.requests.insert(
            id.clone(),
            GoalRunRequest {
                request_id: id,
                role: GoalRunRole::Planner { attempt },
                entry_ref: profile.planner_entry_ref.clone(),
                epoch: record.epoch,
                input_sha: sha.clone(),
                input_digest: sha,
                input_artifact_ids: spec
                    .imports
                    .iter()
                    .map(|import| import.artifact_id.clone())
                    .collect(),
                check_request_id: None,
                run_id: None,
                cancelled: false,
            },
        );
        Ok(())
    }
    pub(crate) fn control(
        &self,
        id: &str,
        key: &str,
        expected_revision: u64,
        caller: &Caller,
        command: GoalControlCommand,
    ) -> Result<GoalCommandReceipt> {
        self.open()?;
        nonempty(key, "/request_key")?;
        let _coordinator = self.coordinator.lock().map_err(Error::io)?;
        let row = self.authorized(id, caller)?;
        let kind = command.name();
        let fp = digest(&bounded_json(
            &json!({"kind":kind,"expected_revision":expected_revision}),
            4096,
        )?);
        if let Some(response) =
            self.inner
                .store
                .goal_command(id, &caller.principal, kind, key, &fp)?
        {
            return decode_command(response);
        }
        let observed = self.inner.store.observe_goal_clock(&self.clock)?;
        let mut next = row.record.clone();
        if observed.trusted {
            self.apply_due_deadlines(&mut next, row.revision + 1, observed.utc_ms);
            self.cleanup_terminal(&mut next)?;
        }
        let control_base = next.clone();
        let apply = (|| -> Result<()> {
            if row.revision != expected_revision {
                return Err(Error::new(
                    "revision_conflict",
                    "/revision",
                    "refresh the Goal before submitting a different command",
                ));
            }
            if next.status == GoalStatus::Finished {
                return Err(Error::new("not_resumable", "/goal", "Goal is finished"));
            }
            match command {
                GoalControlCommand::Cancel => {
                    set_terminal(
                        &mut next,
                        GoalOutcome::Cancelled,
                        "user_cancelled",
                        id,
                        row.revision + 1,
                        observed.utc_ms,
                    );
                    self.cleanup_terminal(&mut next)?;
                }
                GoalControlCommand::Stop => {
                    if next.status == GoalStatus::Stopped {
                        return Ok(());
                    }
                    if next.terminal_intent.is_none() && next.blockers.is_empty() {
                        next.pause_at_utc_ms.get_or_insert(observed.utc_ms);
                        next.status = GoalStatus::Stopping;
                        suspend_unstarted(&mut next)?;
                    }
                    self.close_stopping(&mut next, observed.utc_ms)?;
                }
                GoalControlCommand::Resume => {
                    let facts = next.permissions(observed.utc_ms, observed.trusted);
                    if !facts.resume_allowed(&control::ResumeFacts {
                        unsettled_started_run: next
                            .runs
                            .values()
                            .any(|r| r.view.started && r.view.phase != GoalRunPhase::Settled),
                        bindings_valid: next.binding_fingerprint == self.binding,
                        all_wait_deadlines_valid: next
                            .waiting
                            .values()
                            .filter(|w| !w.consumed)
                            .all(|w| observed.utc_ms < w.expires_at_utc_ms),
                        lost_acceptance: next.runs.values().any(|r| r.view.interrupted),
                        uncommitted_checking: next
                            .nodes
                            .values()
                            .any(|n| n.view.status == GoalNodeStatus::Checking),
                    }) {
                        return Err(Error::new(
                            "not_resumable",
                            "/goal",
                            "Goal has expired, is blocked, or has uncommitted execution",
                        ));
                    }
                    next.pause_at_utc_ms = None;
                    next.status = if next.waiting.values().any(|w| !w.consumed)
                        && next.nodes.values().all(|n| {
                            !matches!(
                                n.view.status,
                                GoalNodeStatus::Ready
                                    | GoalNodeStatus::Running
                                    | GoalNodeStatus::Checking
                            )
                        }) {
                        GoalStatus::Waiting
                    } else {
                        GoalStatus::Running
                    };
                }
            }
            Ok(())
        })();
        let response = match apply {
            Ok(()) => CommandResponse::Accepted {
                receipt: GoalCommandReceipt {
                    protocol_version: 1,
                    goal_id: id.into(),
                    request_key: key.into(),
                    kind: kind.into(),
                    applied_revision: row.revision + 1,
                    accepted: true,
                },
            },
            Err(error) => {
                next = control_base;
                CommandResponse::Rejected { error }
            }
        };
        let guard = if matches!(command, GoalControlCommand::Resume)
            && matches!(response, CommandResponse::Accepted { .. })
        {
            GoalWriteGuard::Resume {
                binding: &self.binding,
            }
        } else {
            GoalWriteGuard::Cleanup
        };
        let encoded = serde_json::to_value(&response)?;
        let saved = self.commit_goal(
            &row,
            &next,
            guard,
            Some(CommandReceipt {
                kind: kind.into(),
                key: key.into(),
                fingerprint: fp,
                response: encoded.clone(),
                not_after_ms: None,
            }),
            &GoalCommitEffects::default(),
            &self.clock,
        )?;
        match saved {
            GoalCommit::Saved(saved) => {
                if !matches!(command, GoalControlCommand::Resume)
                    || saved.record.terminal_intent.is_some()
                {
                    self.cancel_goal_handles(&saved.record);
                }
                self.inner.notify.notify_one();
                decode_command(encoded)
            }
            GoalCommit::Replayed(response) => decode_command(response),
        }
    }
    pub(crate) fn cancel_goal_handles(&self, record: &GoalRecord) {
        let due = record.terminal_intent.is_some()
            || !record.blockers.is_empty()
            || record.pause_at_utc_ms.is_some_and(|at| {
                self.clock
                    .observe()
                    .map(|o| o.utc_ms.saturating_sub(at) >= 15_000)
                    .unwrap_or(true)
            });
        if let Ok(handles) = self.inner.goal_run_handles.lock() {
            for (id, run) in &record.runs {
                if run.view.started
                    && !run.view.native_stop_confirmed
                    && (due
                        || record
                            .requests
                            .get(&run.request_id)
                            .is_some_and(|request| request.cancelled))
                    && let Some(token) = handles.get(id)
                {
                    token.cancel();
                }
            }
        }
    }
    pub(crate) fn cleanup_terminal(&self, record: &mut GoalRecord) -> Result<()> {
        if record.terminal_intent.is_none() {
            return Ok(());
        }
        for wait in record.waiting.values_mut() {
            wait.consumed = true;
        }
        for request in record.requests.values_mut() {
            if request.run_id.is_none() {
                request.cancelled = true;
            }
        }
        for run in record.runs.values_mut() {
            if !run.view.started && run.view.phase != GoalRunPhase::Settled {
                run.settlement_sequence += 1;
                record.budget.settle(
                    &run.view.run_id,
                    run.settlement_sequence,
                    run.active_ms,
                    true,
                    &record.prepared.effective_limits,
                )?;
                run.view.phase = GoalRunPhase::Settled;
                run.view.native_stop_confirmed = true;
                run.business_disposed = true;
            }
        }
        record.budget.protected_final.clear();
        if let Some(plan) = &record.plan {
            loop {
                let failed = plan
                    .proposal
                    .nodes
                    .iter()
                    .filter(|(id, node)| {
                        record.nodes.get(*id).is_some_and(|n| {
                            matches!(
                                n.view.status,
                                GoalNodeStatus::Pending
                                    | GoalNodeStatus::Ready
                                    | GoalNodeStatus::WaitingInput
                            )
                        }) && node.depends_on.iter().any(|dep| {
                            record.nodes.get(dep).is_some_and(|n| {
                                matches!(
                                    n.view.status,
                                    GoalNodeStatus::Failed | GoalNodeStatus::DependencyFailed
                                )
                            })
                        })
                    })
                    .map(|(id, _)| id.clone())
                    .collect::<Vec<_>>();
                if failed.is_empty() {
                    break;
                }
                for id in failed {
                    record.nodes.get_mut(&id).expect("locked node").view.status =
                        GoalNodeStatus::DependencyFailed;
                }
            }
        }
        for node in record.nodes.values_mut() {
            if !node.view.status.terminal()
                && node.view.members.values().all(|m| {
                    m.run_id
                        .as_ref()
                        .and_then(|id| record.runs.get(id))
                        .is_none_or(|r| !r.view.started || r.view.native_stop_confirmed)
                })
            {
                if let Some(nonce) = &node.checking_nonce
                    && let Some(meter) = record.budget.meters.get(nonce).cloned()
                    && record.budget.reserved.contains_key(nonce)
                {
                    record.budget.observe_active(
                        nonce,
                        meter.sequence + 1,
                        meter.active_ms,
                        true,
                        true,
                        &record.prepared.effective_limits,
                    )?;
                }
                node.view.status = GoalNodeStatus::Cancelled;
            }
        }
        for check in record.checks.values_mut() {
            if check.view.receipt_id.is_none() {
                check.view.status = "stale_due_to_terminal".into();
            }
        }
        for check in record.unissued_checks.values_mut() {
            check.status = "not_run_due_to_terminal".into();
        }
        self.close_stopping(record, self.clock.observe()?.utc_ms)
    }
    pub(crate) fn close_stopping(&self, record: &mut GoalRecord, now: u64) -> Result<()> {
        let unsettled = record.runs.values().any(|r| {
            r.view.started && r.view.phase != GoalRunPhase::Settled
                || !r.view.started && r.view.phase == GoalRunPhase::Reserved
        });
        let unknown = record
            .runs
            .values()
            .any(|r| r.view.started && !r.view.native_stop_confirmed && r.view.stop_unknown);
        if let Some(intent) = &record.terminal_intent {
            if !unsettled {
                record.status = GoalStatus::Finished;
                record.outcome = Some(intent.outcome);
            } else if unknown && now.saturating_sub(intent.at_utc_ms) >= 15_000 {
                record.status = GoalStatus::Blocked;
            } else {
                record.status = GoalStatus::Stopping;
            }
        } else if let Some(pause) = record.pause_at_utc_ms {
            if !record.blockers.is_empty() {
                record.status = GoalStatus::Blocked;
            } else if !unsettled
                && record
                    .nodes
                    .values()
                    .all(|n| n.view.status != GoalNodeStatus::Checking)
                && self.pending_evaluation_nodes(record).is_empty()
            {
                record.status = GoalStatus::Stopped;
            } else if now.saturating_sub(pause) >= 15_000 {
                record.status = GoalStatus::Blocked;
                record.blockers.insert(GoalBlocker {
                    reason: if unknown {
                        "stop_unknown"
                    } else {
                        "interrupted"
                    }
                    .into(),
                    subject: "goal".into(),
                });
            } else {
                record.status = GoalStatus::Stopping;
            }
        }
        Ok(())
    }
}
fn decode_command(value: Value) -> Result<GoalCommandReceipt> {
    match serde_json::from_value(value)? {
        CommandResponse::Accepted { receipt } => Ok(receipt),
        CommandResponse::Rejected { error } => Err(error),
    }
}
pub(crate) fn nonempty(value: &str, path: &str) -> Result<()> {
    if value.trim().is_empty() || value.len() > 160 {
        Err(Error::new(
            "input_invalid",
            path,
            "expected a nonempty value up to 160 bytes",
        ))
    } else {
        Ok(())
    }
}
pub(crate) fn set_terminal(
    record: &mut GoalRecord,
    outcome: GoalOutcome,
    reason: &str,
    subject: &str,
    revision: u64,
    now: u64,
) {
    if record.terminal_intent.is_none() {
        record.terminal_intent = Some(GoalTerminalIntent {
            revision,
            at_utc_ms: now,
            outcome,
            reason: reason.into(),
            subject: subject.into(),
        });
        record.epoch += 1;
    }
    record.status = GoalStatus::Stopping;
    record.outcome = None;
}
fn suspend_unstarted(record: &mut GoalRecord) -> Result<()> {
    for run in record.runs.values_mut() {
        if !run.view.started && run.view.phase == GoalRunPhase::Reserved {
            record.budget.suspend_unstarted(&run.view.run_id)?;
            run.view.phase = GoalRunPhase::Suspended;
        }
    }
    Ok(())
}
impl Goals {
    pub async fn submit(&self, request: GoalRequest, caller: &Caller) -> Result<GoalReceipt> {
        self.core
            .submit(&bounded_json(&request, 4_194_304)?, caller)
    }
    pub async fn submit_json(&self, bytes: &[u8], caller: &Caller) -> Result<GoalReceipt> {
        self.core.submit(bytes, caller)
    }
    pub async fn get(&self, id: &str, caller: &Caller) -> Result<GoalSnapshot> {
        self.core.inner.store.goal_snapshot(id, &caller.principal)
    }
    pub async fn plan(&self, id: &str, caller: &Caller) -> Result<LockedPlan> {
        self.core
            .authorized(id, caller)?
            .record
            .plan
            .ok_or_else(|| Error::new("not_ready", "/plan", "Goal plan is not locked"))
    }
    pub async fn cancel(
        &self,
        id: &str,
        key: &str,
        revision: u64,
        caller: &Caller,
    ) -> Result<GoalCommandReceipt> {
        self.core
            .control(id, key, revision, caller, GoalControlCommand::Cancel)
    }
    pub async fn stop(
        &self,
        id: &str,
        key: &str,
        revision: u64,
        caller: &Caller,
    ) -> Result<GoalCommandReceipt> {
        self.core
            .control(id, key, revision, caller, GoalControlCommand::Stop)
    }
    pub async fn resume(
        &self,
        id: &str,
        key: &str,
        revision: u64,
        caller: &Caller,
    ) -> Result<GoalCommandReceipt> {
        self.core
            .control(id, key, revision, caller, GoalControlCommand::Resume)
    }
    pub async fn read_artifact(&self, id: &str, caller: &Caller) -> Result<ArtifactData> {
        let artifact = self.core.inner.store.goal_artifact(id, &caller.principal)?;
        Ok(ArtifactData {
            bytes: self
                .core
                .inner
                .store
                .read_blob(&artifact.artifact.sha256)?
                .into(),
            reference: artifact.artifact,
        })
    }
    pub async fn evaluations(&self, id: &str, caller: &Caller) -> Result<Vec<EvaluationReceipt>> {
        let row = self.core.authorized(id, caller)?;
        row.record
            .checks
            .values()
            .filter_map(|c| c.receipt_sha.as_ref())
            .map(|sha| {
                serde_json::from_slice(&self.core.inner.store.read_blob(sha)?).map_err(Error::io)
            })
            .collect()
    }
    pub async fn budgets(&self, id: &str, caller: &Caller) -> Result<GoalBudgetView> {
        let row = self.core.authorized(id, caller)?;
        let r = row.record;
        Ok(GoalBudgetView {
            protocol_version: 1,
            goal_id: id.into(),
            available: r.budget.available(&r.prepared.effective_limits),
            budget: r.budget,
            effective_limits: r.prepared.effective_limits,
            sources: r.prepared.limit_sources,
        })
    }
}

impl GoalCore {
    /// R01/R06. Pending requests have no Run or slot until this transaction wins.
    pub(crate) fn materialize(&self, id: &str) -> Result<Option<String>> {
        let _coordinator = self.coordinator.lock().map_err(Error::io)?;
        let row = self.inner.store.goal_load(id)?;
        let observed = self.inner.store.observe_goal_clock(&self.clock)?;
        if !row
            .record
            .permissions(observed.utc_ms, observed.trusted)
            .advance_allowed()
        {
            return Ok(None);
        }
        let existing = row
            .record
            .runs
            .iter()
            .filter(|(_, r)| !r.view.started && r.view.phase == GoalRunPhase::Suspended)
            .min_by_key(|(_, r)| r.view.role.order_key())
            .map(|(id, r)| (id.clone(), r.request_id.clone()));
        let request = if let Some((_, request_id)) = &existing {
            row.record.requests.get(request_id)
        } else {
            row.record
                .requests
                .values()
                .filter(|r| !r.cancelled && r.run_id.is_none())
                .min_by_key(|r| r.role.order_key())
        };
        let Some(request) = request.cloned() else {
            return Ok(None);
        };
        let entry = &self.registry.entries[&request.entry_ref];
        let run_id = existing
            .as_ref()
            .map(|(id, _)| id.clone())
            .unwrap_or_else(|| format!("run-{}", uuid::Uuid::new_v4()));
        let mut next = row.record.clone();
        let protected = match &request.role {
            GoalRunRole::FinalCheck { check_id } if existing.is_none() => Some(check_id.as_str()),
            _ => None,
        };
        let amount = if let Some(check) = protected {
            next.budget
                .protected_final
                .get(check)
                .copied()
                .ok_or_else(|| Error::io("final allocation lost"))?
        } else {
            let available = next
                .budget
                .permanent_available(&next.prepared.effective_limits);
            let policy = GoalBudgetAmount::for_entry(&entry.budget)?;
            GoalBudgetAmount {
                active_ms: policy.active_ms.min(available.active_ms),
                tool_calls: policy.tool_calls.min(available.tool_calls),
                candidate_bytes: policy.candidate_bytes.min(available.candidate_bytes),
            }
        };
        if amount.active_ms < 2 || amount.tool_calls == 0 || amount.candidate_bytes == 0 {
            return self.fail_admission(&row, observed.utc_ms);
        }
        match next.budget.reserve(
            &run_id,
            amount,
            protected,
            existing.is_none(),
            &next.prepared.effective_limits,
        )? {
            BudgetAdmission::Pending => return Ok(None),
            BudgetAdmission::Exhausted => return self.fail_admission(&row, observed.utc_ms),
            BudgetAdmission::Granted => {}
        }
        let check_ms = entry
            .budget
            .max_check_ms
            .unwrap_or(0)
            .min(amount.active_ms / 2);
        let run_ms = entry
            .budget
            .max_run_ms
            .unwrap_or(0)
            .min(amount.active_ms - check_ms);
        let check_ms = entry
            .budget
            .max_check_ms
            .unwrap_or(0)
            .min(amount.active_ms - run_ms);
        if run_ms == 0 || check_ms == 0 {
            return self.fail_admission(&row, observed.utc_ms);
        }
        let executor = &self.inner.bindings.executors[&entry.contract.executor];
        let binding = digest(&bounded_json(
            &json!({"configuration_version":self.inner.bindings.configuration_version,"executor":entry.contract.executor,"identity":executor.identity(),"limits":executor.budget_limits()}),
            1_048_576,
        )?);
        if let Some(run) = next.runs.get_mut(&run_id) {
            run.view.phase = GoalRunPhase::Reserved;
            run.allocation = amount;
            run.run_allocation_ms = run_ms;
            run.check_allocation_ms = check_ms;
        } else {
            next.runs.insert(
                run_id.clone(),
                GoalRunRecord {
                    view: GoalRunSnapshot {
                        run_id: run_id.clone(),
                        role: request.role.clone(),
                        entry_ref: request.entry_ref.clone(),
                        epoch: request.epoch,
                        phase: GoalRunPhase::Reserved,
                        started: false,
                        native_stop_confirmed: false,
                        stop_unknown: false,
                        interrupted: false,
                        budget: None,
                        plan_validation_failure: None,
                        error: None,
                    },
                    request_id: request.request_id.clone(),
                    input_sha: request.input_sha.clone(),
                    executor: entry.contract.executor.clone(),
                    executor_binding: binding,
                    candidates: BTreeMap::new(),
                    started_at_utc_ms: None,
                    returned_at_utc_ms: None,
                    usage_sha: None,
                    business_disposed: false,
                    settlement_sequence: 0,
                    active_ms: 0,
                    allocation: amount,
                    run_allocation_ms: run_ms,
                    check_allocation_ms: check_ms,
                    tool_receipt_shas: vec![],
                    execution_outcome: None,
                },
            );
            if let GoalRunRole::Planner { attempt } = request.role {
                next.planning_attempts = attempt;
            }
        }
        next.requests
            .get_mut(&request.request_id)
            .expect("persisted request")
            .run_id = Some(run_id.clone());
        if let Some(check_id) = &request.check_request_id {
            next.checks
                .get_mut(check_id)
                .ok_or_else(|| Error::io("Run request check missing"))?
                .producer_run_id = Some(run_id.clone());
        }
        if let GoalRunRole::NodeMember { node_id, member_id } = &request.role {
            let member = next
                .nodes
                .get_mut(node_id)
                .and_then(|n| n.view.members.get_mut(member_id))
                .ok_or_else(|| Error::io("Run request member missing"))?;
            member.run_id = Some(run_id.clone());
            member.status = GoalMemberStatus::Reserved;
        }
        match self.commit_goal(
            &row,
            &next,
            GoalWriteGuard::Advance,
            None,
            &Default::default(),
            &self.clock,
        ) {
            Ok(GoalCommit::Saved(_)) => Ok(Some(run_id)),
            Ok(GoalCommit::Replayed(_)) => Err(Error::io("internal materialization replay")),
            Err(e) if e.code == "dispatch_capacity" || e.code == "revision_conflict" => Ok(None),
            Err(e) => Err(e),
        }
    }
    fn fail_admission(&self, row: &GoalRow, now: u64) -> Result<Option<String>> {
        let mut next = row.record.clone();
        set_terminal(
            &mut next,
            GoalOutcome::Failed,
            "budget_exhausted",
            "goal",
            row.revision + 1,
            now,
        );
        next.error = Some(Error::new(
            "budget_exhausted",
            "/budget",
            "no positive bounded execution/check allocation remains",
        ));
        self.cleanup_terminal(&mut next)?;
        self.commit_goal(
            row,
            &next,
            GoalWriteGuard::Cleanup,
            None,
            &Default::default(),
            &self.clock,
        )?;
        self.cancel_goal_handles(&next);
        Ok(None)
    }
}

impl GoalCore {
    fn signal(
        &self,
        id: &str,
        request: &GoalSignalRequest,
        caller: &Caller,
    ) -> Result<GoalCommandReceipt> {
        self.open()?;
        nonempty(&request.request_key, "/request_key")?;
        let _coordinator = self.coordinator.lock().map_err(Error::io)?;
        let row = self.authorized(id, caller)?;
        let fp = digest(&bounded_json(request, 4_194_304)?);
        if let Some(response) = self.inner.store.goal_command(
            id,
            &caller.principal,
            "signal",
            &request.request_key,
            &fp,
        )? {
            return decode_command(response);
        }
        let observed = self.inner.store.observe_goal_clock(&self.clock)?;
        let mut next = row.record.clone();
        let mut effects = GoalCommitEffects::default();
        let apply = (|| -> Result<()> {
            if request.protocol_version != 1 {
                return Err(Error::new(
                    "input_invalid",
                    "/protocol_version",
                    "expected version 1",
                ));
            }
            if row.revision != request.expected_revision {
                return Err(Error::new(
                    "revision_conflict",
                    "/revision",
                    "refresh the Goal before a new signal",
                ));
            }
            let wait = next
                .waiting
                .get(&request.wait_id)
                .ok_or_else(|| Error::new("signal_invalid", "/wait_id", "unknown waiting input"))?
                .clone();
            if !next
                .permissions(observed.utc_ms, observed.trusted)
                .signal_allowed(&control::SignalAuthority {
                    wait_identity_valid: wait.nonce == request.nonce && !wait.consumed,
                    schema_and_caller_valid: true,
                    revision_valid: true,
                    expires_at_ms: wait.expires_at_utc_ms,
                })
            {
                return Err(Error::new(
                    "signal_invalid",
                    "/wait",
                    "waiting input is consumed, expired, paused or otherwise closed",
                ));
            }
            self.registry
                .schemas
                .validate(&wait.schema_ref, &request.payload)?;
            if let Some(node_id) = &wait.node_id {
                let node = &next.nodes[node_id];
                if node.view.status != GoalNodeStatus::WaitingInput {
                    return Err(Error::new(
                        "signal_invalid",
                        "/node",
                        "node is not waiting for input",
                    ));
                }
                let artifact =
                    self.public_artifact(id, &next, node_id, "value", &request.payload, "signal")?;
                let node = next.nodes.get_mut(node_id).expect("waiting node");
                node.view.status = GoalNodeStatus::Succeeded;
                node.view
                    .outputs
                    .insert("value".into(), artifact.artifact.clone());
                next.artifacts
                    .insert(artifact.artifact.id.clone(), artifact.clone());
                effects.artifacts.push(artifact);
            } else if next.stage != GoalStage::Planning {
                return Err(Error::new(
                    "signal_invalid",
                    "/stage",
                    "planning input belongs to a different stage",
                ));
            }
            next.waiting
                .get_mut(&request.wait_id)
                .expect("waiting input")
                .consumed = true;
            next.signals.push(SignalFact {
                id: format!("signal-{}", uuid::Uuid::new_v4()),
                event_name: request.wait_id.clone(),
                payload: request.payload.clone(),
                received_at: chrono::Utc::now().to_rfc3339(),
            });
            next.status = GoalStatus::Running;
            if wait.node_id.is_none() {
                if next.planning_attempts > next.prepared.request.spec.planning.max_revisions {
                    set_terminal(
                        &mut next,
                        GoalOutcome::Failed,
                        "planning_exhausted",
                        "planner",
                        row.revision + 1,
                        observed.utc_ms,
                    );
                    self.cleanup_terminal(&mut next)?;
                } else {
                    self.prepare_planner_request(&mut next)?;
                }
            }
            Ok(())
        })();
        let response = match apply {
            Ok(()) => CommandResponse::Accepted {
                receipt: GoalCommandReceipt {
                    protocol_version: 1,
                    goal_id: id.into(),
                    request_key: request.request_key.clone(),
                    kind: "signal".into(),
                    applied_revision: row.revision + 1,
                    accepted: true,
                },
            },
            Err(error) => {
                next = row.record.clone();
                effects = Default::default();
                CommandResponse::Rejected { error }
            }
        };
        let guard = if matches!(response, CommandResponse::Accepted { .. }) {
            GoalWriteGuard::Signal {
                wait_id: &request.wait_id,
                nonce: &request.nonce,
            }
        } else {
            GoalWriteGuard::Cleanup
        };
        let encoded = serde_json::to_value(&response)?;
        let saved = self.commit_goal(
            &row,
            &next,
            guard,
            Some(CommandReceipt {
                kind: "signal".into(),
                key: request.request_key.clone(),
                fingerprint: fp,
                response: encoded.clone(),
                not_after_ms: None,
            }),
            &effects,
            &self.clock,
        )?;
        self.inner.notify.notify_one();
        match saved {
            GoalCommit::Saved(_) => decode_command(encoded),
            GoalCommit::Replayed(value) => decode_command(value),
        }
    }
}
impl Goals {
    pub async fn signal(
        &self,
        id: &str,
        request: GoalSignalRequest,
        caller: &Caller,
    ) -> Result<GoalCommandReceipt> {
        self.core.signal(id, &request, caller)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct SubmitCandidate;
    #[async_trait::async_trait]
    impl Executor for SubmitCandidate {
        fn enforces_json_candidate_limit(&self) -> bool {
            true
        }
        fn capabilities(&self) -> ExecutorCapabilities {
            ExecutorCapabilities::MANAGED
        }
        async fn execute(
            &self,
            invocation: Invocation,
            tools: Arc<dyn ToolBridge>,
            _: tokio_util::sync::CancellationToken,
        ) -> ExecutionOutcome {
            let context = tools.goal_context().expect("Runtime attribution");
            assert_eq!(context.goal_id, invocation.execution_id);
            assert!(matches!(context.role, GoalRunRole::Planner { .. }));
            let output = json!({"protocol_version":1,"kind":"needs_input","input_schema_ref":"goal-input","reason":"A concrete question"});
            tools
                .call("artifact.write", json!({"slot":"proposal","value":output}))
                .await
                .unwrap();
            ExecutionOutcome::Completed {
                usage: json!({"test":true}),
            }
        }
    }
    struct SubmitPlan;
    struct UnknownThenConfirmed(std::sync::atomic::AtomicUsize);
    #[async_trait::async_trait]
    impl Executor for UnknownThenConfirmed {
        fn capabilities(&self) -> ExecutorCapabilities {
            ExecutorCapabilities::MANAGED
        }
        fn enforces_json_candidate_limit(&self) -> bool {
            true
        }
        async fn execute(
            &self,
            _: Invocation,
            _: Arc<dyn ToolBridge>,
            _: tokio_util::sync::CancellationToken,
        ) -> ExecutionOutcome {
            ExecutionOutcome::StopUnknown {
                message: "controlled unknown native stop".into(),
            }
        }
        async fn stop_or_confirm(&self, _: &str) -> StopState {
            if self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst) > 0 {
                StopState::Confirmed
            } else {
                StopState::Unknown
            }
        }
    }
    #[tokio::test]
    async fn unknown_native_stop_is_reconciled_without_accepting_or_replaying_business_work() {
        let executor = Arc::new(UnknownThenConfirmed(std::sync::atomic::AtomicUsize::new(0)));
        let (_directory, runtime, goals, caller, request) =
            setup_mode(Some(executor.clone()), true).await;
        let id = goals.submit(request, &caller).await.unwrap().goal_id;
        let snapshot = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let s = goals.get(&id, &caller).await.unwrap();
                if !s.runs.is_empty() && s.runs.values().all(|r| r.native_stop_confirmed) {
                    break s;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("returned unknown Runs must remain eligible for stop probes");
        assert_eq!(snapshot.status, GoalStatus::Blocked);
        assert_eq!(snapshot.outcome, None);
        assert_eq!(snapshot.budget.runs_used, 1);
        assert!(
            snapshot
                .runs
                .values()
                .all(|r| r.interrupted && r.phase == GoalRunPhase::Settled)
        );
        assert!(snapshot.budget.reserved.is_empty());
        assert!(snapshot.artifacts.is_empty());
        assert_eq!(
            goals
                .resume(&id, "cannot-replay", snapshot.revision, &caller)
                .await
                .unwrap_err()
                .code,
            "not_resumable"
        );
        let current = goals.get(&id, &caller).await.unwrap();
        goals
            .cancel(&id, "close-blocked", current.revision, &caller)
            .await
            .unwrap();
        assert_eq!(
            goals.get(&id, &caller).await.unwrap().outcome,
            Some(GoalOutcome::Cancelled)
        );
        assert!(executor.0.load(std::sync::atomic::Ordering::SeqCst) >= 2);
        runtime.shutdown().await.unwrap();
    }
    struct RequestPlanningInput;
    #[async_trait::async_trait]
    impl Executor for RequestPlanningInput {
        fn enforces_json_candidate_limit(&self) -> bool {
            true
        }
        fn capabilities(&self) -> ExecutorCapabilities {
            ExecutorCapabilities::MANAGED
        }
        async fn execute(
            &self,
            invocation: Invocation,
            tools: Arc<dyn ToolBridge>,
            cancel: tokio_util::sync::CancellationToken,
        ) -> ExecutionOutcome {
            if tools.goal_context().unwrap().role == (GoalRunRole::Planner { attempt: 1 }) {
                tools.call("artifact.write", json!({"slot":"proposal","value":{"protocol_version":1,"kind":"needs_input","input_schema_ref":"text","reason":"Please clarify the review"}})).await.unwrap();
                ExecutionOutcome::Completed { usage: json!({}) }
            } else {
                if matches!(
                    tools.goal_context().unwrap().role,
                    GoalRunRole::Planner { .. }
                ) {
                    assert_eq!(invocation.signal_facts.len(), 1);
                }
                CompleteGoal(EvaluationVerdict::Pass)
                    .execute(invocation, tools, cancel)
                    .await
            }
        }
    }
    #[tokio::test]
    async fn planning_signal_preserves_pause_nonce_and_replays_without_new_work() {
        let (_directory, runtime, goals, caller, request) =
            setup_custom(Some(Arc::new(RequestPlanningInput)), true, |registry| {
                registry
                    .profiles
                    .get_mut("document-review@1")
                    .unwrap()
                    .planning_input_schemas = vec!["text".into()];
            })
            .await;
        let endpoint =
            super::super::control_socket::ControlEndpoint::start(&runtime, caller.clone()).unwrap();
        let client = GoalClient::open(_directory.path().join("store"), caller.clone()).unwrap();
        let original_spec = request.spec.clone();
        let id = goals.submit(request, &caller).await.unwrap().goal_id;
        let waiting = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let s = goals.get(&id, &caller).await.unwrap();
                if !s.waiting.is_empty() {
                    break s;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let wait = &waiting.waiting[0];
        let stopped = client
            .stop(&id, "pause-input", waiting.revision)
            .await
            .unwrap();
        assert_eq!(stopped.status, GoalStatus::Stopped);
        assert!(stopped.pending_runs.is_empty());
        let stopped = stopped.receipt;
        let mut signal = GoalSignalRequest {
            protocol_version: 1,
            wait_id: wait.wait_id.clone(),
            nonce: wait.nonce.clone(),
            payload: json!("Keep the frozen review scope"),
            request_key: "paused-signal".into(),
            expected_revision: stopped.applied_revision,
        };
        assert_eq!(
            client.signal(&id, signal.clone()).await.unwrap_err().code,
            "signal_invalid"
        );
        let paused = goals.get(&id, &caller).await.unwrap();
        assert_eq!(paused.waiting[0].nonce, wait.nonce);
        assert_eq!(paused.waiting[0].expires_at_utc_ms, wait.expires_at_utc_ms);
        let resumed = goals
            .resume(&id, "resume-input", paused.revision, &caller)
            .await
            .unwrap();
        signal.expected_revision = resumed.applied_revision;
        signal.request_key = "accepted-signal".into();
        let accepted = client.signal(&id, signal.clone()).await.unwrap().receipt;
        assert_eq!(
            client
                .signal(&id, signal)
                .await
                .unwrap()
                .receipt
                .applied_revision,
            accepted.applied_revision
        );
        let snapshot = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let s = goals.get(&id, &caller).await.unwrap();
                if s.status == GoalStatus::Finished {
                    break s;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(snapshot.outcome, Some(GoalOutcome::Achieved));
        assert_eq!(snapshot.budget.runs_used, 4);
        assert_eq!(
            serde_json::to_value(snapshot.spec).unwrap(),
            serde_json::to_value(original_spec).unwrap()
        );
        endpoint.close().await;
        assert!(!_directory.path().join("store/owner.sock").exists());
        let revision = client.get(&id).unwrap().revision;
        assert_eq!(
            client
                .cancel(&id, "offline-not-queued", revision)
                .await
                .unwrap_err()
                .code,
            "host_not_running"
        );
        assert_eq!(client.get(&id).unwrap().revision, revision);
        runtime.shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn accepted_planning_signal_keeps_one_pending_request_across_recovery_and_replay() {
        let (_directory, runtime, goals, caller, request) =
            setup_custom(Some(Arc::new(RequestPlanningInput)), false, |registry| {
                registry
                    .profiles
                    .get_mut("document-review@1")
                    .unwrap()
                    .planning_input_schemas = vec!["text".into()];
            })
            .await;
        let id = goals.submit(request, &caller).await.unwrap().goal_id;
        let first = goals.core.materialize(&id).unwrap().unwrap();
        goals
            .core
            .prepare_execution(&id, &first)
            .unwrap()
            .unwrap()
            .execute()
            .await
            .unwrap();
        goals.core.consume_return(&id, &first).unwrap();
        let waiting = goals.get(&id, &caller).await.unwrap();
        let signal = GoalSignalRequest {
            protocol_version: 1,
            wait_id: waiting.waiting[0].wait_id.clone(),
            nonce: waiting.waiting[0].nonce.clone(),
            payload: json!("Keep the original scope"),
            request_key: "persist-before-materializing".into(),
            expected_revision: waiting.revision,
        };
        let receipt = goals.signal(&id, signal.clone(), &caller).await.unwrap();
        let accepted = goals.core.inner.store.goal_load(&id).unwrap();
        assert_eq!(accepted.record.signals.len(), 1);
        let pending = accepted
            .record
            .requests
            .values()
            .filter(|r| r.run_id.is_none() && !r.cancelled)
            .collect::<Vec<_>>();
        assert_eq!(pending.len(), 1);
        let request_id = pending[0].request_id.clone();
        let input_sha = pending[0].input_sha.clone();
        assert_eq!(accepted.record.runs.len(), 1);
        goals.core.recover().unwrap();
        let recovered = goals.get(&id, &caller).await.unwrap();
        assert_eq!(recovered.status, GoalStatus::Stopped);
        assert_eq!(recovered.budget.runs_used, 1);
        assert_eq!(recovered.progress.goal_occupied_slots, 0);
        assert_eq!(recovered.progress.pending_requests, 1);
        goals.core.recover().unwrap();
        assert_eq!(
            goals.get(&id, &caller).await.unwrap().revision,
            recovered.revision
        );
        let replay = goals.signal(&id, signal.clone(), &caller).await.unwrap();
        assert_eq!(
            serde_json::to_value(&replay).unwrap(),
            serde_json::to_value(&receipt).unwrap()
        );
        assert_eq!(replay.applied_revision, receipt.applied_revision);
        assert_eq!(
            goals.get(&id, &caller).await.unwrap().revision,
            recovered.revision
        );
        goals
            .resume(&id, "resume-persisted-request", recovered.revision, &caller)
            .await
            .unwrap();
        let next = goals.core.materialize(&id).unwrap().unwrap();
        let started = goals.core.inner.store.goal_load(&id).unwrap();
        assert_eq!(
            started.record.requests[&request_id].run_id.as_deref(),
            Some(next.as_str())
        );
        assert_eq!(started.record.requests[&request_id].input_sha, input_sha);
        assert_eq!(started.record.signals.len(), 1);
        goals
            .core
            .prepare_execution(&id, &next)
            .unwrap()
            .unwrap()
            .execute()
            .await
            .unwrap();
        goals.core.consume_return(&id, &next).unwrap();
        goals.core.advance(&id).unwrap();
        let planned = goals.get(&id, &caller).await.unwrap();
        assert_eq!(planned.stage, GoalStage::Executing);
        assert_eq!(planned.budget.runs_used, 2);
        assert!(planned.plan_digest.is_some());
        assert_eq!(
            goals
                .signal(&id, signal, &caller)
                .await
                .unwrap()
                .applied_revision,
            receipt.applied_revision
        );
        assert_eq!(
            goals.get(&id, &caller).await.unwrap().revision,
            planned.revision
        );
        runtime.shutdown().await.unwrap();
    }
    struct CompleteGoal(EvaluationVerdict);
    #[async_trait::async_trait]
    impl Executor for CompleteGoal {
        fn enforces_json_candidate_limit(&self) -> bool {
            true
        }
        fn capabilities(&self) -> ExecutorCapabilities {
            ExecutorCapabilities::MANAGED
        }
        async fn execute(
            &self,
            invocation: Invocation,
            tools: Arc<dyn ToolBridge>,
            cancel: tokio_util::sync::CancellationToken,
        ) -> ExecutionOutcome {
            let context = tools.goal_context().expect("Goal attribution");
            match context.role {
                GoalRunRole::Planner { .. } => {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    SubmitPlan.execute(invocation, tools, cancel).await
                }
                GoalRunRole::NodeMember { .. } => {
                    assert_eq!(invocation.stage_input["document"], "Document");
                    tools.call("artifact.write", json!({"slot":"report","value":{"summary":"Document reviewed","findings":["Document"]}})).await.unwrap();
                    ExecutionOutcome::Completed { usage: json!({}) }
                }
                GoalRunRole::FinalCheck { .. } | GoalRunRole::PlanCheck { .. } => {
                    let input: CheckInput = serde_json::from_value(invocation.stage_input).unwrap();
                    let verdict = if input.check_id == "quality-late-pass" {
                        EvaluationVerdict::Pass
                    } else {
                        self.0.clone()
                    };
                    assert!(invocation.signal_facts.is_empty());
                    assert_eq!(
                        invocation.inputs,
                        serde_json::to_value(&input.goal_inputs).unwrap()
                    );
                    let results = input.rules.rules.iter().map(|rule| json!({"id":rule.id,"verdict":verdict,"message":"Controlled test result","evidence_refs":input.evidence_refs})).collect::<Vec<_>>();
                    tools.call("artifact.write", json!({"slot":"evaluation","value":{"protocol_version":1,"verdict":verdict,"results":results,"evidence_refs":input.evidence_refs,"violations":[]}})).await.unwrap();
                    ExecutionOutcome::Completed { usage: json!({}) }
                }
            }
        }
    }
    #[tokio::test]
    async fn one_controlled_pipeline_distinguishes_achieved_not_met_and_checker_error() {
        for (verdict, outcome) in [
            (EvaluationVerdict::Pass, GoalOutcome::Achieved),
            (EvaluationVerdict::Fail, GoalOutcome::NotMet),
            (EvaluationVerdict::Error, GoalOutcome::Failed),
        ] {
            let (_directory, runtime, goals, caller, request) =
                setup(Some(Arc::new(CompleteGoal(verdict.clone())))).await;
            let id = goals.submit(request, &caller).await.unwrap().goal_id;
            for _ in 0..20 {
                goals.core.advance(&id).unwrap();
                if let Some(run) = goals.core.materialize(&id).unwrap() {
                    goals
                        .core
                        .prepare_execution(&id, &run)
                        .unwrap()
                        .unwrap()
                        .execute()
                        .await
                        .unwrap();
                    goals.core.consume_return(&id, &run).unwrap();
                }
                let snapshot = goals.get(&id, &caller).await.unwrap();
                for (node, state) in &snapshot.nodes {
                    if state.status == GoalNodeStatus::Checking {
                        goals.core.finish_node_check(&id, node).unwrap();
                    }
                }
                if goals.get(&id, &caller).await.unwrap().status == GoalStatus::Finished {
                    break;
                }
            }
            let snapshot = goals.get(&id, &caller).await.unwrap();
            assert_eq!(snapshot.status, GoalStatus::Finished, "{snapshot:?}");
            assert_eq!(snapshot.outcome, Some(outcome));
            assert_eq!(snapshot.budget.runs_used, 3);
            assert!(
                snapshot
                    .runs
                    .values()
                    .all(|r| r.phase == GoalRunPhase::Settled && r.native_stop_confirmed)
            );
            assert_eq!(snapshot.artifacts.iter().filter(|a| a.accepted).count(), 1);
            assert_eq!(snapshot.nodes["review"].status, GoalNodeStatus::Succeeded);
            let evaluations = goals.evaluations(&id, &caller).await.unwrap();
            assert_eq!(evaluations.len(), 1);
            assert_eq!(evaluations[0].verdict, verdict);
            assert!(snapshot.budget.reserved.is_empty());
            runtime.shutdown().await.unwrap();
        }
    }
    #[async_trait::async_trait]
    impl Executor for SubmitPlan {
        fn enforces_json_candidate_limit(&self) -> bool {
            true
        }
        fn capabilities(&self) -> ExecutorCapabilities {
            ExecutorCapabilities::MANAGED
        }
        async fn execute(
            &self,
            invocation: Invocation,
            tools: Arc<dyn ToolBridge>,
            _: tokio_util::sync::CancellationToken,
        ) -> ExecutionOutcome {
            let mut proposal: Value = serde_json::from_str(include_str!("../../docs/design/10-goal-dag-orchestration/contract/document-review-package/plan.example.json")).unwrap();
            proposal["goal_spec_digest"] = invocation.stage_input["goal_spec_digest"].clone();
            proposal["profile_digest"] = invocation.stage_input["profile_digest"].clone();
            for requirement in invocation.stage_input["profile"]["checks"]
                .as_array()
                .unwrap()
            {
                if requirement["phase"] == "goal_final" {
                    let id = requirement["id"].as_str().unwrap();
                    proposal["checks"][id] = json!({"phase":"goal_final","checker_ref":requirement["checker_ref"],"targets":requirement["target_outputs"]});
                }
            }
            tools.call("artifact.write", json!({"slot":"proposal","value":{"protocol_version":1,"kind":"proposal","proposal":proposal}})).await.unwrap();
            ExecutionOutcome::Completed { usage: json!({}) }
        }
    }
    struct PausedPlanner(&'static str);
    #[async_trait::async_trait]
    impl Executor for PausedPlanner {
        fn capabilities(&self) -> ExecutorCapabilities {
            ExecutorCapabilities::MANAGED
        }
        fn enforces_json_candidate_limit(&self) -> bool {
            true
        }
        async fn execute(
            &self,
            invocation: Invocation,
            tools: Arc<dyn ToolBridge>,
            cancel: tokio_util::sync::CancellationToken,
        ) -> ExecutionOutcome {
            if self.0 == "needs_input" {
                return RequestPlanningInput
                    .execute(invocation, tools, cancel)
                    .await;
            }
            if self.0 == "valid" {
                return SubmitPlan.execute(invocation, tools, cancel).await;
            }
            let mut plan: Value = serde_json::from_str(include_str!("../../docs/design/10-goal-dag-orchestration/contract/document-review-package/plan.example.json")).unwrap();
            plan["goal_spec_digest"] = invocation.stage_input["goal_spec_digest"].clone();
            plan["profile_digest"] = invocation.stage_input["profile_digest"].clone();
            plan["nodes"]["review"]["depends_on"] = json!(["review"]);
            tools.call("artifact.write", json!({"slot":"proposal","value":{"protocol_version":1,"kind":"proposal","proposal":plan}})).await.unwrap();
            ExecutionOutcome::Completed {
                usage: json!({"controlled":true}),
            }
        }
    }
    #[tokio::test]
    async fn paused_planner_results_preserve_valid_invalid_and_input_branches_until_explicit_resume()
     {
        for branch in ["valid", "invalid", "needs_input"] {
            let (_directory, runtime, goals, caller, request) =
                setup_custom(Some(Arc::new(PausedPlanner(branch))), false, |registry| {
                    registry
                        .profiles
                        .get_mut("document-review@1")
                        .unwrap()
                        .planning_input_schemas = vec!["text".into()];
                })
                .await;
            let spec = request.spec.clone();
            let id = goals.submit(request, &caller).await.unwrap().goal_id;
            let run_id = goals.core.materialize(&id).unwrap().unwrap();
            goals
                .core
                .prepare_execution(&id, &run_id)
                .unwrap()
                .unwrap()
                .execute()
                .await
                .unwrap();
            let returned = goals.get(&id, &caller).await.unwrap();
            assert_eq!(
                returned.runs[&run_id].phase,
                GoalRunPhase::ExecutionReturned
            );
            goals
                .stop(&id, "pause-planner-result", returned.revision, &caller)
                .await
                .unwrap();
            assert!(goals.core.consume_return(&id, &run_id).unwrap());
            assert!(!goals.core.consume_return(&id, &run_id).unwrap());
            let paused = goals.get(&id, &caller).await.unwrap();
            assert_eq!(paused.status, GoalStatus::Stopped, "{branch}: {paused:?}");
            assert_eq!(paused.budget.runs_used, 1);
            assert_eq!(paused.progress.pending_requests, 0);
            assert!(paused.nodes.is_empty());
            assert!(paused.budget.reserved.is_empty());
            assert!(goals.core.materialize(&id).unwrap().is_none());
            assert!(goals.plan(&id, &caller).await.is_err());
            if branch == "invalid" {
                assert!(paused.runs[&run_id].plan_validation_failure.is_some());
            }
            let wait = paused.waiting.first().cloned();
            assert_eq!(wait.is_some(), branch == "needs_input");
            goals
                .resume(&id, "resume-planner-result", paused.revision, &caller)
                .await
                .unwrap();
            goals.core.advance(&id).unwrap();
            let resumed = goals.get(&id, &caller).await.unwrap();
            assert_eq!(resumed.budget.runs_used, 1);
            assert_eq!(
                serde_json::to_value(&resumed.spec).unwrap(),
                serde_json::to_value(&spec).unwrap()
            );
            match branch {
                "valid" => {
                    assert_eq!(resumed.stage, GoalStage::Executing);
                    assert!(goals.plan(&id, &caller).await.is_ok());
                    assert_eq!(resumed.nodes.len(), 1);
                }
                "invalid" => {
                    assert_eq!(resumed.stage, GoalStage::Planning);
                    assert_eq!(resumed.progress.pending_requests, 1);
                }
                "needs_input" => {
                    let wait = wait.unwrap();
                    assert_eq!(resumed.status, GoalStatus::Waiting);
                    assert_eq!(resumed.waiting[0].nonce, wait.nonce);
                    assert_eq!(resumed.waiting[0].expires_at_utc_ms, wait.expires_at_utc_ms);
                    goals
                        .signal(
                            &id,
                            GoalSignalRequest {
                                protocol_version: 1,
                                wait_id: wait.wait_id,
                                nonce: wait.nonce,
                                payload: json!("Clarified without changing the frozen scope"),
                                request_key: "clarify-paused-planner".into(),
                                expected_revision: resumed.revision,
                            },
                            &caller,
                        )
                        .await
                        .unwrap();
                    assert_eq!(
                        goals
                            .get(&id, &caller)
                            .await
                            .unwrap()
                            .progress
                            .pending_requests,
                        1
                    );
                }
                _ => unreachable!(),
            }
            let final_view = goals.get(&id, &caller).await.unwrap();
            assert_eq!(final_view.budget.runs_used, 1);
            goals
                .cancel(&id, "close-planner-branch", final_view.revision, &caller)
                .await
                .unwrap();
            runtime.shutdown().await.unwrap();
        }
    }
    #[tokio::test]
    async fn required_checks_keep_partial_pass_pause_and_late_fail_fast_results_distinct() {
        for first_verdict in [EvaluationVerdict::Pass, EvaluationVerdict::Fail] {
            let (_directory, runtime, goals, caller, request) = setup_custom(
                Some(Arc::new(CompleteGoal(first_verdict.clone()))),
                false,
                |registry| {
                    let profile = registry.profiles.get_mut("document-review@1").unwrap();
                    let mut late = profile.checks[0].clone();
                    late.id = "quality-late-pass".into();
                    profile.checks.push(late);
                },
            )
            .await;
            let id = goals.submit(request, &caller).await.unwrap().goal_id;
            for _ in 0..2 {
                goals.core.advance(&id).unwrap();
                let run = goals.core.materialize(&id).unwrap().unwrap();
                goals
                    .core
                    .prepare_execution(&id, &run)
                    .unwrap()
                    .unwrap()
                    .execute()
                    .await
                    .unwrap();
                goals.core.consume_return(&id, &run).unwrap();
            }
            goals.core.finish_node_check(&id, "review").unwrap();
            goals.core.advance(&id).unwrap();
            let mut returned = Vec::new();
            for _ in 0..2 {
                let run = goals.core.materialize(&id).unwrap().unwrap();
                goals
                    .core
                    .prepare_execution(&id, &run)
                    .unwrap()
                    .unwrap()
                    .execute()
                    .await
                    .unwrap();
                returned.push(run);
            }
            let [first_run, late]: [String; 2] = returned.try_into().unwrap();
            assert!(goals.core.consume_return(&id, &first_run).unwrap());
            let first = goals.get(&id, &caller).await.unwrap();
            assert_eq!(goals.evaluations(&id, &caller).await.unwrap().len(), 1);
            if first_verdict == EvaluationVerdict::Fail {
                assert_eq!(
                    first.terminal_intent.as_ref().unwrap().outcome,
                    GoalOutcome::NotMet
                );
                assert!(goals.core.consume_return(&id, &late).unwrap());
                let final_view = goals.get(&id, &caller).await.unwrap();
                assert_eq!(final_view.outcome, Some(GoalOutcome::NotMet));
                let receipts = goals.evaluations(&id, &caller).await.unwrap();
                assert_eq!(receipts.len(), 1);
                assert_eq!(receipts[0].verdict, EvaluationVerdict::Fail);
                let check = final_view
                    .evaluations
                    .iter()
                    .find(|c| c.check_id == "quality-late-pass")
                    .unwrap();
                assert_eq!(check.status, "stale_due_to_terminal");
                assert!(check.receipt_id.is_none());
                let candidate = final_view
                    .artifacts
                    .iter()
                    .find(|a| a.producer_run_id.as_deref() == Some(late.as_str()))
                    .unwrap();
                assert!(!candidate.accepted);
                assert_eq!(
                    parse_unique(
                        &goals
                            .read_artifact(&candidate.artifact.id, &caller)
                            .await
                            .unwrap()
                            .bytes
                    )
                    .unwrap()["verdict"],
                    "pass"
                );
            } else {
                assert!(first.outcome.is_none());
                assert!(first.terminal_intent.is_none());
                assert_eq!(first.stage, GoalStage::GoalChecking);
                goals
                    .stop(&id, "pause-partial-checks", first.revision, &caller)
                    .await
                    .unwrap();
                assert!(goals.core.consume_return(&id, &late).unwrap());
                let paused = goals.get(&id, &caller).await.unwrap();
                assert_eq!(paused.status, GoalStatus::Stopped);
                assert!(paused.outcome.is_none());
                assert_eq!(paused.budget.runs_used, 4);
                let receipts = goals.evaluations(&id, &caller).await.unwrap();
                assert_eq!(receipts.len(), 2);
                assert!(
                    receipts
                        .iter()
                        .all(|r| r.verdict == EvaluationVerdict::Pass)
                );
                assert!(!goals.core.advance(&id).unwrap());
                goals
                    .resume(&id, "finish-partial-checks", paused.revision, &caller)
                    .await
                    .unwrap();
                goals.core.advance(&id).unwrap();
                assert_eq!(
                    goals.get(&id, &caller).await.unwrap().outcome,
                    Some(GoalOutcome::Achieved)
                );
            }
            let final_view = goals.get(&id, &caller).await.unwrap();
            assert_eq!(final_view.nodes["review"].status, GoalNodeStatus::Succeeded);
            assert_eq!(final_view.budget.runs_used, 4);
            assert_eq!(final_view.progress.goal_occupied_slots, 0);
            assert!(final_view.budget.reserved.is_empty());
            assert!(!goals.core.consume_return(&id, &late).unwrap());
            assert_eq!(
                goals.get(&id, &caller).await.unwrap().revision,
                final_view.revision
            );
            runtime.shutdown().await.unwrap();
        }
    }
    #[tokio::test]
    async fn planner_locks_one_valid_plan_and_preserves_the_original_candidate_identity() {
        let (_directory, runtime, goals, caller, request) = setup(Some(Arc::new(SubmitPlan))).await;
        let id = goals.submit(request, &caller).await.unwrap().goal_id;
        let run_id = goals.core.materialize(&id).unwrap().unwrap();
        goals
            .core
            .prepare_execution(&id, &run_id)
            .unwrap()
            .unwrap()
            .execute()
            .await
            .unwrap();
        assert!(goals.core.consume_return(&id, &run_id).unwrap());
        assert!(!goals.core.consume_return(&id, &run_id).unwrap());
        let snapshot = goals.get(&id, &caller).await.unwrap();
        assert_eq!(snapshot.stage, GoalStage::Executing);
        assert_eq!(snapshot.outcome, None);
        assert_eq!(snapshot.nodes.len(), 1);
        assert_eq!(snapshot.runs[&run_id].phase, GoalRunPhase::Settled);
        assert!(!snapshot.budget.reserved.contains_key(&run_id));
        let record = goals.core.inner.store.goal_load(&id).unwrap().record;
        assert_eq!(
            record.proposals[0].candidate_sha,
            snapshot.artifacts[0].artifact.sha256
        );
        let plan = goals.plan(&id, &caller).await.unwrap();
        assert_eq!(
            plan.plan_digest,
            record.proposals[0].proposal_digest.clone().unwrap()
        );
        runtime.shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn invalid_planner_results_stop_at_the_frozen_attempt_limit() {
        let (_directory, runtime, goals, caller, request) =
            setup(Some(Arc::new(SubmitCandidate))).await;
        let id = goals.submit(request, &caller).await.unwrap().goal_id;
        for attempt in 1..=3 {
            let run_id = goals.core.materialize(&id).unwrap().unwrap();
            goals
                .core
                .prepare_execution(&id, &run_id)
                .unwrap()
                .unwrap()
                .execute()
                .await
                .unwrap();
            goals.core.consume_return(&id, &run_id).unwrap();
            assert_eq!(
                goals.get(&id, &caller).await.unwrap().budget.runs_used,
                attempt
            );
        }
        let snapshot = goals.get(&id, &caller).await.unwrap();
        assert_eq!(snapshot.outcome, Some(GoalOutcome::Failed));
        assert_eq!(
            snapshot.terminal_intent.unwrap().reason,
            "planning_exhausted"
        );
        assert!(goals.core.materialize(&id).unwrap().is_none());
        assert!(snapshot.artifacts.iter().all(|a| !a.accepted));
        runtime.shutdown().await.unwrap();
    }
    #[tokio::test(start_paused = true)]
    async fn expired_planning_deadline_stops_pending_retry_without_resetting_the_original_window() {
        let (_directory, runtime, goals, caller, mut request) =
            setup(Some(Arc::new(SubmitCandidate))).await;
        request.spec.planning.max_planning_ms = 50;
        let id = goals.submit(request, &caller).await.unwrap().goal_id;
        let first = goals.get(&id, &caller).await.unwrap();
        let run_id = goals.core.materialize(&id).unwrap().unwrap();
        goals
            .core
            .prepare_execution(&id, &run_id)
            .unwrap()
            .unwrap()
            .execute()
            .await
            .unwrap();
        goals.core.consume_return(&id, &run_id).unwrap();
        let retry = goals.get(&id, &caller).await.unwrap();
        assert_eq!(retry.stage, GoalStage::Planning);
        assert_eq!(retry.progress.pending_requests, 1);
        assert_eq!(
            retry.planning_deadline_utc_ms,
            first.planning_deadline_utc_ms
        );
        tokio::time::advance(std::time::Duration::from_millis(51)).await;
        assert!(goals.core.materialize(&id).unwrap().is_none());
        goals.core.maintain(&id).unwrap();
        let ended = goals.get(&id, &caller).await.unwrap();
        assert_eq!(ended.outcome, Some(GoalOutcome::Failed));
        assert_eq!(
            ended.terminal_intent.as_ref().unwrap().reason,
            "planning_timeout"
        );
        assert_eq!(ended.budget.runs_used, 1);
        assert_eq!(ended.runs.len(), 1);
        assert!(ended.nodes.is_empty());
        assert!(ended.budget.reserved.is_empty());
        assert_eq!(
            ended.planning_deadline_utc_ms,
            first.planning_deadline_utc_ms
        );
        runtime.shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn managed_goal_run_starts_once_and_native_completion_only_records_diagnostics() {
        let (_directory, runtime, goals, caller, request) =
            setup(Some(Arc::new(SubmitCandidate))).await;
        let id = goals.submit(request, &caller).await.unwrap().goal_id;
        let run_id = goals.core.materialize(&id).unwrap().unwrap();
        let execution = goals.core.prepare_execution(&id, &run_id).unwrap().unwrap();
        assert!(
            goals
                .core
                .prepare_execution(&id, &run_id)
                .unwrap()
                .is_none()
        );
        execution.execute().await.unwrap();
        let snapshot = goals.get(&id, &caller).await.unwrap();
        assert_eq!(snapshot.status, GoalStatus::Running);
        assert_eq!(snapshot.outcome, None);
        assert_eq!(snapshot.budget.runs_used, 1);
        assert_eq!(snapshot.budget.used.tool_calls, 1);
        assert!(snapshot.budget.used.candidate_bytes > 0);
        assert_eq!(
            snapshot.runs[&run_id].phase,
            GoalRunPhase::ExecutionReturned
        );
        assert!(snapshot.runs[&run_id].native_stop_confirmed);
        assert_eq!(snapshot.artifacts.len(), 1);
        assert!(!snapshot.artifacts[0].accepted);
        assert_eq!(
            snapshot.artifacts[0].producer_run_id.as_deref(),
            Some(run_id.as_str())
        );
        assert!(snapshot.budget.reserved[&run_id].remaining.active_ms > 0);
        assert_eq!(snapshot.budget.reserved[&run_id].remaining.tool_calls, 0);
        runtime.shutdown().await.unwrap();
    }
    async fn setup(
        executor: Option<Arc<dyn Executor>>,
    ) -> (tempfile::TempDir, Runtime, Goals, Caller, GoalRequest) {
        setup_mode(executor, false).await
    }
    #[tokio::test]
    async fn exhausted_transition_budget_closes_the_goal_without_starting_reserved_work() {
        let (_directory, runtime, goals, caller, mut request) =
            setup_mode(Some(Arc::new(CompleteGoal(EvaluationVerdict::Pass))), true).await;
        request.spec.limits.max_transitions = 2;
        let id = goals.submit(request, &caller).await.unwrap().goal_id;
        let snapshot = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let s = goals.get(&id, &caller).await.unwrap();
                if s.status == GoalStatus::Finished {
                    break s;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("budget refusal must become a durable failure rather than a scheduling loop");
        assert_eq!(snapshot.outcome, Some(GoalOutcome::Failed));
        assert_eq!(snapshot.error.unwrap().code, "budget_exhausted");
        assert_eq!(snapshot.budget.transitions_used, 2);
        assert!(snapshot.budget.cleanup_transitions > 0);
        assert!(
            snapshot
                .runs
                .values()
                .all(|r| !r.started && r.phase == GoalRunPhase::Settled)
        );
        assert!(snapshot.artifacts.is_empty());
        assert!(snapshot.budget.reserved.is_empty());
        runtime.shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn public_runtime_dispatches_a_goal_without_private_progress_calls() {
        let (_directory, runtime, goals, caller, request) =
            setup_mode(Some(Arc::new(CompleteGoal(EvaluationVerdict::Pass))), true).await;
        let id = goals.submit(request, &caller).await.unwrap().goal_id;
        let snapshot = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let snapshot = goals.get(&id, &caller).await.unwrap();
                if snapshot.status == GoalStatus::Finished {
                    break snapshot;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("public Runtime must drive Planner, member, projection and final check");
        assert_eq!(snapshot.outcome, Some(GoalOutcome::Achieved));
        assert_eq!(snapshot.budget.runs_used, 3);
        assert!(snapshot.budget.reserved.is_empty());
        let trace = goals
            .trace(
                &id,
                TraceQuery {
                    limit: 1000,
                    ..Default::default()
                },
                &caller,
            )
            .unwrap();
        assert!(trace.events.iter().any(|e| e.event.kind == "goal.admitted"));
        assert!(trace.events.iter().any(|e| e.event.kind == "goal.finished"));
        assert!(
            trace
                .events
                .iter()
                .any(|e| e.event.kind == "node.accepted" && e.event.data["node_id"] == "review")
        );
        let trace_id = crate::json::digest(id.as_bytes())[..32].to_owned();
        assert!(trace.events.iter().all(|e| e.event.trace_id == trace_id
            && e.event.data["goal_id"] == id
            && e.event.data["execution_kind"] == "goal"));
        assert_eq!(
            trace
                .events
                .iter()
                .filter(|e| e.event.kind == "phase.started" && e.event.data["scope"] == "run")
                .count(),
            3
        );
        assert!(
            trace
                .events
                .iter()
                .filter(|e| e.event.kind == "tool.started")
                .all(|e| e.event.data["scope"] == "run" && e.event.data["role"].is_object())
        );
        assert!(trace.open_spans.is_empty());
        assert!(!trace.complete); // This controlled executor provides no model/harness observation.
        assert!(
            trace
                .issues
                .iter()
                .all(|issue| issue.starts_with("internal_unavailable:")),
            "{:?}",
            trace.issues
        );
        assert!(
            trace
                .source_cleanup
                .iter()
                .any(|c| c.run_id == id && c.executor.is_none() && c.state == "not_applicable")
        );
        let client = GoalClient::open(_directory.path().join("store"), caller.clone()).unwrap();
        assert_eq!(client.get(&id).unwrap().revision, snapshot.revision);
        assert_eq!(
            client.plan(&id).unwrap().plan_digest,
            goals.plan(&id, &caller).await.unwrap().plan_digest
        );
        assert_eq!(client.budgets(&id).unwrap().budget.runs_used, 3);
        assert_eq!(client.evaluations(&id).unwrap().len(), 1);
        let artifact = snapshot.artifacts.iter().find(|a| a.accepted).unwrap();
        assert_eq!(
            client
                .read_artifact(&artifact.artifact.id)
                .unwrap()
                .reference
                .sha256,
            artifact.artifact.sha256
        );
        let readonly_trace = client
            .trace(
                &id,
                TraceQuery {
                    limit: 1000,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(readonly_trace.events.len(), trace.events.len());
        assert!(
            readonly_trace
                .issues
                .iter()
                .any(|i| i == "collector_not_flushed")
        );
        let other = GoalClient::open(
            _directory.path().join("store"),
            Caller {
                principal: "bob".into(),
                grants: vec![],
            },
        )
        .unwrap();
        assert_eq!(other.get(&id).unwrap_err().code, "not_found");
        assert_eq!(
            other.read_artifact(&artifact.artifact.id).unwrap_err().code,
            "not_found"
        );
        runtime.shutdown().await.unwrap();
        drop(goals);
        drop(runtime);
        let offline = client.get(&id).unwrap();
        assert_eq!(offline.revision, snapshot.revision);
        assert_eq!(offline.budget.runs_used, 3);
        assert_eq!(offline.outcome, Some(GoalOutcome::Achieved));
        assert!(
            !client
                .trace(
                    &id,
                    TraceQuery {
                        limit: 1000,
                        ..Default::default()
                    }
                )
                .unwrap()
                .issues
                .iter()
                .any(|i| i == "collector_not_flushed")
        );
    }
    async fn setup_mode(
        executor: Option<Arc<dyn Executor>>,
        dispatched: bool,
    ) -> (tempfile::TempDir, Runtime, Goals, Caller, GoalRequest) {
        setup_custom(executor, dispatched, |_| {}).await
    }
    async fn setup_custom(
        executor: Option<Arc<dyn Executor>>,
        dispatched: bool,
        configure: impl FnOnce(&mut OrchestrationRegistry),
    ) -> (tempfile::TempDir, Runtime, Goals, Caller, GoalRequest) {
        let (mut registry, mut bindings) = super::super::tests::fixture();
        if let Some(executor) = executor {
            bindings.executors.insert("milkie".into(), executor);
        }
        bindings.grants.insert(
            "goal-user".into(),
            Grant {
                definitions: vec![],
                tools: vec!["artifact.write".into()],
                limits: ResourceLimits {
                    max_active_ms: 5_000_000,
                    max_wall_ms: 1_000_000,
                    max_runs: 20,
                    max_transitions: 1000,
                },
            },
        );
        configure(&mut registry);
        registry.validate_and_freeze(&bindings).unwrap();
        let limits = GoalLimits {
            max_wall_ms: 1_000_000,
            max_active_ms: 5_000_000,
            max_runs: 20,
            max_transitions: 1000,
            max_tool_calls: 200,
            max_total_candidate_bytes: 10_000_000,
            max_concurrent_runs: 4,
        };
        let options = OrchestrationOptions {
            grants: BTreeMap::from([(
                "goal-user".into(),
                GoalGrant {
                    profile_digests: vec![registry.profile_digest("document-review@1").unwrap()],
                    capabilities: registry
                        .frozen()
                        .unwrap()
                        .capabilities
                        .keys()
                        .cloned()
                        .collect(),
                    tools: vec!["artifact.write".into()],
                    limits: limits.clone(),
                },
            )]),
            limits: limits.clone(),
            max_admitted_goals: 8,
            max_spec_bytes: 1_048_576,
            max_plan_bytes: 4_194_304,
            max_nodes: 256,
            max_edges: 4096,
            max_workers_per_node: 32,
            max_json_candidate_bytes: 4_194_304,
        };
        let directory = tempfile::tempdir().unwrap();
        let (runtime, goals) = if dispatched {
            let runtime = Runtime::open_orchestrated(
                RuntimeOptions::new(directory.path().join("store")),
                vec![],
                bindings,
                registry,
                options,
                Some(TraceSetup {
                    options: TraceOptions {
                        max_events_per_run: 1000,
                        max_bytes_per_run: 1_048_576,
                        source_max_events: 1000,
                        source_max_bytes: 1_048_576,
                    },
                    policy: TracePolicy {
                        policy_id: "goal-tests".into(),
                        max_total_events: 10_000,
                        max_total_bytes: 10_485_760,
                        max_runs: 100,
                        retention_ms: 60_000,
                        max_content_bytes: 0,
                        content_tools: BTreeMap::new(),
                        filter_version: None,
                        filter: None,
                    },
                }),
            )
            .await
            .unwrap();
            let goals = runtime.goals().unwrap();
            (runtime, goals)
        } else {
            let runtime = Runtime::open(
                RuntimeOptions::new(directory.path().join("store")),
                vec![],
                bindings,
            )
            .await
            .unwrap();
            let core = GoalCore::new(
                runtime.inner.clone(),
                registry.frozen().unwrap(),
                options,
                "goal-test-binding".into(),
            )
            .unwrap();
            (runtime, Goals { core })
        };
        let caller = Caller {
            principal: "alice".into(),
            grants: vec!["goal-user".into()],
        };
        let request = GoalRequest {
            protocol_version: 1,
            request_key: "request-one".into(),
            grant_ref: "goal-user".into(),
            spec: GoalSpec {
                goal: "Review the document".into(),
                inputs: json!({"document_text":"Document"}),
                profile_ref: "document-review@1".into(),
                planning: GoalPlanning {
                    mode: PlanningMode::Autonomous,
                    max_revisions: 2,
                    max_planning_ms: 100_000,
                    template_ref: None,
                },
                limits,
                parent_goal_id: None,
                imports: vec![],
            },
        };
        (directory, runtime, goals, caller, request)
    }
    #[tokio::test]
    async fn pending_goal_can_pause_replay_resume_and_cancel_without_materializing_a_run() {
        let (_directory, runtime, goals, caller, request) = setup(None).await;
        let admitted = goals.submit(request.clone(), &caller).await.unwrap();
        let id = &admitted.goal_id;
        let initial = goals.get(id, &caller).await.unwrap();
        assert_eq!(initial.status, GoalStatus::Running);
        assert_eq!(initial.budget.runs_used, 0);
        let row = goals.core.inner.store.goal_load(id).unwrap();
        assert_eq!(row.record.requests.len(), 1);
        let pending = row.record.requests.values().next().unwrap();
        let input: Value = serde_json::from_slice(
            &goals
                .core
                .inner
                .store
                .read_blob(&pending.input_sha)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(input["planning_feedback"]["attempt"], 1);
        let stopped = goals
            .stop(id, "stop-one", initial.revision, &caller)
            .await
            .unwrap();
        assert_eq!(
            goals.get(id, &caller).await.unwrap().status,
            GoalStatus::Stopped
        );
        assert_eq!(
            goals
                .stop(id, "stop-one", initial.revision, &caller)
                .await
                .unwrap()
                .applied_revision,
            stopped.applied_revision
        );
        assert!(goals.submit(request, &caller).await.unwrap().replayed);
        assert_eq!(
            goals.get(id, &caller).await.unwrap().status,
            GoalStatus::Stopped
        );
        let resumed = goals
            .resume(id, "resume-one", stopped.applied_revision, &caller)
            .await
            .unwrap();
        assert_eq!(goals.get(id, &caller).await.unwrap().budget.runs_used, 0);
        goals
            .cancel(id, "cancel-one", resumed.applied_revision, &caller)
            .await
            .unwrap();
        let final_state = goals.get(id, &caller).await.unwrap();
        assert_eq!(final_state.status, GoalStatus::Finished);
        assert_eq!(final_state.outcome, Some(GoalOutcome::Cancelled));
        assert!(final_state.budget.protected_final.is_empty());
        assert_eq!(
            goals
                .get(
                    id,
                    &Caller {
                        principal: "bob".into(),
                        grants: vec!["goal-user".into()]
                    }
                )
                .await
                .unwrap_err()
                .code,
            "not_found"
        );
        runtime.shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn clock_blocking_is_idempotent_and_requires_explicit_resume_after_trust_returns() {
        let (_directory, runtime, mut goals, caller, request) = setup(None).await;
        let id = goals.submit(request, &caller).await.unwrap().goal_id;
        let original = goals.get(&id, &caller).await.unwrap();
        Arc::get_mut(&mut goals.core).unwrap().clock =
            Arc::new(GoalClock::new(crate::runtime::now_ms() + 400));
        assert!(goals.core.maintain(&id).unwrap());
        let blocked = goals.get(&id, &caller).await.unwrap();
        assert_eq!(blocked.status, GoalStatus::Blocked);
        assert!(
            blocked
                .blockers
                .iter()
                .any(|b| b.reason == "clock_untrusted")
        );
        assert!(goals.core.materialize(&id).unwrap().is_none());
        let rejected = goals
            .resume(&id, "early-resume", blocked.revision, &caller)
            .await
            .unwrap_err();
        assert!(["clock_untrusted", "not_resumable"].contains(&rejected.code.as_str()));
        // A rejected command has its own durable idempotency receipt/revision.
        let after_rejection = goals.get(&id, &caller).await.unwrap();
        let replay = goals
            .resume(&id, "early-resume", blocked.revision, &caller)
            .await
            .unwrap_err();
        assert_eq!(replay.code, rejected.code);
        assert_eq!(
            goals.get(&id, &caller).await.unwrap().revision,
            after_rejection.revision
        );
        goals.core.recover().unwrap();
        goals.core.recover().unwrap();
        assert_eq!(
            goals.get(&id, &caller).await.unwrap().revision,
            after_rejection.revision
        );
        tokio::time::sleep(std::time::Duration::from_millis(450)).await;
        assert!(goals.core.maintain(&id).unwrap());
        let stopped = goals.get(&id, &caller).await.unwrap();
        assert_eq!(stopped.status, GoalStatus::Stopped);
        assert!(stopped.blockers.is_empty());
        assert_eq!(stopped.budget.runs_used, 0);
        assert_eq!(stopped.progress.pending_requests, 1);
        assert_eq!(stopped.wall_deadline_utc_ms, original.wall_deadline_utc_ms);
        assert_eq!(
            stopped.planning_deadline_utc_ms,
            original.planning_deadline_utc_ms
        );
        goals.core.recover().unwrap();
        let recovered = goals.get(&id, &caller).await.unwrap();
        let revision = recovered.revision;
        goals.core.recover().unwrap();
        assert_eq!(goals.get(&id, &caller).await.unwrap().revision, revision);
        goals
            .resume(&id, "trusted-resume", revision, &caller)
            .await
            .unwrap();
        let resumed = goals.get(&id, &caller).await.unwrap();
        assert_eq!(resumed.status, GoalStatus::Running);
        assert_eq!(resumed.budget.runs_used, 0);
        assert_eq!(resumed.progress.pending_requests, 1);
        goals
            .cancel(&id, "finish-clock-test", resumed.revision, &caller)
            .await
            .unwrap();
        runtime.shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn recovery_before_first_reservation_requires_resume_and_does_not_invent_a_run() {
        let (_directory, runtime, goals, caller, request) = setup(None).await;
        let receipt = goals.submit(request.clone(), &caller).await.unwrap();
        let id = receipt.goal_id;
        let before = goals.get(&id, &caller).await.unwrap();
        assert!(before.runs.is_empty());
        goals.core.recover().unwrap();
        let paused = goals.get(&id, &caller).await.unwrap();
        assert_eq!(paused.status, GoalStatus::Stopped);
        assert!(paused.runs.is_empty());
        assert_eq!(paused.budget.runs_used, 0);
        assert_eq!(paused.progress.pending_requests, 1);
        assert_eq!(paused.progress.host_occupied_slots, 0);
        assert_eq!(paused.spec_digest, before.spec_digest);
        assert_eq!(paused.wall_deadline_utc_ms, before.wall_deadline_utc_ms);
        assert_eq!(
            paused.planning_deadline_utc_ms,
            before.planning_deadline_utc_ms
        );
        goals.core.recover().unwrap();
        assert_eq!(
            goals.get(&id, &caller).await.unwrap().revision,
            paused.revision
        );
        assert!(goals.core.materialize(&id).unwrap().is_none());
        goals
            .resume(&id, "resume-before-first-run", paused.revision, &caller)
            .await
            .unwrap();
        let run = goals.core.materialize(&id).unwrap().unwrap();
        let first = goals.get(&id, &caller).await.unwrap();
        assert_eq!(first.runs.len(), 1);
        assert_eq!(first.budget.runs_used, 1);
        assert!(!first.runs[&run].started);
        assert!(goals.submit(request, &caller).await.unwrap().replayed);
        assert_eq!(goals.get(&id, &caller).await.unwrap().runs.len(), 1);
        goals
            .cancel(&id, "close-first-reservation", first.revision, &caller)
            .await
            .unwrap();
        runtime.shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn recovery_of_reserved_unstarted_work_reuses_its_run_and_charges_no_second_attempt() {
        let (_directory, runtime, goals, caller, request) =
            setup(Some(Arc::new(CompleteGoal(EvaluationVerdict::Pass)))).await;
        let id = goals.submit(request, &caller).await.unwrap().goal_id;
        let run = goals.core.materialize(&id).unwrap().unwrap();
        let before = goals.get(&id, &caller).await.unwrap();
        assert!(!before.runs[&run].started);
        assert_eq!(before.progress.goal_occupied_slots, 1);
        goals.core.recover().unwrap();
        let recovered = goals.get(&id, &caller).await.unwrap();
        assert_eq!(recovered.status, GoalStatus::Stopped);
        assert_eq!(recovered.runs[&run].phase, GoalRunPhase::Suspended);
        assert_eq!(recovered.progress.goal_occupied_slots, 0);
        assert!(!recovered.budget.reserved.contains_key(&run));
        goals.core.recover().unwrap();
        assert_eq!(
            goals.get(&id, &caller).await.unwrap().revision,
            recovered.revision
        );
        goals
            .resume(&id, "resume-first-start", recovered.revision, &caller)
            .await
            .unwrap();
        assert_eq!(goals.core.materialize(&id).unwrap(), Some(run.clone()));
        let resumed = goals.get(&id, &caller).await.unwrap();
        assert_eq!(resumed.budget.runs_used, before.budget.runs_used);
        goals
            .core
            .prepare_execution(&id, &run)
            .unwrap()
            .unwrap()
            .execute()
            .await
            .unwrap();
        goals.core.consume_return(&id, &run).unwrap();
        assert_eq!(
            goals.get(&id, &caller).await.unwrap().stage,
            GoalStage::Executing
        );
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn recovery_cannot_accept_completed_uncommitted_candidates_and_is_idempotent() {
        let (_directory, runtime, goals, caller, request) =
            setup(Some(Arc::new(CompleteGoal(EvaluationVerdict::Pass)))).await;
        let id = goals.submit(request, &caller).await.unwrap().goal_id;
        let run = goals.core.materialize(&id).unwrap().unwrap();
        goals
            .core
            .prepare_execution(&id, &run)
            .unwrap()
            .unwrap()
            .execute()
            .await
            .unwrap();
        let before = goals.get(&id, &caller).await.unwrap();
        assert!(before.runs[&run].native_stop_confirmed);
        assert_eq!(before.runs[&run].phase, GoalRunPhase::ExecutionReturned);
        goals.core.recover().unwrap();
        let recovered = goals.get(&id, &caller).await.unwrap();
        assert_eq!(recovered.status, GoalStatus::Blocked);
        assert!(recovered.blockers.iter().any(|b| b.reason == "interrupted"));
        assert!(recovered.artifacts.iter().all(|a| !a.accepted));
        assert_eq!(recovered.artifacts.len(), before.artifacts.len());
        assert_eq!(recovered.progress.goal_occupied_slots, 0);
        assert!(goals.core.materialize(&id).unwrap().is_none());
        assert!(
            goals
                .resume(&id, "unsafe-replay", recovered.revision, &caller)
                .await
                .is_err()
        );
        let revision = goals.get(&id, &caller).await.unwrap().revision;
        goals.core.recover().unwrap();
        assert_eq!(
            goals.get(&id, &caller).await.unwrap().revision,
            revision,
            "recovery without new facts must not invent a pause or another revision"
        );
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn recovery_of_node_checking_preserves_members_but_never_recomputes_public_success() {
        let (_directory, runtime, goals, caller, request) =
            setup(Some(Arc::new(CompleteGoal(EvaluationVerdict::Pass)))).await;
        let id = goals.submit(request, &caller).await.unwrap().goal_id;
        for _ in 0..2 {
            goals.core.advance(&id).unwrap();
            let run = goals.core.materialize(&id).unwrap().unwrap();
            goals
                .core
                .prepare_execution(&id, &run)
                .unwrap()
                .unwrap()
                .execute()
                .await
                .unwrap();
            goals.core.consume_return(&id, &run).unwrap();
        }
        let before = goals.get(&id, &caller).await.unwrap();
        assert_eq!(before.nodes["review"].status, GoalNodeStatus::Checking);
        let member = before.nodes["review"].members["reviewer"].outputs.clone();
        assert!(!member.is_empty());
        assert!(before.nodes["review"].outputs.is_empty());
        goals.core.recover().unwrap();
        let recovered = goals.get(&id, &caller).await.unwrap();
        assert_eq!(recovered.status, GoalStatus::Blocked);
        assert_eq!(
            recovered.nodes["review"].status,
            GoalNodeStatus::Interrupted
        );
        assert_eq!(
            serde_json::to_value(&recovered.nodes["review"].members["reviewer"].outputs).unwrap(),
            serde_json::to_value(member).unwrap()
        );
        assert!(!recovered.budget.incomplete_active_subjects.is_empty());
        assert!(!goals.core.finish_node_check(&id, "review").unwrap());
        assert!(!goals.core.advance(&id).unwrap());
        assert!(
            goals.get(&id, &caller).await.unwrap().nodes["review"]
                .outputs
                .is_empty()
        );
        let revision = recovered.revision;
        goals.core.recover().unwrap();
        assert_eq!(goals.get(&id, &caller).await.unwrap().revision, revision);
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn a_projection_panic_fails_the_node_without_poisoning_coordination_or_losing_members() {
        let (_directory, runtime, goals, caller, request) =
            setup(Some(Arc::new(CompleteGoal(EvaluationVerdict::Pass)))).await;
        let id = goals.submit(request, &caller).await.unwrap().goal_id;
        for _ in 0..2 {
            goals.core.advance(&id).unwrap();
            let run = goals.core.materialize(&id).unwrap().unwrap();
            goals
                .core
                .prepare_execution(&id, &run)
                .unwrap()
                .unwrap()
                .execute()
                .await
                .unwrap();
            goals.core.consume_return(&id, &run).unwrap();
        }
        let before = goals.get(&id, &caller).await.unwrap();
        assert_eq!(before.nodes["review"].status, GoalNodeStatus::Checking);
        let members = before.nodes["review"].members["reviewer"].outputs.clone();
        assert!(!members.is_empty());
        super::super::nodes::PANIC_NODE_PROJECTION.set(true);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            goals.core.finish_node_check(&id, "review")
        }));
        assert!(
            result.is_ok(),
            "a built-in projection panic escaped the Goal boundary"
        );
        assert!(result.unwrap().unwrap());
        let ended = goals.get(&id, &caller).await.unwrap();
        assert_eq!(ended.outcome, Some(GoalOutcome::Failed));
        assert_eq!(ended.nodes["review"].status, GoalNodeStatus::Failed);
        assert_eq!(
            ended.nodes["review"].error.as_ref().unwrap().code,
            "contract_error"
        );
        assert!(ended.nodes["review"].outputs.is_empty());
        assert!(goals.evaluations(&id, &caller).await.unwrap().is_empty());
        assert_eq!(
            serde_json::to_value(&ended.nodes["review"].members["reviewer"].outputs).unwrap(),
            serde_json::to_value(&members).unwrap()
        );
        for artifact in members.values() {
            assert!(
                !goals
                    .read_artifact(&artifact.id, &caller)
                    .await
                    .unwrap()
                    .bytes
                    .is_empty()
            );
        }
        assert_eq!(ended.budget.runs_used, 2);
        assert_eq!(ended.progress.goal_occupied_slots, 0);
        assert!(ended.budget.reserved.is_empty());
        assert!(!goals.core.finish_node_check(&id, "review").unwrap());
        assert!(!goals.core.advance(&id).unwrap());
        assert_eq!(
            goals.get(&id, &caller).await.unwrap().revision,
            ended.revision
        );
        runtime.shutdown().await.unwrap();
    }
    struct RecoveringStorageFault {
        stage: &'static str,
        inner: Mutex<Option<Arc<Inner>>>,
    }
    #[async_trait::async_trait]
    impl Executor for RecoveringStorageFault {
        fn capabilities(&self) -> ExecutorCapabilities {
            ExecutorCapabilities::MANAGED
        }
        fn enforces_json_candidate_limit(&self) -> bool {
            true
        }
        async fn execute(
            &self,
            invocation: Invocation,
            tools: Arc<dyn ToolBridge>,
            cancel: tokio_util::sync::CancellationToken,
        ) -> ExecutionOutcome {
            let role = tools.goal_context().unwrap().role;
            let target = matches!(
                (self.stage, &role),
                ("planner", GoalRunRole::Planner { .. })
                    | ("worker", GoalRunRole::NodeMember { .. })
                    | ("checker", GoalRunRole::FinalCheck { .. })
            );
            if target {
                let (slot, value) = match &role {
                    GoalRunRole::Planner { .. } => (
                        "proposal",
                        json!({"protocol_version":1,"kind":"needs_input","input_schema_ref":"text","reason":"probe"}),
                    ),
                    GoalRunRole::NodeMember { .. } => {
                        ("report", json!({"summary":"failed attempt","findings":[]}))
                    }
                    _ => {
                        let input: CheckInput =
                            serde_json::from_value(invocation.stage_input.clone()).unwrap();
                        let results = input.rules.rules.iter().map(|rule| json!({"id":rule.id,"verdict":"pass","message":"probe","evidence_refs":input.evidence_refs})).collect::<Vec<_>>();
                        (
                            "evaluation",
                            json!({"protocol_version":1,"verdict":"pass","results":results,"evidence_refs":input.evidence_refs,"violations":[]}),
                        )
                    }
                };
                let inner = self.inner.lock().unwrap().clone().unwrap();
                rusqlite::Connection::open(inner.options.store_dir.join("state.sqlite")).unwrap().execute_batch("CREATE TRIGGER reject_once BEFORE INSERT ON goal_artifacts BEGIN SELECT RAISE(ABORT, 'review regression storage fault'); END;").unwrap();
                let error = tools
                    .call("artifact.write", json!({"slot":slot,"value":value}))
                    .await
                    .unwrap_err();
                assert_eq!(error.code, "storage_error");
                rusqlite::Connection::open(inner.options.store_dir.join("state.sqlite"))
                    .unwrap()
                    .execute_batch("DROP TRIGGER reject_once")
                    .unwrap();
            }
            CompleteGoal(EvaluationVerdict::Pass)
                .execute(invocation, tools, cancel)
                .await
        }
    }
    #[tokio::test]
    async fn review_regression_storage_failure_survives_successful_candidate_retry() {
        for stage in ["planner", "worker", "checker"] {
            let executor = Arc::new(RecoveringStorageFault {
                stage,
                inner: Mutex::new(None),
            });
            let (_dir, runtime, goals, caller, mut request) = setup(Some(executor.clone())).await;
            *executor.inner.lock().unwrap() = Some(runtime.inner.clone());
            request.spec.planning.max_revisions = 0;
            let id = goals.submit(request, &caller).await.unwrap().goal_id;
            for _ in 0..12 {
                goals.core.advance(&id).unwrap();
                if let Some(run) = goals.core.materialize(&id).unwrap() {
                    goals
                        .core
                        .prepare_execution(&id, &run)
                        .unwrap()
                        .unwrap()
                        .execute()
                        .await
                        .unwrap();
                    goals.core.consume_return(&id, &run).unwrap();
                }
                let snapshot = goals.get(&id, &caller).await.unwrap();
                for (node, state) in &snapshot.nodes {
                    if state.status == GoalNodeStatus::Checking {
                        goals.core.finish_node_check(&id, node).unwrap();
                    }
                }
                if goals.get(&id, &caller).await.unwrap().status == GoalStatus::Finished {
                    break;
                }
            }
            let ended = goals.get(&id, &caller).await.unwrap();
            assert_eq!(
                ended.outcome,
                Some(GoalOutcome::Failed),
                "storage failure cannot become achieved"
            );
            assert!(ended.runs.values().any(|run| {
                run.error
                    .as_ref()
                    .is_some_and(|error| error.code == "storage_error")
            }));
            if stage == "worker" {
                assert!(ended.nodes["review"].outputs.is_empty());
            }
            assert!(ended.artifacts.iter().any(|a| !a.accepted
                && a.producer_run_id.as_ref().is_some_and(|id| {
                    ended.runs[id]
                        .error
                        .as_ref()
                        .is_some_and(|e| e.code == "storage_error")
                })));
            assert!(ended.budget.reserved.is_empty());
            assert_eq!(ended.progress.goal_occupied_slots, 0);
            assert!(goals.evaluations(&id, &caller).await.unwrap().is_empty());
            runtime.shutdown().await.unwrap();
        }
    }
    #[tokio::test(start_paused = true)]
    async fn review_regression_cancel_cannot_replace_an_already_due_deadline() {
        for kind in ["wall", "planning", "input"] {
            let (_dir, runtime, goals, caller, mut request) =
                setup_custom(Some(Arc::new(RequestPlanningInput)), false, |registry| {
                    registry
                        .profiles
                        .get_mut("document-review@1")
                        .unwrap()
                        .planning_input_schemas = vec!["text".into()];
                })
                .await;
            if kind == "wall" {
                request.spec.limits.max_wall_ms = 50;
            }
            if kind == "planning" {
                request.spec.planning.max_planning_ms = 50;
            }
            let id = goals.submit(request, &caller).await.unwrap().goal_id;
            if kind == "input" {
                let run = goals.core.materialize(&id).unwrap().unwrap();
                goals
                    .core
                    .prepare_execution(&id, &run)
                    .unwrap()
                    .unwrap()
                    .execute()
                    .await
                    .unwrap();
                goals.core.consume_return(&id, &run).unwrap();
                let row = goals.core.inner.store.goal_load(&id).unwrap();
                let mut next = row.record.clone();
                next.waiting.values_mut().next().unwrap().expires_at_utc_ms =
                    goals.core.clock.observe().unwrap().utc_ms + 50;
                goals
                    .core
                    .commit_goal(
                        &row,
                        &next,
                        GoalWriteGuard::Cleanup,
                        None,
                        &Default::default(),
                        &goals.core.clock,
                    )
                    .unwrap();
            }
            let before = goals.get(&id, &caller).await.unwrap();
            tokio::time::advance(std::time::Duration::from_millis(51)).await;
            let _ = goals
                .cancel(&id, "cancel-at-expiry", before.revision, &caller)
                .await;
            let after = goals.get(&id, &caller).await.unwrap();
            assert_eq!(after.outcome, Some(GoalOutcome::Failed), "{kind}");
            let expected = match kind {
                "wall" => "wall_timeout",
                "planning" => "planning_timeout",
                _ => "input_timeout",
            };
            assert_eq!(after.terminal_intent.as_ref().unwrap().reason, expected);
            let _ = goals
                .cancel(&id, "cancel-at-expiry", before.revision, &caller)
                .await;
            assert_eq!(
                goals.get(&id, &caller).await.unwrap().revision,
                after.revision
            );
            runtime.shutdown().await.unwrap();
        }
    }

    struct DeferredCheckpoint(EvaluationVerdict);
    #[async_trait::async_trait]
    impl Executor for DeferredCheckpoint {
        fn capabilities(&self) -> ExecutorCapabilities {
            ExecutorCapabilities::MANAGED
        }
        fn enforces_json_candidate_limit(&self) -> bool {
            true
        }
        async fn execute(
            &self,
            invocation: Invocation,
            tools: Arc<dyn ToolBridge>,
            cancel: tokio_util::sync::CancellationToken,
        ) -> ExecutionOutcome {
            match tools.goal_context().unwrap().role {
                GoalRunRole::Planner { .. } => {
                    let mut plan: Value = serde_json::from_str(include_str!("../../docs/design/10-goal-dag-orchestration/contract/document-review-package/plan.example.json")).unwrap();
                    plan["goal_spec_digest"] = invocation.stage_input["goal_spec_digest"].clone();
                    plan["profile_digest"] = invocation.stage_input["profile_digest"].clone();
                    plan["nodes"]["draft"] = plan["nodes"]["review"].clone();
                    plan["nodes"]["check"] = json!({"capability_ref":"checkpoint@1","depends_on":["draft"],"inputs":{"report":{"kind":"node","node_id":"draft","port":"report","pointer":""}}});
                    plan["nodes"]["review"]["depends_on"] = json!(["check"]);
                    plan["outputs"]["draft"] = json!({"node_id":"draft","port":"report"});
                    plan["checks"]["draft-check"] = json!({"phase":"node_checkpoint","checker_ref":"quality@1","targets":["draft"],"evaluation_node_id":"check"});
                    tools.call("artifact.write", json!({"slot":"proposal","value":{"protocol_version":1,"kind":"proposal","proposal":plan}})).await.unwrap();
                }
                GoalRunRole::NodeMember { node_id, .. } if node_id == "check" => {
                    let input: CheckInput = serde_json::from_value(invocation.stage_input).unwrap();
                    let verdict = self.0.clone();
                    let results = input.rules.rules.iter().map(|rule| json!({"id":rule.id,"verdict":verdict,"message":"Deferred checkpoint","evidence_refs":input.evidence_refs})).collect::<Vec<_>>();
                    tools.call("artifact.write", json!({"slot":"evaluation","value":{"protocol_version":1,"verdict":verdict,"results":results,"evidence_refs":input.evidence_refs,"violations":[]}})).await.unwrap();
                }
                _ => {
                    return CompleteGoal(EvaluationVerdict::Pass)
                        .execute(invocation, tools, cancel)
                        .await;
                }
            }
            ExecutionOutcome::Completed { usage: json!({}) }
        }
    }
    #[tokio::test]
    async fn review_regression_paused_checkpoint_retries_projection_after_budget_release() {
        for verdict in [
            EvaluationVerdict::Pass,
            EvaluationVerdict::Fail,
            EvaluationVerdict::Error,
        ] {
            let (_dir, runtime, goals, caller, request) = setup_custom(Some(Arc::new(DeferredCheckpoint(verdict.clone()))), false, |registry| {
                registry.entries.get_mut("quality@1").unwrap().budget.max_run_ms = Some(500);
                registry.register_capability(serde_json::from_value(json!({"protocol_version":1,"id":"checkpoint","version":"1","kind":"work","inputs":{"report":{"schema_ref":"report","required":true,"source_policy":{"kind":"artifact"}}},"outputs":{"evaluation":{"schema_ref":"_tl.evaluation-candidate@1"}},"gate":"_tl.schema@1","max_rule_check_ms":2000,"members":{"checker":{"entry_ref":"quality@1","input_mode":"check_request"}},"join":{"kind":"object-by-worker","outputs":{"evaluation":{"kind":"member","member_id":"checker","slot":"evaluation"}}},"evaluation":{"purpose":"checkpoint","checker_ref":"quality@1","target_ports":{"draft":"report"}}})).unwrap()).unwrap();
                let profile = registry.profiles.get_mut("document-review@1").unwrap();
                profile.capabilities.push("checkpoint@1".into());
                profile.outputs.insert("draft".into(), profile.outputs["report"].clone());
                let mut check = profile.checks[0].clone();
                check.id = "draft-check".into(); check.phase = CheckPhase::NodeCheckpoint;
                check.target_outputs = vec!["draft".into()]; check.before_outputs = Some(vec!["report".into()]);
                profile.checks.push(check);
            }).await;
            let id = goals.submit(request, &caller).await.unwrap().goal_id;
            let checkpoint_run = loop {
                goals.core.advance(&id).unwrap();
                let run = goals.core.materialize(&id).unwrap().unwrap();
                goals
                    .core
                    .prepare_execution(&id, &run)
                    .unwrap()
                    .unwrap()
                    .execute()
                    .await
                    .unwrap();
                let returned = goals.get(&id, &caller).await.unwrap();
                if matches!(&returned.runs[&run].role, GoalRunRole::NodeMember { node_id, .. } if node_id == "check")
                {
                    break run;
                }
                goals.core.consume_return(&id, &run).unwrap();
                for (node, state) in &goals.get(&id, &caller).await.unwrap().nodes {
                    if state.status == GoalNodeStatus::Checking {
                        goals.core.finish_node_check(&id, node).unwrap();
                    }
                }
            };
            let returned = goals.get(&id, &caller).await.unwrap();
            goals
                .stop(&id, "pause-checkpoint", returned.revision, &caller)
                .await
                .unwrap();
            // Hold the same active-time ledger capacity as a sibling Run. No native sibling is claimed.
            let row = goals.core.inner.store.goal_load(&id).unwrap();
            let mut next = row.record.clone();
            let amount = GoalBudgetAmount {
                active_ms: next
                    .budget
                    .available(&next.prepared.effective_limits)
                    .active_ms,
                tool_calls: 0,
                candidate_bytes: 0,
            };
            assert_eq!(
                next.budget
                    .reserve(
                        "held-sibling-reservation",
                        amount,
                        None,
                        false,
                        &next.prepared.effective_limits
                    )
                    .unwrap(),
                BudgetAdmission::Granted
            );
            goals
                .core
                .commit_goal(
                    &row,
                    &next,
                    GoalWriteGuard::Cleanup,
                    None,
                    &Default::default(),
                    &goals.core.clock,
                )
                .unwrap();
            goals.core.consume_return(&id, &checkpoint_run).unwrap();
            let pending = goals.get(&id, &caller).await.unwrap();
            assert_eq!(pending.nodes["check"].status, GoalNodeStatus::Running);
            assert_eq!(
                pending.status,
                GoalStatus::Stopping,
                "pending evaluation must stay in pause cleanup"
            );
            assert!(
                pending.nodes["check"]
                    .members
                    .values()
                    .all(|m| m.status == GoalMemberStatus::Succeeded)
            );
            let row = goals.core.inner.store.goal_load(&id).unwrap();
            let mut next = row.record.clone();
            next.budget
                .settle(
                    "held-sibling-reservation",
                    1,
                    0,
                    true,
                    &next.prepared.effective_limits,
                )
                .unwrap();
            goals
                .core
                .commit_goal(
                    &row,
                    &next,
                    GoalWriteGuard::Cleanup,
                    None,
                    &Default::default(),
                    &goals.core.clock,
                )
                .unwrap();
            goals.core.maintain(&id).unwrap();
            goals.core.finish_node_check(&id, "check").unwrap();
            assert_eq!(
                goals.get(&id, &caller).await.unwrap().nodes["check"].status,
                GoalNodeStatus::Checking
            );
            goals.core.finish_node_check(&id, "check").unwrap();
            goals.core.maintain(&id).unwrap();
            let ended = goals.get(&id, &caller).await.unwrap();
            assert_eq!(
                ended.outcome,
                match verdict {
                    EvaluationVerdict::Pass => None,
                    EvaluationVerdict::Fail => Some(GoalOutcome::NotMet),
                    EvaluationVerdict::Error => Some(GoalOutcome::Failed),
                }
            );
            assert_eq!(
                ended.status,
                if verdict == EvaluationVerdict::Pass {
                    GoalStatus::Stopped
                } else {
                    GoalStatus::Finished
                }
            );
            assert_eq!(goals.evaluations(&id, &caller).await.unwrap().len(), 1);
            assert_eq!(ended.budget.runs_used, 3);
            assert!(ended.nodes["review"].outputs.is_empty());
            let revision = ended.revision;
            goals.core.maintain(&id).unwrap();
            assert_eq!(goals.get(&id, &caller).await.unwrap().revision, revision);
            runtime.shutdown().await.unwrap();
        }
    }
}
