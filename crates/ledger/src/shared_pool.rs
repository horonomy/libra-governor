//! The flat shared-quota-pool reservation ledger (HORO-1763):
//! `quota_pools` + `shared_pool_reservations` +
//! `shared_pool_provider_snapshots`.
//!
//! # Atomicity
//!
//! Exactly [`crate::reservation`]'s discipline, reused rather than
//! reinvented: every mutating method opens its transaction with
//! [`TransactionBehavior::Immediate`], so the write lock is taken at
//! `BEGIN`, not deferred to the first write. Two threads/processes
//! computing a pool's remaining capacity and then both deciding to
//! reserve can never both succeed — the second's `BEGIN IMMEDIATE` blocks
//! until the first commits and then re-reads a pool that already
//! reflects the first write. This is what makes AC1 ("concurrent sessions
//! cannot collectively reserve more than the pool's remaining capacity")
//! true at the SQLite layer.
//!
//! # Idempotency
//!
//! `reserve` is idempotent on `(pool_id, principal_id, idempotency_key)`
//! — the principal is part of the scope, not just the pool, because an
//! idempotency key is a caller-chosen string with no uniqueness guarantee
//! across the many different principals that may share one pool; scoping
//! only to the pool would let one principal's replay lookup collide with
//! a different principal's row.
//! `settle`/`release` are idempotent on `id` and its current state via a
//! conditional `UPDATE ... WHERE state = ?` with a zero-rows-affected
//! fallback — the same race-safe primitive `crate::reservation` uses,
//! not a separate locking scheme.
//!
//! # Crash / restart recovery (AC2)
//!
//! Nothing about a pool's remaining capacity is held in process memory.
//! `settled`/`active` are always re-derived from `shared_pool_reservations`
//! rows by a fresh `SUM`/`COUNT` inside the current transaction, so a
//! daemon restart that reopens the same SQLite file sees exactly the
//! state the last committed transaction left — an `active` hold from a
//! session that crashed before settling stays `active`, correctly still
//! counted against the pool, until [`LedgerStore::expire_stale_shared_pool_reservations`]
//! reclaims it past its `expires_at`. See `shared_pool.rs`'s own test
//! module for a reopened-store regression test.
//!
//! # Reconciliation (AC3)
//!
//! A provider [`GaugeReading`] snapshot is never summed into `settled` —
//! there is exactly one snapshot row per pool
//! (`shared_pool_provider_snapshots.pool_id` is its own primary key,
//! upserted on ingest), and it contributes only through
//! [`PoolAdmission::external_overhang`], a term *subtracted* from
//! remaining capacity, never added to spend. See
//! [`LedgerStore::pool_admission`].

use libra_governor_domain::{
    Confidence, GaugeReading, PoolAdmission, PoolId, PrincipalId, QuotaPool, QuotaUnit,
    ReservationState, SharedPoolReservation, SharedPoolReservationId,
    SHARED_POOL_RESERVATION_SCHEMA_VERSION,
};
use rusqlite::{OptionalExtension, TransactionBehavior};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::{store::LedgerStore, transaction::begin, LedgerError};

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

/// A generous-but-finite bound on a reservation's `ttl_secs`, mirroring
/// `quota_window::MAX_SLIDING_LENGTH_SECS`'s own 100-year precedent — no
/// realistic operator-configured TTL approaches this, so clamping to it
/// means the expiry-instant arithmetic in [`LedgerStore::reserve_shared`]
/// never needs an "expires immediately" fallback for an oversized TTL.
const MAX_TTL_SECS: u64 = 100 * 365 * 24 * 3600;

/// SQLite's `INTEGER` column is a signed 64-bit value; every amount this
/// module persists originates as a caller-supplied `u64`. A bare `as i64`
/// cast silently wraps a value above `i64::MAX` into a negative number —
/// exactly the truncation-bug class this ticket's own review discipline
/// calls out (a negative stored "capacity" or "amount" would corrupt
/// every subsequent admission comparison into failing open). Rejected
/// outright via `try_from` rather than saturated: a caller-supplied
/// amount this large is never legitimate input, so there is no sensible
/// value to clamp to — only a request to refuse.
fn checked_i64(value: u64) -> Result<i64, LedgerError> {
    i64::try_from(value).map_err(|_| invalid())
}

fn unit_to_json(unit: &QuotaUnit) -> Result<String, LedgerError> {
    serde_json::to_string(unit)
        .map_err(|e| LedgerError::Sqlite(rusqlite::Error::ToSqlConversionFailure(Box::new(e))))
}

fn unit_from_json(s: &str) -> Result<QuotaUnit, LedgerError> {
    serde_json::from_str(s).map_err(|_| invalid())
}

fn state_from_str(s: &str) -> Result<ReservationState, LedgerError> {
    match s {
        "active" => Ok(ReservationState::Active),
        "settled" => Ok(ReservationState::Settled),
        "released" => Ok(ReservationState::Released),
        "expired" => Ok(ReservationState::Expired),
        _ => Err(invalid()),
    }
}

/// Raw `quota_pools` row shape.
type PoolRow = (String, u64, String, String, String); // unit_json, capacity, schema_version, created_at, updated_at

fn row_to_pool(pool_id: PoolId, row: PoolRow) -> Result<QuotaPool, LedgerError> {
    let (unit_json, capacity, schema_version, created_at, updated_at) = row;
    Ok(QuotaPool {
        pool_id,
        unit: unit_from_json(&unit_json)?,
        capacity,
        schema_version,
        created_at: parse_time(&created_at)?,
        updated_at: parse_time(&updated_at)?,
    })
}

/// Raw `shared_pool_reservations` row shape.
#[allow(clippy::type_complexity)]
type ReservationRow = (
    String,         // id
    String,         // pool_id
    String,         // principal_id
    String,         // session_id
    u64,            // amount
    String,         // state
    Option<u64>,    // settled_amount
    Option<bool>,   // usage_known
    String,         // idempotency_key
    String,         // created_at
    String,         // expires_at
    Option<String>, // settled_at
    Option<String>, // released_at
    String,         // schema_version
);

const RESERVATION_COLUMNS: &str =
    "id, pool_id, principal_id, session_id, amount, state, settled_amount, usage_known,
     idempotency_key, created_at, expires_at, settled_at, released_at, schema_version";

fn read_reservation_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ReservationRow> {
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

fn row_to_reservation(row: ReservationRow) -> Result<SharedPoolReservation, LedgerError> {
    let (
        id,
        pool_id,
        principal_id,
        session_id,
        amount,
        state,
        settled_amount,
        usage_known,
        idempotency_key,
        created_at,
        expires_at,
        settled_at,
        released_at,
        schema_version,
    ) = row;
    Ok(SharedPoolReservation {
        id: SharedPoolReservationId(uuid::Uuid::parse_str(&id).map_err(|_| invalid())?),
        pool_id: PoolId(pool_id),
        principal_id: PrincipalId(principal_id),
        session_id,
        amount,
        state: state_from_str(&state)?,
        settled_amount,
        usage_known,
        idempotency_key,
        created_at: parse_time(&created_at)?,
        expires_at: parse_time(&expires_at)?,
        settled_at: settled_at.map(|s| parse_time(&s)).transpose()?,
        released_at: released_at.map(|s| parse_time(&s)).transpose()?,
        schema_version,
    })
}

/// Request to reserve shared-pool capacity. See
/// [`LedgerStore::reserve_shared`].
pub struct SharedPoolReserveRequest<'a> {
    pub pool_id: &'a PoolId,
    pub principal_id: &'a PrincipalId,
    pub session_id: &'a str,
    pub amount: u64,
    pub unit: &'a QuotaUnit,
    /// Caller-chosen replay key, unique per pool.
    pub idempotency_key: &'a str,
    pub now: OffsetDateTime,
    pub ttl_secs: u64,
}

