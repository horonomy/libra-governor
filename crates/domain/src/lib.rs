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
mod resource_amount;
mod task_identity;

pub use completion_contract::{CompletionContract, CompletionCriterion};
pub use confidence::Confidence;
pub use estimate::{Estimate, ESTIMATOR_VERSION};
pub use execution_event::{ExecutionEvent, ExecutionEventKind};
pub use execution_outcome::ExecutionOutcome;
pub use execution_plan::{ExecutionPlan, PlanId};
pub use execution_receipt::ExecutionReceipt;
pub use resource_amount::{ResourceAmount, ResourceKind};
pub use task_identity::{ExternalRef, TaskId, TaskIdentity};
