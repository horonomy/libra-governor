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
use crate::quota_window::{is_valid_slug, GaugeReading, PoolId, QuotaUnit};
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
    /// The sub-executor within `session_id` this row is attributed to
    /// (HORO-2194). `None` never means "root lineage": together with
    /// [`Self::executor_kind`] it distinguishes the session's main
    /// executor (`AgentAbsent`) from a row whose agent is simply unknown
    /// (`Unattributed`). Legacy rows written before migration 0021 carry
    /// `None` here and `None` in `executor_kind`.
    #[serde(default)]
    pub agent_id: Option<String>,
    /// `None` only for rows predating attribution; read as
    /// [`ExecutorKind::Unattributed`] by usage queries.
    #[serde(default)]
    pub executor_kind: Option<ExecutorKind>,
    /// How the spend reached the ledger, when known.
    #[serde(default)]
    pub charge_source: Option<ChargeSource>,
}

/// Which executor within a session a shared-pool row is attributed to
/// (HORO-2194). The executor key is `(session_id, agent_id | NULL)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutorKind {
    /// The session's main executor: no `agent_id` was supplied. Never
    /// conflated with a subagent and never rewritten to a `root` lineage.
    AgentAbsent,
    /// A specific agent within the session; `agent_id` is `Some`.
    Agent,
    /// The session is known but the agent is not.
    Unattributed,
}

impl ExecutorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AgentAbsent => "agent_absent",
            Self::Agent => "agent",
            Self::Unattributed => "unattributed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "agent_absent" => Some(Self::AgentAbsent),
            "agent" => Some(Self::Agent),
            "unattributed" => Some(Self::Unattributed),
            _ => None,
        }
    }
}

/// How a shared-pool row's spend reached the ledger (HORO-2194).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChargeSource {
    TurnHold,
    SubagentTranscript,
    Gateway,
}

impl ChargeSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TurnHold => "turn_hold",
            Self::SubagentTranscript => "subagent_transcript",
            Self::Gateway => "gateway",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "turn_hold" => Some(Self::TurnHold),
            "subagent_transcript" => Some(Self::SubagentTranscript),
            "gateway" => Some(Self::Gateway),
            _ => None,
        }
    }
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

    /// How far committed spend (settled + active + external overhang)
    /// exceeds `capacity`, i.e. `-remaining()` when negative, else 0
    /// (HORO-2194). The same figure [`Self::remaining`] already exposes as
    /// a negative number, named so a status report need not re-derive it.
    pub fn overrun(&self) -> u64 {
        let remaining = self.remaining() as i128;
        if remaining < 0 {
            (-remaining) as u64
        } else {
            0
        }
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

/// Namespace prefix for a [`MeteredPoolKey`]'s [`PoolId`] encoding
/// (HORO-1915). Disjoint at the first byte from `crates/daemon`'s
/// `launch:<alias>` pools — the two pool kinds are never confusable, and a
/// metered handler refusing anything outside this prefix (see
/// `libra_governor_daemon::metered_pool`) is what keeps a launch-slot hold
/// from ever being settled/released through the metered path or vice
/// versa.
pub const METERED_POOL_PREFIX: &str = "metered:";

/// A validated `metered:<owner>/<alias>` pool identifier (HORO-1915).
/// Deliberately a distinct type from the opaque [`PoolId`] launch pools
/// use directly — construction always validates both slugs, so a
/// malformed owner/alias can never reach the ledger as a pool id.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MeteredPoolKey {
    owner: PrincipalId,
    alias: String,
}

/// Errors constructing a [`MeteredPoolKey`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MeteredPoolKeyError {
    #[error("metered pool owner is not a valid slug: {0:?}")]
    InvalidOwner(String),
    #[error("metered pool alias is not a valid slug: {0:?}")]
    InvalidAlias(String),
}

impl MeteredPoolKey {
    /// Validates `owner` and `alias` against the same slug grammar
    /// [`crate::quota_window::CreditNamespace`] uses
    /// (`^[a-z][a-z0-9_-]{0,31}$`) — which already excludes `/` and `:`,
    /// so the `metered:<owner>/<alias>` encoding this type produces is an
    /// injective, unambiguous round trip (see [`Self::parse`]).
    pub fn new(owner: &PrincipalId, alias: &str) -> Result<Self, MeteredPoolKeyError> {
        if !is_valid_slug(&owner.0) {
            return Err(MeteredPoolKeyError::InvalidOwner(owner.0.clone()));
        }
        if !is_valid_slug(alias) {
            return Err(MeteredPoolKeyError::InvalidAlias(alias.to_string()));
        }
        Ok(Self {
            owner: owner.clone(),
            alias: alias.to_string(),
        })
    }

    /// The exact inverse of [`Self::pool_id`]. Returns `None` for any
    /// [`PoolId`] that is not a well-formed `metered:<owner>/<alias>`
    /// encoding (including a `launch:` pool id, or a malformed metered
    /// one) — callers use this to refuse cross-kind operations rather
    /// than guessing.
    pub fn parse(pool_id: &PoolId) -> Option<Self> {
        let rest = pool_id.0.strip_prefix(METERED_POOL_PREFIX)?;
        let (owner, alias) = rest.split_once('/')?;
        Self::new(&PrincipalId(owner.to_string()), alias).ok()
    }