/// The outcome of [`LedgerStore::reserve_shared`].
#[derive(Debug, Clone, PartialEq)]
pub enum SharedPoolReserveOutcome {
    Granted(Box<SharedPoolReservation>),
    /// Idempotent replay: `(pool_id, idempotency_key)` was already
    /// granted. Nothing was written.
    AlreadyGranted(Box<SharedPoolReservation>),
    /// The requested amount exceeds [`PoolAdmission::remaining`].
    Insufficient {
        admission: PoolAdmission,
        requested: u64,
    },
    /// No `quota_pools` row exists for this pool — [`LedgerStore::ensure_pool`]
    /// was never called for it.
    NoPool,
}

/// The outcome of [`LedgerStore::settle_shared`].
#[derive(Debug, Clone, PartialEq)]
pub enum SharedPoolSettleOutcome {
    Settled {
        reservation: Box<SharedPoolReservation>,
        refunded: Option<u64>,
        overrun: Option<u64>,
    },
    AlreadyFinal(Box<SharedPoolReservation>),
    NotFound,
}

/// The outcome of [`LedgerStore::release_shared`].
#[derive(Debug, Clone, PartialEq)]
pub enum SharedPoolReleaseOutcome {
    Released(Box<SharedPoolReservation>),
    AlreadyFinal(Box<SharedPoolReservation>),
    NotFound,
}

