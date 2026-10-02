//! The hierarchical resource-account tree (HORO-1668): organization ->
//! principal -> task -> session -> agent -> nested-agent sublease ->
//! billable/economic events.
//!
//! # Custody lineage is not provider lineage
//!
//! A [`ResourceAccount`]'s `parent_account_id` edge is Libra-minted and
//! proven only by the act of leasing: a parent account asked for a
//! sublease, so an edge exists. This is a *different tree* from
//! [`crate::AgentLineage`] (provider-proven agent parentage, derived from
//! [`crate::LineageStatus`] — currently an all-roots forest on every
//! supported provider, see `crates/cli/src/agent/identity.rs`). The two
//! may legitimately disagree; see
//! `docs/adr/0008-hierarchical-resource-accounts-and-lease-custody.md`.
//! Nothing here reconstructs provider lineage from timing, process
//! ancestry, or any other inference — a custody edge exists only where a
//! parent account was explicitly asked to fund a child.
//!
//! # Not every level exists
//!
//! A task account with no principal/organization ancestor is fully valid
//! local accounting. Missing upper levels never invalidate what is
//! actually proven; they leave principal/organization rollups reporting
//! that task in an unattributed/partial bucket rather than guessing an
//! ancestor.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::task_identity::TaskId;

/// Schema tag every [`ResourceAccount`] is persisted with, following the
/// same convention as [`crate::RESERVATION_SCHEMA_VERSION`].
pub const RESOURCE_ACCOUNT_SCHEMA_VERSION: &str = "resource-account-v1";

/// Identifier for one [`ResourceAccount`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AccountId(pub Uuid);

impl AccountId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// The deterministic task-level account id: a task's own uuid,
    /// reinterpreted as an [`AccountId`]. No generation needed, and it
    /// keeps `COALESCE(account_id, task_id)` identity-preserving for
    /// legacy (pre-0011) reservation rows.
    pub fn for_task(task_id: TaskId) -> Self {
        Self(task_id.0)
    }
}

impl Default for AccountId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for AccountId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Where a [`ResourceAccount`] sits in the hierarchy.
///
/// There is no separate "nested-agent sublease" variant: a nested-agent
/// sublease is just an [`AccountLevel::Agent`] account whose parent is
/// itself an `Agent` account — recursion, not a new level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountLevel {
    Organization,
    Principal,
    Task,
    Session,
    Agent,
}

impl AccountLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            AccountLevel::Organization => "organization",
            AccountLevel::Principal => "principal",
            AccountLevel::Task => "task",
            AccountLevel::Session => "session",
            AccountLevel::Agent => "agent",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "organization" => Some(AccountLevel::Organization),
            "principal" => Some(AccountLevel::Principal),
            "task" => Some(AccountLevel::Task),
            "session" => Some(AccountLevel::Session),
            "agent" => Some(AccountLevel::Agent),
            _ => None,
        }
    }
}

/// Where an allocation's authority came from.
///
/// [`AllocationAuthority::RemoteLeaseAuthority`] is typed now, produced
/// later — the same discipline [`crate::ResourceBasis::ImportedAllocationSnapshot`]
/// already uses. Constructing an account with this authority is refused
/// in v0.0.3 (see `libra_governor_ledger`): local mode never claims
/// cross-device enforcement without a remote authority to back it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AllocationAuthority {
    LocalUserConfig,
    ImportedCorporateSnapshot,
    RemoteLeaseAuthority,
}

impl AllocationAuthority {
    pub fn as_str(self) -> &'static str {
        match self {
            AllocationAuthority::LocalUserConfig => "local_user_config",
            AllocationAuthority::ImportedCorporateSnapshot => "imported_corporate_snapshot",
            AllocationAuthority::RemoteLeaseAuthority => "remote_lease_authority",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "local_user_config" => Some(AllocationAuthority::LocalUserConfig),
            "imported_corporate_snapshot" => Some(AllocationAuthority::ImportedCorporateSnapshot),
            "remote_lease_authority" => Some(AllocationAuthority::RemoteLeaseAuthority),
            _ => None,
        }
    }
}

