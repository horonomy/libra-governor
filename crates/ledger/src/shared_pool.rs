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
    metered_snapshot_limit, ChargeSource, Confidence, ExecutorKind, GaugeReading, PoolAdmission,
    PoolId, PoolProviderSnapshot, PrincipalId, QuotaAmount, QuotaPool, QuotaUnit, ReservationState,
    SharedPoolReservation, SharedPoolReservationId, SnapshotUnusable,
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

/// Most `message.id`s a lane watermark returns (and a writer keeps).
pub const WATERMARK_ID_CAP: usize = 512;

/// How far past its row's `settled_at` a stored watermark may sit: a
/// record is stamped by the host's clock a little ahead of the daemon's.
pub const WATERMARK_SLACK: time::Duration = time::Duration::minutes(10);

fn watermark_columns(
    watermark: Option<&SettleWatermark>,
) -> Result<(Option<String>, Option<String>), LedgerError> {
    let Some(w) = watermark else {
        return Ok((None, None));
    };
    let at = w.at.map(rfc3339).transpose()?;
    let ids = if w.message_ids.is_empty() {
        None
    } else {
        Some(serde_json::to_string(&w.message_ids).map_err(|_| invalid())?)
    };
    Ok((at, ids))
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
pub(crate) const MAX_TTL_SECS: u64 = 100 * 365 * 24 * 3600;

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
    Option<String>, // agent_id
    Option<String>, // executor_kind
    Option<String>, // charge_source
);

const RESERVATION_COLUMNS: &str =
    "id, pool_id, principal_id, session_id, amount, state, settled_amount, usage_known,
     idempotency_key, created_at, expires_at, settled_at, released_at, schema_version,
     agent_id, executor_kind, charge_source";

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
        row.get(14)?,
        row.get(15)?,
        row.get(16)?,
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
        agent_id,
        executor_kind,
        charge_source,
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
        agent_id,
        executor_kind: executor_kind
            .map(|s| ExecutorKind::parse(&s).ok_or_else(invalid))
            .transpose()?,
        charge_source: charge_source
            .map(|s| ChargeSource::parse(&s).ok_or_else(invalid))
            .transpose()?,
    })
}

/// Which executor within a session a shared-pool row is attributed to,
/// and how its spend arrived (HORO-2194). The default attributes nothing
/// (all columns NULL) so callers that predate attribution are unchanged.
///
/// Executor key: `(session_id, agent_id | NULL)`. A missing `agent_id`
/// with [`ExecutorKind::AgentAbsent`] is the session's main executor --
/// never a `root` lineage; [`ExecutorKind::Unattributed`] means the
/// session is known but the agent is not.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExecutorAttribution<'a> {
    pub agent_id: Option<&'a str>,
    pub executor_kind: Option<ExecutorKind>,
    pub charge_source: Option<ChargeSource>,
}

impl<'a> ExecutorAttribution<'a> {
    /// The session's main executor (no agent id).
    pub fn main_executor(charge_source: Option<ChargeSource>) -> Self {
        Self {
            agent_id: None,
            executor_kind: Some(ExecutorKind::AgentAbsent),
            charge_source,
        }
    }

    /// A specific agent within the session.
    pub fn agent(agent_id: &'a str, charge_source: Option<ChargeSource>) -> Self {
        Self {
            agent_id: Some(agent_id),
            executor_kind: Some(ExecutorKind::Agent),
            charge_source,
        }
    }

    /// Session known, agent unknown.
    pub fn unattributed(charge_source: Option<ChargeSource>) -> Self {
        Self {
            agent_id: None,
            executor_kind: Some(ExecutorKind::Unattributed),
            charge_source,
        }
    }

    /// Validates the `agent_id` / `executor_kind` pairing and resolves it
    /// to the values persisted. `Agent` requires a non-empty id;
    /// `AgentAbsent` / `Unattributed` forbid one; a bare `agent_id` with
    /// no kind is read as `Agent`; neither stays fully NULL.
    fn resolve(&self) -> Result<ResolvedAttribution, LedgerError> {
        let agent_id = match self.agent_id {
            Some("") => return Err(invalid()),
            other => other,
        };
        let executor_kind = match (self.executor_kind, agent_id) {
            (Some(ExecutorKind::Agent), Some(_)) => Some(ExecutorKind::Agent),
            (Some(ExecutorKind::Agent), None) => return Err(invalid()),
            (Some(ExecutorKind::AgentAbsent | ExecutorKind::Unattributed), Some(_)) => {
                return Err(invalid())
            }
            (Some(kind), None) => Some(kind),
            (None, Some(_)) => Some(ExecutorKind::Agent),
            (None, None) => None,
        };
        Ok(ResolvedAttribution {
            agent_id: agent_id.map(str::to_string),
            executor_kind,
            charge_source: self.charge_source,
        })
    }
}

/// Validated, owned form of [`ExecutorAttribution`].
struct ResolvedAttribution {
    agent_id: Option<String>,
    executor_kind: Option<ExecutorKind>,
    charge_source: Option<ChargeSource>,
}

/// Where a metered pool's effective capacity comes from (HORO-2194).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CapacitySource {
    /// `min(pool capacity, provider-declared snapshot limit)`, failing
    /// closed on an unusable snapshot -- the HORO-1915 behavior.
    #[default]
    ProviderSnapshot,
    /// An operator-configured cap, in the pool's own unit. The provider
    /// snapshot limit is not consulted and the stored `quota_pools`
    /// capacity (fixed at first creation) is not used as a bound: the
    /// configured cap is the effective capacity, so raising or lowering
    /// it takes effect on the next admission.
    LocalConfigured { cap: u64 },
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
    /// Executor attribution for the hold; `Default` attributes nothing.
    pub attribution: ExecutorAttribution<'a>,
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

/// Request to reserve against a metered (provider-denominated) pool
/// (HORO-1915) — the same base reserve request, plus an optional
/// cooperative per-session cap. See [`LedgerStore::reserve_shared_metered`].
pub struct MeteredReserveRequest<'a> {
    pub base: SharedPoolReserveRequest<'a>,
    /// When `Some`, refuses a reserve that would push this
    /// `(pool_id, principal_id, session_id)`'s combined active+settled
    /// total above `cap` — an additional constraint layered on top of the
    /// pool's own capacity, never a required allocation (a session with no
    /// cap is bounded only by the pool).
    pub session_cap: Option<u64>,
    /// Where effective capacity comes from. `Default` is
    /// [`CapacitySource::ProviderSnapshot`].
    pub capacity_source: CapacitySource,
}

/// The outcome of [`LedgerStore::reserve_shared_metered`].
#[derive(Debug, Clone, PartialEq)]
pub enum MeteredReserveOutcome {
    Granted(Box<SharedPoolReservation>),
    AlreadyGranted(Box<SharedPoolReservation>),
    /// `admission.capacity` is the *effective* capacity (`min(pool
    /// capacity, declared snapshot limit)`), not necessarily the pool's
    /// raw stored capacity.
    Insufficient {
        admission: PoolAdmission,
        requested: u64,
    },
    NoPool,
    /// The pool's provider snapshot could not supply a trusted declared
    /// limit — refused rather than treating "unknown" as "unlimited".
    SnapshotUnusable(SnapshotUnusable),
    SessionCapExceeded {
        committed: u64,
        cap: u64,
        requested: u64,
    },
}

/// One session's combined active+settled total against a pool (HORO-1915).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedPoolSessionUsage {
    pub principal_id: PrincipalId,
    pub session_id: String,
    pub active: u64,
    pub settled: u64,
}

/// The transcript high-water mark a settled lane row carries (HORO-2196):
/// the newest record timestamp counted, and the `message.id`s counted up to
/// it (so tail lines of an already-counted message are not counted again).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SettleWatermark {
    pub at: Option<OffsetDateTime>,
    pub message_ids: Vec<String>,
}

/// The outcome of [`LedgerStore::reserve_shared_core`] — a superset of
/// both [`SharedPoolReserveOutcome`] and [`MeteredReserveOutcome`]; each
/// public entry point maps it down to its own narrower type.
enum CoreReserveOutcome {
    Granted(Box<SharedPoolReservation>),
    AlreadyGranted(Box<SharedPoolReservation>),
    Insufficient {
        admission: PoolAdmission,
        requested: u64,
    },
    NoPool,
    SnapshotUnusable(SnapshotUnusable),
    SessionCapExceeded {
        committed: u64,
        cap: u64,
        requested: u64,
    },
}

/// Metered-pool-only admission constraints threaded through
/// [`LedgerStore::reserve_shared_core`]. `None` at the call site
/// (`reserve_shared`) means none of this gate's checks run at all — the
/// plain launch-slot path is byte-for-byte unchanged.
struct MeteredGate {
    session_cap: Option<u64>,
    capacity_source: CapacitySource,
}

/// Request to record spend that has *already happened* as an
/// already-settled row (HORO-2194). See [`LedgerStore::charge_shared`].
pub struct SharedPoolChargeRequest<'a> {
    pub pool_id: &'a PoolId,
    pub principal_id: &'a PrincipalId,
    pub session_id: &'a str,
    pub amount: u64,
    pub unit: &'a QuotaUnit,
    /// Replay key, scoped `(pool_id, principal_id, idempotency_key)` --
    /// the same key space as reserve.
    pub idempotency_key: &'a str,
    pub now: OffsetDateTime,
    pub attribution: ExecutorAttribution<'a>,
    /// Only used to report the post-charge [`PoolAdmission`]; a charge is
    /// never gated on it.
    pub capacity_source: CapacitySource,
    /// Whether `amount` was observed (`true`) or is a conservative
    /// placeholder for spend that could not be measured (`false`, HORO-2196).
    /// Recorded as the row's `usage_known`, and part of what makes a replay
    /// identical.
    pub usage_known: bool,
}

/// The outcome of [`LedgerStore::charge_shared`]. There is deliberately
/// no `Insufficient` variant: the spend already occurred.
#[derive(Debug, Clone, PartialEq)]
pub enum SharedPoolChargeOutcome {
    Charged {
        reservation: Box<SharedPoolReservation>,
        /// Pool accounting immediately after this charge.
        admission: PoolAdmission,
        /// `admission.overrun()`: how far committed spend now exceeds
        /// capacity (0 when within capacity). A lower bound when
        /// `capacity_known` is false.
        overrun: u64,
        /// False when `capacity_source` was `ProviderSnapshot` and the
        /// snapshot was unusable: `admission.capacity` is then only the
        /// stored pool capacity, NOT an effective capacity, and
        /// `overrun` / `remaining()` must not be read as authoritative.
        capacity_known: bool,
    },
    /// Idempotent replay of an identical charge; nothing was written.
    AlreadyCharged(Box<SharedPoolReservation>),
    /// The key exists but is not an identical charge (a hold -- including
    /// a settled one --, or a charge differing in amount, session,
    /// agent_id, executor_kind or charge_source). Nothing was written, so
    /// the caller knows its spend was NOT recorded under this key.
    /// Charge keys and reserve keys must be disjoint.
    KeyConflict(Box<SharedPoolReservation>),
    /// No `quota_pools` row exists for this pool.
    NoPool,
}