impl LedgerStore {
    /// Creates `pool_id`'s [`QuotaPool`] with `capacity`/`unit`, if one
    /// does not already exist. Never overwrites an existing pool's
    /// capacity or unit — mirrors
    /// [`crate::reservation`]'s `initialize_task_budget`: the capacity in
    /// force at first creation is the capacity for the life of the pool.
    /// Returns whichever pool ends up persisted.
    pub fn ensure_pool(
        &mut self,
        pool_id: &PoolId,
        unit: &QuotaUnit,
        capacity: u64,
        now: OffsetDateTime,
    ) -> Result<QuotaPool, LedgerError> {
        let unit_json = unit_to_json(unit)?;
        let now_str = rfc3339(now)?;
        let capacity_i64 = checked_i64(capacity)?;
        let tx = begin(&mut self.conn, TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO quota_pools (pool_id, unit_json, capacity, schema_version, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5)
             ON CONFLICT(pool_id) DO NOTHING",
            rusqlite::params![
                pool_id.0,
                unit_json,
                capacity_i64,
                SHARED_POOL_RESERVATION_SCHEMA_VERSION,
                now_str,
            ],
        )?;
        let row: PoolRow = tx.query_row(
            "SELECT unit_json, capacity, schema_version, created_at, updated_at
             FROM quota_pools WHERE pool_id = ?1",
            [&pool_id.0],
            |row| {
                let capacity: i64 = row.get(1)?;
                Ok((
                    row.get(0)?,
                    capacity as u64,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )?;
        tx.commit()?;
        row_to_pool(pool_id.clone(), row)
    }

    /// Reads `pool_id`'s [`QuotaPool`], if one has been created.
    pub fn quota_pool(&self, pool_id: &PoolId) -> Result<Option<QuotaPool>, LedgerError> {
        let row: Option<PoolRow> = self
            .conn
            .query_row(
                "SELECT unit_json, capacity, schema_version, created_at, updated_at
                 FROM quota_pools WHERE pool_id = ?1",
                [&pool_id.0],
                |row| {
                    let capacity: i64 = row.get(1)?;
                    Ok((
                        row.get(0)?,
                        capacity as u64,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .optional()?;
        row.map(|r| row_to_pool(pool_id.clone(), r)).transpose()
    }

    fn settled_and_active_tx(
        tx: &rusqlite::Connection,
        pool_id: &PoolId,
    ) -> Result<(u64, u64), LedgerError> {
        let settled: i64 = tx.query_row(
            "SELECT COALESCE(SUM(settled_amount), 0) FROM shared_pool_reservations
             WHERE pool_id = ?1 AND state = 'settled'",
            [&pool_id.0],
            |row| row.get(0),
        )?;
        let active: i64 = tx.query_row(
            "SELECT COALESCE(SUM(amount), 0) FROM shared_pool_reservations
             WHERE pool_id = ?1 AND state = 'active'",
            [&pool_id.0],
            |row| row.get(0),
        )?;
        Ok((settled as u64, active as u64))
    }

    /// The freshest ingested provider gauge reading for `pool_id`, if any
    /// was ever recorded via [`Self::ingest_pool_provider_snapshot`] — read
    /// inside the caller's own transaction so it observes the same
    /// instant as the settled/active sums.
    #[allow(clippy::type_complexity)]
    /// Sums only `settled` reservations whose own `settled_at` is at or
    /// before `observed_at` — i.e. Libra-side activity the snapshot
    /// could structurally already include, because it existed before
    /// the provider observed its own `used` figure. A `settled` row
    /// that postdates the snapshot can never be part of what the
    /// snapshot measured, and `active` holds are never subtracted here
    /// at all (see [`Self::external_overhang_tx`]'s own docs for why).
    fn settled_before_tx(
        tx: &rusqlite::Connection,
        pool_id: &PoolId,
        observed_at: OffsetDateTime,
    ) -> Result<u64, LedgerError> {
        // Comparing RFC3339 strings with a SQL `<=` is a real
        // parser-differential bug, not a theoretical one: the `time`
        // crate's Rfc3339 formatter omits the fractional-second
        // component entirely when it is zero, so two timestamps sharing
        // the same whole second but differing in fractional precision
        // ("...T00:00:01Z" vs "...T00:00:01.500Z") do not sort the same
        // lexically as they do chronologically — "01." (0x2E) sorts
        // before "01Z" (0x5A) byte-for-byte, inverting a comparison that
        // should go the other way. Parsing every candidate row's own
        // timestamp and comparing as `OffsetDateTime` values sidesteps
        // the representation entirely, at the cost of pulling
        // (typically few) rows into Rust instead of summing in SQL.
        let mut stmt = tx.prepare(
            "SELECT settled_amount, settled_at FROM shared_pool_reservations
             WHERE pool_id = ?1 AND state = 'settled'",
        )?;
        let rows = stmt.query_map([&pool_id.0], |row| {
            let settled_amount: Option<i64> = row.get(0)?;
            let settled_at: Option<String> = row.get(1)?;
            Ok((settled_amount, settled_at))
        })?;
        let mut total: u64 = 0;
        for row in rows {
            let (settled_amount, settled_at) = row?;
            let (Some(settled_amount), Some(settled_at)) = (settled_amount, settled_at) else {
                continue;
            };
            if parse_time(&settled_at)? <= observed_at {
                total = total.saturating_add(settled_amount.max(0) as u64);
            }
        }
        Ok(total)
    }

    /// Computes how much of a provider snapshot's `used` figure is
    /// genuinely *external* — activity Libra has no record of at all —
    /// as opposed to activity Libra already knows about and must not
    /// double-count as spend.
    ///
    /// Two independent-review findings (HORO-1779) shaped this
    /// function's current arithmetic, both confirmed by a real
    /// oversubscription reproduction and empirically verified by a
    /// reverted source patch before being accepted as correct:
    ///
    /// - `active` is **never** subtracted here. A still-`active` hold is
    ///   Libra's own *reserved* capacity, not evidence the provider's
    ///   `used` figure already reflects it — subtracting it let a
    ///   snapshot's overhang silently zero out the moment Libra's own
    ///   active sum caught up to the snapshot's `used` value, even
    ///   though the real external activity the snapshot measured was
    ///   never retracted (a 10-unit pool could be driven to 14 real
    ///   units held this way).
    /// - Only `settled` rows whose `settled_at` is at or before the
    ///   snapshot's own `observed_at` are subtracted — a settlement that
    ///   happened *after* the snapshot was observed cannot possibly be
    ///   part of what the snapshot measured, so including it in the
    ///   subtraction masks real overhang the same way the `active` bug
    ///   did, just via the settled path instead.
    #[allow(clippy::type_complexity)]
    fn external_overhang_tx(
        tx: &rusqlite::Connection,
        pool_id: &PoolId,
        now: OffsetDateTime,
    ) -> Result<u64, LedgerError> {
        let row: Option<(String, Option<String>, i64, Option<i64>, Option<i64>)> = tx
            .query_row(
                "SELECT observed_at, valid_until, disclosed, used_value, declared_limit
                 FROM shared_pool_provider_snapshots WHERE pool_id = ?1",
                [&pool_id.0],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .optional()?;
        let Some((observed_at, valid_until, disclosed, used_value, _declared_limit)) = row else {
            return Ok(0);
        };
        // A stale snapshot (past its own declared `valid_until`) is
        // ignored, not trusted as current evidence — the same "never
        // collapse a stale reading into a trusted one" discipline
        // `quota_window::evaluate_gauge` applies.
        if let Some(valid_until) = valid_until {
            if parse_time(&valid_until)? <= now {
                return Ok(0);
            }
        }
        // Undisclosed is a real, distinct state: it is evidence that
        // *something* may be happening externally, but not a specific
        // quantity — it cannot be subtracted as a number it was never
        // given. It stays visible via `PoolSnapshot`'s own snapshot
        // field, never guessed into this arithmetic.
        if disclosed == 0 {
            return Ok(0);
        }
        let Some(used_value) = used_value else {
            return Ok(0);
        };
        let used_value = used_value.max(0) as u64;
        let observed_at = parse_time(&observed_at)?;
        let settled_before = Self::settled_before_tx(tx, pool_id, observed_at)?;
        Ok(used_value.saturating_sub(settled_before))
    }

    /// Computes [`PoolAdmission`] for `pool_id` as of `now`, inside the
    /// caller's own transaction/connection. `Ok(None)` when no pool has
    /// been created.
    pub fn pool_admission(
        &self,
        pool_id: &PoolId,
        now: OffsetDateTime,
    ) -> Result<Option<PoolAdmission>, LedgerError> {
        let Some(pool) = self.quota_pool(pool_id)? else {
            return Ok(None);
        };
        let (settled, active) = Self::settled_and_active_tx(&self.conn, pool_id)?;
        let external_overhang = Self::external_overhang_tx(&self.conn, pool_id, now)?;
        Ok(Some(PoolAdmission {
            capacity: pool.capacity,
            settled,
            active,
            external_overhang,
        }))
    }

    /// Atomically reserves `req.amount` of a shared pool's capacity. See
    /// module docs for the transaction/idempotency guarantees that make
    /// AC1 (no combined oversubscription under concurrency) hold.
    pub fn reserve_shared(
        &mut self,
        req: SharedPoolReserveRequest<'_>,
    ) -> Result<SharedPoolReserveOutcome, LedgerError> {
        let tx = begin(&mut self.conn, TransactionBehavior::Immediate)?;

        // Scoped to (pool_id, principal_id, idempotency_key) — an
        // idempotency key is a caller-chosen string with no uniqueness
        // guarantee across different principals sharing the same pool.
        // Matching on (pool_id, idempotency_key) alone would let one
        // principal's lookup collide with a different principal's row,
        // handing back someone else's reservation (its principal_id,
        // session_id, amount) as if it were this caller's own idempotent
        // replay. See migration 0016's own docs for the same discipline
        // applied to the unique index this query relies on.
        let existing: Option<ReservationRow> = tx
            .query_row(
                &format!(
                    "SELECT {RESERVATION_COLUMNS}
                     FROM shared_pool_reservations
                     WHERE pool_id = ?1 AND principal_id = ?2 AND idempotency_key = ?3"
                ),
                rusqlite::params![req.pool_id.0, req.principal_id.0, req.idempotency_key],
                read_reservation_row,
            )
            .optional()?;
        if let Some(row) = existing {
            tx.commit()?;
            return Ok(SharedPoolReserveOutcome::AlreadyGranted(Box::new(
                row_to_reservation(row)?,
            )));
        }

        let pool_row: Option<PoolRow> = tx
            .query_row(
                "SELECT unit_json, capacity, schema_version, created_at, updated_at
                 FROM quota_pools WHERE pool_id = ?1",
                [&req.pool_id.0],
                |row| {
                    let capacity: i64 = row.get(1)?;
                    Ok((
                        row.get(0)?,
                        capacity as u64,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .optional()?;
        let Some(pool_row) = pool_row else {
            tx.commit()?;
            return Ok(SharedPoolReserveOutcome::NoPool);
        };
        let pool = row_to_pool(req.pool_id.clone(), pool_row)?;

        if *req.unit != pool.unit {
            return Err(LedgerError::QuotaUnitMismatch {
                expected: pool.unit,
                actual: req.unit.clone(),
            });
        }

        let (settled, active) = Self::settled_and_active_tx(&tx, req.pool_id)?;
        let external_overhang = Self::external_overhang_tx(&tx, req.pool_id, req.now)?;
        let admission = PoolAdmission {
            capacity: pool.capacity,
            settled,
            active,
            external_overhang,
        };

        if !admission.admits(req.amount) {
            tx.commit()?;
            return Ok(SharedPoolReserveOutcome::Insufficient {
                admission,
                requested: req.amount,
            });
        }

        let id = SharedPoolReservationId::new();
        // `ttl_secs` is caller-supplied; a bare `Duration::seconds(ttl_secs
        // as i64)` followed by `now + duration` can both truncate (a
        // `ttl_secs` above `i64::MAX`) and panic (`OffsetDateTime`
        // arithmetic panics outside its representable range) — the same
        // bug class flagged for proactive avoidance in all new code here.
        //
        // An earlier version of this code fell back to `req.now` (i.e.
        // "expires immediately") on overflow. An independent Verify pass
        // (HORO-1779) found this was the opposite of a safe fallback: a
        // caller requesting an effectively-unlimited TTL (a realistic
        // sentinel for a long-running session) had its hold swept as
        // stale on the very next sweep, and its capacity re-granted to a
        // second caller — a genuine phantom-free-allowance bug, not a
        // conservative one. Clamping `ttl_secs` to a generous-but-finite
        // bound (mirroring `quota_window::MAX_SLIDING_LENGTH_SECS`'s own
        // 100-year precedent) instead means `checked_add` below can never
        // actually need a fallback branch.
        let ttl_seconds = i64::try_from(req.ttl_secs.min(MAX_TTL_SECS)).unwrap_or(i64::MAX);
        let expires_at = req
            .now
            .checked_add(time::Duration::seconds(ttl_seconds))
            .ok_or_else(invalid)?;
        let amount_i64 = checked_i64(req.amount)?;
        tx.execute(
            "INSERT INTO shared_pool_reservations (
                id, pool_id, principal_id, session_id, amount, state, settled_amount,
                usage_known, idempotency_key, created_at, expires_at, settled_at, released_at,
                schema_version
             ) VALUES (?1, ?2, ?3, ?4, ?5, 'active', NULL, NULL, ?6, ?7, ?8, NULL, NULL, ?9)",
            rusqlite::params![
                id.0.to_string(),
                req.pool_id.0,
                req.principal_id.0,
                req.session_id,
                amount_i64,
                req.idempotency_key,
                rfc3339(req.now)?,
                rfc3339(expires_at)?,
                SHARED_POOL_RESERVATION_SCHEMA_VERSION,
            ],
        )?;
        tx.commit()?;

        Ok(SharedPoolReserveOutcome::Granted(Box::new(
            SharedPoolReservation {
                id,
                pool_id: req.pool_id.clone(),
                principal_id: req.principal_id.clone(),
                session_id: req.session_id.to_string(),
                amount: req.amount,
                state: ReservationState::Active,
                settled_amount: None,
                usage_known: None,
                idempotency_key: req.idempotency_key.to_string(),
                created_at: req.now,
                expires_at,
                settled_at: None,
                released_at: None,
                schema_version: SHARED_POOL_RESERVATION_SCHEMA_VERSION.to_string(),
            },
        )))
    }

    fn get_shared_reservation_tx(
        tx: &rusqlite::Connection,
        id: SharedPoolReservationId,
    ) -> Result<Option<SharedPoolReservation>, LedgerError> {
        let row: Option<ReservationRow> = tx
            .query_row(
                &format!(
                    "SELECT {RESERVATION_COLUMNS} FROM shared_pool_reservations WHERE id = ?1"
                ),
                [id.0.to_string()],
                read_reservation_row,
            )
            .optional()?;
        row.map(row_to_reservation).transpose()
    }

    /// Reads one shared-pool hold by id, outside any transaction.
    pub fn get_shared_reservation(
        &self,
        id: SharedPoolReservationId,
    ) -> Result<Option<SharedPoolReservation>, LedgerError> {
        let row: Option<ReservationRow> = self
            .conn
            .query_row(
                &format!(
                    "SELECT {RESERVATION_COLUMNS} FROM shared_pool_reservations WHERE id = ?1"
                ),
                [id.0.to_string()],
                read_reservation_row,
            )
            .optional()?;
        row.map(row_to_reservation).transpose()
    }

    /// Atomically settles shared-pool hold `id` with `actual` cost.
    /// `actual == None` is the conservative fallback: settles at the full
    /// reserved amount, mirroring [`crate::reservation::LedgerStore::settle`].
    /// Idempotent.
    pub fn settle_shared(
        &mut self,
        id: SharedPoolReservationId,
        actual: Option<u64>,
        now: OffsetDateTime,
    ) -> Result<SharedPoolSettleOutcome, LedgerError> {
        let tx = begin(&mut self.conn, TransactionBehavior::Immediate)?;
        let Some(reservation) = Self::get_shared_reservation_tx(&tx, id)? else {
            tx.commit()?;
            return Ok(SharedPoolSettleOutcome::NotFound);
        };
        // Late settlement after expiry: still records the real spend as
        // evidence (mirrors `crate::reservation::settle`'s
        // `late_after_expiry` handling) rather than silently discarding
        // it — an overrun surfaces instead of vanishing.
        let late_after_expiry = reservation.state == ReservationState::Expired;
        if reservation.state != ReservationState::Active && !late_after_expiry {
            tx.commit()?;
            return Ok(SharedPoolSettleOutcome::AlreadyFinal(Box::new(reservation)));
        }

        let settled_value = actual.unwrap_or(reservation.amount);
        let settled_value_i64 = checked_i64(settled_value)?;
        let usage_known = actual.is_some();

        let expected_prior_state = if late_after_expiry {
            "expired"
        } else {
            "active"
        };
        let updated = tx.execute(
            "UPDATE shared_pool_reservations SET state = 'settled', settled_amount = ?1,
                                                  usage_known = ?2, settled_at = ?3
             WHERE id = ?4 AND state = ?5",
            rusqlite::params![
                settled_value_i64,
                usage_known,
                rfc3339(now)?,
                id.0.to_string(),
                expected_prior_state,
            ],
        )?;
        if updated == 0 {
            // Lost a race against a concurrent settle/release/expire on
            // this same row between the read above and this write.
            let current = Self::get_shared_reservation_tx(&tx, id)?
                .expect("row existed moments ago under the same write lock");
            tx.commit()?;
            return Ok(SharedPoolSettleOutcome::AlreadyFinal(Box::new(current)));
        }
        let updated_reservation = Self::get_shared_reservation_tx(&tx, id)?
            .expect("row just updated in this same transaction");
        tx.commit()?;

        Ok(SharedPoolSettleOutcome::Settled {
            refunded: updated_reservation.refunded(),
            overrun: updated_reservation.overrun(),
            reservation: Box::new(updated_reservation),
        })
    }

    /// Atomically releases shared-pool hold `id` unspent. Idempotent,
    /// mirror of [`Self::settle_shared`].
    pub fn release_shared(
        &mut self,
        id: SharedPoolReservationId,
        now: OffsetDateTime,
    ) -> Result<SharedPoolReleaseOutcome, LedgerError> {
        let tx = begin(&mut self.conn, TransactionBehavior::Immediate)?;
        let Some(reservation) = Self::get_shared_reservation_tx(&tx, id)? else {
            tx.commit()?;
            return Ok(SharedPoolReleaseOutcome::NotFound);
        };
        if reservation.state != ReservationState::Active {
            tx.commit()?;
            return Ok(SharedPoolReleaseOutcome::AlreadyFinal(Box::new(
                reservation,
            )));
        }

        let updated = tx.execute(
            "UPDATE shared_pool_reservations SET state = 'released', released_at = ?1
             WHERE id = ?2 AND state = 'active'",
            rusqlite::params![rfc3339(now)?, id.0.to_string()],
        )?;
        if updated == 0 {
            let current = Self::get_shared_reservation_tx(&tx, id)?
                .expect("row existed moments ago under the same write lock");
            tx.commit()?;
            return Ok(SharedPoolReleaseOutcome::AlreadyFinal(Box::new(current)));
        }
        let updated_reservation = Self::get_shared_reservation_tx(&tx, id)?
            .expect("row just updated in this same transaction");
        tx.commit()?;
        Ok(SharedPoolReleaseOutcome::Released(Box::new(
            updated_reservation,
        )))
    }

    /// Reclaims every `active` shared-pool hold whose `expires_at <= now`
    /// in one transaction — the crash/restart reconciliation path (AC2):
    /// a hold issued by a process that then crashed without settling
    /// stays `active` (correctly still counted against the pool) until
    /// this sweep reclaims it.
    pub fn expire_stale_shared_pool_reservations(
        &mut self,
        now: OffsetDateTime,
    ) -> Result<Vec<SharedPoolReservation>, LedgerError> {
        let now_str = rfc3339(now)?;
        let tx = begin(&mut self.conn, TransactionBehavior::Immediate)?;
        // Parsed-timestamp comparison, not a SQL `<=` on the RFC3339
        // text — the same parser-differential risk fixed in
        // `settled_before_tx` applies here too: `now` can carry
        // fractional seconds (e.g. a real `OffsetDateTime::now_utc()`
        // call) that a lexical string comparison against `expires_at`
        // would not order chronologically in every case.
        let ids: Vec<String> = {
            let mut stmt = tx.prepare(
                "SELECT id, expires_at FROM shared_pool_reservations WHERE state = 'active'",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            let mut ids = Vec::new();
            for row in rows {
                let (id, expires_at) = row?;
                if parse_time(&expires_at)? <= now {
                    ids.push(id);
                }
            }
            ids
        };

        let mut expired = Vec::with_capacity(ids.len());
        for id_str in ids {
            let id =
                SharedPoolReservationId(uuid::Uuid::parse_str(&id_str).map_err(|_| invalid())?);
            tx.execute(
                "UPDATE shared_pool_reservations SET state = 'expired', released_at = ?1 WHERE id = ?2",
                rusqlite::params![now_str, id_str],
            )?;
            expired.push(
                Self::get_shared_reservation_tx(&tx, id)?
                    .expect("id came from a query against this same transaction, just updated"),
            );
        }
        tx.commit()?;
        Ok(expired)
    }

    /// Ingests a provider-declared gauge reading for `pool_id`, replacing
    /// whatever snapshot was previously recorded for it (AC3: there is
    /// structurally only ever one row to read, so a snapshot can never be
    /// accumulated/summed as spend). A duplicate/replayed ingest of the
    /// same reading is a safe no-op in effect, not just in intent — the
    /// upsert overwrites with the same values.
    pub fn ingest_pool_provider_snapshot(
        &mut self,
        pool_id: &PoolId,
        observed_at: OffsetDateTime,
        valid_until: Option<OffsetDateTime>,
        reading: &GaugeReading,
        confidence: Confidence,
        now: OffsetDateTime,
    ) -> Result<(), LedgerError> {
        let (disclosed, used_value, declared_limit) = match reading {
            GaugeReading::Undisclosed => (false, None, None),
            GaugeReading::Used { used, limit } => (
                true,
                Some(checked_i64(used.value)?),
                limit.map(checked_i64).transpose()?,
            ),
        };
        let confidence_str = serde_json::to_string(&confidence).map_err(|e| {
            LedgerError::Sqlite(rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
        })?;
        let tx = begin(&mut self.conn, TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO shared_pool_provider_snapshots (
                pool_id, observed_at, valid_until, disclosed, used_value, declared_limit,
                confidence, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(pool_id) DO UPDATE SET
                observed_at = excluded.observed_at,
                valid_until = excluded.valid_until,
                disclosed = excluded.disclosed,
                used_value = excluded.used_value,
                declared_limit = excluded.declared_limit,
                confidence = excluded.confidence,
                updated_at = excluded.updated_at",
            rusqlite::params![
                pool_id.0,
                rfc3339(observed_at)?,
                valid_until.map(rfc3339).transpose()?,
                disclosed,
                used_value,
                declared_limit,
                confidence_str,
                rfc3339(now)?,
            ],
        )?;
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libra_governor_domain::QuotaUnit;

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap()
    }

    fn pool_id() -> PoolId {
        PoolId("shared-quota".to_string())
    }

    fn principal(n: u32) -> PrincipalId {
        PrincipalId(format!("principal-{n}"))
    }

    fn reserve(
        store: &mut LedgerStore,
        session_id: &str,
        amount: u64,
        idempotency_key: &str,
    ) -> SharedPoolReserveOutcome {
        store
            .reserve_shared(SharedPoolReserveRequest {
                pool_id: &pool_id(),
                principal_id: &principal(1),
                session_id,
                amount,
                unit: &QuotaUnit::Tokens,
                idempotency_key,
                now: now(),
                ttl_secs: 900,
            })
            .unwrap()
    }

    #[test]
    fn grants_up_to_capacity_then_refuses() {
        let mut store = LedgerStore::open_in_memory().unwrap();
        store
            .ensure_pool(&pool_id(), &QuotaUnit::Tokens, 10, now())
            .unwrap();

        assert!(matches!(
            reserve(&mut store, "s1", 6, "k1"),
            SharedPoolReserveOutcome::Granted(_)
        ));
        assert!(matches!(
            reserve(&mut store, "s2", 4, "k2"),
            SharedPoolReserveOutcome::Granted(_)
        ));
        // Exactly 10 reserved; one more unit must be refused (AC1).
        match reserve(&mut store, "s3", 1, "k3") {
            SharedPoolReserveOutcome::Insufficient {
                admission,
                requested,
            } => {
                assert_eq!(admission.remaining(), 0);
                assert_eq!(requested, 1);
            }
            other => panic!("expected Insufficient, got {other:?}"),
        }
    }

    #[test]
    fn reserve_is_idempotent_on_pool_and_key() {
        let mut store = LedgerStore::open_in_memory().unwrap();
        store
            .ensure_pool(&pool_id(), &QuotaUnit::Tokens, 10, now())
            .unwrap();
        let first = reserve(&mut store, "s1", 5, "same-key");
        let second = reserve(&mut store, "s1", 5, "same-key");
        let (SharedPoolReserveOutcome::Granted(a), SharedPoolReserveOutcome::AlreadyGranted(b)) =
            (first, second)
        else {
            panic!("expected Granted then AlreadyGranted");
        };
        assert_eq!(a.id, b.id);
        // A third distinct key must still see only 5 consumed, not 10:
        // the remaining 5 units are available.
        match reserve(&mut store, "s2", 5, "distinct-key") {
            SharedPoolReserveOutcome::Granted(_) => {}
            other => {
                panic!("expected Granted (5 consumed, 5 requested, 10 capacity), got {other:?}")
            }
        }
    }

    #[test]
    fn cross_principal_idempotency_key_reuse_is_not_merged() {
        // A different principal reusing the exact same idempotency_key
        // against the same pool must get its OWN reservation, never the
        // first principal's — scoping idempotency to (pool_id,
        // principal_id, idempotency_key) rather than (pool_id,
        // idempotency_key) is what this test guards against regressing.
        let mut store = LedgerStore::open_in_memory().unwrap();
        store
            .ensure_pool(&pool_id(), &QuotaUnit::Tokens, 10, now())
            .unwrap();

        let from_principal_1 = store
            .reserve_shared(SharedPoolReserveRequest {
                pool_id: &pool_id(),
                principal_id: &principal(1),
                session_id: "s1",
                amount: 3,
                unit: &QuotaUnit::Tokens,
                idempotency_key: "shared-key",
                now: now(),
                ttl_secs: 900,
            })
            .unwrap();
        let from_principal_2 = store
            .reserve_shared(SharedPoolReserveRequest {
                pool_id: &pool_id(),
                principal_id: &principal(2),
                session_id: "s2",
                amount: 4,
                unit: &QuotaUnit::Tokens,
                idempotency_key: "shared-key",
                now: now(),
                ttl_secs: 900,
            })
            .unwrap();

        let (SharedPoolReserveOutcome::Granted(a), SharedPoolReserveOutcome::Granted(b)) =
            (from_principal_1, from_principal_2)
        else {
            panic!("expected both principals to be independently Granted");
        };
        assert_ne!(
            a.id, b.id,
            "two different principals sharing an idempotency key must never collapse to one row"
        );
        assert_eq!(a.principal_id, principal(1));
        assert_eq!(b.principal_id, principal(2));
        assert_eq!(a.amount, 3);
        assert_eq!(b.amount, 4);

        // Both amounts are actually held against the pool (3 + 4 = 7),
        // not just one of them — proving the second call did not read
        // back the first principal's row as its own.
        let admission = store.pool_admission(&pool_id(), now()).unwrap().unwrap();
        assert_eq!(admission.active, 7);

        // Each principal's OWN replay of their own key is still a true
        // idempotent no-op.
        let replay_1 = store
            .reserve_shared(SharedPoolReserveRequest {
                pool_id: &pool_id(),
                principal_id: &principal(1),
                session_id: "s1",
                amount: 3,
                unit: &QuotaUnit::Tokens,
                idempotency_key: "shared-key",
                now: now(),
                ttl_secs: 900,
            })
            .unwrap();
        match replay_1 {
            SharedPoolReserveOutcome::AlreadyGranted(r) => assert_eq!(r.id, a.id),
            other => panic!("expected AlreadyGranted for principal 1's own replay, got {other:?}"),
        }
    }

    #[test]
    fn reserving_against_a_nonexistent_pool_reports_no_pool() {
        let mut store = LedgerStore::open_in_memory().unwrap();
        match reserve(&mut store, "s1", 1, "k1") {
            SharedPoolReserveOutcome::NoPool => {}
            other => panic!("expected NoPool, got {other:?}"),
        }
    }

    #[test]
    fn settle_with_no_actual_falls_back_to_full_reserved_amount() {
        let mut store = LedgerStore::open_in_memory().unwrap();
        store
            .ensure_pool(&pool_id(), &QuotaUnit::Tokens, 10, now())
            .unwrap();
        let SharedPoolReserveOutcome::Granted(reservation) = reserve(&mut store, "s1", 7, "k1")
        else {
            panic!("expected Granted");
        };
        let outcome = store.settle_shared(reservation.id, None, now()).unwrap();
        let SharedPoolSettleOutcome::Settled {
            reservation,
            refunded,
            overrun,
        } = outcome
        else {
            panic!("expected Settled");
        };
        assert_eq!(reservation.settled_amount, Some(7));
        assert_eq!(reservation.usage_known, Some(false));
        assert_eq!(refunded, None);
        assert_eq!(overrun, None);

        // Settled capacity remains fully accounted for: a fresh hold for
        // the remaining 3 units succeeds, a 4th unit does not (AC1 holds
        // across settle, not just across active holds).
        assert!(matches!(
            reserve(&mut store, "s2", 3, "k2"),
            SharedPoolReserveOutcome::Granted(_)
        ));
        assert!(matches!(
            reserve(&mut store, "s3", 1, "k3"),
            SharedPoolReserveOutcome::Insufficient { .. }
        ));
    }

    #[test]
    fn settle_refunds_unspent_capacity() {
        let mut store = LedgerStore::open_in_memory().unwrap();
        store
            .ensure_pool(&pool_id(), &QuotaUnit::Tokens, 10, now())
            .unwrap();
        let SharedPoolReserveOutcome::Granted(reservation) = reserve(&mut store, "s1", 7, "k1")
        else {
            panic!("expected Granted");
        };
        store.settle_shared(reservation.id, Some(4), now()).unwrap();
        // 4 settled, 6 freed back up: a 6-unit hold now succeeds.
        assert!(matches!(
            reserve(&mut store, "s2", 6, "k2"),
            SharedPoolReserveOutcome::Granted(_)
        ));
    }

    #[test]
    fn settle_and_release_are_idempotent_on_terminal_state() {
        let mut store = LedgerStore::open_in_memory().unwrap();
        store
            .ensure_pool(&pool_id(), &QuotaUnit::Tokens, 10, now())
            .unwrap();
        let SharedPoolReserveOutcome::Granted(reservation) = reserve(&mut store, "s1", 5, "k1")
        else {
            panic!("expected Granted");
        };
        store.settle_shared(reservation.id, Some(5), now()).unwrap();
        match store.settle_shared(reservation.id, Some(1), now()).unwrap() {
            SharedPoolSettleOutcome::AlreadyFinal(r) => assert_eq!(r.settled_amount, Some(5)),
            other => panic!("expected AlreadyFinal, got {other:?}"),
        }
        match store.release_shared(reservation.id, now()).unwrap() {
            SharedPoolReleaseOutcome::AlreadyFinal(_) => {}
            other => panic!("expected AlreadyFinal, got {other:?}"),
        }
    }

    #[test]
    fn release_restores_full_capacity() {
        let mut store = LedgerStore::open_in_memory().unwrap();
        store
            .ensure_pool(&pool_id(), &QuotaUnit::Tokens, 10, now())
            .unwrap();
        let SharedPoolReserveOutcome::Granted(reservation) = reserve(&mut store, "s1", 10, "k1")
        else {
            panic!("expected Granted");
        };
        assert!(matches!(
            reserve(&mut store, "s2", 1, "k2"),
            SharedPoolReserveOutcome::Insufficient { .. }
        ));
        store.release_shared(reservation.id, now()).unwrap();
        assert!(matches!(
            reserve(&mut store, "s2", 10, "k3"),
            SharedPoolReserveOutcome::Granted(_)
        ));
    }

    #[test]
    fn expiry_reclaims_capacity_from_a_holder_that_never_settled() {
        let mut store = LedgerStore::open_in_memory().unwrap();
        store
            .ensure_pool(&pool_id(), &QuotaUnit::Tokens, 10, now())
            .unwrap();
        store
            .reserve_shared(SharedPoolReserveRequest {
                pool_id: &pool_id(),
                principal_id: &principal(1),
                session_id: "crashed-session",
                amount: 10,
                unit: &QuotaUnit::Tokens,
                idempotency_key: "k1",
                now: now(),
                ttl_secs: 60,
            })
            .unwrap();
        // Before expiry: no phantom free allowance.
        assert!(matches!(
            reserve(&mut store, "s2", 1, "k2"),
            SharedPoolReserveOutcome::Insufficient { .. }
        ));
        let later = now() + time::Duration::seconds(120);
        let expired = store.expire_stale_shared_pool_reservations(later).unwrap();
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].state, ReservationState::Expired);
        // After expiry: capacity is reclaimed.
        match store
            .reserve_shared(SharedPoolReserveRequest {
                pool_id: &pool_id(),
                principal_id: &principal(2),
                session_id: "s2",
                amount: 10,
                unit: &QuotaUnit::Tokens,
                idempotency_key: "k2",
                now: later,
                ttl_secs: 60,
            })
            .unwrap()
        {
            SharedPoolReserveOutcome::Granted(_) => {}
            other => panic!("expected Granted, got {other:?}"),
        }
    }

    #[test]
    fn a_crash_equivalent_restart_leaves_no_phantom_free_allowance() {
        // AC2: reopen a fresh LedgerStore against the same on-disk file
        // mid-"process lifetime" — no store-level state survives this
        // boundary except what is committed in SQLite.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.sqlite3");
        {
            let mut store = LedgerStore::open(&path).unwrap();
            store
                .ensure_pool(&pool_id(), &QuotaUnit::Tokens, 10, now())
                .unwrap();
            store
                .reserve_shared(SharedPoolReserveRequest {
                    pool_id: &pool_id(),
                    principal_id: &principal(1),
                    session_id: "s1",
                    amount: 10,
                    unit: &QuotaUnit::Tokens,
                    idempotency_key: "k1",
                    now: now(),
                    ttl_secs: 900,
                })
                .unwrap();
            // Simulated crash: store is dropped here without settling.
        }
        let mut reopened = LedgerStore::open(&path).unwrap();
        match reopened
            .reserve_shared(SharedPoolReserveRequest {
                pool_id: &pool_id(),
                principal_id: &principal(2),
                session_id: "s2",
                amount: 1,
                unit: &QuotaUnit::Tokens,
                idempotency_key: "k2",
                now: now(),
                ttl_secs: 900,
            })
            .unwrap()
        {
            SharedPoolReserveOutcome::Insufficient { admission, .. } => {
                assert_eq!(admission.remaining(), 0);
            }
            other => panic!("expected Insufficient after reopen, got {other:?}"),
        }
    }

    #[test]
    fn duplicate_settlement_receipt_never_double_frees_capacity() {
        let mut store = LedgerStore::open_in_memory().unwrap();
        store
            .ensure_pool(&pool_id(), &QuotaUnit::Tokens, 10, now())
            .unwrap();
        let SharedPoolReserveOutcome::Granted(reservation) = reserve(&mut store, "s1", 10, "k1")
        else {
            panic!("expected Granted");
        };
        // First settlement refunds down to 3 actually spent.
        store.settle_shared(reservation.id, Some(3), now()).unwrap();
        // A duplicate/replayed settlement receipt for the same id must
        // not apply a second time (e.g. re-refunding or re-debiting).
        let replay = store.settle_shared(reservation.id, Some(9), now()).unwrap();
        match replay {
            SharedPoolSettleOutcome::AlreadyFinal(r) => assert_eq!(r.settled_amount, Some(3)),
            other => panic!("expected AlreadyFinal, got {other:?}"),
        }
    }

    #[test]
    fn provider_snapshot_gauge_is_never_summed_as_spend() {
        let mut store = LedgerStore::open_in_memory().unwrap();
        store
            .ensure_pool(&pool_id(), &QuotaUnit::Tokens, 10, now())
            .unwrap();
        // Ingest the SAME snapshot three times (simulating repeat
        // delivery) — must still contribute as exactly one reading, never
        // accumulate.
        for _ in 0..3 {
            store
                .ingest_pool_provider_snapshot(
                    &pool_id(),
                    now(),
                    Some(now() + time::Duration::seconds(3600)),
                    &GaugeReading::Used {
                        used: libra_governor_domain::QuotaAmount::new(QuotaUnit::Tokens, 4),
                        limit: Some(10),
                    },
                    Confidence::High,
                    now(),
                )
                .unwrap();
        }
        let admission = store.pool_admission(&pool_id(), now()).unwrap().unwrap();
        assert_eq!(admission.settled, 0);
        assert_eq!(admission.active, 0);
        // Provider shows 4 used that Libra never observed as settled/
        // active -> conservative overhang of 4, remaining = 6.
        assert_eq!(admission.external_overhang, 4);
        assert_eq!(admission.remaining(), 6);
        assert!(admission.admits(6));
        assert!(!admission.admits(7));
    }

    #[test]
    fn stale_snapshot_contributes_no_overhang() {
        let mut store = LedgerStore::open_in_memory().unwrap();
        store
            .ensure_pool(&pool_id(), &QuotaUnit::Tokens, 10, now())
            .unwrap();
        store
            .ingest_pool_provider_snapshot(
                &pool_id(),
                now(),
                Some(now() + time::Duration::seconds(60)),
                &GaugeReading::Used {
                    used: libra_governor_domain::QuotaAmount::new(QuotaUnit::Tokens, 9),
                    limit: Some(10),
                },
                Confidence::High,
                now(),
            )
            .unwrap();
        let later = now() + time::Duration::seconds(3600);
        let admission = store.pool_admission(&pool_id(), later).unwrap().unwrap();
        assert_eq!(
            admission.external_overhang, 0,
            "a snapshot past its own valid_until must not be trusted as current evidence"
        );
        assert_eq!(admission.remaining(), 10);
    }

    #[test]
    fn undisclosed_snapshot_contributes_no_numeric_overhang() {
        let mut store = LedgerStore::open_in_memory().unwrap();
        store
            .ensure_pool(&pool_id(), &QuotaUnit::Tokens, 10, now())
            .unwrap();
        store
            .ingest_pool_provider_snapshot(
                &pool_id(),
                now(),
                None,
                &GaugeReading::Undisclosed,
                Confidence::Low,
                now(),
            )
            .unwrap();
        let admission = store.pool_admission(&pool_id(), now()).unwrap().unwrap();
        assert_eq!(admission.external_overhang, 0);
        assert_eq!(admission.remaining(), 10);
    }

    #[test]
    fn unit_mismatch_is_rejected_not_silently_coerced() {
        let mut store = LedgerStore::open_in_memory().unwrap();
        store
            .ensure_pool(&pool_id(), &QuotaUnit::Tokens, 10, now())
            .unwrap();
        let outcome = store.reserve_shared(SharedPoolReserveRequest {
            pool_id: &pool_id(),
            principal_id: &principal(1),
            session_id: "s1",
            amount: 1,
            unit: &QuotaUnit::Requests,
            idempotency_key: "k1",
            now: now(),
            ttl_secs: 60,
        });
        assert!(matches!(
            outcome,
            Err(LedgerError::QuotaUnitMismatch { .. })
        ));
    }

    #[test]
    fn capacity_above_i64_max_is_rejected_not_wrapped() {
        // A bare `capacity as i64` would silently wrap a value above
        // `i64::MAX` into a negative number — fail-open territory (a
        // negative stored capacity would make every subsequent admission
        // check nonsensical in the attacker's favor). `ensure_pool` must
        // reject it outright instead.
        let mut store = LedgerStore::open_in_memory().unwrap();
        let outcome = store.ensure_pool(&pool_id(), &QuotaUnit::Tokens, u64::MAX, now());
        assert!(
            outcome.is_err(),
            "expected capacity overflow to be rejected, got {outcome:?}"
        );
    }

    #[test]
    fn reserve_amount_above_i64_max_is_rejected_not_wrapped() {
        // `admission.admits()` uses i128 arithmetic, so an absurd request
        // against a (bounded) pool capacity is already caught as
        // `Insufficient` before the i64 cast guard is ever reached — a
        // request this large can never be legitimately granted in the
        // first place, since `ensure_pool` itself rejects a capacity
        // above `i64::MAX` (see `capacity_above_i64_max_is_rejected_not_wrapped`),
        // which bounds every admissible amount well under `i64::MAX` too.
        // Either way, the outcome here must be a rejection, never a
        // silent grant of a wrapped negative amount.
        let mut store = LedgerStore::open_in_memory().unwrap();
        store
            .ensure_pool(&pool_id(), &QuotaUnit::Tokens, 10, now())
            .unwrap();
        let outcome = store
            .reserve_shared(SharedPoolReserveRequest {
                pool_id: &pool_id(),
                principal_id: &principal(1),
                session_id: "s1",
                amount: u64::MAX,
                unit: &QuotaUnit::Tokens,
                idempotency_key: "k1",
                now: now(),
                ttl_secs: 60,
            })
            .unwrap();
        assert!(
            matches!(outcome, SharedPoolReserveOutcome::Insufficient { .. }),
            "expected Insufficient, got {outcome:?}"
        );
        // And critically: the rejected request must not have been
        // admitted/written — capacity stays fully available.
        let admission = store.pool_admission(&pool_id(), now()).unwrap().unwrap();
        assert_eq!(admission.active, 0);
    }

    #[test]
    fn settle_actual_above_i64_max_is_rejected_not_wrapped() {
        let mut store = LedgerStore::open_in_memory().unwrap();
        store
            .ensure_pool(&pool_id(), &QuotaUnit::Tokens, 10, now())
            .unwrap();
        let SharedPoolReserveOutcome::Granted(reservation) = reserve(&mut store, "s1", 5, "k1")
        else {
            panic!("expected Granted");
        };
        let outcome = store.settle_shared(reservation.id, Some(u64::MAX), now());
        assert!(
            outcome.is_err(),
            "expected settlement overflow to be rejected, got {outcome:?}"
        );
        // The reservation must remain untouched (still Active, not
        // corrupted by a half-applied settlement) after the rejection.
        let still_active = store
            .get_shared_reservation(reservation.id)
            .unwrap()
            .unwrap();
        assert_eq!(still_active.state, ReservationState::Active);
    }

    #[test]
    fn ingested_snapshot_values_above_i64_max_are_rejected_not_wrapped() {
        let mut store = LedgerStore::open_in_memory().unwrap();
        store
            .ensure_pool(&pool_id(), &QuotaUnit::Tokens, 10, now())
            .unwrap();
        let outcome = store.ingest_pool_provider_snapshot(
            &pool_id(),
            now(),
            None,
            &GaugeReading::Used {
                used: libra_governor_domain::QuotaAmount::new(QuotaUnit::Tokens, u64::MAX),
                limit: Some(10),
            },
            Confidence::High,
            now(),
        );
        assert!(
            outcome.is_err(),
            "expected used-value overflow to be rejected, got {outcome:?}"
        );
    }

    #[test]
    fn ensure_pool_never_overwrites_existing_capacity() {
        let mut store = LedgerStore::open_in_memory().unwrap();
        store
            .ensure_pool(&pool_id(), &QuotaUnit::Tokens, 10, now())
            .unwrap();
        let second = store
            .ensure_pool(&pool_id(), &QuotaUnit::Tokens, 999, now())
            .unwrap();
        assert_eq!(second.capacity, 10, "capacity at first creation must stick");
    }

    #[test]
    fn existing_task_reservation_path_is_unaffected_by_shared_pool_presence() {
        // AC4: the single flat-account v0.0.3 path (task_budgets +
        // reservations) must behave IDENTICALLY whether or not a shared
        // pool exists in the same store. This drives both code paths
        // against one `LedgerStore` and asserts the task-level outcome
        // matches what `crate::reservation`'s own unit tests already
        // establish in isolation — proving the two table sets (separate
        // per migration 0016's own docs) do not interact.
        use crate::reservation::test_support::{fixed_reserve, thousand_token_policy};
        use libra_governor_domain::{
            CompletionContract, CompletionCriterion, ReservationClass, ResourceAmount, TaskId,
            TaskIdentity,
        };

        let mut store = LedgerStore::open_in_memory().unwrap();

        // Shared-pool activity happening in the same store/connection.
        store
            .ensure_pool(&pool_id(), &QuotaUnit::Tokens, 10, now())
            .unwrap();
        reserve(&mut store, "pool-session", 10, "pool-key");

        // Ordinary task reservation path, untouched by the above.
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
        store
            .insert_contract(
                task_id,
                &CompletionContract::first(vec![CompletionCriterion::required("done")]),
                now(),
            )
            .unwrap();
        store
            .initialize_task_budget(
                task_id,
                &thousand_token_policy(),
                &fixed_reserve(200),
                now(),
            )
            .unwrap();

        let outcome = store
            .reserve(crate::ReserveRequest {
                task_id,
                session_id: "task-session",
                plan_id: None,
                class: ReservationClass::OptionalWork,
                amount: ResourceAmount::Tokens(300),
                idempotency_key: "task-key",
                now: now(),
                ttl_secs: 900,
            })
            .unwrap();
        let crate::ReserveOutcome::Granted(reservation) = outcome else {
            panic!("expected Granted");
        };
        // Matches exactly what `crate::reservation`'s own tests expect
        // for this fixture (800 optional headroom, 300 requested, 0
        // drawn from the 200-token reserve): the shared pool's existence
        // and its own 10/10 fully-consumed capacity have no bearing on
        // this task's arithmetic.
        assert_eq!(reservation.amount, ResourceAmount::Tokens(300));
        assert_eq!(reservation.drawn_from_reserve, ResourceAmount::Tokens(0));

        let snapshot = store.budget_snapshot(task_id).unwrap().unwrap();
        assert_eq!(snapshot.reserved(), ResourceAmount::Tokens(300));
        assert_eq!(snapshot.completion_reserve(), ResourceAmount::Tokens(200));

        store
            .settle(reservation.id, Some(ResourceAmount::Tokens(250)), now())
            .unwrap();
        let snapshot = store.budget_snapshot(task_id).unwrap().unwrap();
        assert_eq!(snapshot.used(), ResourceAmount::Tokens(250));
        assert_eq!(snapshot.reserved(), ResourceAmount::Tokens(0));

        // And the shared pool's own accounting is likewise untouched by
        // the task-level activity above: still exactly fully consumed.
        let admission = store.pool_admission(&pool_id(), now()).unwrap().unwrap();
        assert_eq!(admission.remaining(), 0);
    }

    #[test]
    fn concurrent_reservations_never_collectively_exceed_capacity_at_exact_limit() {
        // AC1: at the last remaining 10 units, concurrent sessions cannot
        // collectively reserve more than 10 units. Real threads, real
        // separate SQLite connections against the same on-disk file —
        // this repo's documented concurrency model (one `LedgerStore` per
        // thread/process, WAL + busy_timeout serializing writers).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.sqlite3");
        {
            let mut store = LedgerStore::open(&path).unwrap();
            store
                .ensure_pool(&pool_id(), &QuotaUnit::Tokens, 10, now())
                .unwrap();
        }

        let thread_count = 20usize;
        let per_thread_amount = 1u64;
        let mut handles = Vec::with_capacity(thread_count);
        for i in 0..thread_count {
            let path = path.clone();
            handles.push(std::thread::spawn(move || {
                let mut store = LedgerStore::open(&path).unwrap();
                let outcome = store
                    .reserve_shared(SharedPoolReserveRequest {
                        pool_id: &PoolId("shared-quota".to_string()),
                        principal_id: &PrincipalId(format!("principal-{i}")),
                        session_id: &format!("session-{i}"),
                        amount: per_thread_amount,
                        unit: &QuotaUnit::Tokens,
                        idempotency_key: &format!("thread-key-{i}"),
                        now: OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap(),
                        ttl_secs: 900,
                    })
                    .unwrap();
                matches!(outcome, SharedPoolReserveOutcome::Granted(_))
            }));
        }

        let granted_count = handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .filter(|granted| *granted)
            .count();

        assert_eq!(
            granted_count, 10,
            "exactly 10 of 20 one-unit requests against a 10-unit pool must be granted"
        );

        let store = LedgerStore::open(&path).unwrap();
        let admission = store.pool_admission(&pool_id(), now()).unwrap().unwrap();
        assert_eq!(admission.active, 10);
        assert_eq!(admission.remaining(), 0);
    }
}
