use super::{GoalLimits, schema::invalid};
use crate::{Result, RunBudgetPolicy};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct GoalBudgetAmount {
    pub active_ms: u64,
    pub tool_calls: u64,
    pub candidate_bytes: u64,
}
impl GoalBudgetAmount {
    pub(crate) fn for_entry(policy: &RunBudgetPolicy) -> Result<Self> {
        let active = policy
            .max_run_ms
            .zip(policy.max_check_ms)
            .and_then(|(r, c)| r.checked_add(c))
            .ok_or_else(|| {
                invalid(
                    "/budget",
                    "explicit execution/check allocation overflow or missing",
                )
            })?;
        Ok(Self {
            active_ms: active,
            tool_calls: policy.max_tool_calls,
            candidate_bytes: policy.max_candidate_bytes,
        })
    }
    fn plus(self, other: Self) -> Result<Self> {
        Ok(Self {
            active_ms: self
                .active_ms
                .checked_add(other.active_ms)
                .ok_or_else(|| invalid("/budget", "active counter overflow"))?,
            tool_calls: self
                .tool_calls
                .checked_add(other.tool_calls)
                .ok_or_else(|| invalid("/budget", "tool counter overflow"))?,
            candidate_bytes: self
                .candidate_bytes
                .checked_add(other.candidate_bytes)
                .ok_or_else(|| invalid("/budget", "byte counter overflow"))?,
        })
    }
    fn fits(self, limits: &GoalLimits) -> bool {
        self.active_ms <= limits.max_active_ms
            && self.tool_calls <= limits.max_tool_calls
            && self.candidate_bytes <= limits.max_total_candidate_bytes
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalReservation {
    pub allocation: GoalBudgetAmount,
    pub remaining: GoalBudgetAmount,
}
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GoalMeter {
    pub sequence: u64,
    pub active_ms: u64,
    pub native_stop_confirmed: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalBudgetOverrun {
    pub subject: String,
    pub dimension: String,
    pub allocation: u64,
    pub actual: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GoalBudget {
    pub runs_used: u64,
    pub transitions_used: u64,
    pub cleanup_transitions: u64,
    pub used: GoalBudgetAmount,
    pub reserved: BTreeMap<String, GoalReservation>,
    pub protected_final: BTreeMap<String, GoalBudgetAmount>,
    pub meters: BTreeMap<String, GoalMeter>,
    pub logical_accepted_bytes: u64,
    pub rejected_calls: u64,
    pub rejected_bytes: u64,
    pub overruns: Vec<GoalBudgetOverrun>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BudgetAdmission {
    Granted,
    Pending,
    Exhausted,
}
impl GoalBudget {
    pub(crate) fn protect_final(
        &mut self,
        allocations: BTreeMap<String, GoalBudgetAmount>,
        limits: &GoalLimits,
    ) -> Result<()> {
        if allocations.is_empty() || !self.protected_final.is_empty() || self.runs_used != 0 {
            return Err(invalid(
                "/budget",
                "final protection may only be established once before dispatch",
            ));
        }
        let sum = allocations
            .values()
            .try_fold(self.used, |s, v| s.plus(*v))?;
        if !sum.fits(limits) || allocations.len() as u64 >= limits.max_runs {
            return Err(invalid(
                "/budget",
                "Goal cannot protect final checks and the first planner",
            ));
        }
        self.protected_final = allocations;
        Ok(())
    }
    /// Called within the same Store transaction as Run materialization and the physical lease.
    pub(crate) fn reserve(
        &mut self,
        run: &str,
        amount: GoalBudgetAmount,
        final_check: Option<&str>,
        charge_new_run: bool,
        limits: &GoalLimits,
    ) -> Result<BudgetAdmission> {
        if let Some(reservation) = self.reserved.get(run) {
            if reservation.allocation != amount {
                return Err(invalid("/budget", "same RunId has different allocation"));
            }
            return Ok(BudgetAdmission::Granted);
        }
        if self
            .meters
            .get(run)
            .is_some_and(|m| m.native_stop_confirmed)
        {
            return Err(invalid(
                "/budget",
                "a settled Run cannot acquire a new reservation",
            ));
        }
        if !self.overruns.is_empty() || (charge_new_run && self.runs_used >= limits.max_runs) {
            return Ok(BudgetAdmission::Exhausted);
        }
        let protected_run_count = self
            .protected_final
            .keys()
            .filter(|id| Some(id.as_str()) != final_check)
            .count() as u64;
        if charge_new_run
            && self
                .runs_used
                .checked_add(1)
                .and_then(|n| n.checked_add(protected_run_count))
                .is_none_or(|n| n > limits.max_runs)
        {
            return Ok(BudgetAdmission::Exhausted);
        }
        let final_amount = final_check
            .map(|id| {
                self.protected_final.get(id).copied().ok_or_else(|| {
                    invalid(
                        "/budget/protected_final",
                        "final checker has no protected allocation",
                    )
                })
            })
            .transpose()?;
        if final_amount.is_some_and(|a| a != amount) {
            return Err(invalid(
                "/budget/protected_final",
                "final reservation differs from protected allocation",
            ));
        }
        let protected = self
            .protected_final
            .iter()
            .filter(|(id, _)| Some(id.as_str()) != final_check)
            .try_fold(GoalBudgetAmount::default(), |sum, (_, v)| sum.plus(*v))?;
        let permanent = self.used.plus(protected)?.plus(amount)?;
        if !permanent.fits(limits) {
            return Ok(BudgetAdmission::Exhausted);
        }
        let all = self
            .reserved
            .values()
            .try_fold(permanent, |sum, r| sum.plus(r.remaining))?;
        if !all.fits(limits) {
            return Ok(BudgetAdmission::Pending);
        }
        if charge_new_run {
            self.runs_used = self
                .runs_used
                .checked_add(1)
                .ok_or_else(|| invalid("/budget", "Run counter overflow"))?;
        }
        if let Some(id) = final_check {
            self.protected_final.remove(id);
        }
        self.reserved.insert(
            run.into(),
            GoalReservation {
                allocation: amount,
                remaining: amount,
            },
        );
        self.meters.entry(run.into()).or_default();
        Ok(BudgetAdmission::Granted)
    }
    pub(crate) fn admit_tool(&mut self, run: &str) -> Result<()> {
        let reservation = self
            .reserved
            .get_mut(run)
            .ok_or_else(|| invalid("/budget", "Run has no reservation"))?;
        if reservation.remaining.tool_calls == 0 {
            self.rejected_calls = self.rejected_calls.saturating_add(1);
            return Err(invalid(
                "/budget/tool_calls",
                "Goal Run tool allocation exhausted",
            ));
        }
        self.used.tool_calls = self
            .used
            .tool_calls
            .checked_add(1)
            .ok_or_else(|| invalid("/budget", "tool counter overflow"))?;
        reservation.remaining.tool_calls -= 1;
        Ok(())
    }
    pub(crate) fn admit_bytes(&mut self, run: &str, bytes: u64) -> Result<()> {
        let reservation = self
            .reserved
            .get_mut(run)
            .ok_or_else(|| invalid("/budget", "Run has no reservation"))?;
        if bytes > reservation.remaining.candidate_bytes {
            self.rejected_bytes = self.rejected_bytes.saturating_add(bytes);
            return Err(invalid(
                "/budget/candidate_bytes",
                "Goal Run candidate allocation exhausted",
            ));
        }
        self.used.candidate_bytes = self
            .used
            .candidate_bytes
            .checked_add(bytes)
            .ok_or_else(|| invalid("/budget", "byte counter overflow"))?;
        reservation.remaining.candidate_bytes -= bytes;
        Ok(())
    }
    /// Original identity must be verified before this call. Current business epoch, status,
    /// deadlines and exhausted limits deliberately do not participate in trusted settlement.
    pub(crate) fn settle(
        &mut self,
        run: &str,
        sequence: u64,
        cumulative_active_ms: u64,
        native_stop_confirmed: bool,
        limits: &GoalLimits,
    ) -> Result<bool> {
        let previous = self
            .meters
            .get(run)
            .cloned()
            .ok_or_else(|| invalid("/budget", "unknown original Run meter"))?;
        if sequence < previous.sequence {
            return Ok(false);
        }
        if sequence == previous.sequence {
            if cumulative_active_ms == previous.active_ms
                && native_stop_confirmed == previous.native_stop_confirmed
            {
                return Ok(false);
            }
            return Err(invalid(
                "/budget/sequence",
                "same settlement sequence has different facts",
            ));
        }
        if cumulative_active_ms < previous.active_ms
            || previous.native_stop_confirmed && !native_stop_confirmed
        {
            return Err(invalid("/budget", "meter/stop facts cannot move backwards"));
        }
        let delta = cumulative_active_ms - previous.active_ms;
        self.used.active_ms = self
            .used
            .active_ms
            .checked_add(delta)
            .ok_or_else(|| invalid("/budget", "actual active counter overflow"))?;
        if let Some(reservation) = self.reserved.get_mut(run) {
            reservation.remaining.active_ms = reservation.remaining.active_ms.saturating_sub(delta);
            if cumulative_active_ms > reservation.allocation.active_ms
                && !self
                    .overruns
                    .iter()
                    .any(|o| o.subject == run && o.dimension == "run_active_ms")
            {
                self.overruns.push(GoalBudgetOverrun {
                    subject: run.into(),
                    dimension: "run_active_ms".into(),
                    allocation: reservation.allocation.active_ms,
                    actual: cumulative_active_ms,
                });
            }
        }
        if self.used.active_ms > limits.max_active_ms {
            if let Some(overrun) = self
                .overruns
                .iter_mut()
                .find(|o| o.subject == "goal" && o.dimension == "active_ms")
            {
                overrun.actual = self.used.active_ms;
            } else {
                self.overruns.push(GoalBudgetOverrun {
                    subject: "goal".into(),
                    dimension: "active_ms".into(),
                    allocation: limits.max_active_ms,
                    actual: self.used.active_ms,
                });
            }
        }
        self.meters.insert(
            run.into(),
            GoalMeter {
                sequence,
                active_ms: cumulative_active_ms,
                native_stop_confirmed,
            },
        );
        if native_stop_confirmed {
            self.reserved.remove(run);
        }
        Ok(true)
    }
    /// Unstarted suspension refunds unused reservations, never charged calls/bytes or max_runs.
    pub(crate) fn suspend_unstarted(&mut self, run: &str) -> Result<()> {
        let meter = self
            .meters
            .get(run)
            .ok_or_else(|| invalid("/budget", "unknown Run"))?;
        if meter.native_stop_confirmed {
            return Err(invalid("/budget", "cannot suspend a settled Run"));
        }
        self.reserved.remove(run);
        Ok(())
    }
    pub(crate) fn charge_transitions(
        &mut self,
        count: u64,
        cleanup: bool,
        limits: &GoalLimits,
    ) -> Result<()> {
        let target = if cleanup {
            &mut self.cleanup_transitions
        } else {
            &mut self.transitions_used
        };
        let next = target
            .checked_add(count)
            .ok_or_else(|| invalid("/budget", "transition counter overflow"))?;
        if !cleanup && next > limits.max_transitions {
            return Err(invalid(
                "/budget/transitions",
                "business transition budget exhausted",
            ));
        }
        *target = next;
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn limits() -> GoalLimits {
        GoalLimits {
            max_wall_ms: 1000,
            max_active_ms: 400,
            max_runs: 5,
            max_transitions: 5,
            max_tool_calls: 12,
            max_total_candidate_bytes: 120,
            max_concurrent_runs: 4,
        }
    }
    fn amount() -> GoalBudgetAmount {
        GoalBudgetAmount {
            active_ms: 100,
            tool_calls: 3,
            candidate_bytes: 30,
        }
    }
    #[test]
    fn protected_final_is_unavailable_to_ordinary_work_and_moves_once_to_its_run() {
        let mut b = GoalBudget::default();
        let limits = limits();
        b.protect_final(BTreeMap::from([("quality".into(), amount())]), &limits)
            .unwrap();
        for id in ["a", "b", "c"] {
            assert_eq!(
                b.reserve(id, amount(), None, true, &limits).unwrap(),
                BudgetAdmission::Granted
            );
        }
        assert_eq!(
            b.reserve("d", amount(), None, true, &limits).unwrap(),
            BudgetAdmission::Pending
        );
        assert_eq!(
            b.reserve("final", amount(), Some("quality"), true, &limits)
                .unwrap(),
            BudgetAdmission::Granted
        );
        assert!(b.protected_final.is_empty());
        assert_eq!(b.runs_used, 4);
        assert_eq!(
            b.reserve("final", amount(), Some("quality"), true, &limits)
                .unwrap(),
            BudgetAdmission::Granted
        );
        assert_eq!(b.runs_used, 4);
    }
    #[test]
    fn repeated_bytes_are_charged_and_unknown_stop_keeps_unused_reservation() {
        let mut b = GoalBudget::default();
        let limits = limits();
        b.reserve("a", amount(), None, true, &limits).unwrap();
        b.admit_bytes("a", 10).unwrap();
        b.admit_bytes("a", 10).unwrap();
        assert_eq!(b.used.candidate_bytes, 20);
        b.admit_tool("a").unwrap();
        b.settle("a", 1, 50, false, &limits).unwrap();
        assert_eq!(b.reserved["a"].remaining.active_ms, 50);
        assert_eq!(b.reserved["a"].remaining.candidate_bytes, 10);
        b.settle("a", 2, 60, true, &limits).unwrap();
        assert!(!b.reserved.contains_key("a"));
        assert_eq!(b.used.active_ms, 60);
        assert_eq!(b.used.tool_calls, 1);
        assert_eq!(b.runs_used, 1);
        assert!(!b.settle("a", 2, 60, true, &limits).unwrap());
        assert_eq!(b.used.active_ms, 60);
    }
    #[test]
    fn actual_overrun_is_not_clipped_or_rejected_and_closes_future_admission() {
        let mut b = GoalBudget::default();
        let limits = limits();
        b.reserve("a", amount(), None, true, &limits).unwrap();
        b.settle("a", 1, 500, true, &limits).unwrap();
        assert_eq!(b.used.active_ms, 500);
        assert!(b.overruns.iter().any(|o| o.dimension == "active_ms"));
        assert_eq!(
            b.reserve("b", amount(), None, true, &limits).unwrap(),
            BudgetAdmission::Exhausted
        );
        b.charge_transitions(5, false, &limits).unwrap();
        assert!(b.charge_transitions(1, false, &limits).is_err());
        b.charge_transitions(100, true, &limits).unwrap();
        assert_eq!(b.cleanup_transitions, 100);
    }
    #[test]
    fn suspension_and_resume_reuse_run_charge_and_preserve_consumption() {
        let mut b = GoalBudget::default();
        let limits = limits();
        b.reserve("a", amount(), None, true, &limits).unwrap();
        b.settle("a", 1, 5, false, &limits).unwrap();
        b.suspend_unstarted("a").unwrap();
        assert_eq!(b.used.active_ms, 5);
        b.reserve("a", amount(), None, false, &limits).unwrap();
        assert_eq!(b.runs_used, 1);
        b.settle("a", 2, 8, true, &limits).unwrap();
        assert_eq!(b.used.active_ms, 8);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalLimitSources {
    pub requested: GoalLimits,
    pub goal_grant: GoalLimits,
    pub host: GoalLimits,
    pub tool_grant: crate::ResourceLimits,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalBudgetView {
    pub protocol_version: u32,
    pub goal_id: String,
    pub budget: GoalBudget,
    pub effective_limits: GoalLimits,
    pub sources: GoalLimitSources,
    pub available: GoalBudgetAmount,
}
impl GoalBudget {
    pub(crate) fn available(&self, limits: &GoalLimits) -> GoalBudgetAmount {
        let active = self
            .reserved
            .values()
            .map(|r| r.remaining.active_ms as u128)
            .chain(self.protected_final.values().map(|r| r.active_ms as u128))
            .sum::<u128>()
            + self.used.active_ms as u128;
        let calls = self
            .reserved
            .values()
            .map(|r| r.remaining.tool_calls as u128)
            .chain(self.protected_final.values().map(|r| r.tool_calls as u128))
            .sum::<u128>()
            + self.used.tool_calls as u128;
        let bytes = self
            .reserved
            .values()
            .map(|r| r.remaining.candidate_bytes as u128)
            .chain(
                self.protected_final
                    .values()
                    .map(|r| r.candidate_bytes as u128),
            )
            .sum::<u128>()
            + self.used.candidate_bytes as u128;
        GoalBudgetAmount {
            active_ms: (limits.max_active_ms as u128).saturating_sub(active) as u64,
            tool_calls: (limits.max_tool_calls as u128).saturating_sub(calls) as u64,
            candidate_bytes: (limits.max_total_candidate_bytes as u128).saturating_sub(bytes)
                as u64,
        }
    }
}
