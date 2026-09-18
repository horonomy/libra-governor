//! The atomic reservation ledger (HORO-1141): `task_budgets` +
//! `reservations`, implementing
//! `available = hard_limit - settled_spend - active_reservations -
//! completion_reserve`.
//!
//! # Atomicity
//!
//! Every mutating method here opens its transaction with
//! [`rusqlite::TransactionBehavior::Immediate`] — the write lock is
//! taken at `BEGIN`, not deferred to the first write. Two threads
//! computing headroom and then both deciding to write can never both
//! succeed: the second thread's `BEGIN IMMEDIATE` blocks until the first
//! commits (or, past `busy_timeout`, fails loudly) and then re-reads a
//! budget that already reflects the first thread's write. This is what
//! makes "concurrent sessions/subagents cannot double-spend the same
//! resource envelope" true at the SQLite layer, independent of whatever
//! serializes requests above it (the daemon's single-threaded accept
//! loop is a second, weaker layer — see `libra-governor-daemon`'s
//! `server` module docs).
//!
//! # Idempotency
//!
//! `reserve` is idempotent on `(task_id, idempotency_key)` — a replayed
//! request with the same key returns the existing reservation rather
//! than creating a second one ([`ReserveOutcome::AlreadyGranted`]).
//! `settle`/`release` are idempotent on `id` and its current state — a
//! reservation already in a terminal state returns
//! [`SettleOutcome::AlreadyFinal`]/[`ReleaseOutcome::AlreadyFinal`]
//! rather than double-applying the effect.

use libra_governor_domain::{
    CompletionReserveBasis, CompletionReserveEstimate, Headroom, Policy, PlanId, Reservation,
    ReservationClass, ReservationEvidence, ReservationId, ReservationState, ResourceAmount,
    ResourceKind, TaskBudget, TaskId, RESERVATION_SCHEMA_VERSION,
};
use rusqlite::{OptionalExtension, TransactionBehavior};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::{store::LedgerStore, LedgerError};

fn rfc3339(t: OffsetDateTime) -> Result<String, LedgerError> {
    t.format(&Rfc3339)
        .map_err(|e| LedgerError::Sqlite(rusqlite::Error::ToSqlConversionFailure(Box::new(e))))
}

fn parse_time(s: &str) -> Result<OffsetDateTime, LedgerError> {
    OffsetDateTime::parse(s, &Rfc3339).map_err(|_| LedgerError::Sqlite(rusqlite::Error::InvalidQuery))
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

fn class_to_str(class: ReservationClass) -> &'static str {
    match class {
        ReservationClass::RequiredWork => "required_work",
        ReservationClass::OptionalWork => "optional_work",
    }
}

fn class_from_str(s: &str) -> Result<ReservationClass, LedgerError> {
    match s {
        "required_work" => Ok(ReservationClass::RequiredWork),
        "optional_work" => Ok(ReservationClass::OptionalWork),
        _ => Err(LedgerError::Sqlite(rusqlite::Error::InvalidQuery)),
    }
}

fn state_from_str(s: &str) -> Result<ReservationState, LedgerError> {
    match s {
        "active" => Ok(ReservationState::Active),
        "settled" => Ok(ReservationState::Settled),
        "released" => Ok(ReservationState::Released),
        "expired" => Ok(ReservationState::Expired),
        _ => Err(LedgerError::Sqlite(rusqlite::Error::InvalidQuery)),
    }
}

fn basis_to_str(basis: CompletionReserveBasis) -> &'static str {
    match basis {
        CompletionReserveBasis::EstimateP80 => "estimate_p80",
        CompletionReserveBasis::PolicyTarget => "policy_target",
    }
}

fn basis_from_str(s: &str) -> Result<CompletionReserveBasis, LedgerError> {
    match s {
        "estimate_p80" => Ok(CompletionReserveBasis::EstimateP80),
        "policy_target" => Ok(CompletionReserveBasis::PolicyTarget),
        _ => Err(LedgerError::Sqlite(rusqlite::Error::InvalidQuery)),
    }
}

/// Raw `task_budgets` row shape.
type BudgetRow = (
    String, // resource_kind
    f64,    // hard_limit
    f64,    // initial_completion_reserve
    f64,    // completion_reserve
    String, // completion_reserve_basis
    String, // policy_json
    String, // policy_schema_version
    String, // reservation_schema_version
    String, // created_at
    String, // updated_at
);

