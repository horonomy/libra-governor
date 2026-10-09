//! Bounded, auditable task-budget renewal (HORO-1727): the domain types
//! for one grant against a task's [`crate::policy::RenewalBound`].
//!
//! # Scope — mechanism only, no live authorization path
//!
//! This module defines the request/refusal/record shapes and
//! [`libra_governor_ledger::LedgerStore::grant_renewal`]'s gates. It does
//! **not** add any CLI command, protocol request, or webhook path that
//! could create a renewal outside a test — see that ledger method's own
//! docs and the `renewal_not_wired_live` guard test, which fails the
//! build if `daemon/src`, `cli/src`, or `gateway/src` ever calls it.
//!
//! # Why only one [`RenewalAuthority`] variant
//!
//! An automatic/policy-coded authority would be a silent escalation path
//! with no calibration evidence behind it yet, and the Policy Webhook's
//! `apply_external_approval` is documented (see
//! [`crate::external_approval`]) as unable to widen a hard-ceiling
//! `Deny` — making it a renewal authority would need its own ADR change.
//! Both are deliberately out of scope; `Operator` is the only authority a
//! human reviewing this ticket's design actually asked for.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{
    quota_window::BlockingStatus, resource_amount::ResourceAmount, resource_amount::ResourceKind,
    task_identity::TaskId,
};

/// Traceability tag every persisted renewal grant is tagged with,
/// following [`crate::RESERVATION_SCHEMA_VERSION`]'s convention.
pub const TASK_BUDGET_RENEWAL_SCHEMA_VERSION: &str = "task-budget-renewal-v1";

/// Identifier for one persisted [`TaskBudgetRenewal`] grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RenewalId(pub Uuid);

impl RenewalId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for RenewalId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for RenewalId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Who authorized a renewal grant (HORO-1727). Deliberately one variant —
/// see module docs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RenewalAuthority {
    /// A human operator explicitly approved this grant. `operator_id` is
    /// a free-text identifier (a username, an email, a ticket-assigned
    /// reviewer id) — whatever the eventual CLI/approval-flow ticket
    /// decides to populate it with; this type does not prescribe a
    /// format.
    Operator { operator_id: String },
}

/// A request to grant additional capacity against a task's
/// [`crate::policy::RenewalBound`] (HORO-1727). See
/// `libra_governor_ledger::LedgerStore::grant_renewal` for the gates this
/// is evaluated against.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RenewalRequest {
    /// How much capacity to grant, in the task's own [`ResourceKind`].
    pub amount: ResourceAmount,
    pub authority: RenewalAuthority,
    /// The [`crate::completion_contract::CompletionContract::revision`]
    /// this grant was authorized against — recorded so a later audit can
    /// tell whether the Definition of Done changed between grant and
    /// completion.
    pub contract_revision: u32,
    /// Human-readable justification. Operator-authored metadata, never
    /// prompt or tool-output content — matching this crate's privacy
    /// invariant (see module docs at `crate::lib`).
    pub reason: String,
    /// Caller-chosen replay key, unique per task — mirrors
    /// [`crate::reservation::Reservation`]'s idempotency discipline.
    pub idempotency_key: String,
}

/// Why a [`RenewalRequest`] was refused (HORO-1727). One variant per
/// gate, never a generic string, so a caller (or a test) can match on
/// exactly what was wrong.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum RenewalRefusal {
    /// The task's policy carries no [`crate::policy::RenewalBound`] at
    /// all (`Policy::renewal` is `None`) — renewals are disabled by
    /// default for every task unless its policy opts in.
    RenewalsDisabled,
    /// The task has already been granted `max_renewals` renewals.
    MaxRenewalsExceeded { current: u32, max: u32 },
    /// The requested amount's [`ResourceKind`] does not match the task's
    /// own envelope kind.
    ResourceKindMismatch {
        expected: ResourceKind,
        actual: ResourceKind,
    },
    /// A single grant may add at most one allocation's worth of capacity
    /// — `amount` exceeded `policy.resource.hard_ceiling`.
    GrantExceedsHardCeiling {
        amount: ResourceAmount,
        hard_ceiling: ResourceAmount,
    },
    /// `effective_hard + amount` would exceed
    /// `policy.renewal.lifetime_ceiling`.
    LifetimeCeilingExceeded {
        effective_after: ResourceAmount,
        lifetime_ceiling: ResourceAmount,
    },
    /// The caller-supplied upstream quota [`BlockingStatus`] was
    /// `Blocking` or `Indeterminate` — both are refused. See
    /// `grant_renewal`'s own docs for why `Indeterminate` is treated as
    /// unsafe rather than "probably fine": ADR-0015 leaves its exact
    /// semantics open, so this is a deliberate conservative choice, not
    /// an oversight.
    UpstreamQuotaBlocking { status: BlockingStatus },
    /// An authoritative (`Provider`/`GovernorLocal`) `Completed`
    /// attestation already exists for this task — there is no remaining
    /// work a renewal could fund.
    TaskAlreadyCompleted,
}

/// One persisted grant against a task's [`crate::policy::RenewalBound`]
/// (HORO-1727) — the row `libra_governor_ledger`'s
/// `task_budget_renewals` table stores, append-only (see that migration's
/// own docs).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskBudgetRenewal {
    pub id: RenewalId,
    pub task_id: TaskId,
    pub amount: ResourceAmount,
    pub authority: RenewalAuthority,
    pub contract_revision: u32,
    pub reason: String,
    pub idempotency_key: String,
    /// Lifetime settled spend at the moment this grant was evaluated.
    pub settled_at_grant: ResourceAmount,
    /// Active (not yet settled) reservations at the moment this grant
    /// was evaluated.
    pub active_at_grant: ResourceAmount,
    /// `effective_hard_limit` immediately before this grant.
    pub effective_before: ResourceAmount,
    /// `effective_before + amount` — `effective_hard_limit` immediately
    /// after this grant.
    pub effective_after: ResourceAmount,
    /// The upstream quota [`BlockingStatus`] this grant was evaluated
    /// against — recorded so an audit can see why a grant that is later
    /// found suspicious was approved.
    pub quota_status_at_grant: BlockingStatus,
    pub schema_version: String,
    pub granted_at: OffsetDateTime,
}