/// A [`PoolAdmission`] status read together with whether its capacity is
/// an effective one (HORO-2194).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourcedAdmission {
    pub admission: PoolAdmission,
    /// False only for `ProviderSnapshot` with an unusable snapshot: then
    /// `admission.capacity` is just the stored pool capacity, which is NOT
    /// the effective capacity (admission would refuse outright), so
    /// `remaining()` / `overrun()` understate the shortfall and must not
    /// be presented as authoritative.
    pub capacity_known: bool,
}

impl std::ops::Deref for SourcedAdmission {
    type Target = PoolAdmission;
    fn deref(&self) -> &PoolAdmission {
        &self.admission
    }
}

/// One executor's settled (`used`) and active (`reserved`) amounts
/// against a pool (HORO-2194).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedPoolExecutorUsage {
    pub principal_id: PrincipalId,
    pub session_id: String,
    pub agent_id: Option<String>,
    /// Rows with no recorded kind (legacy / omitted attribution) are
    /// reported as [`ExecutorKind::Unattributed`], never dropped.
    pub executor_kind: ExecutorKind,
    pub used: u64,
    pub reserved: u64,
}

/// Per-executor usage plus pool totals. `executors` always sums to
/// `total_used` / `total_reserved`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedPoolExecutorReport {
    pub executors: Vec<SharedPoolExecutorUsage>,
    pub total_used: u64,
    pub total_reserved: u64,
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

/// The outcome of [`LedgerStore::renew_shared`] (HORO-1766).
#[derive(Debug, Clone, PartialEq)]
pub enum SharedPoolRenewOutcome {
    /// `expires_at` was extended. Nothing else about the row changed.
    Renewed(Box<SharedPoolReservation>),
    /// No row exists with this id at all.
    NotFound,
    /// A row exists but did not satisfy the renew contract: either
    /// `principal_id`/`session_id` did not match exactly, or the row was
    /// not currently `active` (already settled/released/expired), or it
    /// was `active` but already past its own `expires_at` (stale,
    /// awaiting a sweep). Refused, never silently extended — the current
    /// stored row is returned for diagnostics; it is never mutated by a
    /// refused renew. See module docs for why a wrong-identity or
    /// non-active renew must never "succeed" quietly.
    Refused(Box<SharedPoolReservation>),
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

    /// `SUM` of one amount column for one state, saturating instead of
    /// failing: SQLite raises an error when an integer `SUM` overflows, and
    /// one oversized row must not make every later admission and charge
    /// fail with it.
    fn saturating_amount_sum(
        tx: &rusqlite::Connection,
        pool_id: &PoolId,
        column: &str,
        state: &str,
    ) -> Result<u64, LedgerError> {
        let summed: rusqlite::Result<i64> = tx.query_row(
            &format!(
                "SELECT COALESCE(SUM({column}), 0) FROM shared_pool_reservations
                 WHERE pool_id = ?1 AND state = '{state}'"
            ),
            [&pool_id.0],
            |row| row.get(0),
        );
        match summed {
            Ok(total) => Ok(total.max(0) as u64),
            Err(_) => {
                let mut stmt = tx.prepare(&format!(
                    "SELECT COALESCE({column}, 0) FROM shared_pool_reservations
                     WHERE pool_id = ?1 AND state = '{state}'"
                ))?;
                let rows = stmt.query_map([&pool_id.0], |row| row.get::<_, i64>(0))?;
                let mut total: u64 = 0;
                for row in rows {
                    total = total.saturating_add(row?.max(0) as u64);
                }
                Ok(total)
            }
        }
    }

    fn settled_and_active_tx(
        tx: &rusqlite::Connection,
        pool_id: &PoolId,
    ) -> Result<(u64, u64), LedgerError> {
        let settled = Self::saturating_amount_sum(tx, pool_id, "settled_amount", "settled")?;
        let active = Self::saturating_amount_sum(tx, pool_id, "amount", "active")?;
        Ok((settled, active))
    }

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

