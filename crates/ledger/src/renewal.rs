//! `task_budget_renewals` (HORO-1727): [`LedgerStore::grant_renewal`], the
//! one write path for the bounded renewal mechanism.
//!
//! # This method has no production caller
//!
//! Nothing in `daemon/src`, `cli/src`, or `gateway/src` may call
//! [`LedgerStore::grant_renewal`] — the `renewal_not_wired_live` test in
//! this crate's `tests/` directory fails the build if any of them ever
//! references it. This ticket ships the mechanism and the read sites that
//! must respect its effect; a CLI command, protocol request, or approval
//! flow that could actually authorize a grant in production is a future
//! ticket's explicit, reviewed decision — see
//! `libra_governor_domain::renewal`'s module docs for why.
//!
//! # Atomicity and idempotency
//!
//! Exactly [`crate::reservation`]'s discipline: one `BEGIN IMMEDIATE`
//! transaction, idempotent replay on `(task_id, idempotency_key)` checked
//! before any gate is evaluated.
//!
//! # Every gate reads the task's *persisted* policy
//!
//! All five refusal gates read `task_budgets.policy_json` — the policy
//! snapshotted at admission (`read_task_budget_tx`) — never the daemon's
//! live `config.policy`. Policy can change between task admission and a
//! renewal request; the bound a grant is evaluated against is the one the
//! task was actually admitted under, matching how `reserve`/`available`
//! already treat the persisted policy as authoritative over daemon
//! configuration.

use libra_governor_domain::{
    BlockingStatus, RenewalAuthority, RenewalId, RenewalRefusal, RenewalRequest, TaskBudgetRenewal,
    TaskId, TASK_BUDGET_RENEWAL_SCHEMA_VERSION,
};
use rusqlite::{OptionalExtension, TransactionBehavior};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::{
    reservation::read_task_budget_tx, store::LedgerStore, transaction::begin, LedgerError,
};

fn rfc3339(t: OffsetDateTime) -> Result<String, LedgerError> {
    t.format(&Rfc3339)
        .map_err(|e| LedgerError::Sqlite(rusqlite::Error::ToSqlConversionFailure(Box::new(e))))
}

fn parse_time(s: &str) -> Result<OffsetDateTime, LedgerError> {
    OffsetDateTime::parse(s, &Rfc3339)
        .map_err(|_| LedgerError::Sqlite(rusqlite::Error::InvalidQuery))
}

fn invalid() -> LedgerError {
    LedgerError::Sqlite(rusqlite::Error::InvalidQuery)
}

fn to_json<T: serde::Serialize>(value: &T) -> Result<String, LedgerError> {
    serde_json::to_string(value)
        .map_err(|e| LedgerError::Sqlite(rusqlite::Error::ToSqlConversionFailure(Box::new(e))))
}

fn from_json<T: serde::de::DeserializeOwned>(s: &str) -> Result<T, LedgerError> {
    serde_json::from_str(s).map_err(|_| invalid())
}

/// Raw `task_budget_renewals` row shape.
#[allow(clippy::type_complexity)]
type RenewalRow = (
    String, // id
    String, // task_id
    f64,    // amount
    String, // authority_json
    u32,    // contract_revision
    String, // reason
    String, // idempotency_key
    f64,    // settled_at_grant
    f64,    // active_at_grant
    f64,    // effective_before
    f64,    // effective_after
    String, // quota_status_json
    String, // schema_version
    String, // granted_at
);

const RENEWAL_COLUMNS: &str =
    "id, task_id, amount, authority_json, contract_revision, reason, idempotency_key,
     settled_at_grant, active_at_grant, effective_before, effective_after, quota_status_json,
     schema_version, granted_at";

fn read_renewal_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RenewalRow> {
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
    ))
}

