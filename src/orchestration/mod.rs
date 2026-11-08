//! Goal contracts and orchestration. A locked plan is data, never authority to execute.
mod admission;
mod check;
mod control;
mod plan;
mod registry;
mod schema;
mod types;
pub use check::{CheckInput, CheckInputPhase, CheckSubject, CheckTarget, EvaluationReceipt};
pub use control::{GoalBlocker, GoalOutcome, GoalStage, GoalStatus, GoalTerminalIntent};
pub use plan::{
    OrchestrationOptions, PlanIssue, PlanIssueCategory, PlanValidationContext,
    PlanValidationFailure, ValidatedPlan,
};
pub use registry::{OrchestrationRegistry, RegistrationReceipt, TemplateNode, WorkflowTemplate};
pub use types::*;

#[cfg(test)]
mod tests;
