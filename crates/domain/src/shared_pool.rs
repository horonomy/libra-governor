//! Flat shared-quota-pool reservation types (HORO-1763).
//!
//! A [`QuotaPool`] is the one concrete shared-pool need this campaign
//! provides: one level, no nesting, no membership/hierarchy (that stays
//! HORO-1694's research-latent scope — see
//! [`crate::quota_window::QuotaSubject::SharedPool`], which this module's
//! [`PoolId`] identifier is the same type as). Multiple concurrent
//! sessions/actors may each hold a [`SharedPoolReservation`] against the
//! same pool; the ledger crate (`libra_governor_ledger::shared_pool`)
//! supplies the atomic reserve/settle/release mechanics that make their
//! combined holds never exceed the pool's capacity — see that module's
//! docs for the transaction discipline, which mirrors
//! `libra_governor_ledger::reservation` exactly rather than inventing a
//! second locking scheme.
//!
//! # Why this reuses [`ReservationState`] and does not invent a second
//! lifecycle enum
//!
//! A shared-pool hold has exactly the same four-state lifecycle as a
//! task-level [`Reservation`] (active, settled, released, expired), for
//! the same reasons. Reusing the type means a reader never has to check
//! whether "settled" here means something subtly different than it does
//! on a task reservation.
//!
//! # Why actor identity is [`PrincipalId`], not a new identity scheme
//!
//! [`PrincipalId`] already exists (HORO-1666, `economic_attribution`) as
//! this crate's "who" for an economic fact. A shared pool is attributing
//! holds across sessions precisely because *who* is holding capacity
//! matters when a race is later examined — the same need
//! `EconomicAttribution` already serves, so this module attaches to it
//! rather than growing a parallel notion of identity.
//!
//! # Why reconciliation here is a subtraction, not [`crate::quota_window`]'s
//! full window evaluation
//!
//! [`crate::quota_window`] models several *independent, time-scoped*
//! quota constraints evaluated as a pure function of evidence. A shared
//! pool's admission question is simpler and does not vary with time: "how
//! much of a fixed capacity is already settled, held, or evidenced as
//! externally consumed but not observed by Libra". This module reuses
//! that contract's *distinction* — a [`GaugeReading`] snapshot is a
//! point-in-time gauge, never additive spend — without reusing its
//! window-kind machinery, which exists to answer a different, harder
//! question this ticket does not have.
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::economic_attribution::PrincipalId;
use crate::quota_window::{GaugeReading, PoolId, QuotaUnit};
use crate::reservation::ReservationState;
use crate::Confidence;

/// Traceability tag every persisted [`QuotaPool`]/[`SharedPoolReservation`]
/// row is tagged with, following this crate's existing
/// `*_SCHEMA_VERSION` convention.
pub const SHARED_POOL_RESERVATION_SCHEMA_VERSION: &str = "shared-pool-reservation-v1";

/// Identifier for one [`SharedPoolReservation`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SharedPoolReservationId(pub Uuid);

impl SharedPoolReservationId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for SharedPoolReservationId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for SharedPoolReservationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A flat, non-hierarchical shared pool's declared capacity (HORO-1763).
/// One level, no membership — see module docs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaPool {
    pub pool_id: PoolId,
    pub unit: QuotaUnit,
    /// The pool's total capacity, in `unit`'s base integer. Fixed at
    /// first creation — mirrors
    /// [`crate::reservation::TaskBudget::hard_limit`]'s "the limit in
    /// force at admission is the limit for the life of" the envelope;
    /// see `libra_governor_ledger::shared_pool::ensure_pool` for the
    /// never-overwrite discipline this implies.
    pub capacity: u64,
    pub schema_version: String,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

