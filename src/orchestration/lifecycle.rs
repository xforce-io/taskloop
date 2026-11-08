use super::{
    core::{GoalCore, set_terminal},
    record::*,
    *,
};
use crate::{store::goals::GoalWriteGuard, *};
use serde_json::json;
use std::sync::Arc;

impl GoalCore {
    pub(crate) fn maintain(&self, id: &str) -> Result<bool> {
        let _coordinator = self.coordinator.lock().map_err(Error::io)?;
        let row = self.inner.store.goal_load(id)?;
        if row.record.status == GoalStatus::Finished {
            return Ok(false);
        }
        let observed = self.inner.store.observe_goal_clock(&self.clock)?;
        let mut next = row.record.clone();
        if !observed.trusted {
            next.blockers.insert(GoalBlocker {
                reason: "clock_untrusted".into(),
                subject: "clock".into(),
            });
            next.status = GoalStatus::Blocked;
        } else {
            next.blockers.retain(|b| b.reason != "clock_untrusted");
            if next.status == GoalStatus::Blocked && next.blockers.is_empty() {
                next.status = GoalStatus::Stopped;
            }
            let mut deadlines = vec![(
                next.prepared.wall_deadline_utc_ms,
                0,
                "wall_timeout",
                "goal".to_owned(),
            )];
            if matches!(next.stage, GoalStage::Planning | GoalStage::PlanChecking) {
                deadlines.push((
                    next.prepared.planning_deadline_utc_ms,
                    1,
                    "planning_timeout",
                    "planner".to_owned(),
                ));
            }
            for wait in next.waiting.values().filter(|w| !w.consumed) {
                deadlines.push((
                    wait.expires_at_utc_ms,
                    2,
                    "input_timeout",
                    wait.node_id.clone().unwrap_or_else(|| "planner".into()),
                ));
            }
            deadlines.sort();
            if let Some((deadline, _, reason, subject)) = deadlines
                .first()
                .filter(|(d, _, _, _)| observed.utc_ms >= *d)
            {
                let _ = deadline;
                if let Some(node) = next.nodes.get_mut(subject) {
                    node.view.status = GoalNodeStatus::Failed;
                    node.view.error = Some(Error::new(
                        *reason,
                        "/deadline",
                        "fixed Goal/input deadline expired",
                    ));
                }
                set_terminal(
                    &mut next,
                    GoalOutcome::Failed,
                    reason,
                    subject,
                    row.revision + 1,
                    observed.utc_ms,
                );
                next.error.get_or_insert_with(|| {
                    Error::new(*reason, "/deadline", "fixed Goal/input deadline expired")
                });
            }
        }
        self.cleanup_terminal(&mut next)?;
        self.close_stopping(&mut next, observed.utc_ms)?;
        let changed = serde_json::to_value(&row.record)? != serde_json::to_value(&next)?
            || serde_json::to_value(&row.record.budget)? != serde_json::to_value(&next.budget)?;
        if changed {
            self.commit_goal(
                &row,
                &next,
                GoalWriteGuard::Cleanup,
                None,
                &Default::default(),
                &self.clock,
            )?;
        }
        self.cancel_goal_handles(&next);
        Ok(changed)
    }
    /// Recovery classifies all records before any business admission or stop probe.
    pub(crate) fn recover(&self) -> Result<()> {
        for row in self.inner.store.goal_all_active()? {
            if row.record.status == GoalStatus::Finished {
                continue;
            }
            let mut next = row.record.clone();
            let mut lost = false;
            let invalidate_epoch = next.runs.values().any(|r| {
                r.view.started && r.view.phase != GoalRunPhase::Settled && !r.view.interrupted
            }) || next
                .nodes
                .values()
                .any(|n| n.view.status == GoalNodeStatus::Checking);
            for run in next
                .runs
                .values_mut()
                .filter(|r| r.view.phase != GoalRunPhase::Settled)
            {
                if run.view.started {
                    if run.view.phase == GoalRunPhase::Checking {
                        next.budget
                            .incomplete_active_subjects
                            .insert(run.view.run_id.clone());
                    }
                    lost = true;
                    run.view.interrupted = true;
                    run.view.stop_unknown = !run.view.native_stop_confirmed;
                    if !run.view.native_stop_confirmed {
                        next.budget
                            .incomplete_active_subjects
                            .insert(run.view.run_id.clone());
                        next.blockers.insert(GoalBlocker {
                            reason: "stop_unknown".into(),
                            subject: run.view.run_id.clone(),
                        });
                    } else {
                        run.settlement_sequence += 1;
                        next.budget.observe_active(
                            &run.view.run_id,
                            run.settlement_sequence,
                            run.active_ms,
                            true,
                            true,
                            &next.prepared.effective_limits,
                        )?;
                        run.view.phase = GoalRunPhase::Settled;
                        run.business_disposed = true;
                    }
                    next.blockers.insert(GoalBlocker {
                        reason: "interrupted".into(),
                        subject: run.view.run_id.clone(),
                    });
                } else if run.view.phase == GoalRunPhase::Reserved {
                    next.budget.suspend_unstarted(&run.view.run_id)?;
                    run.view.phase = GoalRunPhase::Suspended;
                }
            }
            for (id, node) in next
                .nodes
                .iter_mut()
                .filter(|(_, n)| n.view.status == GoalNodeStatus::Checking)
            {
                lost = true;
                node.view.status = GoalNodeStatus::Interrupted;
                if let Some(nonce) = &node.checking_nonce {
                    next.budget.incomplete_active_subjects.insert(nonce.clone());
                }
                next.blockers.insert(GoalBlocker {
                    reason: "interrupted".into(),
                    subject: id.clone(),
                });
            }
            if lost && next.terminal_intent.is_none() {
                if invalidate_epoch {
                    next.epoch += 1;
                }
                next.status = GoalStatus::Blocked;
            } else if next.terminal_intent.is_none() {
                next.status = if next.blockers.is_empty() {
                    GoalStatus::Stopped
                } else {
                    GoalStatus::Blocked
                };
                next.pause_at_utc_ms
                    .get_or_insert(self.clock.observe()?.utc_ms);
            }
            self.cleanup_terminal(&mut next)?;
            if serde_json::to_value(&row.record)? != serde_json::to_value(&next)?
                || serde_json::to_value(&row.record.budget)? != serde_json::to_value(&next.budget)?
            {
                self.commit_goal(
                    &row,
                    &next,
                    GoalWriteGuard::Recovery,
                    None,
                    &Default::default(),
                    &self.clock,
                )?;
            }
            self.maintain(&row.id)?;
        }
        Ok(())
    }
    pub(crate) fn pause_for_shutdown(&self) -> Result<()> {
        let _coordinator = self.coordinator.lock().map_err(Error::io)?;
        for row in self.inner.store.goal_all_active()? {
            if row.record.status == GoalStatus::Finished {
                continue;
            }
            let mut next = row.record.clone();
            if next.terminal_intent.is_none() && next.blockers.is_empty() {
                next.pause_at_utc_ms
                    .get_or_insert(self.clock.observe()?.utc_ms);
                next.status = GoalStatus::Stopping;
                for run in next
                    .runs
                    .values_mut()
                    .filter(|r| !r.view.started && r.view.phase == GoalRunPhase::Reserved)
                {
                    next.budget.suspend_unstarted(&run.view.run_id)?;
                    run.view.phase = GoalRunPhase::Suspended;
                }
            }
            self.close_stopping(&mut next, self.clock.observe()?.utc_ms)?;
            if serde_json::to_value(&row.record)? != serde_json::to_value(&next)?
                || serde_json::to_value(&row.record.budget)? != serde_json::to_value(&next.budget)?
            {
                self.commit_goal(
                    &row,
                    &next,
                    GoalWriteGuard::Cleanup,
                    None,
                    &Default::default(),
                    &self.clock,
                )?;
            }
        }
        Ok(())
    }
    pub(crate) fn fail_preparation(
        &self,
        id: &str,
        run_id: &str,
        error: Error,
        preparation_ms: u64,
    ) -> Result<()> {
        let _coordinator = self.coordinator.lock().map_err(Error::io)?;
        let row = self.inner.store.goal_load(id)?;
        let run = &row.record.runs[run_id];
        if run.view.started {
            return Ok(());
        }
        let mut next = row.record.clone();
        let run = next.runs.get_mut(run_id).expect("original reserved Run");
        run.active_ms = run.active_ms.saturating_add(preparation_ms);
        run.settlement_sequence += 1;
        run.view.native_stop_confirmed = true;
        run.view.phase = GoalRunPhase::Settled;
        run.business_disposed = true;
        next.budget.observe_active(
            run_id,
            run.settlement_sequence,
            run.active_ms,
            true,
            true,
            &next.prepared.effective_limits,
        )?;
        set_terminal(
            &mut next,
            GoalOutcome::Failed,
            &error.code,
            run_id,
            row.revision + 1,
            self.clock.observe()?.utc_ms,
        );
        next.error = Some(error);
        self.cleanup_terminal(&mut next)?;
        self.commit_goal(
            &row,
            &next,
            GoalWriteGuard::Cleanup,
            None,
            &Default::default(),
            &self.clock,
        )?;
        self.cancel_goal_handles(&next);
        Ok(())
    }
    pub(crate) async fn probe_stop(self: Arc<Self>, id: String, run_id: String) -> Result<()> {
        let row = self.inner.store.goal_load(&id)?;
        let original = &row.record.runs[&run_id];
        if !original.view.started || original.view.native_stop_confirmed {
            return Ok(());
        }
        let Some(executor) = self.inner.bindings.executors.get(&original.executor) else {
            return Ok(());
        };
        let binding = crate::json::digest(&super::plan::bounded_json(
            &json!({"configuration_version":self.inner.bindings.configuration_version,"executor":original.executor,"identity":executor.identity(),"limits":executor.budget_limits()}),
            1_048_576,
        )?);
        if binding != original.executor_binding {
            return Ok(());
        }
        if crate::managed::confirm_stop(executor, &run_id).await != StopState::Confirmed {
            return Ok(());
        }
        let _coordinator = self.coordinator.lock().map_err(Error::io)?;
        let row = self.inner.store.goal_load(&id)?;
        let mut next = row.record.clone();
        let run = next
            .runs
            .get_mut(&run_id)
            .ok_or_else(|| Error::io("original Run missing"))?;
        if run.view.native_stop_confirmed {
            return Ok(());
        }
        run.view.native_stop_confirmed = true;
        run.view.stop_unknown = false;
        run.view.phase = GoalRunPhase::Settled;
        run.business_disposed = true;
        run.view.interrupted = true;
        run.settlement_sequence += 1;
        next.budget.observe_active(
            &run_id,
            run.settlement_sequence,
            run.active_ms,
            true,
            true,
            &next.prepared.effective_limits,
        )?;
        next.blockers
            .retain(|b| b.reason != "stop_unknown" || b.subject != run_id);
        if next.terminal_intent.is_none() {
            next.blockers.insert(GoalBlocker {
                reason: "interrupted".into(),
                subject: run_id.clone(),
            });
            next.status = GoalStatus::Blocked;
        }
        self.cleanup_terminal(&mut next)?;
        self.close_stopping(&mut next, self.clock.observe()?.utc_ms)?;
        self.commit_goal(
            &row,
            &next,
            GoalWriteGuard::Settlement {
                run_id: &run_id,
                epoch: original.view.epoch,
                binding: &binding,
            },
            None,
            &Default::default(),
            &self.clock,
        )?;
        self.inner.notify.notify_one();
        Ok(())
    }
}
