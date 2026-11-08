//! Goal contracts and orchestration. A locked plan is data, never authority to execute.
mod admission;
mod check;
mod clock;
pub(crate) mod control;
pub(crate) mod goal_budget;
mod plan;
mod record;
mod registry;
mod schema;
mod types;
pub use check::{
    CheckInput, CheckInputPhase, CheckSubject, CheckTarget, CheckTargetIdentity, EvaluationReceipt,
    EvaluationSubject,
};
pub use control::{GoalBlocker, GoalOutcome, GoalStage, GoalStatus, GoalTerminalIntent};
pub use goal_budget::{
    GoalBudget, GoalBudgetAmount, GoalBudgetOverrun, GoalBudgetView, GoalLimitSources, GoalMeter,
    GoalReservation,
};
pub use plan::{
    OrchestrationOptions, PlanIssue, PlanIssueCategory, PlanValidationContext,
    PlanValidationFailure, ValidatedPlan,
};
pub use record::{
    GoalArtifact, GoalCommandReceipt, GoalEvaluationSummary, GoalMemberSnapshot, GoalMemberStatus,
    GoalNodeSnapshot, GoalNodeStatus, GoalReceipt, GoalRunPhase, GoalRunRole, GoalRunSnapshot,
    GoalSignalRequest, GoalSnapshot, GoalStopSummary, GoalWaiting, LockedPlan,
};
pub use registry::{OrchestrationRegistry, RegistrationReceipt, TemplateNode, WorkflowTemplate};
pub use types::*;

#[cfg(test)]
pub(crate) mod tests;

pub(crate) use record::{
    GoalCheckRecord, GoalCommitEffects, GoalNodeRecord, GoalRecord, GoalRunRecord, GoalRunRequest,
    ProposalRecord,
};

pub(crate) use clock::{ClockObservation, GoalClock};

pub(crate) use registry::FrozenRegistry;