    pub fn pool_id(&self) -> PoolId {
        PoolId(format!(
            "{METERED_POOL_PREFIX}{}/{}",
            self.owner.0, self.alias
        ))
    }

    pub fn owner(&self) -> &PrincipalId {
        &self.owner
    }

    pub fn alias(&self) -> &str {
        &self.alias
    }
}

/// Owner slug of the local monthly shared budget's pool (ADR-0026). The
/// pool id is `metered:local-budget/monthly-YYYY-MM`.
pub const LOCAL_BUDGET_OWNER: &str = "local-budget";

/// The pool alias `monthly-YYYY-MM` for the calendar month that starts at
/// `period_start`, with year and month read in `tz` (ADR-0026). Pass the
/// start returned by [`crate::calendar_month_bounds`]; that start is the
/// first local day, so the label is the month the budget period covers.
pub fn monthly_pool_alias(period_start: OffsetDateTime, tz: &crate::IanaTimeZone) -> String {
    let (year, month) = crate::quota_window::local_year_month(period_start, tz);
    format!("monthly-{year:04}-{month:02}")
}

/// Whether `unit` is one of the provider-denominated units a metered pool
/// may be created in (HORO-1915). Opaque launch-slot credit and
/// basis-point percentages are refused — a metered pool always holds a
/// unit a provider snapshot can be compared against directly, with no
/// conversion.
pub fn is_metered_unit(unit: &QuotaUnit) -> bool {
    matches!(
        unit,
        QuotaUnit::UsdCents | QuotaUnit::Tokens | QuotaUnit::Requests
    )
}

/// Why a [`PoolProviderSnapshot`] could not supply a trusted declared
/// limit for a metered-pool admission decision (HORO-1915). Each variant
/// is a distinct, named reason a caller can report — never collapsed into
/// a single opaque "unavailable".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotUnusable {
    /// No snapshot has ever been ingested for this pool.
    Missing,
    /// The most recent snapshot is [`GaugeReading::Undisclosed`].
    Undisclosed,
    /// The snapshot never declared a `valid_until` — an unbounded
    /// trust window is never accepted for admission (see
    /// `libra_governor_ledger::shared_pool::external_overhang_tx`, which
    /// applies the same discipline to overhang).
    NoValidUntil,
    /// `valid_until` is at or before the evaluation instant.
    Stale,
    /// The reading is [`GaugeReading::Used`] but declared no `limit`.
    LimitMissing,
}