/// Whether an account's accounting is enforced only on this machine, or
/// by a remote authority across devices. Always [`Self::LocalDevice`] in
/// v0.0.3 — [`Self::RemoteAuthoritative`] has no producer (see module
/// docs and ADR-0008).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnforcementScope {
    LocalDevice,
    RemoteAuthoritative,
}

impl EnforcementScope {
    pub fn as_str(self) -> &'static str {
        match self {
            EnforcementScope::LocalDevice => "local_device",
            EnforcementScope::RemoteAuthoritative => "remote_authoritative",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "local_device" => Some(EnforcementScope::LocalDevice),
            "remote_authoritative" => Some(EnforcementScope::RemoteAuthoritative),
            _ => None,
        }
    }
}

/// Where an account came from: created fresh under this ticket's own
/// semantics, or backfilled from a pre-0011 `task_budgets` row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountProvenance {
    Native,
    LegacyBackfill0011,
}

impl AccountProvenance {
    pub fn as_str(self) -> &'static str {
        match self {
            AccountProvenance::Native => "native",
            AccountProvenance::LegacyBackfill0011 => "legacy_backfill_0011",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "native" => Some(AccountProvenance::Native),
            "legacy_backfill_0011" => Some(AccountProvenance::LegacyBackfill0011),
            _ => None,
        }
    }
}

/// The lifecycle state of a [`ResourceAccount`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountState {
    /// Open to new leases against it.
    Open,
    /// Closed deliberately (its funding lease was settled up — see
    /// `libra_governor_ledger::resource_account::close_account`).
    Closed,
    /// Reclaimed by cascade expiry (its funding lease's TTL elapsed while
    /// it, or an ancestor, was never explicitly closed).
    Expired,
}

impl AccountState {
    pub fn as_str(self) -> &'static str {
        match self {
            AccountState::Open => "open",
            AccountState::Closed => "closed",
            AccountState::Expired => "expired",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "open" => Some(AccountState::Open),
            "closed" => Some(AccountState::Closed),
            "expired" => Some(AccountState::Expired),
            _ => None,
        }
    }
}

/// One node in the hierarchical resource-account tree (HORO-1668).
///
/// Capacity (`granted_capacity`/`protected_reserve`) is read through
/// `v_account_capacity` in the ledger, never duplicated here for the
/// `Task` level — see module docs and the migration's own comments for
/// why `task_budgets` stays the one source of truth for task-level
/// numbers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResourceAccount {
    pub account_id: AccountId,
    pub level: AccountLevel,
    pub parent_account_id: Option<AccountId>,
    pub resource_kind: crate::resource_amount::ResourceKind,
    /// Natural key for idempotent creation under concurrency: the
    /// session id for a session account, the caller's sublease handle
    /// for an agent account, the task uuid for a task account, the
    /// configured id for org/principal.
    pub natural_key: String,
    pub task_id: Option<TaskId>,
    pub authority_source: Option<AllocationAuthority>,
    pub enforcement_scope: EnforcementScope,
    /// Recorded from an [`crate::ExecutionIdentity`] when one is
    /// genuinely available. Today this is always `None` from the daemon
    /// path — nothing persists or transmits the envelope yet (see
    /// ADR-0008) — and it is NEVER used to construct a tree edge; the
    /// edge comes only from the act of leasing.
    pub provider_lineage_status: Option<crate::LineageStatus>,
    /// The narrowest [`crate::DimensionKey`] an [`crate::ExecutionIdentity`]
    /// proved for this account, serialized, when available. `None` is the
    /// honest representation of [`crate::UnknownReason::NotExposedByProvider`]
    /// — never faked.
    pub execution_dimension_key_json: Option<String>,
    pub provenance: AccountProvenance,
    pub account_schema_version: String,
    pub state: AccountState,
    pub created_at: time::OffsetDateTime,
    pub updated_at: time::OffsetDateTime,
    pub closed_at: Option<time::OffsetDateTime>,
}

/// An account's capacity, read through `v_account_capacity`.
/// `granted_capacity: None` means no authoritative allocation exists at
/// this level — valid, not an error (see module docs).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AccountCapacity {
    pub account_id: AccountId,
    pub resource_kind: crate::resource_amount::ResourceKind,
    pub granted_capacity: Option<crate::resource_amount::ResourceAmount>,
    pub protected_reserve: Option<crate::resource_amount::ResourceAmount>,
}