fn row_to_budget(task_id: TaskId, row: BudgetRow) -> Result<TaskBudget, LedgerError> {
    let (
        resource_kind,
        hard_limit,
        initial_completion_reserve,
        completion_reserve,
        basis,
        policy_json,
        _policy_schema_version,
        reservation_schema_version,
        created_at,
        updated_at,
    ) = row;
    let resource_kind = kind_from_str(&resource_kind)?;
    let policy: Policy = serde_json::from_str(&policy_json)
        .map_err(|_| LedgerError::Sqlite(rusqlite::Error::InvalidQuery))?;
    Ok(TaskBudget {
        task_id,
        resource_kind,
        hard_limit: ResourceAmount::from_kind_f64(resource_kind, hard_limit),
        initial_completion_reserve: ResourceAmount::from_kind_f64(
            resource_kind,
            initial_completion_reserve,
        ),
        completion_reserve: ResourceAmount::from_kind_f64(resource_kind, completion_reserve),
        completion_reserve_basis: basis_from_str(&basis)?,
        policy,
        reservation_schema_version,
        created_at: parse_time(&created_at)?,
        updated_at: parse_time(&updated_at)?,
    })
}

/// Raw `reservations` row shape.
#[allow(clippy::type_complexity)]
type ReservationRow = (
    String,         // id
    String,         // task_id
    String,         // session_id
    Option<String>, // plan_id
    String,         // class
    String,         // resource_kind
    f64,            // amount
    f64,            // drawn_from_reserve
    String,         // state
    Option<f64>,    // settled_amount
    Option<bool>,   // usage_known
    String,         // idempotency_key
    String,         // created_at
    String,         // expires_at
    Option<String>, // settled_at
    Option<String>, // released_at
);

fn row_to_reservation(row: ReservationRow) -> Result<Reservation, LedgerError> {
    let (
        id,
        task_id,
        session_id,
        plan_id,
        class,
        resource_kind,
        amount,
        drawn_from_reserve,
        state,
        settled_amount,
        usage_known,
        idempotency_key,
        created_at,
        expires_at,
        settled_at,
        released_at,
    ) = row;
    let resource_kind = kind_from_str(&resource_kind)?;
    Ok(Reservation {
        id: ReservationId(
            uuid::Uuid::parse_str(&id).map_err(|_| LedgerError::Sqlite(rusqlite::Error::InvalidQuery))?,
        ),
        task_id: TaskId(
            uuid::Uuid::parse_str(&task_id)
                .map_err(|_| LedgerError::Sqlite(rusqlite::Error::InvalidQuery))?,
        ),
        session_id,
        plan_id: plan_id
            .map(|s| {
                uuid::Uuid::parse_str(&s)
                    .map(PlanId)
                    .map_err(|_| LedgerError::Sqlite(rusqlite::Error::InvalidQuery))
            })
            .transpose()?,
        class: class_from_str(&class)?,
        amount: ResourceAmount::from_kind_f64(resource_kind, amount),
        drawn_from_reserve: ResourceAmount::from_kind_f64(resource_kind, drawn_from_reserve),
        state: state_from_str(&state)?,
        settled_amount: settled_amount.map(|a| ResourceAmount::from_kind_f64(resource_kind, a)),
        usage_known,
        idempotency_key,
        created_at: parse_time(&created_at)?,
        expires_at: parse_time(&expires_at)?,
        settled_at: settled_at.map(|s| parse_time(&s)).transpose()?,
        released_at: released_at.map(|s| parse_time(&s)).transpose()?,
    })
}

/// Request to reserve resource capacity (HORO-1141). See
/// [`LedgerStore::reserve`].
pub struct ReserveRequest<'a> {
    pub task_id: TaskId,
    pub session_id: &'a str,
    pub plan_id: Option<PlanId>,
    pub class: ReservationClass,
    pub amount: ResourceAmount,
    /// Caller-chosen replay key, unique per task — a retry with the same
    /// key is a safe no-op (see [`ReserveOutcome::AlreadyGranted`]).
    pub idempotency_key: &'a str,
    pub now: OffsetDateTime,
    pub ttl_secs: u64,
}