/// One actor's hold against a [`QuotaPool`] (HORO-1763). Structurally the
/// shared-pool analogue of [`crate::reservation::Reservation`]: same
/// lifecycle, same idempotency contract, scoped to a pool instead of a
/// task.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SharedPoolReservation {
    pub id: SharedPoolReservationId,
    pub pool_id: PoolId,
    /// Who is reserving — recorded for attribution, never itself an
    /// authorization token (same discipline as
    /// [`crate::reservation::Reservation::session_id`]): only the ledger,
    /// inside an atomic transaction validated against the pool's own
    /// recorded capacity, can create, settle, or release a hold.
    pub principal_id: PrincipalId,
    /// Which session/subagent under that principal holds this
    /// reservation.
    pub session_id: String,
    pub amount: u64,
    pub state: ReservationState,
    /// `Some` iff `state == Settled`.
    pub settled_amount: Option<u64>,
    /// `Some(false)` when settlement had no reported usage figure to
    /// compare against (the conservative fallback settled at the full
    /// reserved amount) — same convention as
    /// [`crate::reservation::Reservation::usage_known`].
    pub usage_known: Option<bool>,
    /// Caller-supplied replay key, unique per pool — the structural half
    /// of idempotent reserve retries.
    pub idempotency_key: String,
    pub created_at: OffsetDateTime,
    pub expires_at: OffsetDateTime,
    pub settled_at: Option<OffsetDateTime>,
    pub released_at: Option<OffsetDateTime>,
    pub schema_version: String,
}

impl SharedPoolReservation {
    /// `settled_amount - amount` when the actual cost exceeded what was
    /// reserved. Derived, never stored — mirrors
    /// [`crate::reservation::Reservation::overrun`].
    pub fn overrun(&self) -> Option<u64> {
        let settled = self.settled_amount?;
        settled.checked_sub(self.amount).filter(|delta| *delta > 0)
    }

    /// `amount - settled_amount` when the actual cost came in under what
    /// was reserved.
    pub fn refunded(&self) -> Option<u64> {
        let settled = self.settled_amount?;
        self.amount.checked_sub(settled).filter(|delta| *delta > 0)
    }

    /// How much of this hold is still outstanding against the pool given
    /// its current state — `Active` outstands the full amount,
    /// `Settled` outstands nothing further (the ledger's settle path
    /// removes it from the `active` sum in the same transaction it writes
    /// `settled_amount`), `Released`/`Expired` outstand none.
    pub fn outstanding(&self) -> u64 {
        match self.state {
            ReservationState::Active => self.amount,
            ReservationState::Settled | ReservationState::Released | ReservationState::Expired => 0,
        }
    }
}

/// The ledger-internal arithmetic a pool admission decision is derived
/// from, read in one transaction — the shared-pool analogue of
/// [`crate::reservation::BudgetSnapshot`].
///
/// `external_overhang` is the conservative-reduction term from AC3: when a
/// fresh provider [`GaugeReading::Used`] snapshot reports more consumption
/// than Libra's own `settled + active` sum accounts for, the difference is
/// evidence of external activity Libra never observed — it reduces
/// [`Self::remaining`] rather than being added to `settled` (a gauge
/// reading is never additive spend; see module docs). A stale, missing,
/// or undisclosed snapshot contributes zero overhang: it is not numeric
/// evidence of a specific unobserved amount, so it cannot be subtracted as
/// one — it remains visible to a caller via
/// `libra_governor_ledger::shared_pool::PoolSnapshot`'s own fields instead
/// of being guessed into this arithmetic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolAdmission {
    pub capacity: u64,
    pub settled: u64,
    pub active: u64,
    pub external_overhang: u64,
}

impl PoolAdmission {
    /// `capacity - settled - active - external_overhang`, computed with
    /// i128 intermediates and saturated into i64 — never clamped at zero,
    /// so an overrun (or an overhang alone exceeding capacity) stays
    /// visible rather than silently reading as "full headroom available".
    /// Saturating rather than a checked/wrapping subtraction: this is the
    /// same fail-closed discipline `quota_window::evaluate`'s
    /// `remaining_i64` already applies, reused here rather than
    /// reinvented.
    pub fn remaining(&self) -> i64 {
        let value = self.capacity as i128
            - self.settled as i128
            - self.active as i128
            - self.external_overhang as i128;
        value.clamp(i64::MIN as i128, i64::MAX as i128) as i64
    }

