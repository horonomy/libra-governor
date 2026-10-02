//! The hierarchical resource-account tree (HORO-1668): provisioning,
//! sublease granting, and cascade close/expire. See
//! `migrations/0011_hierarchical_resource_accounts.sql` and
//! `docs/adr/0008-hierarchical-resource-accounts-and-lease-custody.md`
//! for the full design rationale.
//!
//! # The invariant is checked one level deep
//!
//! For account `A`:
//!
//! ```text
//! sum(active leases against A) + sum(settled leases against A)
//!     + protected_reserve(A) <= granted_capacity(A) + overrun slack
//! ```
//!
//! A child account's capacity is the amount of its funding lease, which
//! is itself an active lease against the parent. So this one-level check
//! *inductively implies* the global guarantee: no descendant can hold
//! capacity its ancestor has not already carved out and accounted for.
//! This keeps the hot path an indexed `SUM`, never a recursive CTE on the
//! write path.

use libra_governor_domain::{
    AccountCapacity, AccountId, AccountLevel, AccountProvenance, AccountState, AllocationAuthority,
    EnforcementScope, Headroom, LeaseKind, NoSpendBasis, Reservation, ReservationId,
    ResourceAccount, ResourceAmount, ResourceKind, SpendScope, SpendSoFar,
    RESOURCE_ACCOUNT_SCHEMA_VERSION,
};
use rusqlite::{OptionalExtension, TransactionBehavior};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::reservation::{ReservationRow, RESERVATION_COLUMNS};
use crate::{store::LedgerStore, LedgerError};

fn rfc3339(t: OffsetDateTime) -> Result<String, LedgerError> {
    t.format(&Rfc3339)
        .map_err(|e| LedgerError::Sqlite(rusqlite::Error::ToSqlConversionFailure(Box::new(e))))
}

fn parse_time(s: &str) -> Result<OffsetDateTime, LedgerError> {
    OffsetDateTime::parse(s, &Rfc3339)
        .map_err(|_| LedgerError::Sqlite(rusqlite::Error::InvalidQuery))
}

fn kind_to_str(kind: ResourceKind) -> &'static str {
    match kind {
        ResourceKind::Usd => "usd",
        ResourceKind::Tokens => "tokens",
        ResourceKind::QuotaPercent => "quota_percent",
    }
}

fn kind_from_str(s: &str) -> Result<ResourceKind, LedgerError> {
    match s {
        "usd" => Ok(ResourceKind::Usd),
        "tokens" => Ok(ResourceKind::Tokens),
        "quota_percent" => Ok(ResourceKind::QuotaPercent),
        _ => Err(LedgerError::Sqlite(rusqlite::Error::InvalidQuery)),
    }
}

#[allow(clippy::type_complexity)]
type AccountRow = (
    String,         // account_id
    String,         // level
    Option<String>, // parent_account_id
    String,         // resource_kind
    String,         // natural_key
    Option<String>, // task_id
    Option<f64>,    // granted_capacity
    Option<f64>,    // protected_reserve
    Option<String>, // authority_source
    String,         // enforcement_scope
    Option<String>, // provider_lineage_status
    Option<String>, // execution_dimension_key_json
    String,         // provenance
    String,         // account_schema_version
    String,         // state
    String,         // created_at
    String,         // updated_at
    Option<String>, // closed_at
);

fn row_to_account(row: AccountRow) -> Result<ResourceAccount, LedgerError> {
    let (
        account_id,
        level,
        parent_account_id,
        resource_kind,
        natural_key,
        task_id,
        _granted_capacity,
        _protected_reserve,
        authority_source,
        enforcement_scope,
        provider_lineage_status,
        execution_dimension_key_json,
        provenance,
        account_schema_version,
        state,
        created_at,
        updated_at,
        closed_at,
    ) = row;
    let invalid = || LedgerError::Sqlite(rusqlite::Error::InvalidQuery);
    Ok(ResourceAccount {
        account_id: AccountId(uuid::Uuid::parse_str(&account_id).map_err(|_| invalid())?),
        level: AccountLevel::parse(&level).ok_or_else(invalid)?,
        parent_account_id: parent_account_id
            .map(|s| uuid::Uuid::parse_str(&s).map(AccountId))
            .transpose()
            .map_err(|_| invalid())?,
        resource_kind: kind_from_str(&resource_kind)?,
        natural_key,
        task_id: task_id
            .map(|s| uuid::Uuid::parse_str(&s).map(libra_governor_domain::TaskId))
            .transpose()
            .map_err(|_| invalid())?,
        authority_source: authority_source
            .map(|s| AllocationAuthority::parse(&s).ok_or_else(invalid))
            .transpose()?,
        enforcement_scope: EnforcementScope::parse(&enforcement_scope).ok_or_else(invalid)?,
        provider_lineage_status: None, // never reconstructed from storage alone — see module docs; populated only at write time by a real ExecutionIdentity in a later ticket.
        execution_dimension_key_json,
        provenance: AccountProvenance::parse(&provenance).ok_or_else(invalid)?,
        account_schema_version,
        state: AccountState::parse(&state).ok_or_else(invalid)?,
        created_at: parse_time(&created_at)?,
        updated_at: parse_time(&updated_at)?,
        closed_at: closed_at.map(|s| parse_time(&s)).transpose()?,
    })
    .map(|mut a: ResourceAccount| {
        a.provider_lineage_status = provider_lineage_status.as_deref().and_then(|s| match s {
            "root" => Some(libra_governor_domain::LineageStatus::Root),
            "child" => Some(libra_governor_domain::LineageStatus::Child),
            "unknown" => Some(libra_governor_domain::LineageStatus::Unknown),
            _ => None,
        });
        a
    })
}