/// The outcome of [`LedgerStore::reserve`].
///
/// `Granted`/`AlreadyGranted` box their [`Reservation`] payload: it
/// dominates this enum's size, matching how this repo already boxes
/// `FinalizeOutcome`/`Response` variants to satisfy
/// `clippy::large_enum_variant`.
#[derive(Debug, Clone, PartialEq)]
pub enum ReserveOutcome {
    Granted(Box<Reservation>),
    /// Idempotent replay: `(task_id, idempotency_key)` was already
    /// granted. Nothing was written; the existing row is returned.
    AlreadyGranted(Box<Reservation>),
    /// The requested amount exceeds the headroom available to this
    /// [`ReservationClass`] — for `OptionalWork` this can be true even
    /// while `RequiredWork` still has room, because optional work can
    /// never dip into `protected_reserve`.
    Insufficient {
        available: Headroom,
        requested: ResourceAmount,
        protected_reserve: ResourceAmount,
    },
    /// No `task_budgets` row exists for this task — admission-time
    /// budget initialization never ran for it.
    NoBudget,
}

/// The outcome of [`LedgerStore::settle`].
#[derive(Debug, Clone, PartialEq)]
pub enum SettleOutcome {
    Settled {
        reservation: Box<Reservation>,
        refunded: Option<ResourceAmount>,
        overrun: Option<ResourceAmount>,
        restored_to_reserve: ResourceAmount,
    },
    /// Duplicate settlement event: the reservation was already
    /// `Settled`/`Released`/`Expired`. Nothing written; the existing row
    /// is returned unchanged.
    AlreadyFinal(Box<Reservation>),
    NotFound,
}

/// The outcome of [`LedgerStore::release`].
#[derive(Debug, Clone, PartialEq)]
pub enum ReleaseOutcome {
    Released(Box<Reservation>),
    AlreadyFinal(Box<Reservation>),
    NotFound,
}

/// The outcome of [`LedgerStore::adjust_completion_reserve`].
#[derive(Debug, Clone, PartialEq)]
pub enum AdjustOutcome {
    Adjusted {
        from: ResourceAmount,
        to: ResourceAmount,
    },
    /// A requested INCREASE exceeded uncommitted (non-reserve) headroom.
    /// Lowering the reserve is always permitted (see
    /// [`LedgerStore::adjust_completion_reserve`] docs).
    Insufficient {
        available: Headroom,
        requested_increase: ResourceAmount,
    },
    NoBudget,
}