    /// Whether `requested` additional units may be admitted without the
    /// combined hold exceeding [`Self::remaining`]. Saturating-safe: both
    /// sides are widened to i128 before comparison so a `requested` near
    /// `u64::MAX` can never wrap into a false `true`.
    pub fn admits(&self, requested: u64) -> bool {
        (requested as i128) <= (self.remaining() as i128)
    }
}

/// A provider-declared gauge reading ingested for one pool, plus the
/// freshness bookkeeping needed to decide whether it is trusted (HORO-1763,
/// reusing [`crate::quota_window`]'s "a stale/missing/undisclosed snapshot
/// can never collapse to not-blocking" discipline at the arithmetic level:
/// see [`PoolAdmission`] docs — only a *fresh*, *disclosed* reading ever
/// contributes a nonzero `external_overhang`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PoolProviderSnapshot {
    pub pool_id: PoolId,
    pub observed_at: OffsetDateTime,
    pub valid_until: Option<OffsetDateTime>,
    pub reading: GaugeReading,
    pub confidence: Confidence,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_reservation() -> SharedPoolReservation {
        SharedPoolReservation {
            id: SharedPoolReservationId::new(),
            pool_id: PoolId("shared-quota".to_string()),
            principal_id: PrincipalId("principal-1".to_string()),
            session_id: "session-1".to_string(),
            amount: 500,
            state: ReservationState::Active,
            settled_amount: None,
            usage_known: None,
            idempotency_key: "key-1".to_string(),
            created_at: OffsetDateTime::UNIX_EPOCH,
            expires_at: OffsetDateTime::UNIX_EPOCH,
            settled_at: None,
            released_at: None,
            schema_version: SHARED_POOL_RESERVATION_SCHEMA_VERSION.to_string(),
        }
    }

    #[test]
    fn outstanding_matches_state() {
        let mut r = base_reservation();
        assert_eq!(r.outstanding(), 500);

        r.state = ReservationState::Settled;
        r.settled_amount = Some(300);
        assert_eq!(r.outstanding(), 0);
        assert_eq!(r.refunded(), Some(200));
        assert_eq!(r.overrun(), None);

        r.settled_amount = Some(700);
        assert_eq!(r.overrun(), Some(200));
        assert_eq!(r.refunded(), None);

        r.state = ReservationState::Released;
        assert_eq!(r.outstanding(), 0);

        r.state = ReservationState::Expired;
        assert_eq!(r.outstanding(), 0);
    }

    #[test]
    fn remaining_subtracts_overhang_and_never_sums_it_into_settled() {
        let admission = PoolAdmission {
            capacity: 10,
            settled: 2,
            active: 3,
            external_overhang: 1,
        };
        // 10 - 2 - 3 - 1 = 4
        assert_eq!(admission.remaining(), 4);
        assert!(admission.admits(4));
        assert!(!admission.admits(5));
    }

    #[test]
    fn overhang_alone_can_exhaust_capacity_without_any_known_spend() {
        // AC3: unknown external activity conservatively reduces admitted
        // certainty even when Libra's own ledger shows nothing settled or
        // held — this is what "conservative" means structurally.
        let admission = PoolAdmission {
            capacity: 10,
            settled: 0,
            active: 0,
            external_overhang: 10,
        };
        assert_eq!(admission.remaining(), 0);
        assert!(!admission.admits(1));
    }

    #[test]
    fn remaining_stays_visible_as_negative_on_overrun_rather_than_clamping() {
        let admission = PoolAdmission {
            capacity: 10,
            settled: 8,
            active: 5,
            external_overhang: 0,
        };
        assert_eq!(admission.remaining(), -3);
        assert!(!admission.admits(0));
    }

    #[test]
    fn remaining_never_panics_or_wraps_at_u64_extremes() {
        let admission = PoolAdmission {
            capacity: u64::MAX,
            settled: u64::MAX,
            active: u64::MAX,
            external_overhang: u64::MAX,
        };
        // Saturates at i64::MIN rather than wrapping into a large
        // positive (which would falsely admit).
        assert_eq!(admission.remaining(), i64::MIN);
        assert!(!admission.admits(u64::MAX));
    }
}