fn row_to_renewal(
    row: RenewalRow,
    resource_kind: libra_governor_domain::ResourceKind,
) -> Result<TaskBudgetRenewal, LedgerError> {
    use libra_governor_domain::ResourceAmount;
    let (
        id,
        task_id,
        amount,
        authority_json,
        contract_revision,
        reason,
        idempotency_key,
        settled_at_grant,
        active_at_grant,
        effective_before,
        effective_after,
        quota_status_json,
        schema_version,
        granted_at,
    ) = row;
    Ok(TaskBudgetRenewal {
        id: RenewalId(uuid::Uuid::parse_str(&id).map_err(|_| invalid())?),
        task_id: TaskId(uuid::Uuid::parse_str(&task_id).map_err(|_| invalid())?),
        amount: ResourceAmount::from_kind_f64(resource_kind, amount),
        authority: from_json::<RenewalAuthority>(&authority_json)?,
        contract_revision,
        reason,
        idempotency_key,
        settled_at_grant: ResourceAmount::from_kind_f64(resource_kind, settled_at_grant),
        active_at_grant: ResourceAmount::from_kind_f64(resource_kind, active_at_grant),
        effective_before: ResourceAmount::from_kind_f64(resource_kind, effective_before),
        effective_after: ResourceAmount::from_kind_f64(resource_kind, effective_after),
        quota_status_at_grant: from_json::<BlockingStatus>(&quota_status_json)?,
        schema_version,
        granted_at: parse_time(&granted_at)?,
    })
}

/// The outcome of [`LedgerStore::grant_renewal`].
#[derive(Debug, Clone, PartialEq)]
pub enum GrantRenewalOutcome {
    Granted(Box<TaskBudgetRenewal>),
    /// Idempotent replay: `(task_id, idempotency_key)` was already
    /// granted. Nothing was written; the existing row is returned.
    AlreadyGranted(Box<TaskBudgetRenewal>),
    /// One of [`RenewalRefusal`]'s gates failed. Nothing was written.
    Refused(RenewalRefusal),
    /// No `task_budgets` row exists for this task.
    NoBudget,
}