impl LedgerStore {
    /// Initializes `task_id`'s [`TaskBudget`] from `policy` and the
    /// computed `reserve`, if one does not already exist. Never
    /// overwrites an existing budget — the limit in force at admission is
    /// the limit for the life of the task; a second preflight in the same
    /// session must not silently re-baseline it (use
    /// [`Self::adjust_completion_reserve`] to recompute just the reserve
    /// after a replan). Returns whichever budget ends up persisted
    /// (freshly inserted, or the one already there).
    pub fn initialize_task_budget(
        &mut self,
        task_id: TaskId,
        policy: &Policy,
        reserve: &CompletionReserveEstimate,
        now: OffsetDateTime,
    ) -> Result<TaskBudget, LedgerError> {
        let resource_kind = policy.resource.target.kind();
        let policy_json = serde_json::to_string(policy)
            .map_err(|e| LedgerError::Sqlite(rusqlite::Error::ToSqlConversionFailure(Box::new(e))))?;
        let now_str = rfc3339(now)?;

        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO task_budgets (
                task_id, resource_kind, hard_limit, initial_completion_reserve,
                completion_reserve, completion_reserve_basis, policy_json,
                policy_schema_version, reservation_schema_version, created_at, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(task_id) DO NOTHING",
            rusqlite::params![
                task_id.to_string(),
                kind_to_str(resource_kind),
                policy.resource.hard_ceiling.as_f64(),
                reserve.amount.as_f64(),
                reserve.amount.as_f64(),
                basis_to_str(reserve.basis),
                policy_json,
                policy.policy_schema_version,
                RESERVATION_SCHEMA_VERSION,
                now_str,
                now_str,
            ],
        )?;
        let row: BudgetRow = tx.query_row(
            "SELECT resource_kind, hard_limit, initial_completion_reserve, completion_reserve,
                    completion_reserve_basis, policy_json, policy_schema_version,
                    reservation_schema_version, created_at, updated_at
             FROM task_budgets WHERE task_id = ?1",
            [task_id.to_string()],
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
                ))
            },
        )?;
        tx.commit()?;
        row_to_budget(task_id, row)
    }

    /// Reads `task_id`'s [`TaskBudget`], if one has been initialized.
    pub fn task_budget(&self, task_id: TaskId) -> Result<Option<TaskBudget>, LedgerError> {
        let row: Option<BudgetRow> = self
            .conn
            .query_row(
                "SELECT resource_kind, hard_limit, initial_completion_reserve, completion_reserve,
                        completion_reserve_basis, policy_json, policy_schema_version,
                        reservation_schema_version, created_at, updated_at
                 FROM task_budgets WHERE task_id = ?1",
                [task_id.to_string()],
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
                    ))
                },
            )
            .optional()?;
        row.map(|r| row_to_budget(task_id, r)).transpose()
    }

    /// Computes the [`Headroom`] available to `class` right now:
    /// `hard_limit - settled - active` for [`ReservationClass::RequiredWork`]
    /// (the reserve is NOT subtracted — required work may draw into it),
    /// and that same figure minus `completion_reserve` for
    /// [`ReservationClass::OptionalWork`] — the ticket's formula applied
    /// literally for the class that must never touch the protected floor.
    /// `Ok(None)` when no budget has been initialized for this task.
    pub fn available(
        &self,
        task_id: TaskId,
        class: ReservationClass,
    ) -> Result<Option<Headroom>, LedgerError> {
        let Some(budget) = self.task_budget(task_id)? else {
            return Ok(None);
        };
        let (settled, active) = self.settled_and_active(task_id)?;
        let general = budget.hard_limit.as_f64() - settled - active;
        let value = match class {
            ReservationClass::RequiredWork => general,
            ReservationClass::OptionalWork => general - budget.completion_reserve.as_f64(),
        };
        Ok(Some(Headroom {
            kind: budget.resource_kind,
            value,
        }))
    }

    fn settled_and_active(&self, task_id: TaskId) -> Result<(f64, f64), LedgerError> {
        let settled: f64 = self.conn.query_row(
            "SELECT COALESCE(SUM(settled_amount), 0.0) FROM reservations
             WHERE task_id = ?1 AND state = 'settled'",
            [task_id.to_string()],
            |row| row.get(0),
        )?;
        let active: f64 = self.conn.query_row(
            "SELECT COALESCE(SUM(amount), 0.0) FROM reservations
             WHERE task_id = ?1 AND state = 'active'",
            [task_id.to_string()],
            |row| row.get(0),
        )?;
        Ok((settled, active))
    }

    /// Atomically reserves `req.amount` of capacity, drawing into the
    /// protected Completion Reserve only when `req.class ==
    /// RequiredWork` and ordinary (non-reserve) headroom is insufficient
    /// on its own. See module docs for the transaction/idempotency
    /// guarantees.
    pub fn reserve(&mut self, req: ReserveRequest<'_>) -> Result<ReserveOutcome, LedgerError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;

        let existing: Option<ReservationRow> = tx
            .query_row(
                "SELECT id, task_id, session_id, plan_id, class, resource_kind, amount,
                        drawn_from_reserve, state, settled_amount, usage_known,
                        idempotency_key, created_at, expires_at, settled_at, released_at
                 FROM reservations WHERE task_id = ?1 AND idempotency_key = ?2",
                rusqlite::params![req.task_id.to_string(), req.idempotency_key],
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
                    ))
                },
            )
            .optional()?;
        if let Some(row) = existing {
            tx.commit()?;
            return Ok(ReserveOutcome::AlreadyGranted(Box::new(row_to_reservation(
                row,
            )?)));
        }

        let budget_row: Option<BudgetRow> = tx
            .query_row(
                "SELECT resource_kind, hard_limit, initial_completion_reserve, completion_reserve,
                        completion_reserve_basis, policy_json, policy_schema_version,
                        reservation_schema_version, created_at, updated_at
                 FROM task_budgets WHERE task_id = ?1",
                [req.task_id.to_string()],
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
                    ))
                },
            )
            .optional()?;
        let Some(budget_row) = budget_row else {
            tx.commit()?;
            return Ok(ReserveOutcome::NoBudget);
        };
        let budget = row_to_budget(req.task_id, budget_row)?;

        if req.amount.kind() != budget.resource_kind {
            return Err(LedgerError::ResourceKindMismatch {
                expected: budget.resource_kind,
                actual: req.amount.kind(),
            });
        }

        let settled: f64 = tx.query_row(
            "SELECT COALESCE(SUM(settled_amount), 0.0) FROM reservations
             WHERE task_id = ?1 AND state = 'settled'",
            [req.task_id.to_string()],
            |row| row.get(0),
        )?;
        let active: f64 = tx.query_row(
            "SELECT COALESCE(SUM(amount), 0.0) FROM reservations
             WHERE task_id = ?1 AND state = 'active'",
            [req.task_id.to_string()],
            |row| row.get(0),
        )?;
        let general = budget.hard_limit.as_f64() - settled - active;
        let reserve = budget.completion_reserve.as_f64();
        let optional_headroom = general - reserve;
        let requested = req.amount.as_f64();

        let draw: f64 = match req.class {
            ReservationClass::OptionalWork => {
                if requested > optional_headroom {
                    tx.commit()?;
                    return Ok(ReserveOutcome::Insufficient {
                        available: Headroom {
                            kind: budget.resource_kind,
                            value: optional_headroom,
                        },
                        requested: req.amount,
                        protected_reserve: budget.completion_reserve,
                    });
                }
                0.0
            }
            ReservationClass::RequiredWork => {
                if requested > general {
                    tx.commit()?;
                    return Ok(ReserveOutcome::Insufficient {
                        available: Headroom {
                            kind: budget.resource_kind,
                            value: general,
                        },
                        requested: req.amount,
                        protected_reserve: budget.completion_reserve,
                    });
                }
                (requested - optional_headroom).max(0.0)
            }
        };

        let new_reserve = reserve - draw;
        tx.execute(
            "UPDATE task_budgets SET completion_reserve = ?1, updated_at = ?2 WHERE task_id = ?3",
            rusqlite::params![new_reserve, rfc3339(req.now)?, req.task_id.to_string()],
        )?;

        let id = ReservationId::new();
        let expires_at = req.now + time::Duration::seconds(req.ttl_secs as i64);
        tx.execute(
            "INSERT INTO reservations (
                id, task_id, session_id, plan_id, class, resource_kind, amount,
                drawn_from_reserve, state, settled_amount, usage_known, idempotency_key,
                created_at, expires_at, settled_at, released_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'active', NULL, NULL, ?9, ?10, ?11, NULL, NULL)",
            rusqlite::params![
                id.0.to_string(),
                req.task_id.to_string(),
                req.session_id,
                req.plan_id.map(|p| p.0.to_string()),
                class_to_str(req.class),
                kind_to_str(budget.resource_kind),
                requested,
                draw,
                req.idempotency_key,
                rfc3339(req.now)?,
                rfc3339(expires_at)?,
            ],
        )?;
        tx.commit()?;

        Ok(ReserveOutcome::Granted(Box::new(Reservation {
            id,
            task_id: req.task_id,
            session_id: req.session_id.to_string(),
            plan_id: req.plan_id,
            class: req.class,
            amount: req.amount,
            drawn_from_reserve: ResourceAmount::from_kind_f64(budget.resource_kind, draw),
            state: ReservationState::Active,
            settled_amount: None,
            usage_known: None,
            idempotency_key: req.idempotency_key.to_string(),
            created_at: req.now,
            expires_at,
            settled_at: None,
            released_at: None,
        })))
    }

    fn get_reservation_tx(
        tx: &rusqlite::Transaction<'_>,
        id: ReservationId,
    ) -> Result<Option<Reservation>, LedgerError> {
        let row: Option<ReservationRow> = tx
            .query_row(
                "SELECT id, task_id, session_id, plan_id, class, resource_kind, amount,
                        drawn_from_reserve, state, settled_amount, usage_known,
                        idempotency_key, created_at, expires_at, settled_at, released_at
                 FROM reservations WHERE id = ?1",
                [id.0.to_string()],
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
                    ))
                },
            )
            .optional()?;
        row.map(row_to_reservation).transpose()
    }

    /// Atomically settles reservation `id` with `actual` cost. `actual ==
    /// None` is the conservative fallback used when no reported usage
    /// figure exists (the normal path today — see
    /// `libra_governor_domain::Reservation::usage_known` docs): the
    /// reservation settles at its full reserved amount, so nothing is
    /// refunded that cannot be proven unspent. Restores whatever portion
    /// of [`Reservation::drawn_from_reserve`] was not consumed by the
    /// actual cost back onto the task's Completion Reserve, in the same
    /// transaction. Idempotent: settling an already-final reservation
    /// returns [`SettleOutcome::AlreadyFinal`] and writes nothing.
    pub fn settle(
        &mut self,
        id: ReservationId,
        actual: Option<ResourceAmount>,
        now: OffsetDateTime,
    ) -> Result<SettleOutcome, LedgerError> {
        if let Some(a) = actual {
            if a.as_f64() < 0.0 {
                return Err(LedgerError::NegativeSettlement);
            }
        }

        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(reservation) = Self::get_reservation_tx(&tx, id)? else {
            tx.commit()?;
            return Ok(SettleOutcome::NotFound);
        };
        if reservation.state != ReservationState::Active {
            tx.commit()?;
            return Ok(SettleOutcome::AlreadyFinal(Box::new(reservation)));
        }
        if let Some(a) = actual {
            if a.kind() != reservation.amount.kind() {
                return Err(LedgerError::ResourceKindMismatch {
                    expected: reservation.amount.kind(),
                    actual: a.kind(),
                });
            }
        }

        let (settled_value, usage_known) = match actual {
            Some(a) => (a.as_f64(), true),
            None => (reservation.amount.as_f64(), false),
        };
        let refund = (reservation.amount.as_f64() - settled_value).max(0.0);
        let restore = refund.min(reservation.drawn_from_reserve.as_f64());

        let updated = tx.execute(
            "UPDATE reservations SET state = 'settled', settled_amount = ?1, usage_known = ?2,
                                      settled_at = ?3
             WHERE id = ?4 AND state = 'active'",
            rusqlite::params![settled_value, usage_known, rfc3339(now)?, id.0.to_string()],
        )?;
        if updated == 0 {
            // Lost a race against a concurrent settle/release/expire on
            // this same row between the read above and this write.
            let current = Self::get_reservation_tx(&tx, id)?
                .expect("row existed moments ago under the same write lock");
            tx.commit()?;
            return Ok(SettleOutcome::AlreadyFinal(Box::new(current)));
        }
        if restore > 0.0 {
            tx.execute(
                "UPDATE task_budgets SET completion_reserve = completion_reserve + ?1,
                                          updated_at = ?2
                 WHERE task_id = ?3",
                rusqlite::params![restore, rfc3339(now)?, reservation.task_id.to_string()],
            )?;
        }
        let updated_reservation = Self::get_reservation_tx(&tx, id)?
            .expect("row just updated in this same transaction");
        tx.commit()?;

        let refunded = updated_reservation.refunded();
        let overrun = updated_reservation.overrun();
        Ok(SettleOutcome::Settled {
            reservation: Box::new(updated_reservation),
            refunded,
            overrun,
            restored_to_reserve: ResourceAmount::from_kind_f64(reservation.amount.kind(), restore),
        })
    }

    /// Atomically releases reservation `id` unspent, restoring its full
    /// [`Reservation::drawn_from_reserve`] back onto the Completion
    /// Reserve. Used when a reservation's owning plan is superseded
    /// (e.g. by a replan) before any of it was spent. Idempotent, mirror
    /// of [`Self::settle`].
    pub fn release(
        &mut self,
        id: ReservationId,
        now: OffsetDateTime,
    ) -> Result<ReleaseOutcome, LedgerError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(reservation) = Self::get_reservation_tx(&tx, id)? else {
            tx.commit()?;
            return Ok(ReleaseOutcome::NotFound);
        };
        if reservation.state != ReservationState::Active {
            tx.commit()?;
            return Ok(ReleaseOutcome::AlreadyFinal(Box::new(reservation)));
        }

        let updated = tx.execute(
            "UPDATE reservations SET state = 'released', released_at = ?1
             WHERE id = ?2 AND state = 'active'",
            rusqlite::params![rfc3339(now)?, id.0.to_string()],
        )?;
        if updated == 0 {
            let current = Self::get_reservation_tx(&tx, id)?
                .expect("row existed moments ago under the same write lock");
            tx.commit()?;
            return Ok(ReleaseOutcome::AlreadyFinal(Box::new(current)));
        }
        if reservation.drawn_from_reserve.as_f64() > 0.0 {
            tx.execute(
                "UPDATE task_budgets SET completion_reserve = completion_reserve + ?1,
                                          updated_at = ?2
                 WHERE task_id = ?3",
                rusqlite::params![
                    reservation.drawn_from_reserve.as_f64(),
                    rfc3339(now)?,
                    reservation.task_id.to_string()
                ],
            )?;
        }
        let updated_reservation = Self::get_reservation_tx(&tx, id)?
            .expect("row just updated in this same transaction");
        tx.commit()?;
        Ok(ReleaseOutcome::Released(Box::new(updated_reservation)))
    }

    /// Releases every `active` reservation tied to `(task_id, plan_id)`
    /// in one transaction — used when a replan supersedes a plan so its
    /// unspent envelope is refunded rather than stranded `active`
    /// forever.
    pub fn release_active_for_plan(
        &mut self,
        task_id: TaskId,
        plan_id: PlanId,
        now: OffsetDateTime,
    ) -> Result<Vec<Reservation>, LedgerError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let ids: Vec<String> = {
            let mut stmt = tx.prepare(
                "SELECT id FROM reservations WHERE task_id = ?1 AND plan_id = ?2 AND state = 'active'",
            )?;
            let rows = stmt.query_map(
                rusqlite::params![task_id.to_string(), plan_id.0.to_string()],
                |row| row.get::<_, String>(0),
            )?;
            rows.collect::<Result<_, _>>()?
        };

        let mut released = Vec::with_capacity(ids.len());
        for id_str in ids {
            let id = ReservationId(
                uuid::Uuid::parse_str(&id_str)
                    .map_err(|_| LedgerError::Sqlite(rusqlite::Error::InvalidQuery))?,
            );
            let reservation = Self::get_reservation_tx(&tx, id)?
                .expect("id came from a query against this same transaction");
            tx.execute(
                "UPDATE reservations SET state = 'released', released_at = ?1 WHERE id = ?2",
                rusqlite::params![rfc3339(now)?, id_str],
            )?;
            if reservation.drawn_from_reserve.as_f64() > 0.0 {
                tx.execute(
                    "UPDATE task_budgets SET completion_reserve = completion_reserve + ?1,
                                              updated_at = ?2
                     WHERE task_id = ?3",
                    rusqlite::params![
                        reservation.drawn_from_reserve.as_f64(),
                        rfc3339(now)?,
                        task_id.to_string()
                    ],
                )?;
            }
            released.push(Self::get_reservation_tx(&tx, id)?.expect("just updated"));
        }
        tx.commit()?;
        Ok(released)
    }

    /// Reclaims every `active` reservation whose `expires_at <= now` in
    /// one transaction — the crash/restart reconciliation path: a
    /// reservation issued by a process that then crashed without
    /// settling stays `active` forever otherwise. Restores each
    /// reservation's `drawn_from_reserve` back onto its task's
    /// Completion Reserve.
    pub fn expire_stale_reservations(
        &mut self,
        now: OffsetDateTime,
    ) -> Result<Vec<Reservation>, LedgerError> {
        let now_str = rfc3339(now)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let ids: Vec<String> = {
            let mut stmt = tx.prepare(
                "SELECT id FROM reservations WHERE state = 'active' AND expires_at <= ?1",
            )?;
            let rows = stmt.query_map([&now_str], |row| row.get::<_, String>(0))?;
            rows.collect::<Result<_, _>>()?
        };

        let mut expired = Vec::with_capacity(ids.len());
        for id_str in ids {
            let id = ReservationId(
                uuid::Uuid::parse_str(&id_str)
                    .map_err(|_| LedgerError::Sqlite(rusqlite::Error::InvalidQuery))?,
            );
            let reservation = Self::get_reservation_tx(&tx, id)?
                .expect("id came from a query against this same transaction");
            tx.execute(
                "UPDATE reservations SET state = 'expired', released_at = ?1 WHERE id = ?2",
                rusqlite::params![now_str, id_str],
            )?;
            if reservation.drawn_from_reserve.as_f64() > 0.0 {
                tx.execute(
                    "UPDATE task_budgets SET completion_reserve = completion_reserve + ?1,
                                              updated_at = ?2
                     WHERE task_id = ?3",
                    rusqlite::params![
                        reservation.drawn_from_reserve.as_f64(),
                        now_str,
                        reservation.task_id.to_string()
                    ],
                )?;
            }
            expired.push(Self::get_reservation_tx(&tx, id)?.expect("just updated"));
        }
        tx.commit()?;
        Ok(expired)
    }

    /// Recomputes `task_id`'s Completion Reserve to `new_amount` (driven
    /// by fresh evidence — e.g. a replan recalculating remaining work).
    /// Lowering the reserve is always permitted: it does not weaken the
    /// protection guarantee, which is carried by the optional-headroom
    /// formula at reserve time, not by the reserve only ever growing.
    /// Raising it is permitted only up to the currently uncommitted
    /// (non-reserve) headroom — raising past that would require
    /// retroactively invalidating an already-granted optional
    /// reservation, which this ledger never does.
    pub fn adjust_completion_reserve(
        &mut self,
        task_id: TaskId,
        new_amount: ResourceAmount,
        basis: CompletionReserveBasis,
        now: OffsetDateTime,
    ) -> Result<AdjustOutcome, LedgerError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let budget_row: Option<BudgetRow> = tx
            .query_row(
                "SELECT resource_kind, hard_limit, initial_completion_reserve, completion_reserve,
                        completion_reserve_basis, policy_json, policy_schema_version,
                        reservation_schema_version, created_at, updated_at
                 FROM task_budgets WHERE task_id = ?1",
                [task_id.to_string()],
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
                    ))
                },
            )
            .optional()?;
        let Some(budget_row) = budget_row else {
            tx.commit()?;
            return Ok(AdjustOutcome::NoBudget);
        };
        let budget = row_to_budget(task_id, budget_row)?;

        if new_amount.kind() != budget.resource_kind {
            return Err(LedgerError::ResourceKindMismatch {
                expected: budget.resource_kind,
                actual: new_amount.kind(),
            });
        }

        let current = budget.completion_reserve.as_f64();
        let target = new_amount.as_f64();
        let delta = target - current;
        if delta > 0.0 {
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
            let optional_headroom = budget.hard_limit.as_f64() - settled - active - current;
            if delta > optional_headroom {
                tx.commit()?;
                return Ok(AdjustOutcome::Insufficient {
                    available: Headroom {
                        kind: budget.resource_kind,
                        value: optional_headroom,
                    },
                    requested_increase: ResourceAmount::from_kind_f64(budget.resource_kind, delta),
                });
            }
        }

        tx.execute(
            "UPDATE task_budgets SET completion_reserve = ?1, completion_reserve_basis = ?2,
                                      updated_at = ?3
             WHERE task_id = ?4",
            rusqlite::params![
                target,
                basis_to_str(basis),
                rfc3339(now)?,
                task_id.to_string()
            ],
        )?;
        tx.commit()?;

        Ok(AdjustOutcome::Adjusted {
            from: budget.completion_reserve,
            to: new_amount,
        })
    }

    /// Returns every reservation recorded for `task_id`, most recent
    /// first.
    pub fn reservations_for_task(&self, task_id: TaskId) -> Result<Vec<Reservation>, LedgerError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, task_id, session_id, plan_id, class, resource_kind, amount,
                    drawn_from_reserve, state, settled_amount, usage_known,
                    idempotency_key, created_at, expires_at, settled_at, released_at
             FROM reservations WHERE task_id = ?1 ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map([task_id.to_string()], |row| {
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
            ))
        })?;
        rows.map(|r| row_to_reservation(r?)).collect()
    }

    /// Summarizes `task_id`'s reservation history into
    /// [`ReservationEvidence`] for receipt finalization (HORO-1141). `Ok(None)`
    /// when no budget was ever initialized for this task.
    pub fn reservation_evidence(
        &self,
        task_id: TaskId,
    ) -> Result<Option<ReservationEvidence>, LedgerError> {
        let Some(budget) = self.task_budget(task_id)? else {
            return Ok(None);
        };
        let reservations = self.reservations_for_task(task_id)?;
        let kind = budget.resource_kind;

        let mut reserved_total = 0.0;
        let mut settled_total = 0.0;
        let mut released_total = 0.0;
        let mut overrun_total = 0.0;
        let mut usage_known_count = 0u32;

        for r in &reservations {
            reserved_total += r.amount.as_f64();
            match r.state {
                ReservationState::Settled => {
                    settled_total += r.settled_amount.map(|a| a.as_f64()).unwrap_or(0.0);
                    if let Some(overrun) = r.overrun() {
                        overrun_total += overrun.as_f64();
                    }
                    if r.usage_known == Some(true) {
                        usage_known_count += 1;
                    }
                }
                ReservationState::Released | ReservationState::Expired => {
                    released_total += r.amount.as_f64();
                }
                ReservationState::Active => {}
            }
        }

        Ok(Some(ReservationEvidence {
            reserved_total: ResourceAmount::from_kind_f64(kind, reserved_total),
            settled_total: ResourceAmount::from_kind_f64(kind, settled_total),
            released_total: ResourceAmount::from_kind_f64(kind, released_total),
            overrun_total: if overrun_total > 0.0 {
                Some(ResourceAmount::from_kind_f64(kind, overrun_total))
            } else {
                None
            },
            completion_reserve_initial: budget.initial_completion_reserve,
            completion_reserve_remaining: budget.completion_reserve,
            reservation_count: reservations.len() as u32,
            usage_known_count,
        }))
    }
}
