use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    Running,
    Waiting,
    Stopping,
    Stopped,
    Blocked,
    Finished,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GoalStage {
    Planning,
    PlanChecking,
    Executing,
    GoalChecking,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GoalOutcome {
    Achieved,
    NotMet,
    Failed,
    Cancelled,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct GoalBlocker {
    pub reason: String,
    pub subject: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalTerminalIntent {
    pub outcome: GoalOutcome,
    pub reason: String,
    pub subject: String,
}

/// All business permissions consume the same immutable transaction view.
/// Source assertions below are established by Runtime, never deserialized from checker output.
pub(crate) struct PermissionFacts {
    pub status: GoalStatus,
    pub stage: GoalStage,
    pub terminal: bool,
    pub blocked: bool,
    pub clock_trusted: bool,
    pub pause_at_ms: Option<u64>,
    pub epoch: u64,
    pub now_ms: u64,
    pub wall_deadline_ms: u64,
    pub planning_deadline_ms: u64,
}
pub(crate) struct ResultAuthority {
    pub original_source_valid: bool,
    pub ownership_and_digests_valid: bool,
    pub epoch: u64,
    pub predecessor_valid: bool,
}
pub(crate) struct SignalAuthority {
    pub wait_identity_valid: bool,
    pub schema_and_caller_valid: bool,
    pub revision_valid: bool,
    pub expires_at_ms: u64,
}
pub(crate) struct ResumeFacts {
    pub unsettled_started_run: bool,
    pub bindings_valid: bool,
    pub all_wait_deadlines_valid: bool,
    pub lost_acceptance: bool,
    pub uncommitted_checking: bool,
}
pub(crate) struct FinishFacts {
    pub required_proofs_match_and_pass: bool,
    pub all_nodes_succeeded: bool,
    pub all_runs_settled: bool,
    pub cleanup_complete: bool,
}
impl PermissionFacts {
    fn deadlines_valid(&self) -> bool {
        self.now_ms < self.wall_deadline_ms
            && (!matches!(self.stage, GoalStage::Planning | GoalStage::PlanChecking)
                || self.now_ms < self.planning_deadline_ms)
    }
    fn business_open(&self) -> bool {
        !self.terminal && !self.blocked && self.clock_trusted && self.deadlines_valid()
    }
    pub fn advance_allowed(&self) -> bool {
        self.business_open()
            && self.pause_at_ms.is_none()
            && matches!(self.status, GoalStatus::Running | GoalStatus::Waiting)
    }
    pub fn result_allowed(&self, source: &ResultAuthority) -> bool {
        if !source.original_source_valid
            || !source.ownership_and_digests_valid
            || source.epoch != self.epoch
            || !source.predecessor_valid
            || !self.business_open()
        {
            return false;
        }
        match self.pause_at_ms {
            Some(start) => {
                self.status == GoalStatus::Stopping
                    && self
                        .now_ms
                        .checked_sub(start)
                        .is_some_and(|elapsed| elapsed < 15_000)
            }
            None => matches!(self.status, GoalStatus::Running | GoalStatus::Waiting),
        }
    }
    pub fn signal_allowed(&self, signal: &SignalAuthority) -> bool {
        self.advance_allowed()
            && signal.wait_identity_valid
            && signal.schema_and_caller_valid
            && signal.revision_valid
            && self.now_ms < signal.expires_at_ms
    }
    pub fn resume_allowed(&self, resume: &ResumeFacts) -> bool {
        self.status == GoalStatus::Stopped
            && self.business_open()
            && !resume.unsettled_started_run
            && resume.bindings_valid
            && resume.all_wait_deadlines_valid
            && !resume.lost_acceptance
            && !resume.uncommitted_checking
    }
    pub fn finish_allowed(&self, finish: &FinishFacts) -> bool {
        self.advance_allowed()
            && finish.required_proofs_match_and_pass
            && finish.all_nodes_succeeded
            && finish.all_runs_settled
            && finish.cleanup_complete
    }
}
/// Settlement is deliberately independent of the current Goal's business epoch or deadlines.
pub(crate) fn settlement_allowed(
    original_run_and_owner_valid: bool,
    original_epoch_valid: bool,
    original_binding_valid: bool,
    sequence_valid: bool,
) -> bool {
    original_run_and_owner_valid && original_epoch_valid && original_binding_valid && sequence_valid
}
#[cfg(test)]
mod tests {
    use super::*;
    fn facts() -> PermissionFacts {
        PermissionFacts {
            status: GoalStatus::Running,
            stage: GoalStage::Executing,
            terminal: false,
            blocked: false,
            clock_trusted: true,
            pause_at_ms: None,
            epoch: 2,
            now_ms: 1000,
            wall_deadline_ms: 60_000,
            planning_deadline_ms: 500,
        }
    }
    fn source() -> ResultAuthority {
        ResultAuthority {
            original_source_valid: true,
            ownership_and_digests_valid: true,
            epoch: 2,
            predecessor_valid: true,
        }
    }
    #[test]
    fn pause_keeps_only_in_flight_results_for_the_fixed_window() {
        let mut f = facts();
        assert!(f.advance_allowed());
        assert!(f.result_allowed(&source()));
        f.pause_at_ms = Some(900);
        f.status = GoalStatus::Stopping;
        assert!(!f.advance_allowed());
        assert!(f.result_allowed(&source()));
        f.now_ms = 15_900;
        assert!(!f.result_allowed(&source()));
        f.now_ms = 1000;
        f.status = GoalStatus::Stopped;
        assert!(!f.result_allowed(&source()));
        f.status = GoalStatus::Stopping;
        f.terminal = true;
        assert!(!f.result_allowed(&source()));
    }
    #[test]
    fn settlement_survives_closed_business_authority_but_not_forged_origin() {
        let mut f = facts();
        f.epoch += 1;
        f.status = GoalStatus::Finished;
        f.terminal = true;
        f.now_ms = 90_000;
        assert!(!f.result_allowed(&source()));
        assert!(settlement_allowed(true, true, true, true));
        assert!(!settlement_allowed(true, true, false, true));
    }
    #[test]
    fn all_business_paths_share_deadline_blocker_and_clock_rejection() {
        for mutate in [
            |f: &mut PermissionFacts| f.now_ms = 60_000,
            |f: &mut PermissionFacts| f.blocked = true,
            |f: &mut PermissionFacts| f.clock_trusted = false,
            |f: &mut PermissionFacts| f.terminal = true,
        ] {
            let mut f = facts();
            mutate(&mut f);
            assert!(!f.advance_allowed());
            assert!(!f.result_allowed(&source()));
            assert!(!f.signal_allowed(&SignalAuthority {
                wait_identity_valid: true,
                schema_and_caller_valid: true,
                revision_valid: true,
                expires_at_ms: 100_000
            }));
        }
        let mut planning = facts();
        planning.stage = GoalStage::Planning;
        assert!(!planning.advance_allowed());
    }
    #[test]
    fn resume_and_achieved_require_complete_evidence_not_only_free_slots() {
        let mut f = facts();
        f.status = GoalStatus::Stopped;
        f.pause_at_ms = Some(900);
        let mut resume = ResumeFacts {
            unsettled_started_run: false,
            bindings_valid: true,
            all_wait_deadlines_valid: true,
            lost_acceptance: false,
            uncommitted_checking: false,
        };
        assert!(f.resume_allowed(&resume));
        resume.uncommitted_checking = true;
        assert!(!f.resume_allowed(&resume));
        f.status = GoalStatus::Running;
        f.pause_at_ms = None;
        let mut finish = FinishFacts {
            required_proofs_match_and_pass: true,
            all_nodes_succeeded: true,
            all_runs_settled: true,
            cleanup_complete: true,
        };
        assert!(f.finish_allowed(&finish));
        finish.all_runs_settled = false;
        assert!(!f.finish_allowed(&finish));
    }
}
