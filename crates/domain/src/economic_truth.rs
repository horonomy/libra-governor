//! [`EconomicTruth`] — reconstructing "what did this cost, really" from
//! the real persisted ledger tables, without manual DB inspection
//! (HORO-1672).
//!
//! # Two honesty findings this module is built on
//!
//! 1. **No [`crate::EconomicEvent`] stream is actually persisted.**
//!    `crates/domain/src/economic_event.rs` defines the type and
//!    `crates/domain/src/economic_rollup.rs` can project over a `Vec`
//!    of them, but nothing in `crates/daemon` or `crates/ledger` ever
//!    writes one to durable storage today. "Reconcile against canonical
//!    ledger events" therefore cannot mean replaying a persisted event
//!    stream — it means reconciling the real tables that *are*
//!    persisted (`resource_accounts`, `reservations`,
//!    `gateway_requests`) against each other. This module does that
//!    honestly rather than fabricating a synthetic `EconomicEvent` feed
//!    to make the word "events" literally true.
//! 2. **The custody tree (`resource_accounts.parent_account_id`) and
//!    the provider-proven agent-lineage forest
//!    ([`crate::economic_rollup::AgentLineage`]) are two intentionally
//!    separate trees** — see `docs/adr/0008-hierarchical-resource-accounts-and-lease-custody.md`.
//!    [`CustodyNode::provider_lineage`] cross-references the second tree
//!    without ever reconciling the two into one.
//!
//! # Scope anti-widening
//!
//! Many sessions can share one task-level account. Every amount this
//! module produces carries its own [`AmountScope`] so a widened figure
//! (e.g. a task's whole spend, read while explaining one session) can
//! never be printed unlabeled as if it belonged to the narrower scope.
//!
//! # No statusline/host-payload capture
//!
//! Per the HORO-1667 decision (ADR-0009), this module never reads or
//! depends on a host's statusline economics payload. Everything here is
//! reconstructed from Libra's own ledger tables only.

use serde::{Deserialize, Serialize};

use crate::economic_event::{ResourceBasis, TruthStrength};
use crate::execution_identity::{redacted_display_id, LineageStatus};
use crate::progressive::{RemainingWorkEstimate, SpendSoFar};
use crate::reservation::{ReservationId, TaskBudget};
use crate::resource_account::{
    AccountId, AccountLevel, AccountProvenance, AccountState, AllocationAuthority, EnforcementScope,
};
use crate::resource_amount::{ResourceAmount, ResourceKind};
use crate::Estimate;

/// Schema tag every [`EconomicTruth`] is serialized with, following the
/// convention of [`crate::RESERVATION_SCHEMA_VERSION`] and friends.
pub const ECONOMIC_TRUTH_SCHEMA_VERSION: &str = "economic-truth-v1";

impl ResourceKind {
    /// The epsilon below which two amounts of this kind are treated as
    /// reconciled rather than discrepant — chosen per unit since a cent
    /// of USD and a token are not comparable magnitudes.
    pub const fn reconciliation_epsilon(self) -> f64 {
        match self {
            ResourceKind::Usd => 1.0,
            ResourceKind::Tokens => 1.0,
            ResourceKind::QuotaPercent => 0.01,
        }
    }
}

/// Echoes back exactly what selector/value `economics explain` was
/// invoked with, so a rendered report is self-describing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectorEcho {
    pub selector: String,
    pub value: String,
}

/// Why no economic basis exists for the requested selector — a genuine
/// absence, never coerced into an empty-but-present report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "reason")]
pub enum NoEconomicBasis {
    NoSuchTask,
    NoSuchAccount,
    NoSuchSession,
    /// `--principal`/`--organization` are accepted selectors, but v0.0.3
    /// has no configured principal/organization account — see
    /// `AllocationAuthority::RemoteLeaseAuthority`'s "typed now, produced
    /// later" discipline.
    NotConfigured,
    /// A remote authority would be required to answer this selector
    /// authoritatively, and none exists in local-only v0.0.3.
    NoAuthoritativeRemoteAllocation,
}

/// A non-task/account column a scope was further filtered by, when the
/// account tree alone cannot express the requested scope (e.g. a session
/// selector when no dedicated session-level account was ever minted).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeFilter {
    SessionIdColumn,
    GatewaySessionIdColumn,
}

/// How a requested selector was resolved onto the real ledger tables.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeResolution {
    /// Resolved directly to one [`AccountId`] in the custody tree.
    Account {
        account_id: AccountId,
    },
    /// Resolved to an enclosing account, then further filtered by a
    /// non-account column.
    FilteredWithin {
        account_id: AccountId,
        filter: ScopeFilter,
    },
    NoBasis {
        reason: NoEconomicBasis,
    },
}