/// Errors specific to the hierarchical resource-account tree.
#[derive(Debug, Clone, PartialEq)]
pub enum AccountError {
    ParentNotFound,
    ParentNotOpen,
    ResourceKindMismatch {
        expected: ResourceKind,
        actual: ResourceKind,
    },
    /// The requested child lease's expiry exceeds its funding lease's
    /// expiry — a child can never outlive the capacity that funds it.
    ChildExpiryExceedsFunding,
    RemoteLeaseAuthorityNotSupported,
    /// `grant_sublease`'s parent has no `task_id` lineage. v0.0.3 only
    /// wires session/agent accounts hanging off a task account (see
    /// ADR-0008's documented gap: standalone organization/principal
    /// leases with no task ancestor are not provisioned by this ticket).
    ParentHasNoTaskLineage,
}

/// The outcome of [`LedgerStore::ensure_task_account`]/[`LedgerStore::ensure_child_account`].
#[derive(Debug, Clone, PartialEq)]
pub struct EnsureAccountOutcome {
    pub account: ResourceAccount,
    pub created: bool,
}

/// Request to fund `child_account_id` from `parent_account_id`'s
/// capacity. See [`LedgerStore::grant_sublease`].
pub struct GrantSubleaseRequest<'a> {
    pub parent_account_id: AccountId,
    pub child_account_id: AccountId,
    /// A human-readable label for the lease row's `session_id` column
    /// (reused here as "which child" context — the real identity is
    /// `grants_account_id`/`child_account_id`, not this string).
    pub child_natural_key: &'a str,
    pub amount: ResourceAmount,
    pub idempotency_key: &'a str,
    pub now: OffsetDateTime,
    pub ttl_secs: u64,
}

/// The outcome of [`LedgerStore::grant_sublease`].
#[derive(Debug, Clone, PartialEq)]
pub enum GrantSubleaseOutcome {
    Granted(Box<Reservation>),
    AlreadyGranted(Box<Reservation>),
    Insufficient {
        available: Headroom,
        requested: ResourceAmount,
    },
}