impl LedgerStore {
    /// Evaluates and, if every gate passes, persists one [`RenewalRequest`]
    /// against `task_id`'s [`libra_governor_domain::policy::RenewalBound`]
    /// (HORO-1727). `blocking` is the upstream quota
    /// [`BlockingStatus`] the caller has already evaluated (this method
    /// does not itself consult `quota_window` — see module docs on why:
    /// the ledger stays the single place capacity is accounted, not a
    /// second place quota evaluation logic is duplicated).
    ///
    /// See module docs for why nothing in production may call this today.
    pub fn grant_renewal(
        &mut self,
        task_id: TaskId,
        request: RenewalRequest,
        blocking: BlockingStatus,
        now: OffsetDateTime,
    ) -> Result<GrantRenewalOutcome, LedgerError> {
        let tx = begin(&mut self.conn, TransactionBehavior::Immediate)?;

        let existing: Option<RenewalRow> = tx
            .query_row(
                &format!(
                    "SELECT {RENEWAL_COLUMNS}
                     FROM task_budget_renewals WHERE task_id = ?1 AND idempotency_key = ?2"
                ),
                rusqlite::params![task_id.to_string(), request.idempotency_key],
                read_renewal_row,
            )
            .optional()?;

        let Some(budget) = read_task_budget_tx(&tx, task_id)? else {
            tx.commit()?;
            return Ok(GrantRenewalOutcome::NoBudget);
        };

        if let Some(row) = existing {
            tx.commit()?;
            return Ok(GrantRenewalOutcome::AlreadyGranted(Box::new(
                row_to_renewal(row, budget.resource_kind)?,
            )));
        }

        // Gate: resource kind must match the task's own envelope — a
        // mismatched kind makes every arithmetic comparison below
        // meaningless, so it is refused before any of them run.
        if request.amount.kind() != budget.resource_kind {
            tx.commit()?;
            return Ok(GrantRenewalOutcome::Refused(
                RenewalRefusal::ResourceKindMismatch {
                    expected: budget.resource_kind,
                    actual: request.amount.kind(),
                },
            ));
        }

        // Gate: renewals must be enabled, and this task must not already
        // be at its lifetime renewal count.
        let Some(renewal_bound) = budget.policy.renewal.clone() else {
            tx.commit()?;
            return Ok(GrantRenewalOutcome::Refused(
                RenewalRefusal::RenewalsDisabled,
            ));
        };
        let current_renewals: u32 = tx.query_row(
            "SELECT COUNT(*) FROM task_budget_renewals WHERE task_id = ?1",
            [task_id.to_string()],
            |row| row.get(0),
        )?;
        if current_renewals >= renewal_bound.max_renewals {
            tx.commit()?;
            return Ok(GrantRenewalOutcome::Refused(
                RenewalRefusal::MaxRenewalsExceeded {
                    current: current_renewals,
                    max: renewal_bound.max_renewals,
                },
            ));
        }

        // Gate: a single grant may add at most one allocation's worth.
        let hard_ceiling = budget.policy.resource.hard_ceiling;
        if request.amount.as_f64() > hard_ceiling.as_f64() {
            tx.commit()?;
            return Ok(GrantRenewalOutcome::Refused(
                RenewalRefusal::GrantExceedsHardCeiling {
                    amount: request.amount,
                    hard_ceiling,
                },
            ));
        }

        // Gate: the lifetime ceiling.
        let effective_before = budget.effective_hard_limit();
        let effective_after = libra_governor_domain::ResourceAmount::from_kind_f64(
            budget.resource_kind,
            effective_before.as_f64() + request.amount.as_f64(),
        );
        if effective_after.as_f64() > renewal_bound.lifetime_ceiling.as_f64() {
            tx.commit()?;
            return Ok(GrantRenewalOutcome::Refused(
                RenewalRefusal::LifetimeCeilingExceeded {
                    effective_after,
                    lifetime_ceiling: renewal_bound.lifetime_ceiling,
                },
            ));
        }

        // Gate: upstream quota must not be blocking or indeterminate.
        // `Indeterminate` is refused rather than treated as safe — see
        // `RenewalRefusal::UpstreamQuotaBlocking`'s own docs for why this
        // is a deliberate conservative choice ADR-0015 leaves room to
        // revisit, not an oversight.
        if !matches!(blocking, BlockingStatus::NotBlocking) {
            tx.commit()?;
            return Ok(GrantRenewalOutcome::Refused(
                RenewalRefusal::UpstreamQuotaBlocking { status: blocking },
            ));
        }

        // Gate: the request must be authorized against the task's
        // *current* contract revision — a request carrying a stale
        // revision must not silently grant against whatever revision is
        // active now (HORO-1727 Decision 2). A task with no recorded
        // contract yet (`current_revision: None`) has nothing to compare
        // against, so this gate is skipped rather than refusing on an
        // absence it cannot interpret.
        let current_revision = Self::current_contract_revision_tx(&tx, task_id)?;
        if let Some(current) = current_revision {
            if request.contract_revision != current {
                tx.commit()?;
                return Ok(GrantRenewalOutcome::Refused(
                    RenewalRefusal::ContractRevisionMismatch {
                        requested: request.contract_revision,
                        current,
                    },
                ));
            }
        }

        // Gate: no authoritative Completed attestation may already exist
        // for the current revision, and disagreeing authoritative
        // terminal claims at that revision must surface as a conflict
        // rather than resolve silently (HORO-1727 Decision 2).
        // `contract_revision IS NULL` attestations are unbound and are
        // matched here only when the task itself has no current revision
        // either — otherwise they are handled by the dedicated
        // `UnboundLegacyCompletion` gate below.
        let scoped_kinds = Self::authoritative_terminal_kinds_tx(&tx, task_id, current_revision)?;
        if scoped_kinds.len() > 1 {
            tx.commit()?;
            return Ok(GrantRenewalOutcome::Refused(
                RenewalRefusal::ConflictingCompletionOutcomes,
            ));
        }
        if scoped_kinds.iter().any(|kind| kind == "completed") {
            tx.commit()?;
            return Ok(GrantRenewalOutcome::Refused(
                RenewalRefusal::TaskAlreadyCompleted,
            ));
        }

        // Gate: an unbound (legacy, `contract_revision IS NULL`)
        // authoritative Completed attestation cannot be proven to apply —
        // or not to apply — to the task's current revision once one
        // exists, so it is refused conservatively rather than trusted or
        // ignored.
        if current_revision.is_some() {
            let legacy_completed: u32 = tx.query_row(
                "SELECT COUNT(*) FROM outcome_attestations
                 WHERE task_id = ?1 AND contract_revision IS NULL
                   AND authoritative = 1 AND outcome_kind = 'completed'",
                [task_id.to_string()],
                |row| row.get(0),
            )?;
            if legacy_completed > 0 {
                tx.commit()?;
                return Ok(GrantRenewalOutcome::Refused(
                    RenewalRefusal::UnboundLegacyCompletion,
                ));
            }
        }

        // All gates passed — read the audit snapshot and persist.
        let (settled, active) = Self::settled_and_active_for_renewal(&tx, task_id)?;
        let id = RenewalId::new();
        let authority_json = to_json(&request.authority)?;
        let quota_status_json = to_json(&blocking)?;
        let granted_at = rfc3339(now)?;
        tx.execute(
            "INSERT INTO task_budget_renewals (
                id, task_id, amount, authority_json, contract_revision, reason,
                idempotency_key, settled_at_grant, active_at_grant, effective_before,
                effective_after, quota_status_json, schema_version, granted_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            rusqlite::params![
                id.0.to_string(),
                task_id.to_string(),
                request.amount.as_f64(),
                authority_json,
                request.contract_revision,
                request.reason,
                request.idempotency_key,
                settled,
                active,
                effective_before.as_f64(),
                effective_after.as_f64(),
                quota_status_json,
                TASK_BUDGET_RENEWAL_SCHEMA_VERSION,
                granted_at,
            ],
        )?;
        tx.commit()?;

        Ok(GrantRenewalOutcome::Granted(Box::new(TaskBudgetRenewal {
            id,
            task_id,
            amount: request.amount,
            authority: request.authority,
            contract_revision: request.contract_revision,
            reason: request.reason,
            idempotency_key: request.idempotency_key,
            settled_at_grant: libra_governor_domain::ResourceAmount::from_kind_f64(
                budget.resource_kind,
                settled,
            ),
            active_at_grant: libra_governor_domain::ResourceAmount::from_kind_f64(
                budget.resource_kind,
                active,
            ),
            effective_before,
            effective_after,
            quota_status_at_grant: blocking,
            schema_version: TASK_BUDGET_RENEWAL_SCHEMA_VERSION.to_string(),
            granted_at: now,
        })))
    }

