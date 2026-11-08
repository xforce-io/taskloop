//! Goal contracts and orchestration. A locked plan is data, never authority to execute.
mod admission;
mod check;
mod cli_driver;
mod client;
mod clock;
pub(crate) mod control;
mod control_socket;
mod coordinator;
mod core;
mod document_review;
mod driver;
pub(crate) mod goal_budget;
mod lifecycle;
mod nodes;
mod observation;
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
    GoalNodeSnapshot, GoalNodeStatus, GoalProgress, GoalReceipt, GoalRunPhase, GoalRunRole,
    GoalRunSnapshot, GoalSignalRequest, GoalSnapshot, GoalStopSummary, GoalWaiting, LockedPlan,
};
pub use registry::{OrchestrationRegistry, RegistrationReceipt, TemplateNode, WorkflowTemplate};
pub use types::*;

#[cfg(test)]
pub(crate) mod tests;

pub(crate) use record::{
    GoalCheckRecord, GoalCommitEffects, GoalNodeRecord, GoalRecord, GoalRunRecord, GoalRunRequest,
};

pub(crate) use clock::{ClockObservation, GoalClock};

pub(crate) use registry::FrozenRegistry;
pub(crate) use registry::bindings_digest;

pub use cli_driver::CliDriver;
pub use client::GoalClient;
pub use control_socket::GoalCommandResult;
pub(crate) use core::GoalCore;
pub use core::Goals;
pub use document_review::document_review_registry;

pub(crate) use record::run_scope_valid;
