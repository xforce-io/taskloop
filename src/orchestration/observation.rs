//! Rebuildable observation from committed Goal facts. Trace delivery cannot alter business acceptance.
use super::{core::GoalCore, record::*, *};
use crate::{
    store::{
        CommandReceipt,
        goals::{GoalCommit, GoalRow, GoalWriteGuard},
    },
    *,
};
use serde_json::json;
use std::sync::Arc;

impl GoalCore {
    fn root_trace(&self, id: &str) -> Option<Arc<crate::trace::RunTrace>> {
        let store = self.inner.trace.as_ref()?;
        let mut roots = self.root_traces.lock().ok()?;
        Some(
            roots
                .entry(id.into())
                .or_insert_with(|| {
                    let trace = store.goal_coordinator_session(id);
                    trace.reserve_coordinator();
                    trace.emit("phase.started", &trace.phase_span(), None, json!({}));
                    trace
                })
                .clone(),
        )
    }
    pub(crate) fn trace_admitted(&self, row: &GoalRow) {
        if let Some(trace) = self.root_trace(&row.id) {
            trace.emit("goal.admitted", &trace.phase_span(), None, json!({"revision":row.revision,"spec_digest":row.record.prepared.spec_digest,"profile_digest":row.record.prepared.profile_digest}));
        }
    }
    pub(crate) fn commit_goal(
        &self,
        row: &GoalRow,
        next: &GoalRecord,
        guard: GoalWriteGuard<'_>,
        command: Option<CommandReceipt>,
        effects: &GoalCommitEffects,
        clock: &GoalClock,
    ) -> Result<GoalCommit> {
        let committed =
            self.inner
                .store
                .goal_commit(row, next, guard, command.clone(), effects, clock);
        let committed = match committed {
            Ok(committed) => committed,
            Err(error) if error.code == "budget_exhausted" => {
                // A refused business transition closes dispatch through cleanup, which is not
                // blocked by the exhausted business counter. Retain measured physical facts only.
                let current = self.inner.store.goal_load(&row.id)?;
                let mut failed = current.record.clone();
                for (id, run) in &mut failed.runs {
                    if let Some(measured) = next.runs.get(id)
                        && run.view.native_stop_confirmed
                        && run.view.phase != GoalRunPhase::Settled
                        && measured.view.phase == GoalRunPhase::Settled
                        && measured.active_ms >= run.active_ms
                    {
                        run.active_ms = measured.active_ms;
                        run.settlement_sequence += 1;
                        run.view.phase = GoalRunPhase::Settled;
                        run.business_disposed = true;
                        run.view.budget = measured.view.budget.clone();
                        failed.budget.observe_active(
                            id,
                            run.settlement_sequence,
                            run.active_ms,
                            true,
                            true,
                            &failed.prepared.effective_limits,
                        )?;
                    }
                }
                for (id, meter) in &next.budget.meters {
                    if !failed.runs.contains_key(id)
                        && let Some(original) = failed.budget.meters.get(id).cloned()
                        && meter.sequence > original.sequence
                        && meter.native_stop_confirmed
                        && meter.active_ms >= original.active_ms
                    {
                        failed.budget.observe_active(
                            id,
                            original.sequence + 1,
                            meter.active_ms,
                            true,
                            true,
                            &failed.prepared.effective_limits,
                        )?;
                    }
                }
                super::core::set_terminal(
                    &mut failed,
                    GoalOutcome::Failed,
                    "budget_exhausted",
                    "goal",
                    current.revision + 1,
                    clock.observe()?.utc_ms,
                );
                failed.error.get_or_insert(error.clone());
                self.cleanup_terminal(&mut failed)?;
                let command = command.map(|mut receipt| {
                    receipt.response = json!({"kind":"rejected","error":error});
                    receipt
                });
                let committed = self.inner.store.goal_commit(
                    &current,
                    &failed,
                    GoalWriteGuard::Cleanup,
                    command,
                    &Default::default(),
                    clock,
                )?;
                if let GoalCommit::Saved(saved) = &committed {
                    self.trace_committed(&current, saved);
                }
                self.inner.notify.notify_one();
                return Err(error);
            }
            Err(error) => return Err(error),
        };
        if let GoalCommit::Saved(saved) = &committed {
            self.trace_committed(row, saved);
        }
        Ok(committed)
    }
    fn trace_committed(&self, before: &GoalRow, after: &GoalRow) {
        let mut events = Vec::new();
        for proposal in after
            .record
            .proposals
            .iter()
            .skip(before.record.proposals.len())
        {
            events.push((if proposal.failure.is_some() { "plan.rejected" } else { "plan.proposed" }, json!({"attempt":proposal.attempt,"candidate_sha":proposal.candidate_sha,"plan_digest":proposal.proposal_digest})));
        }
        if before.record.plan.is_none() && after.record.plan.is_some() {
            events.push((
                "plan.locked",
                json!({"plan_digest":after.record.plan.as_ref().map(|p| &p.plan_digest)}),
            ));
        }
        for (id, node) in &after.record.nodes {
            let prior = before.record.nodes.get(id).map(|n| n.view.status);
            if prior == Some(node.view.status) {
                continue;
            }
            let kind = match node.view.status {
                GoalNodeStatus::Ready => "node.ready",
                GoalNodeStatus::Running => "node.started",
                GoalNodeStatus::Succeeded => "node.accepted",
                GoalNodeStatus::DependencyFailed => "node.dependency_failed",
                GoalNodeStatus::WaitingInput => "node.waiting_input",
                GoalNodeStatus::Failed => "node.failed",
                GoalNodeStatus::Cancelled => "node.cancelled",
                _ => continue,
            };
            events.push((kind, json!({"node_id":id,"status":node.view.status,"output_ids":node.view.outputs.values().map(|a| &a.id).collect::<Vec<_>>()})));
        }
        for (id, check) in &after.record.checks {
            if check.receipt_sha.is_some()
                && before
                    .record
                    .checks
                    .get(id)
                    .is_none_or(|c| c.receipt_sha.is_none())
            {
                events.push(("check.accepted", json!({"evaluation_id":id,"check_id":check.view.check_id,"phase":check.view.phase,"verdict":check.view.verdict,"receipt_sha":check.receipt_sha})));
            }
        }
        if before.record.status != after.record.status {
            let kind = match after.record.status {
                GoalStatus::Finished => "goal.finished",
                GoalStatus::Stopped => "goal.stopped",
                GoalStatus::Stopping | GoalStatus::Blocked => "stop.pending",
                GoalStatus::Running | GoalStatus::Waiting => "goal.progress",
            };
            events.push((kind, json!({"status":after.record.status,"stage":after.record.stage,"outcome":after.record.outcome})));
        }
        for (id, run) in &after.record.runs {
            if run.view.native_stop_confirmed
                && before
                    .record
                    .runs
                    .get(id)
                    .is_some_and(|r| !r.view.native_stop_confirmed)
            {
                events.push(("stop.confirmed", json!({"native_run_id":id})));
            }
        }
        if events.is_empty() {
            return;
        }
        if let Some(trace) = self.root_trace(&after.id) {
            for (kind, mut data) in events {
                data["revision"] = json!(after.revision);
                data["plan_digest"] = json!(after.record.plan.as_ref().map(|p| &p.plan_digest));
                trace.emit(kind, &trace.phase_span(), None, data);
            }
            if after.record.status == GoalStatus::Finished {
                trace.emit(
                    "phase.finished",
                    &trace.phase_span(),
                    None,
                    json!({"status":after.record.status,"outcome":after.record.outcome}),
                );
                if let Ok(mut roots) = self.root_traces.lock() {
                    roots.remove(&after.id);
                }
            }
        }
    }
}
impl Goals {
    pub fn trace(&self, id: &str, query: TraceQuery, caller: &Caller) -> Result<TracePage> {
        self.core.authorized(id, caller)?;
        match &self.core.inner.trace {
            Some(store) => store.read(id, &query),
            None => crate::trace::disabled(&query),
        }
    }
    pub fn export_trace(&self, id: &str, query: TraceQuery, caller: &Caller) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec(&self.trace(id, query, caller)?)?)
    }
    pub fn read_trace_content(
        &self,
        id: &str,
        content_id: &str,
        caller: &Caller,
    ) -> Result<TraceContent> {
        self.core.authorized(id, caller)?;
        self.core
            .inner
            .trace
            .as_ref()
            .ok_or_else(|| Error::new("not_found", "/trace/content", "trace content not found"))?
            .read_content(id, content_id)
    }
}