/// Which scope a [`ScopedAmount`] is honestly reporting — the structural
/// guard against scope anti-widening.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AmountScope {
    /// This account's own leases only, excluding descendants.
    OwnAccountExclusive,
    /// This account plus every proven descendant.
    OwnAccountInclusive,
    /// A wider enclosing account's figure, explicitly labeled as such.
    EnclosingAccount,
    /// Rows filtered by a non-account column within an enclosing
    /// account (e.g. gateway rows matched by `session_id`).
    FilteredRows,
}

/// A single resource amount carrying its truth basis and its scope,
/// never unlabeled. Uses `value: f64` (not [`ResourceAmount`]) so
/// callers can sum several `ScopedAmount`s before rounding —
/// [`ResourceAmount::from_kind_f64`] rounds USD to cents and saturates
/// `QuotaPercent` at 100.0, which breaks additivity mid-sum (mirrors
/// [`crate::economic_rollup::Subtotal`]'s same rationale).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ScopedAmount {
    pub kind: ResourceKind,
    pub value: f64,
    pub basis: ResourceBasis,
    pub scope: AmountScope,
}

impl ScopedAmount {
    pub fn as_resource_amount_clamped(&self) -> ResourceAmount {
        ResourceAmount::from_kind_f64(self.kind, self.value)
    }
}

/// A value that may genuinely have no basis — distinguished from a
/// fabricated zero throughout this module.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Availability<T> {
    Known(T),
    NoBasis { reason: NoEconomicBasis },
}

/// A category whose row count is nonzero but whose amount could not be
/// computed cleanly — a closed, structured reason, never free text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnattributedReason {
    LegacyPreMigration0011,
    GatewayRowWithoutTask,
    GatewaySettledWithoutLease,
    AccountWithoutProvenLineage,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PartialBucket {
    pub amount: Option<ScopedAmount>,
    pub row_count: u32,
    pub reason: UnattributedReason,
}

// ---------------------------------------------------------------------
// The 10 economic categories
// ---------------------------------------------------------------------

/// Capacity granted to this account, and where that grant's authority
/// came from. `authority`/`funding_lease` are `None` when the account
/// has no recorded authority source (e.g. a bare task account with no
/// `task_budgets` row).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Allocation {
    pub granted_capacity: Option<ScopedAmount>,
    pub authority: Option<AllocationAuthority>,
    pub enforcement_scope: EnforcementScope,
    /// The [`ReservationId`] of the `subaccount_funding` lease that
    /// funded this account, when this account was created via
    /// sublease rather than top-level configuration.
    pub funding_lease: Option<ReservationId>,
}

/// The most recent progressive/base estimate recorded for this scope,
/// and how many [`crate::DecisionPoint`]s have been recorded since —
/// never itself spend, per [`crate::economic_event::FactRole::Projection`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Forecast {
    pub at_admission: Option<Estimate>,
    pub latest_remaining: Option<RemainingWorkEstimate>,
    pub latest_decided_at: Option<time::OffsetDateTime>,
    pub decision_points_recorded: usize,
}

/// Capacity claimed but not (yet) consumed — [`crate::economic_event::FactRole::Hold`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LeaseHolds {
    pub work_hold_required: Option<ScopedAmount>,
    pub work_hold_optional: Option<ScopedAmount>,
    pub subaccount_funding: Option<ScopedAmount>,
    pub count: u32,
    pub next_expiry: Option<time::OffsetDateTime>,
}

/// The reserve protected for completion, and how much of it has already
/// been drawn down — see [`crate::COMPLETION_RESERVE_BASE_FRACTION`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompletionReservePosture {
    pub protected: Option<ScopedAmount>,
    pub outstanding_draw: Option<ScopedAmount>,
    pub basis: Option<crate::reservation::CompletionReserveBasis>,
    pub required_criteria_count: usize,
}

/// Resource actually consumed — [`crate::economic_event::FactRole::Spend`] — split by whether
/// the gateway ever reported a real usage figure
/// (`reservations.usage_known`) or the figure is the conservative
/// settle-at-reserved-amount fallback. Collapsing this split into one
/// "actual spend" number is exactly the cosmetic-trustworthiness failure
/// this module exists to avoid.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SettledSpend {
    pub observed: Option<ScopedAmount>,
    pub assumed: Option<ScopedAmount>,
    pub overrun: Option<ScopedAmount>,
    pub refunded: Option<ScopedAmount>,
    pub settled_after_expiry_count: u32,
    pub count_observed: u32,
    pub count_assumed: u32,
}