    /// Reads `pool_id`'s most recent ingested [`PoolProviderSnapshot`]
    /// inside the caller's transaction/connection, if any was ever
    /// recorded via [`Self::ingest_pool_provider_snapshot`]. `unit` is the
    /// pool's own declared unit — the snapshot row stores a bare integer
    /// `used_value`/`declared_limit`, not a tagged [`QuotaUnit`], so the
    /// caller (who has already looked up the pool) supplies it.
    fn pool_provider_snapshot_tx(
        tx: &rusqlite::Connection,
        pool_id: &PoolId,
        unit: &QuotaUnit,
    ) -> Result<Option<PoolProviderSnapshot>, LedgerError> {
        #[allow(clippy::type_complexity)]
        let row: Option<(
            String,
            Option<String>,
            i64,
            Option<i64>,
            Option<i64>,
            String,
        )> = tx
            .query_row(
                "SELECT observed_at, valid_until, disclosed, used_value, declared_limit, confidence
                 FROM shared_pool_provider_snapshots WHERE pool_id = ?1",
                [&pool_id.0],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .optional()?;
        let Some((observed_at, valid_until, disclosed, used_value, declared_limit, confidence_str)) =
            row
        else {
            return Ok(None);
        };
        let confidence: Confidence =
            serde_json::from_str(&confidence_str).map_err(|_| invalid())?;
        let reading = if disclosed == 0 {
            GaugeReading::Undisclosed
        } else {
            GaugeReading::Used {
                used: QuotaAmount::new(unit.clone(), used_value.unwrap_or(0).max(0) as u64),
                limit: declared_limit.map(|l| l.max(0) as u64),
            }
        };
        Ok(Some(PoolProviderSnapshot {
            pool_id: pool_id.clone(),
            observed_at: parse_time(&observed_at)?,
            valid_until: valid_until.map(|s| parse_time(&s)).transpose()?,
            reading,
            confidence,
        }))
    }

    /// Reads `pool_id`'s most recent ingested [`PoolProviderSnapshot`],
    /// outside any transaction. `Ok(None)` when no pool exists, or no
    /// snapshot has ever been ingested for it.
    pub fn pool_provider_snapshot(
        &self,
        pool_id: &PoolId,
    ) -> Result<Option<PoolProviderSnapshot>, LedgerError> {
        let Some(pool) = self.quota_pool(pool_id)? else {
            return Ok(None);
        };
        Self::pool_provider_snapshot_tx(&self.conn, pool_id, &pool.unit)
    }

    /// One `(principal_id, session_id)`'s combined active+settled total
    /// against `pool_id`, inside the caller's transaction — the
    /// accounting [`Self::reserve_shared_core`]'s optional session-cap
    /// gate checks against. Released and expired holds contribute
    /// nothing, matching [`SharedPoolReservation::outstanding`]'s own
    /// state accounting.
    fn session_committed_tx(
        tx: &rusqlite::Connection,
        pool_id: &PoolId,
        principal_id: &PrincipalId,
        session_id: &str,
    ) -> Result<u64, LedgerError> {
        let active: i64 = tx.query_row(
            "SELECT COALESCE(SUM(amount), 0) FROM shared_pool_reservations
             WHERE pool_id = ?1 AND principal_id = ?2 AND session_id = ?3 AND state = 'active'",
            rusqlite::params![pool_id.0, principal_id.0, session_id],
            |row| row.get(0),
        )?;
        let settled: i64 = tx.query_row(
            "SELECT COALESCE(SUM(settled_amount), 0) FROM shared_pool_reservations
             WHERE pool_id = ?1 AND principal_id = ?2 AND session_id = ?3 AND state = 'settled'",
            rusqlite::params![pool_id.0, principal_id.0, session_id],
            |row| row.get(0),
        )?;
        Ok((active.max(0) as u64).saturating_add(settled.max(0) as u64))
    }

    /// Every distinct `(principal_id, session_id)`'s combined active and
    /// settled totals against `pool_id` (HORO-1915) — the per-session
    /// breakdown a metered-pool status report shows.
    pub fn shared_pool_session_usage(
        &self,
        pool_id: &PoolId,
    ) -> Result<Vec<SharedPoolSessionUsage>, LedgerError> {
        let mut stmt = self.conn.prepare(
            "SELECT principal_id, session_id,
                    COALESCE(SUM(CASE WHEN state = 'active' THEN amount ELSE 0 END), 0),
                    COALESCE(SUM(CASE WHEN state = 'settled' THEN settled_amount ELSE 0 END), 0)
             FROM shared_pool_reservations WHERE pool_id = ?1
             GROUP BY principal_id, session_id",
        )?;
        let rows = stmt.query_map([&pool_id.0], |row| {
            let active: i64 = row.get(2)?;
            let settled: i64 = row.get(3)?;
            Ok(SharedPoolSessionUsage {
                principal_id: PrincipalId(row.get(0)?),
                session_id: row.get(1)?,
                active: active.max(0) as u64,
                settled: settled.max(0) as u64,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(LedgerError::Sqlite)
    }

    /// Per-executor used (settled) and reserved (active) amounts against
    /// `pool_id`, plus the pool totals (HORO-2194). Rows are grouped by
    /// `(principal_id, session_id, agent_id, executor_kind)`; rows with no
    /// recorded kind (legacy / attribution omitted) are reported as
    /// [`ExecutorKind::Unattributed`] so nothing is silently omitted, and
    /// `sum(executors) == totals` for both figures. Totals come from the
    /// same queries `pool_admission` uses, read in one snapshot.
    pub fn shared_pool_executor_usage(
        &self,
        pool_id: &PoolId,
    ) -> Result<SharedPoolExecutorReport, LedgerError> {
        let tx = self.conn.unchecked_transaction()?;
        let (total_used, total_reserved) = Self::settled_and_active_tx(&tx, pool_id)?;
        let executors = {
            let mut stmt = tx.prepare(
                "SELECT principal_id, session_id, agent_id,
                        COALESCE(executor_kind, 'unattributed'),
                        COALESCE(SUM(CASE WHEN state = 'settled' THEN settled_amount ELSE 0 END), 0),
                        COALESCE(SUM(CASE WHEN state = 'active' THEN amount ELSE 0 END), 0)
                 FROM shared_pool_reservations
                 WHERE pool_id = ?1 AND state IN ('active', 'settled')
                 GROUP BY principal_id, session_id, agent_id, COALESCE(executor_kind, 'unattributed')
                 ORDER BY principal_id, session_id, agent_id, 4",
            )?;
            let rows = stmt.query_map([&pool_id.0], |row| {
                let kind: String = row.get(3)?;
                let used: i64 = row.get(4)?;
                let reserved: i64 = row.get(5)?;
                Ok((
                    PrincipalId(row.get(0)?),
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    kind,
                    used.max(0) as u64,
                    reserved.max(0) as u64,
                ))
            })?;
            let mut out = Vec::new();
            for row in rows {
                let (principal_id, session_id, agent_id, kind, used, reserved) = row?;
                out.push(SharedPoolExecutorUsage {
                    principal_id,
                    session_id,
                    agent_id,
                    executor_kind: ExecutorKind::parse(&kind).ok_or_else(invalid)?,
                    used,
                    reserved,
                });
            }
            out
        };
        Ok(SharedPoolExecutorReport {
            executors,
            total_used,
            total_reserved,
        })
    }

    /// Atomically records spend that has *already happened* as an
    /// already-`settled` row (HORO-2194), e.g. a subagent's transcript
    /// usage or a gateway-observed charge.
    ///
    /// - Idempotent on `(pool_id, principal_id, idempotency_key)`: an
    ///   identical replay writes nothing and returns
    ///   [`SharedPoolChargeOutcome::AlreadyCharged`]; the same key naming
    ///   anything else is a [`SharedPoolChargeOutcome::KeyConflict`].
    /// - **Never refused for lack of capacity.** The money is already
    ///   spent, so refusing would only make the ledger lie. If the charge
    ///   pushes committed spend beyond capacity the excess is reported as
    ///   overrun -- the same way a late settle past its hold is: nothing
    ///   is clamped, [`PoolAdmission::remaining`] goes negative,
    ///   [`PoolAdmission::overrun`] is the excess, and every later
    ///   `reserve` is refused until capacity is available again.
    /// - Counts toward `remaining` (`capacity - settled - active`)
    ///   immediately: it is a `settled` row like any other.
    ///
    /// Same `BEGIN IMMEDIATE` discipline as every other mutator here.
    pub fn charge_shared(
        &mut self,
        req: SharedPoolChargeRequest<'_>,
    ) -> Result<SharedPoolChargeOutcome, LedgerError> {
        self.charge_shared_with_watermark(req, None)
    }

    /// [`Self::charge_shared`] that writes the row and its transcript
    /// high-water mark in **one** transaction (HORO-2196), so a crash can
    /// never leave a charge without its mark (which a later window would
    /// recount). An identical replay whose row has no mark yet adopts the
    /// supplied one.
    pub fn charge_shared_with_watermark(
        &mut self,
        req: SharedPoolChargeRequest<'_>,
        watermark: Option<&SettleWatermark>,
    ) -> Result<SharedPoolChargeOutcome, LedgerError> {
        let (watermark_text, ids_text) = watermark_columns(watermark)?;
        let attribution = req.attribution.resolve()?;
        let amount_i64 = checked_i64(req.amount)?;
        let tx = begin(&mut self.conn, TransactionBehavior::Immediate)?;

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
            let existing = row_to_reservation(row)?;
            // Identical means the same charge, field for field (NULL is its
            // own value): a settled turn hold, or a charge attributed
            // differently, must never be mistaken for a replay or the
            // spend would be silently dropped.
            let identical = Self::is_charge_row_tx(&tx, existing.id)?
                && existing.state == ReservationState::Settled
                && existing.settled_amount == Some(req.amount)
                && existing.usage_known == Some(req.usage_known)
                && existing.session_id == req.session_id
                && existing.agent_id == attribution.agent_id
                && existing.executor_kind == attribution.executor_kind
                && existing.charge_source == attribution.charge_source;
            if identical && watermark_text.is_some() {
                tx.execute(
                    "UPDATE shared_pool_reservations
                     SET watermark_ts = ?1, watermark_message_ids = ?2
                     WHERE id = ?3 AND watermark_ts IS NULL",
                    rusqlite::params![watermark_text, ids_text, existing.id.0.to_string()],
                )?;
            }
            tx.commit()?;
            return Ok(if identical {
                SharedPoolChargeOutcome::AlreadyCharged(Box::new(existing))
            } else {
                SharedPoolChargeOutcome::KeyConflict(Box::new(existing))
            });
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
            return Ok(SharedPoolChargeOutcome::NoPool);
        };
        let pool = row_to_pool(req.pool_id.clone(), pool_row)?;
        if *req.unit != pool.unit {
            return Err(LedgerError::QuotaUnitMismatch {
                expected: pool.unit,
                actual: req.unit.clone(),
            });
        }

        let id = SharedPoolReservationId::new();
        let now_str = rfc3339(req.now)?;
        tx.execute(
            "INSERT INTO shared_pool_reservations (
                id, pool_id, principal_id, session_id, amount, state, settled_amount,
                usage_known, idempotency_key, created_at, expires_at, settled_at, released_at,
                schema_version, agent_id, executor_kind, charge_source, is_charge,
                watermark_ts, watermark_message_ids
             ) VALUES (?1, ?2, ?3, ?4, ?5, 'settled', ?5, ?12, ?6, ?7, ?7, ?7, NULL, ?8,
                       ?9, ?10, ?11, 1, ?13, ?14)",
            rusqlite::params![
                id.0.to_string(),
                req.pool_id.0,
                req.principal_id.0,
                req.session_id,
                amount_i64,
                req.idempotency_key,
                now_str,
                SHARED_POOL_RESERVATION_SCHEMA_VERSION,
                attribution.agent_id,
                attribution.executor_kind.map(ExecutorKind::as_str),
                attribution.charge_source.map(ChargeSource::as_str),
                req.usage_known,
                watermark_text,
                ids_text,
            ],
        )?;

        // Reporting only -- the row is already written. An unusable
        // provider snapshot falls back to the stored pool capacity and is
        // flagged `capacity_known: false`.
        let effective = Self::effective_capacity_tx(&tx, &pool, req.capacity_source, req.now)?;
        let capacity_known = effective.is_ok();
        let capacity = effective.unwrap_or(pool.capacity);
        let (settled, active) = Self::settled_and_active_tx(&tx, req.pool_id)?;
        let external_overhang =
            Self::overhang_for_tx(&tx, req.pool_id, req.capacity_source, req.now)?;
        let admission = PoolAdmission {
            capacity,
            settled,
            active,
            external_overhang,
        };
        let reservation = Self::get_shared_reservation_tx(&tx, id)?
            .expect("row just inserted in this same transaction");
        tx.commit()?;
        Ok(SharedPoolChargeOutcome::Charged {
            reservation: Box::new(reservation),
            admission,
            overrun: admission.overrun(),
            capacity_known,
        })
    }

    /// [`Self::pool_admission`] for a pool whose capacity comes from
    /// `source` (HORO-2194): with [`CapacitySource::LocalConfigured`] the
    /// configured cap, otherwise the stored capacity clamped to a usable
    /// provider snapshot's declared limit (the stored capacity alone when
    /// the snapshot is unusable -- this is a status read, not an
    /// admission; `capacity_known` is then false). `LocalConfigured`
    /// ignores any provider snapshot overhang. `remaining()` is never clamped, so an overrun shows as
    /// a negative figure and via [`PoolAdmission::overrun`].
    pub fn pool_admission_for(
        &self,
        pool_id: &PoolId,
        now: OffsetDateTime,
        source: CapacitySource,
    ) -> Result<Option<SourcedAdmission>, LedgerError> {
        let tx = self.conn.unchecked_transaction()?;
        let Some(pool) = self.quota_pool(pool_id)? else {
            return Ok(None);
        };
        let effective = Self::effective_capacity_tx(&tx, &pool, source, now)?;
        let capacity_known = effective.is_ok();
        let capacity = effective.unwrap_or(pool.capacity);
        let (settled, active) = Self::settled_and_active_tx(&tx, pool_id)?;
        let external_overhang = Self::overhang_for_tx(&tx, pool_id, source, now)?;
        Ok(Some(SourcedAdmission {
            admission: PoolAdmission {
                capacity,
                settled,
                active,
                external_overhang,
            },
            capacity_known,
        }))
    }

    /// The active, unexpired, non-charge hold of one executor lane in
    /// `pool_id` (HORO-2195), or `None`.
    ///
    /// A lane is `(session_id, agent_id | NULL)` plus its `executor_kind`:
    /// the NULL-agent comparison uses `IS` (plain `=` never matches NULL)
    /// and the kind is part of the key so a main-executor lane is never
    /// confused with an `unattributed` row that also has no agent. Expiry
    /// and recency are compared as parsed instants, not RFC3339 text (see
    /// [`Self::settled_before_tx`]). If more than one active hold matches
    /// (which the daemon never creates), the most recently created wins.
    pub fn active_shared_hold_for_lane(
        &self,
        pool_id: &PoolId,
        principal_id: &PrincipalId,
        session_id: &str,
        agent_id: Option<&str>,
        executor_kind: ExecutorKind,
        now: OffsetDateTime,
    ) -> Result<Option<SharedPoolReservation>, LedgerError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {RESERVATION_COLUMNS} FROM shared_pool_reservations
             WHERE pool_id = ?1 AND principal_id = ?2 AND session_id = ?3
               AND agent_id IS ?4 AND COALESCE(executor_kind, 'unattributed') = ?5
               AND state = 'active' AND is_charge = 0"
        ))?;
        let rows = stmt.query_map(
            rusqlite::params![
                pool_id.0,
                principal_id.0,
                session_id,
                agent_id,
                executor_kind.as_str()
            ],
            read_reservation_row,
        )?;
        let mut best: Option<SharedPoolReservation> = None;
        for row in rows {
            let reservation = row_to_reservation(row?)?;
            if reservation.expires_at <= now {
                continue;
            }
            if best
                .as_ref()
                .is_none_or(|b| reservation.created_at > b.created_at)
            {
                best = Some(reservation);
            }
        }
        Ok(best)
    }