/// The single definition of "is this pool's most recent provider snapshot
/// usable for a metered-pool admission decision", shared by the ledger's
/// reservation gate and by status reporting (HORO-1915) — evaluated in
/// this fixed order so both callers always agree.
pub fn metered_snapshot_limit(
    snapshot: Option<&PoolProviderSnapshot>,
    now: OffsetDateTime,
) -> Result<u64, SnapshotUnusable> {
    let snapshot = snapshot.ok_or(SnapshotUnusable::Missing)?;
    let GaugeReading::Used { limit, .. } = &snapshot.reading else {
        return Err(SnapshotUnusable::Undisclosed);
    };
    let Some(valid_until) = snapshot.valid_until else {
        return Err(SnapshotUnusable::NoValidUntil);
    };
    if valid_until <= now {
        return Err(SnapshotUnusable::Stale);
    }
    limit.ok_or(SnapshotUnusable::LimitMissing)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_budget_pool_id_uses_the_period_start_month_in_the_zone() {
        use time::macros::datetime;
        let tz = crate::IanaTimeZone::new("America/New_York").unwrap();
        let (start, _) = crate::calendar_month_bounds(datetime!(2025-01-15 12:00:00 UTC), &tz);
        // Start is 2025-01-01 05:00Z; the alias reads the local month.
        let alias = monthly_pool_alias(start, &tz);
        assert_eq!(alias, "monthly-2025-01");
        let key = MeteredPoolKey::new(&PrincipalId(LOCAL_BUDGET_OWNER.to_string()), &alias)
            .expect("local-budget and monthly-YYYY-MM are valid slugs");
        assert_eq!(key.pool_id().0, "metered:local-budget/monthly-2025-01");
        assert_eq!(MeteredPoolKey::parse(&key.pool_id()), Some(key));
    }

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
            agent_id: None,
            executor_kind: None,
            charge_source: None,
        }
    }

    #[test]
    fn executor_kind_and_charge_source_round_trip_their_column_spellings() {
        for k in [
            ExecutorKind::AgentAbsent,
            ExecutorKind::Agent,
            ExecutorKind::Unattributed,
        ] {
            assert_eq!(ExecutorKind::parse(k.as_str()), Some(k));
        }
        for c in [
            ChargeSource::TurnHold,
            ChargeSource::SubagentTranscript,
            ChargeSource::Gateway,
        ] {
            assert_eq!(ChargeSource::parse(c.as_str()), Some(c));
        }
        assert_eq!(ExecutorKind::parse("root"), None);
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
    fn metered_pool_key_round_trips_through_pool_id() {
        let key = MeteredPoolKey::new(&PrincipalId("acme".to_string()), "usd-2026-10").unwrap();
        let pool_id = key.pool_id();
        assert_eq!(pool_id.0, "metered:acme/usd-2026-10");
        let parsed = MeteredPoolKey::parse(&pool_id).unwrap();
        assert_eq!(parsed, key);
    }

    #[test]
    fn metered_pool_key_refuses_invalid_slugs() {
        assert!(MeteredPoolKey::new(&PrincipalId("ACME".to_string()), "usd-2026-10").is_err());
        assert!(MeteredPoolKey::new(&PrincipalId("acme".to_string()), "has/slash").is_err());
    }

    #[test]
    fn metered_pool_key_parse_refuses_non_metered_pool_ids() {
        assert!(MeteredPoolKey::parse(&PoolId("launch:default".to_string())).is_none());
        assert!(MeteredPoolKey::parse(&PoolId("metered:no-alias".to_string())).is_none());
    }

    #[test]
    fn is_metered_unit_accepts_only_provider_denominated_units() {
        assert!(is_metered_unit(&QuotaUnit::UsdCents));
        assert!(is_metered_unit(&QuotaUnit::Tokens));
        assert!(is_metered_unit(&QuotaUnit::Requests));
        assert!(!is_metered_unit(&QuotaUnit::Percent));
        assert!(!is_metered_unit(&QuotaUnit::OpaqueCredit(
            crate::quota_window::CreditNamespace::new("libra-launch-slot").unwrap()
        )));
    }

    fn base_snapshot(
        reading: GaugeReading,
        valid_until: Option<OffsetDateTime>,
    ) -> PoolProviderSnapshot {
        PoolProviderSnapshot {
            pool_id: PoolId("metered:acme/usd-2026-10".to_string()),
            observed_at: OffsetDateTime::UNIX_EPOCH,
            valid_until,
            reading,
            confidence: Confidence::Medium,
        }
    }

    #[test]
    fn metered_snapshot_limit_missing_when_no_snapshot() {
        assert_eq!(
            metered_snapshot_limit(None, OffsetDateTime::UNIX_EPOCH),
            Err(SnapshotUnusable::Missing)
        );
    }

    #[test]
    fn metered_snapshot_limit_undisclosed() {
        let snap = base_snapshot(
            GaugeReading::Undisclosed,
            Some(OffsetDateTime::UNIX_EPOCH + time::Duration::hours(1)),
        );
        assert_eq!(
            metered_snapshot_limit(Some(&snap), OffsetDateTime::UNIX_EPOCH),
            Err(SnapshotUnusable::Undisclosed)
        );
    }

    #[test]
    fn metered_snapshot_limit_no_valid_until() {
        let snap = base_snapshot(
            GaugeReading::Used {
                used: crate::quota_window::QuotaAmount::new(QuotaUnit::UsdCents, 0),
                limit: Some(50_000),
            },
            None,
        );
        assert_eq!(
            metered_snapshot_limit(Some(&snap), OffsetDateTime::UNIX_EPOCH),
            Err(SnapshotUnusable::NoValidUntil)
        );
    }

    #[test]
    fn metered_snapshot_limit_stale() {
        let now = OffsetDateTime::UNIX_EPOCH + time::Duration::hours(2);
        let snap = base_snapshot(
            GaugeReading::Used {
                used: crate::quota_window::QuotaAmount::new(QuotaUnit::UsdCents, 0),
                limit: Some(50_000),
            },
            Some(OffsetDateTime::UNIX_EPOCH + time::Duration::hours(1)),
        );
        assert_eq!(
            metered_snapshot_limit(Some(&snap), now),
            Err(SnapshotUnusable::Stale)
        );
    }

    #[test]
    fn metered_snapshot_limit_missing_limit() {
        let snap = base_snapshot(
            GaugeReading::Used {
                used: crate::quota_window::QuotaAmount::new(QuotaUnit::UsdCents, 0),
                limit: None,
            },
            Some(OffsetDateTime::UNIX_EPOCH + time::Duration::hours(1)),
        );
        assert_eq!(
            metered_snapshot_limit(Some(&snap), OffsetDateTime::UNIX_EPOCH),
            Err(SnapshotUnusable::LimitMissing)
        );
    }

    #[test]
    fn metered_snapshot_limit_ok_when_fresh_and_disclosed() {
        let snap = base_snapshot(
            GaugeReading::Used {
                used: crate::quota_window::QuotaAmount::new(QuotaUnit::UsdCents, 12_000),
                limit: Some(50_000),
            },
            Some(OffsetDateTime::UNIX_EPOCH + time::Duration::hours(1)),
        );
        assert_eq!(
            metered_snapshot_limit(Some(&snap), OffsetDateTime::UNIX_EPOCH),
            Ok(50_000)
        );
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