    fn current_contract_revision_tx(
        tx: &rusqlite::Connection,
        task_id: TaskId,
    ) -> Result<Option<u32>, LedgerError> {
        let revision: Option<i64> = tx
            .query_row(
                "SELECT MAX(revision) FROM contracts WHERE task_id = ?1",
                [task_id.to_string()],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        Ok(revision.map(|r| r as u32))
    }

    fn authoritative_terminal_kinds_tx(
        tx: &rusqlite::Connection,
        task_id: TaskId,
        revision: Option<u32>,
    ) -> Result<Vec<String>, LedgerError> {
        let kinds: Vec<String> = match revision {
            Some(revision) => {
                let mut stmt = tx.prepare(
                    "SELECT DISTINCT outcome_kind FROM outcome_attestations
                     WHERE task_id = ?1 AND contract_revision = ?2 AND authoritative = 1
                       AND outcome_kind IN ('completed', 'failed', 'aborted')",
                )?;
                let rows = stmt
                    .query_map(rusqlite::params![task_id.to_string(), revision], |row| {
                        row.get::<_, String>(0)
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                rows
            }
            None => {
                let mut stmt = tx.prepare(
                    "SELECT DISTINCT outcome_kind FROM outcome_attestations
                     WHERE task_id = ?1 AND contract_revision IS NULL AND authoritative = 1
                       AND outcome_kind IN ('completed', 'failed', 'aborted')",
                )?;
                let rows = stmt
                    .query_map([task_id.to_string()], |row| row.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?;
                rows
            }
        };
        Ok(kinds)
    }

    fn settled_and_active_for_renewal(
        tx: &rusqlite::Connection,
        task_id: TaskId,
    ) -> Result<(f64, f64), LedgerError> {
        let settled: f64 = tx.query_row(
            "SELECT COALESCE(SUM(settled_amount), 0.0) FROM reservations
             WHERE task_id = ?1 AND state = 'settled'",
            [task_id.to_string()],
            |row| row.get(0),
        )?;
        let active: f64 = tx.query_row(
            "SELECT COALESCE(SUM(amount), 0.0) FROM reservations
             WHERE task_id = ?1 AND state = 'active'",
            [task_id.to_string()],
            |row| row.get(0),
        )?;
        Ok((settled, active))
    }
}