    /// The earliest-created row `principal_id` holds under `idempotency_key`
    /// in any pool starting with `pool_prefix` (HORO-2195). Reserve and
    /// charge replay lookups are scoped to one pool; this finds a key
    /// across periods so a retry cannot be booked twice when the pool it
    /// would resolve to has changed.
    pub fn find_shared_reservation_by_key(
        &self,
        principal_id: &PrincipalId,
        idempotency_key: &str,
        pool_prefix: &str,
    ) -> Result<Option<SharedPoolReservation>, LedgerError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {RESERVATION_COLUMNS} FROM shared_pool_reservations
             WHERE principal_id = ?1 AND idempotency_key = ?2
               AND substr(pool_id, 1, length(?3)) = ?3"
        ))?;
        let rows = stmt.query_map(
            rusqlite::params![principal_id.0, idempotency_key, pool_prefix],
            read_reservation_row,
        )?;
        let mut best: Option<SharedPoolReservation> = None;
        for row in rows {
            let reservation = row_to_reservation(row?)?;
            if best
                .as_ref()
                .is_none_or(|b| reservation.created_at < b.created_at)
            {
                best = Some(reservation);
            }
        }
        Ok(best)
    }

    /// Every non-charge hold (any state) of one executor lane in any pool
    /// whose id starts with `pool_prefix` (HORO-2196) -- the history a
    /// settlement derives its window watermark and its target hold from,
    /// with no extra persisted state. The lane is `(session_id, agent_id |
    /// NULL)` plus `executor_kind`, compared as in
    /// [`Self::active_shared_hold_for_lane`].
    pub fn shared_lane_holds(
        &self,
        principal_id: &PrincipalId,
        session_id: &str,
        agent_id: Option<&str>,
        executor_kind: ExecutorKind,
        pool_prefix: &str,
    ) -> Result<Vec<SharedPoolReservation>, LedgerError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {RESERVATION_COLUMNS} FROM shared_pool_reservations
             WHERE principal_id = ?1 AND session_id = ?2 AND agent_id IS ?3
               AND COALESCE(executor_kind, 'unattributed') = ?4
               AND substr(pool_id, 1, length(?5)) = ?5 AND is_charge = 0"
        ))?;
        let rows = stmt.query_map(
            rusqlite::params![
                principal_id.0,
                session_id,
                agent_id,
                executor_kind.as_str(),
                pool_prefix
            ],
            read_reservation_row,
        )?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row_to_reservation(row?)?);
        }
        Ok(out)
    }

    /// The newest transcript high-water mark persisted on any settled hold
    /// of one executor lane (HORO-2196), or `None` when no hold of the lane
    /// carries one (legacy rows and windows that counted no record).
    /// Compared as parsed instants, not text.
    pub fn shared_lane_watermark(
        &self,
        principal_id: &PrincipalId,
        session_id: &str,
        agent_id: Option<&str>,
        executor_kind: ExecutorKind,
        pool_prefix: &str,
    ) -> Result<Option<SettleWatermark>, LedgerError> {
        let mut stmt = self.conn.prepare(
            "SELECT watermark_ts, watermark_message_ids, settled_at FROM shared_pool_reservations
             WHERE principal_id = ?1 AND session_id = ?2 AND agent_id IS ?3
               AND COALESCE(executor_kind, 'unattributed') = ?4
               AND substr(pool_id, 1, length(?5)) = ?5
               AND state = 'settled'
               AND (watermark_ts IS NOT NULL OR watermark_message_ids IS NOT NULL)
             ORDER BY settled_at DESC",
        )?;
        let rows = stmt.query_map(
            rusqlite::params![
                principal_id.0,
                session_id,
                agent_id,
                executor_kind.as_str(),
                pool_prefix
            ],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )?;
        // `at` is the newest trustworthy mark; the ids are the union over
        // trustworthy rows, newest row first, capped at WATERMARK_ID_CAP
        // (a hash set keeps the merge linear).
        let mut best: Option<OffsetDateTime> = None;
        let mut ids: Vec<String> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut any = false;
        for row in rows {
            let (at, row_ids, settled_at) = row?;
            let at = at.as_deref().map(parse_time).transpose()?;
            // Defence in depth: a mark far ahead of the instant its row
            // settled can only come from a poisoned record (the writer
            // clamps, but an older build or a hand edit might not). Trusting
            // it would hide every later record, so it is ignored.
            if let (Some(at), Some(settled)) =
                (at, settled_at.as_deref().map(parse_time).transpose()?)
            {
                if at > settled + WATERMARK_SLACK {
                    continue;
                }
            }
            any = true;
            if let Some(at) = at {
                best = Some(best.map_or(at, |b| b.max(at)));
            }
            if ids.len() < WATERMARK_ID_CAP {
                if let Some(list) =
                    row_ids.and_then(|j| serde_json::from_str::<Vec<String>>(&j).ok())
                {
                    for id in list {
                        if ids.len() >= WATERMARK_ID_CAP {
                            break;
                        }
                        if seen.insert(id.clone()) {
                            ids.push(id);
                        }
                    }
                }
            }
        }
        Ok(any.then_some(SettleWatermark {
            at: best,
            message_ids: ids,
        }))
    }

    /// Records a watermark on an already-written settled row (the tail
    /// charge writes its row first, then its mark; a replay of the same
    /// charge finds the row and writes the same mark again).
    pub fn record_shared_watermark(
        &mut self,
        id: SharedPoolReservationId,
        watermark: &SettleWatermark,
    ) -> Result<(), LedgerError> {
        let (at, ids) = watermark_columns(Some(watermark))?;
        self.conn.execute(
            "UPDATE shared_pool_reservations SET watermark_ts = ?1, watermark_message_ids = ?2
             WHERE id = ?3 AND state = 'settled'",
            rusqlite::params![at, ids, id.0.to_string()],
        )?;
        Ok(())
    }

    /// Sum of the settled amounts of every charge row `principal_id` holds
    /// whose idempotency key starts with `key_prefix`, across pools whose
    /// id starts with `pool_prefix` (HORO-2196). It is how a cumulative
    /// charge (a subagent transcript) finds what it has already booked,
    /// whichever period each earlier row landed in.
    pub fn shared_charges_total_by_key_prefix(
        &self,
        principal_id: &PrincipalId,
        key_prefix: &str,
        pool_prefix: &str,
    ) -> Result<u64, LedgerError> {
        let total: i64 = self.conn.query_row(
            "SELECT COALESCE(SUM(settled_amount), 0) FROM shared_pool_reservations
             WHERE principal_id = ?1 AND is_charge = 1 AND state = 'settled'
               AND substr(idempotency_key, 1, length(?2)) = ?2
               AND substr(pool_id, 1, length(?3)) = ?3",
            rusqlite::params![principal_id.0, key_prefix, pool_prefix],
            |row| row.get(0),
        )?;
        // Plus what a retention prune folded away (HORO-2250). Only the
        // exact family prefix is ever stored, so match it exactly.
        let floor: i64 = self.conn.query_row(
            "SELECT COALESCE(SUM(total), 0) FROM shared_charge_floor
             WHERE principal_id = ?1 AND key_prefix = ?2",
            rusqlite::params![principal_id.0, key_prefix],
            |row| row.get(0),
        )?;
        Ok(total.saturating_add(floor).max(0) as u64)
    }

    /// Every active, unexpired, non-charge hold `principal_id` has for
    /// `session_id` in any pool whose id starts with `pool_prefix`
    /// (HORO-2195) -- the set a tool-event heartbeat renews.
    pub fn active_shared_holds_for_session(
        &self,
        principal_id: &PrincipalId,
        session_id: &str,
        pool_prefix: &str,
        now: OffsetDateTime,
    ) -> Result<Vec<SharedPoolReservation>, LedgerError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {RESERVATION_COLUMNS} FROM shared_pool_reservations
             WHERE principal_id = ?1 AND session_id = ?2
               AND substr(pool_id, 1, length(?3)) = ?3
               AND state = 'active' AND is_charge = 0"
        ))?;
        let rows = stmt.query_map(
            rusqlite::params![principal_id.0, session_id, pool_prefix],
            read_reservation_row,
        )?;
        let mut out = Vec::new();
        for row in rows {
            let reservation = row_to_reservation(row?)?;
            if reservation.expires_at > now {
                out.push(reservation);
            }
        }
        Ok(out)
    }

    /// Ids of every pool whose id starts with `prefix`, sorted
    /// (HORO-2195). Used to detect a pool of a later period than the
    /// clock says exists.
    pub fn shared_pool_ids_with_prefix(&self, prefix: &str) -> Result<Vec<PoolId>, LedgerError> {
        let mut stmt = self.conn.prepare(
            "SELECT pool_id FROM quota_pools WHERE substr(pool_id, 1, length(?1)) = ?1
             ORDER BY pool_id",
        )?;
        let rows = stmt.query_map([prefix], |row| row.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map(|ids| ids.into_iter().map(PoolId).collect())
            .map_err(LedgerError::Sqlite)
    }

    /// The pool of the most recently created hold (any state, charges
    /// excluded) `principal_id` has for `session_id` among pools starting
    /// with `pool_prefix` (HORO-2195) -- the fallback period for a charge
    /// that names no event time.
    pub fn latest_shared_hold_pool_for_session(
        &self,
        principal_id: &PrincipalId,
        session_id: &str,
        pool_prefix: &str,
    ) -> Result<Option<PoolId>, LedgerError> {
        let mut stmt = self.conn.prepare(
            "SELECT pool_id, created_at FROM shared_pool_reservations
             WHERE principal_id = ?1 AND session_id = ?2
               AND substr(pool_id, 1, length(?3)) = ?3 AND is_charge = 0",
        )?;
        let rows = stmt.query_map(
            rusqlite::params![principal_id.0, session_id, pool_prefix],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )?;
        let mut best: Option<(OffsetDateTime, String)> = None;
        for row in rows {
            let (pool, created_at) = row?;
            let created_at = parse_time(&created_at)?;
            if best.as_ref().is_none_or(|(t, _)| created_at > *t) {
                best = Some((created_at, pool));
            }
        }
        Ok(best.map(|(_, pool)| PoolId(pool)))
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
    ///
    /// Thin wrapper over [`Self::reserve_shared_core`] with no metered
    /// gate — behavior and signature are unchanged from before HORO-1915.
    pub fn reserve_shared(
        &mut self,
        req: SharedPoolReserveRequest<'_>,
    ) -> Result<SharedPoolReserveOutcome, LedgerError> {
        match self.reserve_shared_core(req, None)? {
            CoreReserveOutcome::Granted(r) => Ok(SharedPoolReserveOutcome::Granted(r)),
            CoreReserveOutcome::AlreadyGranted(r) => {
                Ok(SharedPoolReserveOutcome::AlreadyGranted(r))
            }
            CoreReserveOutcome::Insufficient {
                admission,
                requested,
            } => Ok(SharedPoolReserveOutcome::Insufficient {
                admission,
                requested,
            }),
            CoreReserveOutcome::NoPool => Ok(SharedPoolReserveOutcome::NoPool),
            CoreReserveOutcome::SnapshotUnusable(_)
            | CoreReserveOutcome::SessionCapExceeded { .. } => {
                unreachable!(
                    "reserve_shared_core only produces metered-only outcomes when a metered \
                     gate is passed, and reserve_shared never passes one"
                )
            }
        }
    }

    /// Atomically reserves `req.base.amount` against a metered
    /// (provider-denominated) pool (HORO-1915): identical admission
    /// mechanics to [`Self::reserve_shared`], plus a fail-closed gate on
    /// the pool's most recent provider snapshot (an effective capacity of
    /// `min(pool capacity, declared snapshot limit)`, refusing outright
    /// when the snapshot is missing/undisclosed/unbounded/stale/without a
    /// limit — see [`libra_governor_domain::metered_snapshot_limit`]) and
    /// an optional cooperative per-session cap.
    pub fn reserve_shared_metered(
        &mut self,
        req: MeteredReserveRequest<'_>,
    ) -> Result<MeteredReserveOutcome, LedgerError> {
        let MeteredReserveRequest {
            base,
            session_cap,
            capacity_source,
        } = req;
        let gate = MeteredGate {
            session_cap,
            capacity_source,
        };
        match self.reserve_shared_core(base, Some(&gate))? {
            CoreReserveOutcome::Granted(r) => Ok(MeteredReserveOutcome::Granted(r)),
            CoreReserveOutcome::AlreadyGranted(r) => Ok(MeteredReserveOutcome::AlreadyGranted(r)),
            CoreReserveOutcome::Insufficient {
                admission,
                requested,
            } => Ok(MeteredReserveOutcome::Insufficient {
                admission,
                requested,
            }),
            CoreReserveOutcome::NoPool => Ok(MeteredReserveOutcome::NoPool),
            CoreReserveOutcome::SnapshotUnusable(reason) => {
                Ok(MeteredReserveOutcome::SnapshotUnusable(reason))
            }
            CoreReserveOutcome::SessionCapExceeded {
                committed,
                cap,
                requested,
            } => Ok(MeteredReserveOutcome::SessionCapExceeded {
                committed,
                cap,
                requested,
            }),
        }
    }

    /// Shared reserve mechanics for both [`Self::reserve_shared`] (no
    /// `metered` gate) and [`Self::reserve_shared_metered`] (`Some` gate).
    /// See module docs for the transaction/idempotency guarantees that
    /// make AC1 (no combined oversubscription under concurrency) hold.
    fn reserve_shared_core(
        &mut self,
        req: SharedPoolReserveRequest<'_>,
        metered: Option<&MeteredGate>,
    ) -> Result<CoreReserveOutcome, LedgerError> {
        // Validated before any admission decision, like `charge_shared`.
        let attribution = req.attribution.resolve()?;
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
            let existing = row_to_reservation(row)?;
            // A row created by `charge_shared` is spend, not a hold: handing
            // it back as `AlreadyGranted` would make the caller believe it
            // holds capacity. Charge keys and reserve keys must be disjoint.
            let is_charge = Self::is_charge_row_tx(&tx, existing.id)?;
            tx.commit()?;
            if is_charge {
                return Err(LedgerError::SharedPoolKeyConflict);
            }
            return Ok(CoreReserveOutcome::AlreadyGranted(Box::new(existing)));
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
            return Ok(CoreReserveOutcome::NoPool);
        };
        let pool = row_to_pool(req.pool_id.clone(), pool_row)?;

        if *req.unit != pool.unit {
            return Err(LedgerError::QuotaUnitMismatch {
                expected: pool.unit,
                actual: req.unit.clone(),
            });
        }

        // Metered-only: the pool's effective capacity is clamped to the
        // provider's own most recent declared limit, and a missing/
        // undisclosed/unbounded/stale/limit-less snapshot refuses the
        // reserve outright rather than treating "unknown" as "unlimited"
        // — see `libra_governor_domain::metered_snapshot_limit`'s own
        // docs for the exact rule order.
        let mut effective_capacity = pool.capacity;
        if let Some(gate) = metered {
            match Self::effective_capacity_tx(&tx, &pool, gate.capacity_source, req.now)? {
                Ok(capacity) => effective_capacity = capacity,
                Err(reason) => {
                    tx.commit()?;
                    return Ok(CoreReserveOutcome::SnapshotUnusable(reason));
                }
            }
        }

        // Metered-only: an optional cooperative per-session cap, checked
        // before the pool-wide admission so a caller sees the more
        // specific refusal reason first.
        if let Some(gate) = metered {
            if let Some(cap) = gate.session_cap {
                let committed =
                    Self::session_committed_tx(&tx, req.pool_id, req.principal_id, req.session_id)?;
                if committed.saturating_add(req.amount) > cap {
                    tx.commit()?;
                    return Ok(CoreReserveOutcome::SessionCapExceeded {
                        committed,
                        cap,
                        requested: req.amount,
                    });
                }
            }
        }

        let (settled, active) = Self::settled_and_active_tx(&tx, req.pool_id)?;
        let overhang_source = metered
            .map(|g| g.capacity_source)
            .unwrap_or(CapacitySource::ProviderSnapshot);
        let external_overhang = Self::overhang_for_tx(&tx, req.pool_id, overhang_source, req.now)?;
        let admission = PoolAdmission {
            capacity: effective_capacity,
            settled,
            active,
            external_overhang,
        };

        if !admission.admits(req.amount) {
            tx.commit()?;
            return Ok(CoreReserveOutcome::Insufficient {
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
                schema_version, agent_id, executor_kind, charge_source
             ) VALUES (?1, ?2, ?3, ?4, ?5, 'active', NULL, NULL, ?6, ?7, ?8, NULL, NULL, ?9,
                       ?10, ?11, ?12)",
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
                attribution.agent_id,
                attribution.executor_kind.map(ExecutorKind::as_str),
                attribution.charge_source.map(ChargeSource::as_str),
            ],
        )?;
        tx.commit()?;

        Ok(CoreReserveOutcome::Granted(Box::new(
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
                agent_id: attribution.agent_id,
                executor_kind: attribution.executor_kind,
                charge_source: attribution.charge_source,
            },
        )))
    }

    /// Provider-snapshot overhang, except for `LocalConfigured`, whose cap
    /// is the operator's own and is never shrunk by a (possibly leftover)
    /// provider snapshot.
    fn overhang_for_tx(
        tx: &rusqlite::Connection,
        pool_id: &PoolId,
        source: CapacitySource,
        now: OffsetDateTime,
    ) -> Result<u64, LedgerError> {
        match source {
            CapacitySource::LocalConfigured { .. } => Ok(0),
            CapacitySource::ProviderSnapshot => Self::external_overhang_tx(tx, pool_id, now),
        }
    }

    /// The effective capacity for `source` inside the caller's
    /// transaction. `Err(reason)` (inner) only for
    /// [`CapacitySource::ProviderSnapshot`] with an unusable snapshot.
    fn effective_capacity_tx(
        tx: &rusqlite::Connection,
        pool: &QuotaPool,
        source: CapacitySource,
        now: OffsetDateTime,
    ) -> Result<Result<u64, SnapshotUnusable>, LedgerError> {
        match source {
            CapacitySource::LocalConfigured { cap } => Ok(Ok(cap)),
            CapacitySource::ProviderSnapshot => {
                let snapshot = Self::pool_provider_snapshot_tx(tx, &pool.pool_id, &pool.unit)?;
                Ok(metered_snapshot_limit(snapshot.as_ref(), now)
                    .map(|declared_limit| pool.capacity.min(declared_limit)))
            }
        }
    }

    fn is_charge_row_tx(
        tx: &rusqlite::Connection,
        id: SharedPoolReservationId,
    ) -> Result<bool, LedgerError> {
        let flag: i64 = tx.query_row(
            "SELECT is_charge FROM shared_pool_reservations WHERE id = ?1",
            [id.0.to_string()],
            |row| row.get(0),
        )?;
        Ok(flag != 0)
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
        self.settle_shared_attributed(id, actual, now, ExecutorAttribution::default())
    }

    /// [`Self::settle_shared`] that can also attribute the row at settle
    /// time (HORO-2194). Attribution recorded at hold time is
    /// authoritative: `attribution` only fills what the row does not yet
    /// have -- `agent_id`/`executor_kind` together if the row has no
    /// kind, and `charge_source` if it has none. It never rewrites an
    /// existing value.
    pub fn settle_shared_attributed(
        &mut self,
        id: SharedPoolReservationId,
        actual: Option<u64>,
        now: OffsetDateTime,
        attribution: ExecutorAttribution<'_>,
    ) -> Result<SharedPoolSettleOutcome, LedgerError> {
        self.settle_shared_with_watermark(id, actual, now, attribution, None)
    }

    /// [`Self::settle_shared_attributed`] that also records the transcript
    /// high-water mark the settled window counted (HORO-2196). `None`
    /// leaves the column NULL: the window counted no record, and readers
    /// keep the lane's previous watermark.
    pub fn settle_shared_with_watermark(
        &mut self,
        id: SharedPoolReservationId,
        actual: Option<u64>,
        now: OffsetDateTime,
        attribution: ExecutorAttribution<'_>,
        watermark: Option<&SettleWatermark>,
    ) -> Result<SharedPoolSettleOutcome, LedgerError> {
        let (watermark_text, ids_text) = watermark_columns(watermark)?;
        let supplied = attribution.resolve()?;
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
        let (agent_id, executor_kind) = if reservation.executor_kind.is_some() {
            (
                reservation.agent_id.clone(),
                reservation.executor_kind.map(ExecutorKind::as_str),
            )
        } else {
            (
                supplied.agent_id.clone(),
                supplied.executor_kind.map(ExecutorKind::as_str),
            )
        };
        let charge_source = reservation
            .charge_source
            .or(supplied.charge_source)
            .map(ChargeSource::as_str);
        let updated = tx.execute(
            "UPDATE shared_pool_reservations SET state = 'settled', settled_amount = ?1,
                                                  usage_known = ?2, settled_at = ?3,
                                                  agent_id = ?6, executor_kind = ?7,
                                                  charge_source = ?8, watermark_ts = ?9,
                                                  watermark_message_ids = ?10
             WHERE id = ?4 AND state = ?5",
            rusqlite::params![
                settled_value_i64,
                usage_known,
                rfc3339(now)?,
                id.0.to_string(),
                expected_prior_state,
                agent_id,
                executor_kind,
                charge_source,
                watermark_text,
                ids_text,
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

    /// Atomically extends shared-pool hold `id`'s `expires_at` to
    /// `now + min(ttl_secs, MAX_TTL_SECS)` (HORO-1766) — the launch-gate's
    /// keep-alive primitive for a long-running hold. Runs in an IMMEDIATE
    /// transaction, exactly the same discipline as every other mutating
    /// method in this module.
    ///
    /// Extends the expiry **only** for a row that is, in the same instant
    /// this transaction observes it: currently `active`, held by exactly
    /// this `principal_id`/`session_id` pair, and not already past its
    /// own `expires_at` (a stale-but-not-yet-swept hold is refused, not
    /// silently resurrected — it must go through the normal
    /// expire-then-reserve path instead). Anything else — wrong identity,
    /// not found, not active, already stale — is refused and the row is
    /// left completely untouched; this never "succeeds" quietly on a
    /// mismatch.
    pub fn renew_shared(
        &mut self,
        id: SharedPoolReservationId,
        principal_id: &PrincipalId,
        session_id: &str,
        now: OffsetDateTime,
        ttl_secs: u64,
    ) -> Result<SharedPoolRenewOutcome, LedgerError> {
        let tx = begin(&mut self.conn, TransactionBehavior::Immediate)?;
        let Some(reservation) = Self::get_shared_reservation_tx(&tx, id)? else {
            tx.commit()?;
            return Ok(SharedPoolRenewOutcome::NotFound);
        };

        let identity_matches =
            reservation.principal_id == *principal_id && reservation.session_id == session_id;
        let currently_fresh =
            reservation.state == ReservationState::Active && reservation.expires_at > now;
        if !identity_matches || !currently_fresh {
            tx.commit()?;
            return Ok(SharedPoolRenewOutcome::Refused(Box::new(reservation)));
        }

        // Same overflow-safe TTL clamp/checked-add discipline as
        // `reserve_shared` — see that method's own docs for why a bare
        // cast/duration-add is rejected outright rather than silently
        // truncated or allowed to panic.
        let ttl_seconds = i64::try_from(ttl_secs.min(MAX_TTL_SECS)).unwrap_or(i64::MAX);
        let new_expires_at = now
            .checked_add(time::Duration::seconds(ttl_seconds))
            .ok_or_else(invalid)?;

        let updated = tx.execute(
            "UPDATE shared_pool_reservations SET expires_at = ?1
             WHERE id = ?2 AND state = 'active' AND principal_id = ?3 AND session_id = ?4",
            rusqlite::params![
                rfc3339(new_expires_at)?,
                id.0.to_string(),
                principal_id.0,
                session_id,
            ],
        )?;
        if updated == 0 {
            // Lost a race against a concurrent settle/release/expire on
            // this same row between the read above and this write.
            let current = Self::get_shared_reservation_tx(&tx, id)?
                .expect("row existed moments ago under the same write lock");
            tx.commit()?;
            return Ok(SharedPoolRenewOutcome::Refused(Box::new(current)));
        }
        let updated_reservation = Self::get_shared_reservation_tx(&tx, id)?
            .expect("row just updated in this same transaction");
        tx.commit()?;
        Ok(SharedPoolRenewOutcome::Renewed(Box::new(
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
    ///
    /// # Ordering (HORO-1880)
    ///
    /// Only replaces the stored row when the new row's `observed_at` is
    /// at least as recent as the stored row's `observed_at`, or no row is
    /// stored yet. A delivery that arrives later in wall-clock time but
    /// carries a strictly OLDER `observed_at` than what is already stored
    /// (a reordered/retried provider poll) is a safe no-op instead of an
    /// overwrite — without this check, such a stale reading could
    /// silently replace a newer, more-restrictive reading and reopen
    /// capacity that is still really spent externally. On an exact tie
    /// (`observed_at` equal to the stored value) the new row wins — the
    /// same last-write-wins discipline
    /// `overlapping_provider_snapshots_reflect_only_the_latest_value_never_summed`
    /// (in `tests/shared_pool_adversarial.rs`) already exercises for
    /// same-instant re-ingests, and the only sensible tie-break since
    /// neither reading can be shown to be "more current" than the other.
    ///
    /// Comparing as parsed `OffsetDateTime` values rather than RFC3339
    /// strings sidesteps the same parser-differential risk
    /// `settled_before_tx`'s own docs describe: the `time` crate's
    /// Rfc3339 formatter omits a zero fractional-second component, so two
    /// timestamps sharing a whole second but differing in fractional
    /// precision do not always sort the same lexically as chronologically.
    ///
    /// `observed_at` strictly after `now` is rejected outright (`Err`),
    /// never stored — zero tolerance, matching the exact precedent
    /// `quota-source::provenance::AcquiredReading::validated` already
    /// sets at the acquisition boundary (`snapshot.observed_at() > now`
    /// => `AcquisitionError::FutureObservedAt`, no skew allowance): any
    /// reading that reaches this method through that boundary already
    /// satisfies `observed_at <= now`, so this guard can never reject
    /// anything the boundary accepted. Keep this in sync with
    /// `AcquiredReading::validated` — if that check ever grows a skew
    /// tolerance, this one must too, or a reading the boundary accepts
    /// would start being rejected here. This method has no live caller
    /// today (verified by `rg ingest_pool_provider_snapshot`), so it
    /// cannot assume every caller will route through that boundary
    /// first; without its own guard, a single future-dated `observed_at`
    /// (provider clock skew, a seconds/milliseconds unit bug, a malformed
    /// timestamp) would permanently win every ordering comparison above:
    /// every subsequent *correct* reading has an older `observed_at` and
    /// would be silently rejected as "stale" forever, and once the
    /// poisoned row's own `valid_until` passes, `external_overhang_tx`
    /// returns 0 — a permanent, silent fail-open exactly as serious as
    /// the bug this ticket fixes.
    pub fn ingest_pool_provider_snapshot(
        &mut self,
        pool_id: &PoolId,
        observed_at: OffsetDateTime,
        valid_until: Option<OffsetDateTime>,
        reading: &GaugeReading,
        confidence: Confidence,
        now: OffsetDateTime,
    ) -> Result<(), LedgerError> {
        if observed_at > now {
            return Err(invalid());
        }
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

        // No pool, no ingest: this method never implicitly creates a
        // pool, and a snapshot's declared unit must match the pool's own
        // — the same discipline `reserve_shared_core` enforces on the
        // reserve side.
        let pool_row: Option<PoolRow> = tx
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
        let Some(pool_row) = pool_row else {
            tx.commit()?;
            return Err(LedgerError::NoSuchPool {
                pool_id: pool_id.0.clone(),
            });
        };
        let pool = row_to_pool(pool_id.clone(), pool_row)?;
        if let GaugeReading::Used { used, .. } = reading {
            if used.unit != pool.unit {
                return Err(LedgerError::QuotaUnitMismatch {
                    expected: pool.unit,
                    actual: used.unit.clone(),
                });
            }
        }

        let existing_observed_at: Option<String> = tx
            .query_row(
                "SELECT observed_at FROM shared_pool_provider_snapshots WHERE pool_id = ?1",
                [&pool_id.0],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(existing_observed_at) = existing_observed_at {
            if observed_at < parse_time(&existing_observed_at)? {
                // Stale, out-of-order delivery: strictly predates the row
                // already stored. Never overwrite — see this method's own
                // docs for why.
                tx.commit()?;
                return Ok(());
            }
        }

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
                attribution: Default::default(),
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
                attribution: Default::default(),
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
                attribution: Default::default(),
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
                attribution: Default::default(),
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
                attribution: Default::default(),
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
                attribution: Default::default(),
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
                    attribution: Default::default(),
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
                attribution: Default::default(),
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
            attribution: Default::default(),
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
                attribution: Default::default(),
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
    fn renew_extends_expiry_for_the_matching_active_holder() {
        let mut store = LedgerStore::open_in_memory().unwrap();
        store
            .ensure_pool(&pool_id(), &QuotaUnit::Tokens, 10, now())
            .unwrap();
        let SharedPoolReserveOutcome::Granted(reservation) = reserve(&mut store, "s1", 5, "k1")
        else {
            panic!("expected Granted");
        };
        let original_expiry = reservation.expires_at;

        let later = now() + time::Duration::seconds(60);
        let outcome = store
            .renew_shared(reservation.id, &principal(1), "s1", later, 900)
            .unwrap();
        match outcome {
            SharedPoolRenewOutcome::Renewed(renewed) => {
                assert!(renewed.expires_at > original_expiry);
                assert_eq!(renewed.expires_at, later + time::Duration::seconds(900));
                assert_eq!(renewed.state, ReservationState::Active);
            }
            other => panic!("expected Renewed, got {other:?}"),
        }
    }

    #[test]
    fn renew_with_wrong_principal_is_refused_not_silently_applied() {
        let mut store = LedgerStore::open_in_memory().unwrap();
        store
            .ensure_pool(&pool_id(), &QuotaUnit::Tokens, 10, now())
            .unwrap();
        let SharedPoolReserveOutcome::Granted(reservation) = reserve(&mut store, "s1", 5, "k1")
        else {
            panic!("expected Granted");
        };
        let original_expiry = reservation.expires_at;

        let outcome = store
            .renew_shared(reservation.id, &principal(2), "s1", now(), 900)
            .unwrap();
        match outcome {
            SharedPoolRenewOutcome::Refused(current) => {
                assert_eq!(
                    current.expires_at, original_expiry,
                    "expiry must be untouched"
                );
            }
            other => panic!("expected Refused, got {other:?}"),
        }
    }

    #[test]
    fn renew_with_wrong_session_is_refused_not_silently_applied() {
        let mut store = LedgerStore::open_in_memory().unwrap();
        store
            .ensure_pool(&pool_id(), &QuotaUnit::Tokens, 10, now())
            .unwrap();
        let SharedPoolReserveOutcome::Granted(reservation) = reserve(&mut store, "s1", 5, "k1")
        else {
            panic!("expected Granted");
        };
        let original_expiry = reservation.expires_at;

        let outcome = store
            .renew_shared(
                reservation.id,
                &principal(1),
                "a-different-session",
                now(),
                900,
            )
            .unwrap();
        match outcome {
            SharedPoolRenewOutcome::Refused(current) => {
                assert_eq!(current.expires_at, original_expiry);
            }
            other => panic!("expected Refused, got {other:?}"),
        }
    }

    #[test]
    fn renew_on_a_released_row_is_refused() {
        let mut store = LedgerStore::open_in_memory().unwrap();
        store
            .ensure_pool(&pool_id(), &QuotaUnit::Tokens, 10, now())
            .unwrap();
        let SharedPoolReserveOutcome::Granted(reservation) = reserve(&mut store, "s1", 5, "k1")
        else {
            panic!("expected Granted");
        };
        store.release_shared(reservation.id, now()).unwrap();

        let outcome = store
            .renew_shared(reservation.id, &principal(1), "s1", now(), 900)
            .unwrap();
        match outcome {
            SharedPoolRenewOutcome::Refused(current) => {
                assert_eq!(current.state, ReservationState::Released);
            }
            other => panic!("expected Refused, got {other:?}"),
        }
    }

    #[test]
    fn renew_on_an_expired_row_is_refused() {
        let mut store = LedgerStore::open_in_memory().unwrap();
        store
            .ensure_pool(&pool_id(), &QuotaUnit::Tokens, 10, now())
            .unwrap();
        store
            .reserve_shared(SharedPoolReserveRequest {
                pool_id: &pool_id(),
                principal_id: &principal(1),
                session_id: "s1",
                amount: 5,
                unit: &QuotaUnit::Tokens,
                idempotency_key: "k1",
                now: now(),
                ttl_secs: 60,
                attribution: Default::default(),
            })
            .unwrap();
        let later = now() + time::Duration::seconds(120);
        let expired = store.expire_stale_shared_pool_reservations(later).unwrap();
        assert_eq!(expired.len(), 1);

        let outcome = store
            .renew_shared(expired[0].id, &principal(1), "s1", later, 900)
            .unwrap();
        match outcome {
            SharedPoolRenewOutcome::Refused(current) => {
                assert_eq!(current.state, ReservationState::Expired);
            }
            other => panic!("expected Refused, got {other:?}"),
        }
    }

    #[test]
    fn renew_on_a_stale_but_not_yet_swept_active_row_is_refused() {
        // A hold that is still technically `state == active` in storage
        // but already past its own `expires_at` (the sweep just hasn't
        // run yet) must not be resurrected by a renew — it must go
        // through the expire-then-reserve path instead.
        let mut store = LedgerStore::open_in_memory().unwrap();
        store
            .ensure_pool(&pool_id(), &QuotaUnit::Tokens, 10, now())
            .unwrap();
        let SharedPoolReserveOutcome::Granted(reservation) = store
            .reserve_shared(SharedPoolReserveRequest {
                pool_id: &pool_id(),
                principal_id: &principal(1),
                session_id: "s1",
                amount: 5,
                unit: &QuotaUnit::Tokens,
                idempotency_key: "k1",
                now: now(),
                ttl_secs: 60,
                attribution: Default::default(),
            })
            .unwrap()
        else {
            panic!("expected Granted");
        };
        let past_expiry = now() + time::Duration::seconds(120);

        let outcome = store
            .renew_shared(reservation.id, &principal(1), "s1", past_expiry, 900)
            .unwrap();
        match outcome {
            SharedPoolRenewOutcome::Refused(current) => {
                assert_eq!(current.state, ReservationState::Active);
            }
            other => panic!("expected Refused, got {other:?}"),
        }
    }

    #[test]
    fn renew_on_a_nonexistent_id_reports_not_found() {
        let mut store = LedgerStore::open_in_memory().unwrap();
        let outcome = store
            .renew_shared(
                SharedPoolReservationId::new(),
                &principal(1),
                "s1",
                now(),
                900,
            )
            .unwrap();
        assert!(matches!(outcome, SharedPoolRenewOutcome::NotFound));
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
                        attribution: Default::default(),
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

    mod metered {
        use super::*;
        use libra_governor_domain::{QuotaAmount, SnapshotUnusable};

        fn metered_pool_id() -> PoolId {
            PoolId("metered:acme/usd-2026-10".to_string())
        }

        fn metered_reserve(
            store: &mut LedgerStore,
            session_id: &str,
            amount: u64,
            idempotency_key: &str,
            session_cap: Option<u64>,
        ) -> MeteredReserveOutcome {
            store
                .reserve_shared_metered(MeteredReserveRequest {
                    base: SharedPoolReserveRequest {
                        pool_id: &metered_pool_id(),
                        principal_id: &principal(1),
                        session_id,
                        amount,
                        unit: &QuotaUnit::UsdCents,
                        idempotency_key,
                        now: now(),
                        ttl_secs: 3600,
                        attribution: Default::default(),
                    },
                    session_cap,
                    capacity_source: CapacitySource::ProviderSnapshot,
                })
                .unwrap()
        }

        fn ingest_used(
            store: &mut LedgerStore,
            used: u64,
            limit: Option<u64>,
            valid_until: Option<OffsetDateTime>,
        ) {
            store
                .ingest_pool_provider_snapshot(
                    &metered_pool_id(),
                    now(),
                    valid_until,
                    &GaugeReading::Used {
                        used: QuotaAmount::new(QuotaUnit::UsdCents, used),
                        limit,
                    },
                    Confidence::Medium,
                    now(),
                )
                .unwrap();
        }

        #[test]
        fn reserve_with_no_snapshot_is_refused_as_missing() {
            let mut store = LedgerStore::open_in_memory().unwrap();
            store
                .ensure_pool(&metered_pool_id(), &QuotaUnit::UsdCents, 50_000, now())
                .unwrap();
            match metered_reserve(&mut store, "s1", 100, "k1", None) {
                MeteredReserveOutcome::SnapshotUnusable(SnapshotUnusable::Missing) => {}
                other => panic!("expected SnapshotUnusable(Missing), got {other:?}"),
            }
        }

        #[test]
        fn reserve_with_undisclosed_snapshot_is_refused() {
            let mut store = LedgerStore::open_in_memory().unwrap();
            store
                .ensure_pool(&metered_pool_id(), &QuotaUnit::UsdCents, 50_000, now())
                .unwrap();
            store
                .ingest_pool_provider_snapshot(
                    &metered_pool_id(),
                    now(),
                    Some(now() + time::Duration::hours(1)),
                    &GaugeReading::Undisclosed,
                    Confidence::Medium,
                    now(),
                )
                .unwrap();
            match metered_reserve(&mut store, "s1", 100, "k1", None) {
                MeteredReserveOutcome::SnapshotUnusable(SnapshotUnusable::Undisclosed) => {}
                other => panic!("expected SnapshotUnusable(Undisclosed), got {other:?}"),
            }
        }

        #[test]
        fn reserve_with_no_valid_until_is_refused() {
            let mut store = LedgerStore::open_in_memory().unwrap();
            store
                .ensure_pool(&metered_pool_id(), &QuotaUnit::UsdCents, 50_000, now())
                .unwrap();
            ingest_used(&mut store, 0, Some(50_000), None);
            match metered_reserve(&mut store, "s1", 100, "k1", None) {
                MeteredReserveOutcome::SnapshotUnusable(SnapshotUnusable::NoValidUntil) => {}
                other => panic!("expected SnapshotUnusable(NoValidUntil), got {other:?}"),
            }
        }

        #[test]
        fn reserve_with_stale_snapshot_is_refused() {
            let mut store = LedgerStore::open_in_memory().unwrap();
            store
                .ensure_pool(&metered_pool_id(), &QuotaUnit::UsdCents, 50_000, now())
                .unwrap();
            ingest_used(
                &mut store,
                0,
                Some(50_000),
                Some(now() - time::Duration::seconds(1)),
            );
            match metered_reserve(&mut store, "s1", 100, "k1", None) {
                MeteredReserveOutcome::SnapshotUnusable(SnapshotUnusable::Stale) => {}
                other => panic!("expected SnapshotUnusable(Stale), got {other:?}"),
            }
        }

        #[test]
        fn reserve_with_missing_limit_is_refused() {
            let mut store = LedgerStore::open_in_memory().unwrap();
            store
                .ensure_pool(&metered_pool_id(), &QuotaUnit::UsdCents, 50_000, now())
                .unwrap();
            ingest_used(&mut store, 0, None, Some(now() + time::Duration::hours(1)));
            match metered_reserve(&mut store, "s1", 100, "k1", None) {
                MeteredReserveOutcome::SnapshotUnusable(SnapshotUnusable::LimitMissing) => {}
                other => panic!("expected SnapshotUnusable(LimitMissing), got {other:?}"),
            }
        }

        #[test]
        fn effective_capacity_is_min_of_pool_capacity_and_declared_limit() {
            let mut store = LedgerStore::open_in_memory().unwrap();
            store
                .ensure_pool(&metered_pool_id(), &QuotaUnit::UsdCents, 50_000, now())
                .unwrap();
            // Declared limit (30_000) is below the pool's stored capacity
            // (50_000) — effective capacity must be the smaller value.
            ingest_used(
                &mut store,
                0,
                Some(30_000),
                Some(now() + time::Duration::hours(1)),
            );
            assert!(matches!(
                metered_reserve(&mut store, "s1", 30_000, "k1", None),
                MeteredReserveOutcome::Granted(_)
            ));
            match metered_reserve(&mut store, "s2", 1, "k2", None) {
                MeteredReserveOutcome::Insufficient { admission, .. } => {
                    assert_eq!(admission.capacity, 30_000);
                    assert_eq!(admission.remaining(), 0);
                }
                other => panic!("expected Insufficient, got {other:?}"),
            }
        }

        #[test]
        fn fresh_usable_snapshot_grants_up_to_effective_capacity() {
            let mut store = LedgerStore::open_in_memory().unwrap();
            store
                .ensure_pool(&metered_pool_id(), &QuotaUnit::UsdCents, 50_000, now())
                .unwrap();
            ingest_used(
                &mut store,
                0,
                Some(50_000),
                Some(now() + time::Duration::hours(1)),
            );
            assert!(matches!(
                metered_reserve(&mut store, "s1", 50_000, "k1", None),
                MeteredReserveOutcome::Granted(_)
            ));
        }

        #[test]
        fn session_cap_refuses_before_pool_wide_capacity_is_exhausted() {
            let mut store = LedgerStore::open_in_memory().unwrap();
            store
                .ensure_pool(&metered_pool_id(), &QuotaUnit::UsdCents, 50_000, now())
                .unwrap();
            ingest_used(
                &mut store,
                0,
                Some(50_000),
                Some(now() + time::Duration::hours(1)),
            );
            assert!(matches!(
                metered_reserve(&mut store, "s1", 20_000, "k1", Some(25_000)),
                MeteredReserveOutcome::Granted(_)
            ));
            match metered_reserve(&mut store, "s1", 6_000, "k2", Some(25_000)) {
                MeteredReserveOutcome::SessionCapExceeded {
                    committed,
                    cap,
                    requested,
                } => {
                    assert_eq!(committed, 20_000);
                    assert_eq!(cap, 25_000);
                    assert_eq!(requested, 6_000);
                }
                other => panic!("expected SessionCapExceeded, got {other:?}"),
            }
            // A different session under the same pool is unaffected by
            // s1's cap.
            assert!(matches!(
                metered_reserve(&mut store, "s2", 6_000, "k3", Some(25_000)),
                MeteredReserveOutcome::Granted(_)
            ));
        }

        #[test]
        fn session_cap_counts_settled_amount_not_original_reserved_amount() {
            let mut store = LedgerStore::open_in_memory().unwrap();
            store
                .ensure_pool(&metered_pool_id(), &QuotaUnit::UsdCents, 50_000, now())
                .unwrap();
            ingest_used(
                &mut store,
                0,
                Some(50_000),
                Some(now() + time::Duration::hours(1)),
            );
            let MeteredReserveOutcome::Granted(reservation) =
                metered_reserve(&mut store, "s1", 20_000, "k1", Some(25_000))
            else {
                panic!("expected Granted");
            };
            // Settle at 12_000 (8_000 refunded) — the cap accounting must
            // reflect the settled amount, not the original 20_000 reserve.
            store
                .settle_shared(reservation.id, Some(12_000), now())
                .unwrap();
            match metered_reserve(&mut store, "s1", 13_000, "k2", Some(25_000)) {
                MeteredReserveOutcome::Granted(_) => {}
                other => panic!(
                    "expected Granted (12_000 settled + 13_000 requested = 25_000 == cap), got {other:?}"
                ),
            }
        }

        #[test]
        fn ingest_refuses_unit_mismatch() {
            let mut store = LedgerStore::open_in_memory().unwrap();
            store
                .ensure_pool(&metered_pool_id(), &QuotaUnit::UsdCents, 50_000, now())
                .unwrap();
            let err = store
                .ingest_pool_provider_snapshot(
                    &metered_pool_id(),
                    now(),
                    Some(now() + time::Duration::hours(1)),
                    &GaugeReading::Used {
                        used: QuotaAmount::new(QuotaUnit::Tokens, 0),
                        limit: Some(50_000),
                    },
                    Confidence::Medium,
                    now(),
                )
                .unwrap_err();
            assert!(matches!(err, LedgerError::QuotaUnitMismatch { .. }));
        }

        #[test]
        fn ingest_refuses_when_no_pool_exists() {
            let mut store = LedgerStore::open_in_memory().unwrap();
            let err = store
                .ingest_pool_provider_snapshot(
                    &metered_pool_id(),
                    now(),
                    Some(now() + time::Duration::hours(1)),
                    &GaugeReading::Used {
                        used: QuotaAmount::new(QuotaUnit::UsdCents, 0),
                        limit: Some(50_000),
                    },
                    Confidence::Medium,
                    now(),
                )
                .unwrap_err();
            assert!(matches!(err, LedgerError::NoSuchPool { .. }));
        }

        #[test]
        fn reserve_shared_unmetered_path_is_unaffected_by_missing_snapshot() {
            // AC: the plain launch-slot path (no metered gate) must never
            // consult the snapshot gate at all — a pool with no snapshot
            // still admits normally through `reserve_shared`.
            let mut store = LedgerStore::open_in_memory().unwrap();
            store
                .ensure_pool(&pool_id(), &QuotaUnit::Tokens, 10, now())
                .unwrap();
            assert!(matches!(
                reserve(&mut store, "s1", 10, "k1"),
                SharedPoolReserveOutcome::Granted(_)
            ));
        }

        #[test]
        fn snapshot_gate_is_read_inside_the_reserve_transaction() {
            // A concurrent writer commits a stale/undisclosed snapshot
            // between test setup and the reserve call — but because the
            // gate re-reads the snapshot inside `reserve_shared_core`'s
            // own IMMEDIATE transaction (not from a value captured
            // earlier), the reserve sees the up-to-date row, not a stale
            // in-memory copy.
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("ledger.sqlite3");
            {
                let mut store = LedgerStore::open(&path).unwrap();
                store
                    .ensure_pool(&metered_pool_id(), &QuotaUnit::UsdCents, 50_000, now())
                    .unwrap();
                store
                    .ingest_pool_provider_snapshot(
                        &metered_pool_id(),
                        now(),
                        Some(now() + time::Duration::hours(1)),
                        &GaugeReading::Used {
                            used: QuotaAmount::new(QuotaUnit::UsdCents, 0),
                            limit: Some(50_000),
                        },
                        Confidence::Medium,
                        now(),
                    )
                    .unwrap();
            }
            // A second connection against the same file, simulating a
            // separate writer process, replaces the snapshot with an
            // undisclosed reading before this process's reserve call.
            {
                let mut writer = LedgerStore::open(&path).unwrap();
                writer
                    .ingest_pool_provider_snapshot(
                        &metered_pool_id(),
                        now() + time::Duration::seconds(1),
                        Some(now() + time::Duration::hours(2)),
                        &GaugeReading::Undisclosed,
                        Confidence::Medium,
                        now() + time::Duration::seconds(1),
                    )
                    .unwrap();
            }
            let mut store = LedgerStore::open(&path).unwrap();
            match metered_reserve(&mut store, "s1", 100, "k1", None) {
                MeteredReserveOutcome::SnapshotUnusable(SnapshotUnusable::Undisclosed) => {}
                other => panic!(
                    "expected the reserve to observe the concurrently-committed undisclosed \
                     snapshot, got {other:?}"
                ),
            }
        }

        #[test]
        fn shared_pool_session_usage_reports_per_session_active_and_settled() {
            let mut store = LedgerStore::open_in_memory().unwrap();
            store
                .ensure_pool(&metered_pool_id(), &QuotaUnit::UsdCents, 50_000, now())
                .unwrap();
            ingest_used(
                &mut store,
                0,
                Some(50_000),
                Some(now() + time::Duration::hours(1)),
            );
            let MeteredReserveOutcome::Granted(r1) =
                metered_reserve(&mut store, "s1", 10_000, "k1", None)
            else {
                panic!("expected Granted");
            };
            store.settle_shared(r1.id, Some(6_000), now()).unwrap();
            assert!(matches!(
                metered_reserve(&mut store, "s2", 4_000, "k2", None),
                MeteredReserveOutcome::Granted(_)
            ));

            let mut usage = store.shared_pool_session_usage(&metered_pool_id()).unwrap();
            usage.sort_by(|a, b| a.session_id.cmp(&b.session_id));
            assert_eq!(usage.len(), 2);
            assert_eq!(usage[0].session_id, "s1");
            assert_eq!(usage[0].active, 0);
            assert_eq!(usage[0].settled, 6_000);
            assert_eq!(usage[1].session_id, "s2");
            assert_eq!(usage[1].active, 4_000);
            assert_eq!(usage[1].settled, 0);
        }

        #[test]
        fn pool_provider_snapshot_returns_the_most_recently_ingested_reading() {
            let mut store = LedgerStore::open_in_memory().unwrap();
            store
                .ensure_pool(&metered_pool_id(), &QuotaUnit::UsdCents, 50_000, now())
                .unwrap();
            assert!(store
                .pool_provider_snapshot(&metered_pool_id())
                .unwrap()
                .is_none());
            ingest_used(
                &mut store,
                12_000,
                Some(50_000),
                Some(now() + time::Duration::hours(1)),
            );
            let snapshot = store
                .pool_provider_snapshot(&metered_pool_id())
                .unwrap()
                .unwrap();
            match snapshot.reading {
                GaugeReading::Used { used, limit } => {
                    assert_eq!(used.value, 12_000);
                    assert_eq!(limit, Some(50_000));
                }
                other => panic!("expected Used, got {other:?}"),
            }
        }
    }
}

/// HORO-2195 read helpers: lane lookup, per-session holds, pool listing.
#[cfg(test)]
mod lane_read_tests {
    use super::*;
    use libra_governor_domain::{ExecutorKind, QuotaUnit};

    fn t0() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap()
    }

    fn pool(n: &str) -> PoolId {
        PoolId(format!("metered:local-budget/{n}"))
    }

    fn owner() -> PrincipalId {
        PrincipalId("local-budget".to_string())
    }

    fn hold(
        store: &mut LedgerStore,
        pool_id: &PoolId,
        session: &str,
        attribution: ExecutorAttribution<'_>,
        key: &str,
        now: OffsetDateTime,
    ) -> SharedPoolReservation {
        match store
            .reserve_shared(SharedPoolReserveRequest {
                pool_id,
                principal_id: &owner(),
                session_id: session,
                amount: 10,
                unit: &QuotaUnit::UsdCents,
                idempotency_key: key,
                now,
                ttl_secs: 100,
                attribution,
            })
            .unwrap()
        {
            SharedPoolReserveOutcome::Granted(r) => *r,
            other => panic!("expected Granted, got {other:?}"),
        }
    }

    fn store_with(pools: &[&str]) -> LedgerStore {
        let mut store = LedgerStore::open_in_memory().unwrap();
        for p in pools {
            store
                .ensure_pool(&pool(p), &QuotaUnit::UsdCents, 1_000, t0())
                .unwrap();
        }
        store
    }

    #[test]
    fn lane_lookup_distinguishes_agent_absent_unattributed_and_named_agent() {
        let mut store = store_with(&["monthly-2027-01"]);
        let p = pool("monthly-2027-01");
        let main = hold(
            &mut store,
            &p,
            "s",
            ExecutorAttribution::main_executor(None),
            "k-main",
            t0(),
        );
        let unknown = hold(
            &mut store,
            &p,
            "s",
            ExecutorAttribution::unattributed(None),
            "k-unk",
            t0(),
        );
        let agent = hold(
            &mut store,
            &p,
            "s",
            ExecutorAttribution::agent("a1", None),
            "k-a1",
            t0(),
        );
        let find = |agent_id: Option<&str>, kind| {
            store
                .active_shared_hold_for_lane(&p, &owner(), "s", agent_id, kind, t0())
                .unwrap()
                .map(|r| r.id)
        };
        assert_eq!(find(None, ExecutorKind::AgentAbsent), Some(main.id));
        assert_eq!(find(None, ExecutorKind::Unattributed), Some(unknown.id));
        assert_eq!(find(Some("a1"), ExecutorKind::Agent), Some(agent.id));
        assert_eq!(find(Some("a2"), ExecutorKind::Agent), None);
        assert_eq!(
            store
                .active_shared_hold_for_lane(
                    &p,
                    &owner(),
                    "other",
                    None,
                    ExecutorKind::AgentAbsent,
                    t0()
                )
                .unwrap(),
            None
        );
    }

    #[test]
    fn lane_lookup_ignores_expired_settled_and_charge_rows() {
        let mut store = store_with(&["monthly-2027-01"]);
        let p = pool("monthly-2027-01");
        let h = hold(
            &mut store,
            &p,
            "s",
            ExecutorAttribution::main_executor(None),
            "k",
            t0(),
        );
        let lane = |store: &LedgerStore, now| {
            store
                .active_shared_hold_for_lane(
                    &p,
                    &owner(),
                    "s",
                    None,
                    ExecutorKind::AgentAbsent,
                    now,
                )
                .unwrap()
        };
        assert!(lane(&store, t0() + time::Duration::seconds(99)).is_some());
        // expires_at == now is expired.
        assert!(lane(&store, t0() + time::Duration::seconds(100)).is_none());
        store.settle_shared(h.id, Some(5), t0()).unwrap();
        assert!(lane(&store, t0()).is_none());
        // A settled charge row for the same lane is never an active hold.
        store
            .charge_shared(SharedPoolChargeRequest {
                pool_id: &p,
                principal_id: &owner(),
                session_id: "s",
                amount: 3,
                unit: &QuotaUnit::UsdCents,
                idempotency_key: "c",
                now: t0(),
                attribution: ExecutorAttribution::main_executor(None),
                capacity_source: CapacitySource::LocalConfigured { cap: 1_000 },
                usage_known: true,
            })
            .unwrap();
        assert!(lane(&store, t0()).is_none());
    }

    #[test]
    fn session_holds_and_latest_pool_follow_creation_time_not_pool_name() {
        let mut store = store_with(&["monthly-2027-01", "monthly-2027-02"]);
        let jan = pool("monthly-2027-01");
        let feb = pool("monthly-2027-02");
        hold(
            &mut store,
            &feb,
            "s",
            ExecutorAttribution::main_executor(None),
            "k1",
            t0(),
        );
        // Created later but in the alphabetically earlier pool.
        let later = t0() + time::Duration::seconds(10);
        hold(
            &mut store,
            &jan,
            "s",
            ExecutorAttribution::agent("a", None),
            "k2",
            later,
        );
        assert_eq!(
            store
                .latest_shared_hold_pool_for_session(&owner(), "s", "metered:local-budget/monthly-")
                .unwrap(),
            Some(jan.clone())
        );
        assert_eq!(
            store
                .latest_shared_hold_pool_for_session(
                    &owner(),
                    "nobody",
                    "metered:local-budget/monthly-"
                )
                .unwrap(),
            None
        );
        // A different prefix matches nothing.
        assert_eq!(
            store
                .latest_shared_hold_pool_for_session(&owner(), "s", "metered:acme/")
                .unwrap(),
            None
        );
        let holds = store
            .active_shared_holds_for_session(&owner(), "s", "metered:local-budget/", later)
            .unwrap();
        assert_eq!(holds.len(), 2);
        assert!(store
            .active_shared_holds_for_session(
                &owner(),
                "s",
                "metered:local-budget/",
                later + time::Duration::seconds(1000)
            )
            .unwrap()
            .is_empty());
    }

    #[test]
    fn pool_listing_is_prefix_exact_and_sorted() {
        let mut store = store_with(&["monthly-2027-03", "monthly-2027-01"]);
        store
            .ensure_pool(
                &PoolId("metered:acme/monthly-2027-09".into()),
                &QuotaUnit::UsdCents,
                1,
                t0(),
            )
            .unwrap();
        let ids = store
            .shared_pool_ids_with_prefix("metered:local-budget/monthly-")
            .unwrap();
        assert_eq!(ids, vec![pool("monthly-2027-01"), pool("monthly-2027-03")]);
    }

    #[test]
    fn key_lookup_spans_pools_and_sums_saturate_instead_of_failing() {
        let mut store = store_with(&["monthly-2027-01", "monthly-2027-02"]);
        let jan = pool("monthly-2027-01");
        let charge = |store: &mut LedgerStore, pool_id: &PoolId, key: &str, amount: u64| {
            store
                .charge_shared(SharedPoolChargeRequest {
                    pool_id,
                    principal_id: &owner(),
                    session_id: "s",
                    amount,
                    unit: &QuotaUnit::UsdCents,
                    idempotency_key: key,
                    now: t0(),
                    attribution: ExecutorAttribution::main_executor(None),
                    capacity_source: CapacitySource::LocalConfigured { cap: 1_000 },
                    usage_known: true,
                })
                .unwrap()
        };
        charge(&mut store, &jan, "k", 5);
        let found = store
            .find_shared_reservation_by_key(&owner(), "k", "metered:local-budget/")
            .unwrap()
            .unwrap();
        assert_eq!(found.pool_id, jan);
        assert!(store
            .find_shared_reservation_by_key(&owner(), "other", "metered:local-budget/")
            .unwrap()
            .is_none());

        // Three rows of i64::MAX overflow SQLite's integer SUM; the pool
        // must still be readable and chargeable, with the total saturated.
        for key in ["a", "b", "c"] {
            charge(&mut store, &jan, key, i64::MAX as u64);
        }
        let admission = store
            .pool_admission_for(&jan, t0(), CapacitySource::LocalConfigured { cap: 1_000 })
            .unwrap()
            .unwrap();
        assert_eq!(admission.settled, u64::MAX);
        assert_eq!(admission.remaining(), i64::MIN);
        assert!(matches!(
            charge(&mut store, &jan, "d", 1),
            SharedPoolChargeOutcome::Charged { .. }
        ));
    }
}