/// Capacity given back — released deliberately, or reclaimed by cascade
/// expiry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReleasedCapacity {
    pub released: Option<ScopedAmount>,
    pub expired: Option<ScopedAmount>,
    pub released_count: u32,
    pub expired_count: u32,
}

/// The exclusive total — this account's own leases only — and the
/// inclusive total — this account plus every proven descendant. Reuses
/// [`crate::SpendSoFar`] verbatim, never reimplemented.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Totals {
    pub exclusive: SpendSoFar,
    pub inclusive: SpendSoFar,
}

/// Rows that exist but could not be cleanly attributed to this scope —
/// each in exactly one named bucket, counted, and excluded from every
/// other total so a reader can never silently double-count them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Unattributed {
    pub legacy_backfilled: PartialBucket,
    pub gateway_rows_without_task: PartialBucket,
    pub gateway_settled_without_lease: PartialBucket,
    pub accounts_without_proven_lineage: u32,
    pub orphan_accounts: Vec<AccountId>,
}

/// All 10 economic categories for one scope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EconomicCategories {
    pub allocation: Allocation,
    pub forecast: Forecast,
    pub active_leases: LeaseHolds,
    pub completion_reserve: CompletionReservePosture,
    pub settled: SettledSpend,
    pub released: ReleasedCapacity,
    pub totals: Totals,
    pub unattributed: Unattributed,
}

// ---------------------------------------------------------------------
// Custody tree
// ---------------------------------------------------------------------

/// Bounds on how large a rendered custody-tree walk may grow — an
/// output-size bound, not cycle safety (no writer ever creates a cycle
/// in `resource_accounts.parent_account_id`). Mirrors
/// `crates/daemon/src/recon.rs`'s `ReconBudget`/`truncated` precedent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeBudget {
    pub max_nodes: usize,
    pub max_depth: usize,
}

