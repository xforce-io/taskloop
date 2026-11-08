//! Small Rust runtime for explicit business workflows and bounded Agent Harness execution.
//! Domain methods and quality rules are supplied by the application; runtime state remains private.
mod definition;
mod error;
mod json;
pub use definition::{
    AttachmentPolicy, Bundle, Definition, EntryContract, Flow, OutputSlot, Requirement,
    SkillPackage, State, ValidationReport,
};
pub use error::{Error, Result};
pub use json::parse_unique;

mod model;
pub use model::*;

mod store;

mod skill;
pub use skill::{SkillValidationReport, validate_skill};

mod execution;
pub use execution::*;

mod managed;
mod phase;
mod runtime;
pub use runtime::{Runtime, RuntimeOptions, TraceSetup, validate};

mod codex;
pub use codex::CodexExecutor;

mod milkie;
pub use milkie::{
    MilkieConfig, MilkieExecutor, MilkieObservationOptions, MilkieObserver, MilkieStorage,
    ToolCallMetadata,
};

mod trace;
pub use trace::{
    OpenSpan, SourceCleanup, TraceEvent, TraceItem, TraceOptions, TracePage, TraceQuery,
    TraceSource,
};

mod trace_policy;
pub use trace_policy::{
    TraceContent, TraceContentFilter, TraceContentRef, TracePolicy, TracePolicySnapshot,
    TraceToolContent,
};

mod budget;
pub use budget::{
    BudgetHint, BudgetLimit, BudgetView, ExecutorBudgetLimits, ModelIterationSupport,
    RunBudgetPolicy, RunBudgetSnapshot, TaskLimitSources,
};

mod orchestration;
pub use orchestration::*;