impl LedgerStore {
    /// Provisions (or returns the already-provisioned) task-level
    /// account for `task_id`, with `account_id == AccountId::for_task(task_id)`.
    /// Idempotent under concurrency via `ON CONFLICT ... DO NOTHING` plus
    /// a re-read — mirrors [`LedgerStore::initialize_task_budget`].
    pub fn ensure_task_account(
        &mut self,
        task_id: libra_governor_domain::TaskId,
        resource_kind: ResourceKind,
        now: OffsetDateTime,
    ) -> Result<EnsureAccountOutcome, LedgerError> {
        let account_id = AccountId::for_task(task_id);
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now_str = rfc3339(now)?;
        let inserted = tx.execute(
            "INSERT INTO resource_accounts (
                account_id, level, parent_account_id, resource_kind, natural_key, task_id,
                granted_capacity, protected_reserve, funding_lease_id, authority_source,
                enforcement_scope, provider_lineage_status, execution_dimension_key_json,
                provenance, account_schema_version, state, created_at, updated_at, closed_at
             ) VALUES (?1, 'task', NULL, ?2, ?3, ?4, NULL, NULL, NULL, 'local_user_config',
                       'local_device', NULL, NULL, 'native', ?5, 'open', ?6, ?6, NULL)
             ON CONFLICT(account_id) DO NOTHING",
            rusqlite::params![
                account_id.to_string(),
                kind_to_str(resource_kind),
                task_id.to_string(),
                task_id.to_string(),
                RESOURCE_ACCOUNT_SCHEMA_VERSION,
                now_str,
            ],
        )?;
        let row = Self::get_account_tx(&tx, account_id)?
            .expect("row just inserted or already present under the same write lock");
        tx.commit()?;
        Ok(EnsureAccountOutcome {
            account: row,
            created: inserted > 0,
        })
    }

    /// Provisions (or returns the already-provisioned) `level` account
    /// under `parent_account_id`, identified by `natural_key` — the
    /// session id for a [`AccountLevel::Session`] account, the caller's
    /// sublease handle for an [`AccountLevel::Agent`] account. A
    /// nested-agent sublease is simply an `Agent` account whose parent is
    /// itself an `Agent` account (see domain module docs). Idempotent
    /// under concurrency: `INSERT ... ON CONFLICT(parent_account_id,
    /// level, natural_key) DO NOTHING` then a re-read, mirroring
    /// [`Self::ensure_task_account`].
    pub fn ensure_child_account(
        &mut self,
        parent_account_id: AccountId,
        level: AccountLevel,
        natural_key: &str,
        now: OffsetDateTime,
    ) -> Result<Result<EnsureAccountOutcome, AccountError>, LedgerError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(parent) = Self::get_account_tx(&tx, parent_account_id)? else {
            tx.commit()?;
            return Ok(Err(AccountError::ParentNotFound));
        };

        let new_id = AccountId::new();
        let now_str = rfc3339(now)?;
        tx.execute(
            "INSERT INTO resource_accounts (
                account_id, level, parent_account_id, resource_kind, natural_key, task_id,
                granted_capacity, protected_reserve, funding_lease_id, authority_source,
                enforcement_scope, provider_lineage_status, execution_dimension_key_json,
                provenance, account_schema_version, state, created_at, updated_at, closed_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, NULL, NULL, NULL,
                       'local_device', NULL, NULL, 'native', ?7, 'open', ?8, ?8, NULL)
             ON CONFLICT(parent_account_id, level, natural_key) DO NOTHING",
            rusqlite::params![
                new_id.to_string(),
                level.as_str(),
                parent_account_id.to_string(),
                kind_to_str(parent.resource_kind),
                natural_key,
                parent.task_id.map(|t| t.to_string()),
                RESOURCE_ACCOUNT_SCHEMA_VERSION,
                now_str,
            ],
        )?;
        let row: AccountRow = tx.query_row(
            "SELECT account_id, level, parent_account_id, resource_kind, natural_key,
                    task_id, granted_capacity, protected_reserve, authority_source,
                    enforcement_scope, provider_lineage_status, execution_dimension_key_json,
                    provenance, account_schema_version, state, created_at, updated_at, closed_at
             FROM resource_accounts WHERE parent_account_id = ?1 AND level = ?2 AND natural_key = ?3",
            rusqlite::params![parent_account_id.to_string(), level.as_str(), natural_key],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                    row.get(9)?,
                    row.get(10)?,
                    row.get(11)?,
                    row.get(12)?,
                    row.get(13)?,
                    row.get(14)?,
                    row.get(15)?,
                    row.get(16)?,
                    row.get(17)?,
                ))
            },
        )?;
        let account = row_to_account(row)?;
        let created = account.account_id == new_id;
        tx.commit()?;
        Ok(Ok(EnsureAccountOutcome { account, created }))
    }

    /// Grants a sublease: `amount` of `parent_account_id`'s capacity is
    /// funded into `child_account_id` as a `subaccount_funding` lease,
    /// atomically reducing the parent's available capacity in the same
    /// transaction this inserts the lease — "immediately and atomically"
    /// is structural, not a convention (see module docs). Idempotent on
    /// `idempotency_key` scoped to `parent_account_id`, same shape as
    /// [`LedgerStore::reserve`].
    pub fn grant_sublease(
        &mut self,
        req: GrantSubleaseRequest<'_>,
    ) -> Result<Result<GrantSubleaseOutcome, AccountError>, LedgerError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;

        let existing: Option<ReservationRow> = tx
            .query_row(
                &format!(
                    "SELECT {RESERVATION_COLUMNS}
                     FROM reservations WHERE account_id = ?1 AND idempotency_key = ?2"
                ),
                rusqlite::params![req.parent_account_id.to_string(), req.idempotency_key],
                crate::reservation::read_reservation_row,
            )
            .optional()?;
        if let Some(row) = existing {
            tx.commit()?;
            return Ok(Ok(GrantSubleaseOutcome::AlreadyGranted(Box::new(
                crate::reservation::row_to_reservation(row)?,
            ))));
        }

        let Some(parent) = Self::get_account_tx(&tx, req.parent_account_id)? else {
            tx.commit()?;
            return Ok(Err(AccountError::ParentNotFound));
        };
        if parent.state != AccountState::Open {
            tx.commit()?;
            return Ok(Err(AccountError::ParentNotOpen));
        }
        if req.amount.kind() != parent.resource_kind {
            tx.commit()?;
            return Ok(Err(AccountError::ResourceKindMismatch {
                expected: parent.resource_kind,
                actual: req.amount.kind(),
            }));
        }

        // TTL clamp: a child lease may never outlive the lease that funds
        // its own parent (root task accounts have no funding lease, so
        // there is nothing to clamp against).
        let parent_funding_lease_id: Option<String> = tx.query_row(
            "SELECT funding_lease_id FROM resource_accounts WHERE account_id = ?1",
            [req.parent_account_id.to_string()],
            |row| row.get(0),
        )?;
        let requested_expires_at = req.now + time::Duration::seconds(req.ttl_secs as i64);
        if let Some(funding_id) = parent_funding_lease_id {
            let funding_expires_at: String = tx.query_row(
                "SELECT expires_at FROM reservations WHERE id = ?1",
                [funding_id],
                |row| row.get(0),
            )?;
            let funding_expires_at = parse_time(&funding_expires_at)?;
            if requested_expires_at > funding_expires_at {
                tx.commit()?;
                return Ok(Err(AccountError::ChildExpiryExceedsFunding));
            }
        }

        let (active, settled) = {
            let active: f64 = tx.query_row(
                "SELECT COALESCE(SUM(amount), 0.0) FROM reservations
                 WHERE account_id = ?1 AND state = 'active'",
                [req.parent_account_id.to_string()],
                |row| row.get(0),
            )?;
            let settled: f64 = tx.query_row(
                "SELECT COALESCE(SUM(settled_amount), 0.0) FROM reservations
                 WHERE account_id = ?1 AND state = 'settled'",
                [req.parent_account_id.to_string()],
                |row| row.get(0),
            )?;
            (active, settled)
        };
        let capacity_row: (Option<f64>, Option<f64>) = tx.query_row(
            "SELECT granted_capacity, protected_reserve FROM v_account_capacity WHERE account_id = ?1",
            [req.parent_account_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let (granted_capacity, protected_reserve) = capacity_row;
        let granted_capacity = granted_capacity.unwrap_or(f64::MAX);
        let protected_reserve = protected_reserve.unwrap_or(0.0);
        let available = granted_capacity - active - settled - protected_reserve;
        let requested = req.amount.as_f64();
        if requested > available {
            tx.commit()?;
            return Ok(Ok(GrantSubleaseOutcome::Insufficient {
                available: Headroom {
                    kind: parent.resource_kind,
                    value: available,
                },
                requested: req.amount,
            }));
        }

        let Some(task_id) = parent.task_id else {
            tx.commit()?;
            return Ok(Err(AccountError::ParentHasNoTaskLineage));
        };

        let id = ReservationId::new();
        tx.execute(
            "INSERT INTO reservations (
                id, task_id, session_id, plan_id, class, resource_kind, amount,
                drawn_from_reserve, state, settled_amount, usage_known, idempotency_key,
                created_at, expires_at, settled_at, released_at,
                account_id, grants_account_id, lease_kind, settled_after_expiry, legacy_pre_0011
             ) VALUES (?1, ?2, ?3, NULL, 'optional_work', ?4, ?5, 0, 'active', NULL, NULL, ?6,
                       ?7, ?8, NULL, NULL, ?9, ?10, 'subaccount_funding', 0, 0)",
            rusqlite::params![
                id.0.to_string(),
                task_id.to_string(),
                req.child_natural_key,
                kind_to_str(parent.resource_kind),
                requested,
                req.idempotency_key,
                rfc3339(req.now)?,
                rfc3339(requested_expires_at)?,
                req.parent_account_id.to_string(),
                req.child_account_id.to_string(),
            ],
        )?;
        tx.execute(
            "UPDATE resource_accounts SET granted_capacity = ?1, funding_lease_id = ?2,
                                           updated_at = ?3
             WHERE account_id = ?4",
            rusqlite::params![
                requested,
                id.0.to_string(),
                rfc3339(req.now)?,
                req.child_account_id.to_string(),
            ],
        )?;
        tx.commit()?;

        Ok(Ok(GrantSubleaseOutcome::Granted(Box::new(Reservation {
            id,
            task_id,
            session_id: req.child_natural_key.to_string(),
            plan_id: None,
            class: libra_governor_domain::ReservationClass::OptionalWork,
            amount: req.amount,
            drawn_from_reserve: libra_governor_domain::ResourceAmount::from_kind_f64(
                parent.resource_kind,
                0.0,
            ),
            state: libra_governor_domain::ReservationState::Active,
            settled_amount: None,
            usage_known: None,
            idempotency_key: req.idempotency_key.to_string(),
            created_at: req.now,
            expires_at: requested_expires_at,
            settled_at: None,
            released_at: None,
            account_id: req.parent_account_id,
            grants_account: Some(req.child_account_id),
            lease_kind: LeaseKind::SubaccountFunding,
            settled_after_expiry: false,
            legacy_pre_0011: false,
        }))))
    }

    fn get_account_tx(
        tx: &rusqlite::Transaction<'_>,
        account_id: AccountId,
    ) -> Result<Option<ResourceAccount>, LedgerError> {
        let row: Option<AccountRow> = tx
            .query_row(
                "SELECT account_id, level, parent_account_id, resource_kind, natural_key,
                        task_id, granted_capacity, protected_reserve, authority_source,
                        enforcement_scope, provider_lineage_status, execution_dimension_key_json,
                        provenance, account_schema_version, state, created_at, updated_at, closed_at
                 FROM resource_accounts WHERE account_id = ?1",
                [account_id.to_string()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                        row.get(8)?,
                        row.get(9)?,
                        row.get(10)?,
                        row.get(11)?,
                        row.get(12)?,
                        row.get(13)?,
                        row.get(14)?,
                        row.get(15)?,
                        row.get(16)?,
                        row.get(17)?,
                    ))
                },
            )
            .optional()?;
        row.map(row_to_account).transpose()
    }

    /// Reads an account's row, if it exists.
    pub fn account(&self, account_id: AccountId) -> Result<Option<ResourceAccount>, LedgerError> {
        let row: Option<AccountRow> = self
            .conn
            .query_row(
                "SELECT account_id, level, parent_account_id, resource_kind, natural_key,
                        task_id, granted_capacity, protected_reserve, authority_source,
                        enforcement_scope, provider_lineage_status, execution_dimension_key_json,
                        provenance, account_schema_version, state, created_at, updated_at, closed_at
                 FROM resource_accounts WHERE account_id = ?1",
                [account_id.to_string()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                        row.get(8)?,
                        row.get(9)?,
                        row.get(10)?,
                        row.get(11)?,
                        row.get(12)?,
                        row.get(13)?,
                        row.get(14)?,
                        row.get(15)?,
                        row.get(16)?,
                        row.get(17)?,
                    ))
                },
            )
            .optional()?;
        row.map(row_to_account).transpose()
    }

    /// Reads `account_id`'s spend-so-far (HORO-1669): `Exclusive` sums
    /// only `work_hold` leases held directly against this account;
    /// `Inclusive` adds every proven descendant's exclusive spend via a
    /// recursive CTE over `resource_accounts.parent_account_id`.
    ///
    /// # Read-path/write-path asymmetry, deliberate
    ///
    /// Migration 0011's module docs say "never a recursive CTE on the
    /// *write* path" — this is a read-only query off the hot path, so
    /// that rule is not violated.
    ///
    /// # Not `economic_rollup::inclusive_spend`
    ///
    /// That function sums the provider-proven agent-lineage forest (a
    /// different tree — see ADR-0008: custody lineage is Libra-minted,
    /// proven by the act of leasing, and may legitimately disagree with
    /// provider lineage). This function sums the custody tree instead.
    /// The two are cross-referenced, never unified.
    ///
    /// `subaccount_funding` leases are always excluded from both scopes:
    /// that capacity belongs to the child it funds, not to the parent
    /// that handed it down — counting it here would double-count with
    /// the child's own exclusive spend.
    pub fn account_spend(
        &self,
        account_id: AccountId,
        scope: SpendScope,
    ) -> Result<SpendSoFar, LedgerError> {
        let Some(account) = self.account(account_id)? else {
            return Ok(SpendSoFar::NoBasis {
                reason: NoSpendBasis::NoAccount,
            });
        };

        let (settled, active_holds, account_count): (f64, f64, u32) = match scope {
            SpendScope::Exclusive => {
                let settled: f64 = self.conn.query_row(
                    "SELECT COALESCE(SUM(settled_amount), 0.0) FROM reservations
                     WHERE account_id = ?1 AND lease_kind = 'work_hold' AND state = 'settled'",
                    [account_id.to_string()],
                    |row| row.get(0),
                )?;
                let active: f64 = self.conn.query_row(
                    "SELECT COALESCE(SUM(amount), 0.0) FROM reservations
                     WHERE account_id = ?1 AND lease_kind = 'work_hold' AND state = 'active'",
                    [account_id.to_string()],
                    |row| row.get(0),
                )?;
                (settled, active, 1)
            }
            SpendScope::Inclusive => {
                let settled: f64 = self.conn.query_row(
                    "WITH RECURSIVE subtree(account_id) AS (
                        SELECT account_id FROM resource_accounts WHERE account_id = ?1
                        UNION ALL
                        SELECT a.account_id FROM resource_accounts a
                        JOIN subtree s ON a.parent_account_id = s.account_id
                     )
                     SELECT COALESCE(SUM(r.settled_amount), 0.0) FROM reservations r
                     JOIN subtree s ON r.account_id = s.account_id
                     WHERE r.lease_kind = 'work_hold' AND r.state = 'settled'",
                    [account_id.to_string()],
                    |row| row.get(0),
                )?;
                let active: f64 = self.conn.query_row(
                    "WITH RECURSIVE subtree(account_id) AS (
                        SELECT account_id FROM resource_accounts WHERE account_id = ?1
                        UNION ALL
                        SELECT a.account_id FROM resource_accounts a
                        JOIN subtree s ON a.parent_account_id = s.account_id
                     )
                     SELECT COALESCE(SUM(r.amount), 0.0) FROM reservations r
                     JOIN subtree s ON r.account_id = s.account_id
                     WHERE r.lease_kind = 'work_hold' AND r.state = 'active'",
                    [account_id.to_string()],
                    |row| row.get(0),
                )?;
                let count: u32 = self.conn.query_row(
                    "WITH RECURSIVE subtree(account_id) AS (
                        SELECT account_id FROM resource_accounts WHERE account_id = ?1
                        UNION ALL
                        SELECT a.account_id FROM resource_accounts a
                        JOIN subtree s ON a.parent_account_id = s.account_id
                     )
                     SELECT COUNT(*) FROM subtree",
                    [account_id.to_string()],
                    |row| row.get(0),
                )?;
                (settled, active, count)
            }
        };

        Ok(SpendSoFar::Known {
            kind: account.resource_kind,
            settled,
            active_holds,
            scope,
            account_count,
        })
    }

    /// Persists a [`Shadow`]-wrapped runtime decision to
    /// `shadow_runtime_decisions` (HORO-1669 Stage B) — the comparison
    /// substrate a later evidence-gate ticket (HORO-1673) reads.
    /// Purely additive: this is the only write this module makes on the
    /// shadow-recording path, and it touches no other table. The
    /// decision itself is never read back as a `RuntimeDecision` here —
    /// only `Shadow::summary()`'s non-addressable fields are used to
    /// populate the indexable columns, consistent with the type's own
    /// no-accessor guarantee.
    #[allow(clippy::too_many_arguments)]
    pub fn record_shadow_decision(
        &self,
        task_id: libra_governor_domain::TaskId,
        plan_id: libra_governor_domain::PlanId,
        session_id: &str,
        shadow: &libra_governor_domain::Shadow<libra_governor_domain::RuntimeDecision>,
        elapsed_secs: u64,
        tool_calls_total: u64,
        decided_at: OffsetDateTime,
    ) -> Result<(), LedgerError> {
        let summary = shadow.summary();
        let decision_json = serde_json::to_string(shadow).map_err(|e| {
            LedgerError::Sqlite(rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
        })?;
        self.conn.execute(
            "INSERT INTO shadow_runtime_decisions (
                decision_id, task_id, plan_id, session_id, proposal_kind,
                decision_json, decision_schema_version, elapsed_secs,
                tool_calls_total, decided_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT (task_id, plan_id, decided_at) DO NOTHING",
            rusqlite::params![
                uuid::Uuid::new_v4().to_string(),
                task_id.0.to_string(),
                plan_id.0.to_string(),
                session_id,
                summary.proposal_kind,
                decision_json,
                libra_governor_domain::RUNTIME_DECISION_SCHEMA_VERSION,
                elapsed_secs,
                tool_calls_total,
                rfc3339(decided_at)?,
            ],
        )?;
        Ok(())
    }

    /// The most recent `decided_at` recorded for this task in
    /// `shadow_runtime_decisions`, or `None` if no shadow decision has
    /// ever been recorded for it. The daemon's cadence gate uses this
    /// directly instead of tracking a separate "last progressive run"
    /// timestamp in its own state — the shadow table is already the
    /// authoritative record of when a progressive computation last ran,
    /// so a second copy of that fact would be exactly the kind of
    /// duplicated bookkeeping this campaign avoids elsewhere.
    pub fn last_shadow_decision_at(
        &self,
        task_id: libra_governor_domain::TaskId,
    ) -> Result<Option<OffsetDateTime>, LedgerError> {
        // A bare aggregate with no GROUP BY always returns exactly one
        // row (NULL when nothing matches) — no `.optional()` needed.
        let row: Option<String> = self.conn.query_row(
            "SELECT MAX(decided_at) FROM shadow_runtime_decisions WHERE task_id = ?1",
            [task_id.0.to_string()],
            |row| row.get(0),
        )?;
        row.map(|s| parse_time(&s)).transpose()
    }

    /// `(active_lease_count, child_account_count)` over `account_id`'s
    /// subtree — audit/evidence metadata for [`ProgressEvidence`], not
    /// used by the remaining-estimate arithmetic itself (that only reads
    /// [`Self::account_spend`]). `child_account_count` excludes the
    /// account itself. `(0, 0)` when the account does not exist.
    pub fn subtree_counts(&self, account_id: AccountId) -> Result<(u32, u32), LedgerError> {
        if self.account(account_id)?.is_none() {
            return Ok((0, 0));
        }
        let active_leases: u32 = self.conn.query_row(
            "WITH RECURSIVE subtree(account_id) AS (
                SELECT account_id FROM resource_accounts WHERE account_id = ?1
                UNION ALL
                SELECT a.account_id FROM resource_accounts a
                JOIN subtree s ON a.parent_account_id = s.account_id
             )
             SELECT COUNT(*) FROM reservations r
             JOIN subtree s ON r.account_id = s.account_id
             WHERE r.state = 'active'",
            [account_id.to_string()],
            |row| row.get(0),
        )?;
        let child_accounts: u32 = self.conn.query_row(
            "SELECT COUNT(*) FROM resource_accounts WHERE parent_account_id = ?1",
            [account_id.to_string()],
            |row| row.get(0),
        )?;
        Ok((active_leases, child_accounts))
    }

    /// Reads an account's capacity through `v_account_capacity` — the
    /// one place the task-vs-other-level case analysis lives. `Ok(None)`
    /// when the account itself does not exist.
    pub fn account_capacity(
        &self,
        account_id: AccountId,
    ) -> Result<Option<AccountCapacity>, LedgerError> {
        let row: Option<(String, Option<f64>, Option<f64>)> = self
            .conn
            .query_row(
                "SELECT resource_kind, granted_capacity, protected_reserve
                 FROM v_account_capacity WHERE account_id = ?1",
                [account_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let Some((kind_str, granted, reserve)) = row else {
            return Ok(None);
        };
        let kind = kind_from_str(&kind_str)?;
        Ok(Some(AccountCapacity {
            account_id,
            resource_kind: kind,
            granted_capacity: granted
                .map(|v| libra_governor_domain::ResourceAmount::from_kind_f64(kind, v)),
            protected_reserve: reserve
                .map(|v| libra_governor_domain::ResourceAmount::from_kind_f64(kind, v)),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libra_governor_domain::{
        AutonomyBoundary, CompletionContract, CompletionCriterion, CompletionReserveEstimate,
        Confidence, ConstraintMode, Policy, ResourceAmount, ResourceBound, TaskId, TaskIdentity,
        TimeBound,
    };

    fn now() -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH
    }

    fn thousand_token_policy() -> Policy {
        Policy::validated(
            "test",
            ResourceBound {
                mode: ConstraintMode::Hard,
                target: ResourceAmount::Tokens(1000),
                elastic_ceiling: None,
                hard_ceiling: ResourceAmount::Tokens(1000),
            },
            TimeBound {
                mode: ConstraintMode::Hard,
                target_secs: 600,
                elastic_ceiling_secs: None,
                hard_ceiling_secs: Some(600),
                deadline: None,
            },
            CompletionContract::first(vec![CompletionCriterion::required("tests pass")]),
            Confidence::Low,
            AutonomyBoundary::AskOnApproval,
        )
        .unwrap()
    }

    fn setup(store: &mut LedgerStore, reserve_amount: u64) -> TaskId {
        let task_id = TaskId::new();
        store
            .insert_task(
                &TaskIdentity {
                    id: task_id,
                    external_ref: None,
                },
                now(),
            )
            .unwrap();
        let policy = thousand_token_policy();
        let reserve = CompletionReserveEstimate {
            amount: ResourceAmount::Tokens(reserve_amount),
            basis: libra_governor_domain::CompletionReserveBasis::PolicyTarget,
            fraction: reserve_amount as f64 / 1000.0,
            required_criteria_count: 1,
        };
        store
            .initialize_task_budget(task_id, &policy, &reserve, now())
            .unwrap();
        task_id
    }

    #[test]
    fn ensure_task_account_is_idempotent_and_deterministic() {
        let mut store = LedgerStore::open_in_memory().unwrap();
        // `initialize_task_budget` (inside `setup`) already provisions the
        // task-level account in the same transaction as the budget row —
        // so by the time this test calls `ensure_task_account` itself,
        // the row already exists. That is the intended behavior (see
        // `initialize_task_budget` docs): this test instead confirms
        // `ensure_task_account` is a safe, idempotent no-op against it.
        let task_id = setup(&mut store, 200);

        let first = store
            .ensure_task_account(task_id, ResourceKind::Tokens, now())
            .unwrap();
        assert!(!first.created);
        assert_eq!(first.account.account_id, AccountId::for_task(task_id));

        let second = store
            .ensure_task_account(task_id, ResourceKind::Tokens, now())
            .unwrap();
        assert!(!second.created);
        assert_eq!(second.account, first.account);
    }

    #[test]
    fn task_level_account_rows_never_carry_capacity_or_reserve() {
        let mut store = LedgerStore::open_in_memory().unwrap();
        let task_id = setup(&mut store, 200);
        store
            .ensure_task_account(task_id, ResourceKind::Tokens, now())
            .unwrap();

        // The CHECK constraint refuses an attempt to smuggle capacity
        // onto a task-level row.
        let result = store.conn.execute(
            "UPDATE resource_accounts SET granted_capacity = 100 WHERE account_id = ?1",
            [AccountId::for_task(task_id).to_string()],
        );
        assert!(result.is_err(), "CHECK constraint must reject this write");
    }

    #[test]
    fn account_capacity_reads_task_level_numbers_from_task_budgets() {
        let mut store = LedgerStore::open_in_memory().unwrap();
        let task_id = setup(&mut store, 200);
        store
            .ensure_task_account(task_id, ResourceKind::Tokens, now())
            .unwrap();

        let capacity = store
            .account_capacity(AccountId::for_task(task_id))
            .unwrap()
            .expect("capacity row must exist via the view");
        assert_eq!(
            capacity.granted_capacity,
            Some(ResourceAmount::Tokens(1000))
        );
        assert_eq!(
            capacity.protected_reserve,
            Some(ResourceAmount::Tokens(200))
        );
    }

    /// Inserts a settled `work_hold` lease directly against `account_id`.
    /// There is no public reserve-against-an-arbitrary-account API today
    /// (only `reserve`, which always targets the task-level account) — a
    /// future ticket may add one; until then this mirrors how the
    /// existing `task_level_account_rows_never_carry_capacity_or_reserve`
    /// test already reaches into `store.conn` directly to set up a
    /// scenario the public API alone cannot construct.
    fn insert_settled_work_hold(
        store: &LedgerStore,
        task_id: TaskId,
        account_id: AccountId,
        settled_amount: f64,
    ) {
        store
            .conn
            .execute(
                "INSERT INTO reservations (
                    id, task_id, session_id, plan_id, class, resource_kind, amount,
                    drawn_from_reserve, state, settled_amount, usage_known, idempotency_key,
                    created_at, expires_at, settled_at, released_at,
                    account_id, grants_account_id, lease_kind, settled_after_expiry, legacy_pre_0011
                 ) VALUES (?1, ?2, 'test-session', NULL, 'optional_work', 'tokens', ?3,
                           0, 'settled', ?3, 1, ?4, ?5, ?5, ?5, NULL, ?6, NULL, 'work_hold', 0, 0)",
                rusqlite::params![
                    ReservationId::new().0.to_string(),
                    task_id.to_string(),
                    settled_amount,
                    format!("wh-{}", uuid::Uuid::new_v4()),
                    rfc3339(now()).unwrap(),
                    account_id.to_string(),
                ],
            )
            .unwrap();
    }

    #[test]
    fn account_spend_inclusive_reflects_a_grandchild_settlement_exclusive_does_not() {
        let mut store = LedgerStore::open_in_memory().unwrap();
        let task_id = setup(&mut store, 200);
        let task_account = AccountId::for_task(task_id);
        store
            .ensure_task_account(task_id, ResourceKind::Tokens, now())
            .unwrap();

        let session = store
            .ensure_child_account(task_account, AccountLevel::Session, "sess-1", now())
            .unwrap()
            .unwrap()
            .account;
        store
            .grant_sublease(GrantSubleaseRequest {
                parent_account_id: task_account,
                child_account_id: session.account_id,
                child_natural_key: "sess-1",
                amount: ResourceAmount::Tokens(500),
                idempotency_key: "fund-session",
                now: now(),
                ttl_secs: 900,
            })
            .unwrap()
            .unwrap();

        let agent = store
            .ensure_child_account(session.account_id, AccountLevel::Agent, "agent-1", now())
            .unwrap()
            .unwrap()
            .account;
        store
            .grant_sublease(GrantSubleaseRequest {
                parent_account_id: session.account_id,
                child_account_id: agent.account_id,
                child_natural_key: "agent-1",
                amount: ResourceAmount::Tokens(200),
                idempotency_key: "fund-agent",
                now: now(),
                ttl_secs: 900,
            })
            .unwrap()
            .unwrap();

        // Settle real work directly at the grandchild (agent) account.
        insert_settled_work_hold(&store, task_id, agent.account_id, 150.0);

        // Match only on the variant discriminant for the panic message —
        // `SpendSoFar`/`account_spend`'s row read flows through
        // `reservations.idempotency_key`, which trips CodeQL's generic
        // sensitive-field-name heuristic if the whole value is
        // `{:?}`-formatted, even though no field here is actually
        // sensitive (same pattern as HORO-1668's PR #57 fix).
        fn spend_kind(spend: &SpendSoFar) -> &'static str {
            match spend {
                SpendSoFar::NoBasis { .. } => "NoBasis",
                SpendSoFar::Known { .. } => "Known",
            }
        }

        let task_inclusive = store
            .account_spend(task_account, SpendScope::Inclusive)
            .unwrap();
        match task_inclusive {
            SpendSoFar::Known {
                settled,
                account_count,
                ..
            } => {
                assert_eq!(settled, 150.0, "grandchild's settled work must roll up");
                assert_eq!(account_count, 3, "task + session + agent");
            }
            other => panic!("expected Known, got {}", spend_kind(&other)),
        }

        let task_exclusive = store
            .account_spend(task_account, SpendScope::Exclusive)
            .unwrap();
        match task_exclusive {
            SpendSoFar::Known { settled, .. } => {
                assert_eq!(
                    settled, 0.0,
                    "exclusive spend at the task account must not see the grandchild's spend"
                );
            }
            other => panic!("expected Known, got {}", spend_kind(&other)),
        }

        // The funding leases themselves (subaccount_funding) must never
        // be double-counted as spend.
        let session_inclusive = store
            .account_spend(session.account_id, SpendScope::Inclusive)
            .unwrap();
        match session_inclusive {
            SpendSoFar::Known { settled, .. } => {
                assert_eq!(
                    settled, 150.0,
                    "funding leases must not inflate the session's inclusive spend"
                );
            }
            other => panic!("expected Known, got {}", spend_kind(&other)),
        }
    }

    #[test]
    fn account_spend_reports_no_basis_when_no_account_exists() {
        let store = LedgerStore::open_in_memory().unwrap();
        let spend = store
            .account_spend(AccountId::new(), SpendScope::Exclusive)
            .unwrap();
        assert_eq!(
            spend,
            SpendSoFar::NoBasis {
                reason: NoSpendBasis::NoAccount
            }
        );
    }
}