impl Default for TreeBudget {
    fn default() -> Self {
        TreeBudget {
            max_nodes: 200,
            max_depth: 8,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Truncation {
    NodeBudgetExhausted,
    DepthBudgetExhausted,
}

/// Whether provider-proven lineage was ever recorded for this account —
/// see module docs: never used to construct a custody-tree edge, only
/// cross-referenced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeLineage {
    Recorded(LineageStatus),
    NotRecorded,
}

/// One node in a bounded custody-tree walk.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CustodyNode {
    pub account_id: AccountId,
    pub parent: Option<AccountId>,
    pub depth: u16,
    pub level: AccountLevel,
    /// Redacted via [`redacted_display_id`] — never the raw natural key.
    pub natural_key_display: String,
    pub state: AccountState,
    pub provenance: AccountProvenance,
    pub exclusive: SpendSoFar,
    /// Cross-references the provider-proven agent-lineage forest for
    /// this node — see module docs on why this is never unified with
    /// the custody edge itself. `NotRecorded` in v0.0.3 for every real
    /// account: nothing persists an `ExecutionIdentity` envelope on the
    /// daemon path yet (see
    /// `crate::resource_account::ResourceAccount::provider_lineage_status`'s
    /// own doc comment). Reconstructing the full forest (not just this
    /// node's own recorded status) is out of scope for HORO-1672 — see
    /// `docs/adr/0013-economic-truth-reconstruction.md`.
    pub provider_lineage: NodeLineage,
}

impl CustodyNode {
    pub fn redact_natural_key(account_id: AccountId, raw_natural_key: &str) -> String {
        redacted_display_id(&format!("natural_key:{account_id}"), raw_natural_key)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CustodyTree {
    pub root: Option<AccountId>,
    pub nodes: Vec<CustodyNode>,
    pub truncated: Option<Truncation>,
}

// ---------------------------------------------------------------------
// Reconciliation — real cross-source checks, reported, never equalized
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckId {
    /// A subtree's inclusive total equals the sum of its direct
    /// children's inclusive totals plus its own exclusive total.
    SubtreeAdditivity,
    /// `granted_capacity` minus `settled` minus `active_holds` equals
    /// the headroom the reservation ledger itself would report via
    /// `available`.
    EnvelopeFormula,
    /// A gateway request's `settled_amount` agrees with its linked
    /// reservation's `settled_amount`, joined by `reservation_id`.
    GatewayLedgerAgreement,
}

/// A real cross-source check can legitimately disagree in a correct
/// system — this is reported data, never silently equalized.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "outcome")]
pub enum CheckOutcome {
    Reconciled,
    Discrepant { delta: f64 },
    NotApplicable,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ReconciliationCheck {
    pub check: CheckId,
    pub outcome: CheckOutcome,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reconciliation {
    pub checks: Vec<ReconciliationCheck>,
    pub all_reconciled: bool,
}

impl Reconciliation {
    pub fn from_checks(checks: Vec<ReconciliationCheck>) -> Self {
        let all_reconciled = checks.iter().all(|c| {
            matches!(
                c.outcome,
                CheckOutcome::Reconciled | CheckOutcome::NotApplicable
            )
        });
        Reconciliation {
            checks,
            all_reconciled,
        }
    }
}

// ---------------------------------------------------------------------
// Provenance
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TruthSource {
    ResourceAccounts,
    Reservations,
    GatewayRequests,
    TaskBudgets,
    ShadowDecisions,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TruthProvenance {
    pub weakest_truth: Option<TruthStrength>,
    pub account_provenance: Option<AccountProvenance>,
    pub account_schema_version: Option<String>,
    pub sources_read: Vec<TruthSource>,
    pub pricing_versions: Vec<String>,
}

// ---------------------------------------------------------------------
// The top-level report
// ---------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EconomicTruth {
    pub schema_version: String,
    pub selector: SelectorEcho,
    pub resolution: ScopeResolution,
    pub categories: Option<EconomicCategories>,
    pub custody_tree: Option<CustodyTree>,
    pub reconciliation: Option<Reconciliation>,
    pub provenance: TruthProvenance,
    /// Task-level budget, when this scope resolved onto a task with one
    /// configured — surfaced verbatim, never recomputed.
    pub task_budget: Option<TaskBudget>,
}

impl EconomicTruth {
    /// Builds the `NoBasis` shape for a selector that resolved to
    /// nothing — every other field is honestly empty, never guessed.
    pub fn no_basis(selector: SelectorEcho, reason: NoEconomicBasis) -> Self {
        EconomicTruth {
            schema_version: ECONOMIC_TRUTH_SCHEMA_VERSION.to_string(),
            selector,
            resolution: ScopeResolution::NoBasis { reason },
            categories: None,
            custody_tree: None,
            reconciliation: None,
            provenance: TruthProvenance {
                weakest_truth: None,
                account_provenance: None,
                account_schema_version: None,
                sources_read: Vec::new(),
                pricing_versions: Vec::new(),
            },
            task_budget: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scoped_amount_clamps_only_at_display() {
        let amount = ScopedAmount {
            kind: ResourceKind::Usd,
            value: 199.6,
            basis: ResourceBasis::GatewayMeteredActual,
            scope: AmountScope::OwnAccountExclusive,
        };
        assert_eq!(
            amount.as_resource_amount_clamped(),
            ResourceAmount::UsdCents(200)
        );
    }

    #[test]
    fn no_basis_report_never_fabricates_categories() {
        let truth = EconomicTruth::no_basis(
            SelectorEcho {
                selector: "task".to_string(),
                value: "missing".to_string(),
            },
            NoEconomicBasis::NoSuchTask,
        );
        assert!(truth.categories.is_none());
        assert!(truth.custody_tree.is_none());
        assert_eq!(
            truth.resolution,
            ScopeResolution::NoBasis {
                reason: NoEconomicBasis::NoSuchTask
            }
        );
    }

    #[test]
    fn redacted_natural_key_never_contains_the_raw_value() {
        let account_id = AccountId::new();
        let redacted = CustodyNode::redact_natural_key(account_id, "super-secret-session-token");
        assert!(!redacted.contains("super-secret-session-token"));
    }

    #[test]
    fn reconciliation_is_all_reconciled_only_when_every_check_agrees() {
        let clean = Reconciliation::from_checks(vec![ReconciliationCheck {
            check: CheckId::SubtreeAdditivity,
            outcome: CheckOutcome::Reconciled,
        }]);
        assert!(clean.all_reconciled);

        let dirty = Reconciliation::from_checks(vec![ReconciliationCheck {
            check: CheckId::GatewayLedgerAgreement,
            outcome: CheckOutcome::Discrepant { delta: 5.0 },
        }]);
        assert!(!dirty.all_reconciled);
    }

    #[test]
    fn reconciliation_not_applicable_counts_as_reconciled() {
        let r = Reconciliation::from_checks(vec![ReconciliationCheck {
            check: CheckId::EnvelopeFormula,
            outcome: CheckOutcome::NotApplicable,
        }]);
        assert!(r.all_reconciled);
    }
}
