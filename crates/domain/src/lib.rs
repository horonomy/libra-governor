//! `libra-governor-domain` — the canonical, harness-agnostic domain types
//! Libra's daemon, ledger, and estimator are built on.
//!
//! # Privacy invariant
//!
//! None of the types in this crate carry a field for raw prompt text or
//! raw tool output content. [`ExecutionEventKind::ToolInvoked`] carries a
//! tool *name* only; [`ExecutionOutcome`] carries evidence *references*
//! (URLs, IDs, paths), never inlined content. This is a structural
//! guarantee, not a convention some caller could violate by populating an
//! optional field — the field simply does not exist on the type.
//!
//! [`TaskFeatures`] (HORO-1130) extends this same guarantee to
//! estimator bucketing: every one of its fields is an irreversible
//! derived scalar — a count, a boolean, a length, a small closed-set
//! enum, or a truncated SHA-256 digest of a canonicalized path — never
//! the prompt text, tool output, or file path it was computed from. A
//! reviewer does not need to trust a convention here either: there is no
//! `String`-typed field on [`TaskFeatures`] that could hold raw content
//! except `repo_key` (a hex digest, not a path) and `model` (a short
//! model identifier, never derived from prompt or file content).
//!
//! # The economic unit
//!
//! [`TaskIdentity`], not a session or a token, is the anchor every other
//! type in this crate attaches to. See
//! `docs/adr/0002-task-not-session-as-economic-unit.md` for why.

mod completion_contract;
mod confidence;
mod estimate;
mod execution_event;
mod execution_outcome;
mod execution_plan;
mod execution_receipt;
mod policy;
mod resource_amount;
mod task_features;
mod task_identity;

pub use completion_contract::{CompletionContract, CompletionCriterion};
pub use confidence::{Confidence, MIN_CLASS_SAMPLES};
pub use estimate::{Estimate, ESTIMATOR_VERSION};
pub use execution_event::{ExecutionEvent, ExecutionEventKind};
pub use execution_outcome::ExecutionOutcome;
pub use execution_plan::{ExecutionPlan, PlanId};
pub use execution_receipt::ExecutionReceipt;
pub use policy::{
    Admission, ApprovalRequest, AutonomyBoundary, ConstraintMode, ConstraintOutcome, DenyReason,
    Policy, PolicyDecision, PolicyEvaluationError, PolicyPresetInputs, PolicyValidationError,
    ResourceBound, TimeBound, POLICY_SCHEMA_VERSION,
};
pub use resource_amount::{ResourceAmount, ResourceKind};
pub use task_features::{BucketTier, BuildTopology, TaskFeatures, FEATURE_SCHEMA_VERSION};
pub use task_identity::{ExternalRef, TaskId, TaskIdentity};
