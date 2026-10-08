//! Versioned quota-window contract (HORO-1762, `docs/adr/0015-quota-windows.md`).
//!
//! Represents simultaneously independent quota constraints — an hourly
//! cap, a rolling six-hour cap, a daily cap, a weekly cap, a provider's
//! own opaque subscription-period gauge — each evaluated on its own
//! terms from a shared log of usage/holds/snapshots. A reset on one
//! window never resets another: a weekly reset does not reset a
//! concurrently-tracked rolling six-hour window, because evaluation is a
//! pure function over evidence the caller supplies, not a mutable
//! counter that something could "reset".
//!
//! This module deliberately does **not**:
//! - persist anything (a later story),
//! - ingest/reconcile provider snapshots against Libra-observed usage (a
//!   later story),
//! - implement unit conversion (explicitly out of scope — see
//!   [`QuotaUnit`]),
//! - decide what admission does with an [`BlockingStatus::Indeterminate`]
//!   result (a later story's policy decision),
//! - touch hierarchical custody (`crate::resource_account`'s
//!   `ensure_child_action`/`grant_sublease`) — out of scope per
//!   HORO-1694; [`QuotaSubject::SharedPool`] covers the one flat
//!   shared-pool need this campaign actually has.

mod calendar;
pub(crate) use calendar::next_working_instant;
pub use calendar::{WorkingHours, WorkingHoursError};
mod evaluate;

#[cfg(test)]
mod tests;

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::economic_event::occurred_at_wire;
use crate::{
    Confidence, EconomicEvent, EconomicEventId, PrincipalId, Reservation, ReservationId,
    ReservationState, ResourceAmount, ResourceKind,
};

pub use evaluate::{
    BlockingStatus, BucketState, GaugeFreshness, GaugeState, IndeterminateReason, PeriodState,
    Relief, StaleReason, WindowEvaluation, WindowState,
};

/// The current schema tag for [`QuotaWindow`] and [`ProviderSnapshot`].
/// Follows this crate's existing convention (`POLICY_SCHEMA_VERSION`,
/// `RESERVATION_SCHEMA_VERSION`, ...) of a string tag rather than an
/// integer: readers only ever test it for equality, never arithmetic.
pub const QUOTA_WINDOW_SCHEMA_VERSION: &str = "quota-window-v1";

/// Independent unit kinds a [`QuotaWindow`] may be denominated in.
///
/// Never implicitly converted to one another. A dollar figure, a token
/// count, a request count, an opaque provider credit, and a percentage
/// of an undisclosed base are different things; converting between them
/// requires an explicit, separately authored and separately versioned
/// conversion policy, which does not exist in this contract.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "unit", content = "namespace", rename_all = "snake_case")]
pub enum QuotaUnit {
    UsdCents,
    Tokens,
    /// A request count. "Requests per minute" is this unit paired with a
    /// 60-second [`WindowKind::Sliding`] or a [`WindowKind::RefillBucket`]
    /// — the rate lives in the window, not the unit.
    Requests,
    /// Opaque provider credit, scoped to a namespace so credits from
    /// different providers (or different programs from the same
    /// provider) are never silently treated as the same unit.
    OpaqueCredit(CreditNamespace),
    /// Basis points (10_000 = 100%) of an undisclosed base. Valid only on
    /// [`WindowKind::OpaqueProviderSnapshot`] — see
    /// [`QuotaWindowError::PercentOnNonGaugeWindow`].
    Percent,
}

impl From<ResourceKind> for QuotaUnit {
    /// A relabeling into the quota-unit vocabulary, not a conversion —
    /// `ResourceKind::Usd` and [`QuotaUnit::UsdCents`] already share the
    /// same underlying integer-cents representation. Deliberately no
    /// reverse `impl`: not every `QuotaUnit` (e.g. `Requests`,
    /// `OpaqueCredit`) has a `ResourceKind` counterpart.
    fn from(kind: ResourceKind) -> Self {
        match kind {
            ResourceKind::Usd => QuotaUnit::UsdCents,
            ResourceKind::Tokens => QuotaUnit::Tokens,
            ResourceKind::QuotaPercent => QuotaUnit::Percent,
        }
    }
}

/// A validated opaque-credit namespace slug (`^[a-z][a-z0-9_-]{0,31}$`).
/// Its own validator, deliberately not reusing
/// [`crate::is_valid_tool_provider`] — that function validates harness/
/// tool-provider identifiers, a different vocabulary that happens to
/// share a similar shape.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct CreditNamespace(String);

impl CreditNamespace {
    pub fn new(slug: impl Into<String>) -> Result<Self, QuotaWindowError> {
        let slug = slug.into();
        if is_valid_slug(&slug) {
            Ok(Self(slug))
        } else {
            Err(QuotaWindowError::InvalidSlug(slug))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for CreditNamespace {
    type Error = QuotaWindowError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<CreditNamespace> for String {
    fn from(value: CreditNamespace) -> Self {
        value.0
    }
}

fn is_valid_slug(slug: &str) -> bool {
    let mut chars = slug.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_lowercase() {
        return false;
    }
    if slug.len() > 32 {
        return false;
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// An additive quantity in one [`QuotaUnit`]'s base integer (cents,
/// tokens, requests, the provider's smallest credit unit, or basis
/// points). Not [`Copy`]: [`QuotaUnit::OpaqueCredit`] carries an owned
/// namespace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotaAmount {
    pub unit: QuotaUnit,
    pub value: u64,
}

impl QuotaAmount {
    pub fn new(unit: QuotaUnit, value: u64) -> Self {
        Self { unit, value }
    }
}

impl TryFrom<ResourceAmount> for QuotaAmount {
    type Error = QuotaWindowError;

    /// A negative `UsdCents` is refused (a quota amount cannot be
    /// negative); `QuotaPercent` rounds to the nearest whole basis
    /// point, refusing `NaN` and negative input.
    fn try_from(amount: ResourceAmount) -> Result<Self, Self::Error> {
        match amount {
            ResourceAmount::UsdCents(c) => {
                let value = u64::try_from(c).map_err(|_| QuotaWindowError::NegativeAmount)?;
                Ok(QuotaAmount::new(QuotaUnit::UsdCents, value))
            }
            ResourceAmount::Tokens(t) => Ok(QuotaAmount::new(QuotaUnit::Tokens, t)),
            ResourceAmount::QuotaPercent(p) => {
                if !p.is_finite() || p < 0.0 {
                    return Err(QuotaWindowError::NegativeAmount);
                }
                let basis_points = (p as f64 * 100.0).round();
                if !basis_points.is_finite() || basis_points < 0.0 {
                    return Err(QuotaWindowError::NegativeAmount);
                }
                Ok(QuotaAmount::new(QuotaUnit::Percent, basis_points as u64))
            }
        }
    }
}

/// A validated IANA zone name, checked against the bundled timezone
/// database at construction/deserialize time — never a fallback to UTC
/// on an unrecognized name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct IanaTimeZone(String);

impl IanaTimeZone {
    pub fn new(name: impl Into<String>) -> Result<Self, QuotaWindowError> {
        let name = name.into();
        if calendar::is_known_zone(&name) {
            Ok(Self(name))
        } else {
            Err(QuotaWindowError::UnknownTimeZone(name))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for IanaTimeZone {
    type Error = QuotaWindowError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<IanaTimeZone> for String {
    fn from(value: IanaTimeZone) -> Self {
        value.0
    }
}

/// A validated wall-clock time of day.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WallClockTime {
    hour: u8,
    minute: u8,
}

impl WallClockTime {
    pub fn new(hour: u8, minute: u8) -> Result<Self, QuotaWindowError> {
        if hour >= 24 || minute >= 60 {
            return Err(QuotaWindowError::InvalidWallClockTime { hour, minute });
        }
        Ok(Self { hour, minute })
    }

    pub fn hour(&self) -> u8 {
        self.hour
    }

    pub fn minute(&self) -> u8 {
        self.minute
    }
}

/// A weekday a weekly reset may align to. A dedicated enum rather than
/// reusing `time::Weekday`: that type's serde shape is not the wire
/// representation this contract wants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResetWeekday {
    Monday,
    Tuesday,
    Wednesday,
    Thursday,
    Friday,
    Saturday,
    Sunday,
}

/// A fixed-aligned reset period: the boundary is a wall-clock instant in
/// an explicit timezone, not a fixed UTC offset — so DST transitions are
/// handled by stepping calendar dates/times in that zone, never by
/// adding a constant number of seconds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "every", rename_all = "snake_case")]
pub enum AlignedPeriod {
    /// Resets at the top of every local hour. Both occurrences of a
    /// repeated hour (a DST fall-back fold) are the *same* period: the
    /// window truncates to the hour's current occurrence rather than
    /// treating the fold as a reset.
    Hour,
    Day {
        at: WallClockTime,
    },
    Week {
        starts_on: ResetWeekday,
        at: WallClockTime,
    },
}

/// A quota window's reset/accrual mechanics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WindowKind {
    /// Resets at an aligned wall-clock boundary in `time_zone`.
    FixedAligned {
        period: AlignedPeriod,
        time_zone: IanaTimeZone,
        limit: u64,
    },
    /// A rolling interval: usage counts when it falls in
    /// `(now - length_secs, now]`. Never "resets" — it continuously
    /// slides, which is exactly why a weekly reset elsewhere does not
    /// affect it.
    Sliding { length_secs: u64, limit: u64 },
    /// A continuously-refilling token bucket.
    RefillBucket {
        capacity: u64,
        /// Refill rate: `refill_amount` per `refill_period_secs`,
        /// expressed as an integer ratio so arithmetic never uses
        /// floats.
        refill_amount: u64,
        refill_period_secs: u64,
        /// Replay starts here; usage recorded before this instant is
        /// ignored. Required because without an anchor, evaluating the
        /// bucket means replaying unbounded history.
        anchored_at: OffsetDateTime,
        /// The bucket's level at `anchored_at`. Must be `<= capacity`.
        level_at_anchor: u64,
    },
    /// A provider-declared gauge with undisclosed reset mechanics. The
    /// limit (if any) lives in the snapshot itself, not here — this
    /// variant only bounds how stale a snapshot may be before it is
    /// distrusted.
    OpaqueProviderSnapshot { max_staleness_secs: u64 },
}

/// Who/what this window's declared entitlement applies to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum QuotaSubject {
    Principal(PrincipalId),
    /// A flat, non-hierarchical shared pool — the one concrete
    /// shared-pool need this campaign provides. Deliberately has no
    /// membership/hierarchy modeled here (that remains HORO-1694's
    /// research-latent scope).
    SharedPool(PoolId),
    Provider(ProviderAccount),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PoolId(pub String);

/// An operator-chosen label for a provider account — never an API key,
/// org secret, or credential of any kind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderAccount {
    pub provider: String,
    pub account: Option<String>,
}

/// What declared this window's entitlement exists at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntitlementSource {
    OperatorConfigured,
    ProviderDeclared,
    ImportedSnapshot,
}

/// The entitlement-scope envelope: who the window applies to, who said
/// so, how much that's trusted, and whether that claim has expired.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaScope {
    pub subject: QuotaSubject,
    pub source: EntitlementSource,
    pub confidence: Confidence,
    #[serde(with = "occurred_at_wire")]
    pub observed_at: OffsetDateTime,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "optional_occurred_at_wire"
    )]
    pub valid_until: Option<OffsetDateTime>,
}

pub(crate) mod optional_occurred_at_wire {
    use serde::de::Error as _;
    use serde::{Deserialize, Deserializer, Serializer};
    use time::format_description::well_known::Rfc3339;
    use time::OffsetDateTime;

    pub fn serialize<S: Serializer>(
        value: &Option<OffsetDateTime>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match value {
            Some(v) => super::occurred_at_wire::serialize(v, serializer),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<OffsetDateTime>, D::Error> {
        let raw = Option::<String>::deserialize(deserializer)?;
        match raw {
            None => Ok(None),
            Some(raw) => {
                let rfc3339 = raw
                    .strip_suffix('Z')
                    .map(|s| format!("{s}+00:00"))
                    .unwrap_or(raw);
                OffsetDateTime::parse(&rfc3339, &Rfc3339)
                    .map(Some)
                    .map_err(D::Error::custom)
            }
        }
    }
}

/// Errors constructing or decoding any type in this module.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum QuotaWindowError {
    #[error("quota amount cannot be negative")]
    NegativeAmount,
    #[error("invalid slug: {0:?}")]
    InvalidSlug(String),
    #[error("unknown IANA time zone: {0:?}")]
    UnknownTimeZone(String),
    #[error("invalid wall-clock time {hour:02}:{minute:02}")]
    InvalidWallClockTime { hour: u8, minute: u8 },
    #[error("Percent unit is only valid on an OpaqueProviderSnapshot window")]
    PercentOnNonGaugeWindow,
    #[error("a required numeric field was zero: {field}")]
    ZeroField { field: &'static str },
    #[error("level_at_anchor ({level_at_anchor}) exceeds capacity ({capacity})")]
    LevelExceedsCapacity { level_at_anchor: u64, capacity: u64 },
    #[error("timestamp must be UTC, got offset {0:?}")]
    NotUtc(time::UtcOffset),
    #[error("schema_version field is missing")]
    MissingSchemaVersion,
    #[error("unrecognized field value: {0}")]
    Unrecognized(String),
    #[error("a declared limit must be present for a non-percent gauge reading")]
    GaugeMissingLimit,
    #[error("a Percent gauge reading must not declare a separate limit")]
    GaugeUnexpectedLimit,
    #[error("{field} ({value}) exceeds the maximum representable window length ({max})")]
    FieldOutOfRange {
        field: &'static str,
        value: u64,
        max: u64,
    },
}

/// The maximum a [`WindowKind::Sliding`]'s `length_secs` may be: 100
/// years in seconds. No real quota window is anywhere near this long;
/// the bound exists purely so `now ± Duration::seconds(length_secs)`
/// can never panic in [`evaluate`] — `OffsetDateTime` arithmetic panics
/// on overflow, and `length_secs` is caller-supplied, so it must be
/// refused at construction rather than trusted at evaluation time.
const MAX_SLIDING_LENGTH_SECS: u64 = 100 * 365 * 24 * 3600;

/// Normalizes a UTC timestamp to whole milliseconds, matching
/// [`occurred_at_wire`]'s own truncation — so a sub-millisecond
/// timestamp evaluates identically before and after a JSON round trip.
/// Refuses a non-UTC offset.
fn normalize_utc_ms(value: OffsetDateTime) -> Result<OffsetDateTime, QuotaWindowError> {
    if value.offset() != time::UtcOffset::UTC {
        return Err(QuotaWindowError::NotUtc(value.offset()));
    }
    let millis = value.millisecond();
    value
        .replace_millisecond(millis)
        .map_err(|_| QuotaWindowError::NotUtc(value.offset()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct QuotaWindowId(pub Uuid);

impl QuotaWindowId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for QuotaWindowId {
    fn default() -> Self {
        Self::new()
    }
}

/// A versioned, validated quota-window definition. Construct only via
/// [`Self::validated`] (or deserialize, which runs the same validation —
/// see the `TryFrom` wiring below) so an invalid window can never exist.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "QuotaWindowWire", into = "QuotaWindowWire")]
pub struct QuotaWindow {
    schema_version: String,
    id: QuotaWindowId,
    scope: QuotaScope,
    unit: QuotaUnit,
    kind: WindowKind,
}

/// The permissive wire shape `QuotaWindow` round-trips through, so
/// validation runs on every deserialize and cannot be bypassed — the
/// same discipline `EconomicEvent` uses.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct QuotaWindowWire {
    schema_version: String,
    id: QuotaWindowId,
    scope: QuotaScope,
    unit: QuotaUnit,
    kind: WindowKind,
}

impl From<QuotaWindow> for QuotaWindowWire {
    fn from(value: QuotaWindow) -> Self {
        Self {
            schema_version: value.schema_version,
            id: value.id,
            scope: value.scope,
            unit: value.unit,
            kind: value.kind,
        }
    }
}

impl TryFrom<QuotaWindowWire> for QuotaWindow {
    type Error = QuotaWindowError;

    fn try_from(wire: QuotaWindowWire) -> Result<Self, Self::Error> {
        if wire.schema_version != QUOTA_WINDOW_SCHEMA_VERSION {
            return Err(QuotaWindowError::Unrecognized(format!(
                "unsupported schema_version {:?} for QuotaWindow (expected {:?}); use decode_quota_window for forward compatibility",
                wire.schema_version, QUOTA_WINDOW_SCHEMA_VERSION
            )));
        }
        QuotaWindow::validated(wire.id, wire.scope, wire.unit, wire.kind)
    }
}

impl QuotaWindow {
    pub fn validated(
        id: QuotaWindowId,
        scope: QuotaScope,
        unit: QuotaUnit,
        kind: WindowKind,
    ) -> Result<Self, QuotaWindowError> {
        if unit == QuotaUnit::Percent && !matches!(kind, WindowKind::OpaqueProviderSnapshot { .. })
        {
            return Err(QuotaWindowError::PercentOnNonGaugeWindow);
        }
        let scope = QuotaScope {
            observed_at: normalize_utc_ms(scope.observed_at)?,
            valid_until: scope.valid_until.map(normalize_utc_ms).transpose()?,
            ..scope
        };
        let kind = match kind {
            WindowKind::FixedAligned {
                period,
                time_zone,
                limit,
            } => WindowKind::FixedAligned {
                period,
                time_zone,
                limit,
            },
            WindowKind::Sliding { length_secs, limit } => {
                if length_secs == 0 {
                    return Err(QuotaWindowError::ZeroField {
                        field: "length_secs",
                    });
                }
                if length_secs > MAX_SLIDING_LENGTH_SECS {
                    return Err(QuotaWindowError::FieldOutOfRange {
                        field: "length_secs",
                        value: length_secs,
                        max: MAX_SLIDING_LENGTH_SECS,
                    });
                }
                WindowKind::Sliding { length_secs, limit }
            }
            WindowKind::RefillBucket {
                capacity,
                refill_amount,
                refill_period_secs,
                anchored_at,
                level_at_anchor,
            } => {
                if capacity == 0 {
                    return Err(QuotaWindowError::ZeroField { field: "capacity" });
                }
                if refill_amount == 0 {
                    return Err(QuotaWindowError::ZeroField {
                        field: "refill_amount",
                    });
                }
                if refill_period_secs == 0 {
                    return Err(QuotaWindowError::ZeroField {
                        field: "refill_period_secs",
                    });
                }
                if level_at_anchor > capacity {
                    return Err(QuotaWindowError::LevelExceedsCapacity {
                        level_at_anchor,
                        capacity,
                    });
                }
                WindowKind::RefillBucket {
                    capacity,
                    refill_amount,
                    refill_period_secs,
                    anchored_at: normalize_utc_ms(anchored_at)?,
                    level_at_anchor,
                }
            }
            WindowKind::OpaqueProviderSnapshot { max_staleness_secs } => {
                if max_staleness_secs == 0 {
                    return Err(QuotaWindowError::ZeroField {
                        field: "max_staleness_secs",
                    });
                }
                WindowKind::OpaqueProviderSnapshot { max_staleness_secs }
            }
        };
        Ok(Self {
            schema_version: QUOTA_WINDOW_SCHEMA_VERSION.to_string(),
            id,
            scope,
            unit,
            kind,
        })
    }

    pub fn id(&self) -> QuotaWindowId {
        self.id
    }

    pub fn scope(&self) -> &QuotaScope {
        &self.scope
    }

    pub fn unit(&self) -> &QuotaUnit {
        &self.unit
    }

    pub fn kind(&self) -> &WindowKind {
        &self.kind
    }

    pub fn schema_version(&self) -> &str {
        &self.schema_version
    }
}

/// One additive spend fact evaluated against a window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotaUsage {
    id: EconomicEventId,
    #[serde(with = "occurred_at_wire")]
    occurred_at: OffsetDateTime,
    amount: QuotaAmount,
}

impl QuotaUsage {
    pub fn new(
        id: EconomicEventId,
        occurred_at: OffsetDateTime,
        amount: QuotaAmount,
    ) -> Result<Self, QuotaWindowError> {
        if amount.unit == QuotaUnit::Percent {
            return Err(QuotaWindowError::PercentOnNonGaugeWindow);
        }
        Ok(Self {
            id,
            occurred_at: normalize_utc_ms(occurred_at)?,
            amount,
        })
    }

    /// `Some` iff `event`'s fact is additive spend (`FactRole::Spend` —
    /// `GatewayMeteredActual` or `ProviderReportedActual`) in a
    /// non-Percent unit. A hold, projection, or snapshot-basis fact
    /// (`QuotaSnapshot`, `ImportedAllocationSnapshot`,
    /// `LibraReservationHold`, `LibraForecast`, `HostEstimatedCost`)
    /// never reaches an additive sum — see `tests.rs` for the full
    /// mapping table.
    ///
    /// A single gateway request emits sibling USD and token events for
    /// the same request — never derive a `Requests` usage count by
    /// counting these events; that double-counts every request.
    pub fn from_economic_event(event: &EconomicEvent) -> Option<Self> {
        let fact = event.fact();
        if !fact.basis.is_additive() || fact.basis.role() != crate::economic_event::FactRole::Spend
        {
            return None;
        }
        let amount = QuotaAmount::try_from(fact.amount).ok()?;
        Self::new(event.id(), event.occurred_at(), amount).ok()
    }

    pub fn id(&self) -> EconomicEventId {
        self.id
    }

    pub fn occurred_at(&self) -> OffsetDateTime {
        self.occurred_at
    }

    pub fn amount(&self) -> &QuotaAmount {
        &self.amount
    }
}

/// Outstanding (not yet settled) reserved capacity, counted the same way
/// the ledger counts it: every `Active` reservation counts, with no
/// `expires_at` filter here — expiry is the ledger's own sweep
/// (`crates/ledger/src/reservation.rs`'s `expire_stale_reservations`),
/// and this contract must not invent a second expiry rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutstandingHold {
    id: ReservationId,
    amount: QuotaAmount,
}

impl OutstandingHold {
    /// `Some` iff `reservation.state == ReservationState::Active`.
    pub fn from_reservation(reservation: &Reservation) -> Option<Self> {
        if reservation.state != ReservationState::Active {
            return None;
        }
        let amount = QuotaAmount::try_from(reservation.amount).ok()?;
        Some(Self {
            id: reservation.id,
            amount,
        })
    }

    pub fn id(&self) -> ReservationId {
        self.id
    }

    pub fn amount(&self) -> &QuotaAmount {
        &self.amount
    }

    /// Builds a synthetic, never-persisted hold for a candidate or
    /// in-flight simulated task (`libra_governor_domain::pacing`'s
    /// earliest-safe-admit forecast). `id` must be a synthetic id —
    /// never a real [`ReservationId`] that could collide with a ledger
    /// row — callers are responsible for keeping the synthetic id space
    /// disjoint from `Uuid::new_v4()`'s range (see `pacing`'s id
    /// scheme). `pub(crate)`: this is an internal seam for the pacing
    /// simulator, not a public way to fabricate holds outside this
    /// crate.
    pub(crate) fn projected(id: ReservationId, amount: QuotaAmount) -> Self {
        Self { id, amount }
    }
}

/// A provider-declared gauge reading. [`Self::Undisclosed`] is a real,
/// distinct state — never conflated with 0% used or 100% available.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum GaugeReading {
    Undisclosed,
    Used {
        used: QuotaAmount,
        /// Must be `None` when `used.unit == QuotaUnit::Percent` (a
        /// percentage already implies a 10_000-basis-point limit);
        /// required otherwise.
        limit: Option<u64>,
    },
}

/// A single provider-reported snapshot of one window's gauge.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "ProviderSnapshotWire", into = "ProviderSnapshotWire")]
pub struct ProviderSnapshot {
    schema_version: String,
    window_id: QuotaWindowId,
    #[serde(with = "occurred_at_wire")]
    observed_at: OffsetDateTime,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "optional_occurred_at_wire"
    )]
    valid_until: Option<OffsetDateTime>,
    /// Informational only — the provider's own claim about when it
    /// resets. Never computed into [`Relief`]; a fresh snapshot's
    /// declared reset surfaces as [`Relief::ProviderDeclared`], kept
    /// distinct from Libra's own computed [`Relief::At`].
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "optional_occurred_at_wire"
    )]
    declared_reset_at: Option<OffsetDateTime>,
    reading: GaugeReading,
    confidence: Confidence,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProviderSnapshotWire {
    schema_version: String,
    window_id: QuotaWindowId,
    #[serde(with = "occurred_at_wire")]
    observed_at: OffsetDateTime,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "optional_occurred_at_wire"
    )]
    valid_until: Option<OffsetDateTime>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "optional_occurred_at_wire"
    )]
    declared_reset_at: Option<OffsetDateTime>,
    reading: GaugeReading,
    confidence: Confidence,
}

impl From<ProviderSnapshot> for ProviderSnapshotWire {
    fn from(value: ProviderSnapshot) -> Self {
        Self {
            schema_version: value.schema_version,
            window_id: value.window_id,
            observed_at: value.observed_at,
            valid_until: value.valid_until,
            declared_reset_at: value.declared_reset_at,
            reading: value.reading,
            confidence: value.confidence,
        }
    }
}

impl TryFrom<ProviderSnapshotWire> for ProviderSnapshot {
    type Error = QuotaWindowError;

    fn try_from(wire: ProviderSnapshotWire) -> Result<Self, Self::Error> {
        if wire.schema_version != QUOTA_WINDOW_SCHEMA_VERSION {
            return Err(QuotaWindowError::Unrecognized(format!(
                "unsupported schema_version {:?} for ProviderSnapshot (expected {:?}); use decode_provider_snapshot for forward compatibility",
                wire.schema_version, QUOTA_WINDOW_SCHEMA_VERSION
            )));
        }
        ProviderSnapshot::validated(
            wire.window_id,
            wire.observed_at,
            wire.valid_until,
            wire.declared_reset_at,
            wire.reading,
            wire.confidence,
        )
    }
}

impl ProviderSnapshot {
    #[allow(clippy::too_many_arguments)]
    pub fn validated(
        window_id: QuotaWindowId,
        observed_at: OffsetDateTime,
        valid_until: Option<OffsetDateTime>,
        declared_reset_at: Option<OffsetDateTime>,
        reading: GaugeReading,
        confidence: Confidence,
    ) -> Result<Self, QuotaWindowError> {
        if let GaugeReading::Used { used, limit } = &reading {
            match (used.unit == QuotaUnit::Percent, limit) {
                (true, Some(_)) => return Err(QuotaWindowError::GaugeUnexpectedLimit),
                (false, None) => return Err(QuotaWindowError::GaugeMissingLimit),
                _ => {}
            }
        }
        Ok(Self {
            schema_version: QUOTA_WINDOW_SCHEMA_VERSION.to_string(),
            window_id,
            observed_at: normalize_utc_ms(observed_at)?,
            valid_until: valid_until.map(normalize_utc_ms).transpose()?,
            declared_reset_at: declared_reset_at.map(normalize_utc_ms).transpose()?,
            reading,
            confidence,
        })
    }

    pub fn window_id(&self) -> QuotaWindowId {
        self.window_id
    }

    pub fn observed_at(&self) -> OffsetDateTime {
        self.observed_at
    }

    pub fn valid_until(&self) -> Option<OffsetDateTime> {
        self.valid_until
    }

    pub fn declared_reset_at(&self) -> Option<OffsetDateTime> {
        self.declared_reset_at
    }

    pub fn reading(&self) -> &GaugeReading {
        &self.reading
    }

    pub fn confidence(&self) -> Confidence {
        self.confidence
    }
}

/// The evidence a caller supplies to evaluate one [`QuotaWindow`]. The
/// evaluator does no scope matching: the caller must already have
/// selected evidence for this window's scope (deciding whether a
/// principal's spend also counts toward its team pool is hierarchy,
/// which stays out of this contract's scope).
pub struct QuotaEvidence<'a> {
    pub usage: &'a [QuotaUsage],
    pub holds: &'a [OutstandingHold],
    pub snapshots: &'a [ProviderSnapshot],
}

/// The result of decoding a possibly-future-versioned [`QuotaWindow`]
/// from raw JSON: either a window this build understands, or one tagged
/// with a schema version it doesn't — kept, unparsed, for safe
/// re-writing, and reported as [`BlockingStatus::Indeterminate`] rather
/// than guessed at.
#[derive(Debug)]
pub enum DecodedQuotaWindow {
    Current(QuotaWindow),
    Unsupported {
        schema_version: String,
        raw: serde_json::Value,
    },
}

impl DecodedQuotaWindow {
    pub fn blocking(&self) -> BlockingStatus {
        match self {
            DecodedQuotaWindow::Current(_) => BlockingStatus::NotBlocking,
            DecodedQuotaWindow::Unsupported { .. } => {
                BlockingStatus::Indeterminate(IndeterminateReason::UnsupportedSchemaVersion)
            }
        }
    }
}

/// Decodes raw JSON into a [`QuotaWindow`], tolerating an unrecognized
/// `schema_version` by keeping the payload unparsed rather than
/// misinterpreting its bytes under the current schema.
pub fn decode_quota_window(
    raw: &serde_json::Value,
) -> Result<DecodedQuotaWindow, QuotaWindowError> {
    let schema_version = raw
        .get("schema_version")
        .and_then(|v| v.as_str())
        .ok_or(QuotaWindowError::MissingSchemaVersion)?
        .to_string();
    if schema_version != QUOTA_WINDOW_SCHEMA_VERSION {
        return Ok(DecodedQuotaWindow::Unsupported {
            schema_version,
            raw: raw.clone(),
        });
    }
    let window: QuotaWindow = serde_json::from_value(raw.clone())
        .map_err(|e| QuotaWindowError::Unrecognized(e.to_string()))?;
    Ok(DecodedQuotaWindow::Current(window))
}

/// The result of decoding a possibly-future-versioned [`ProviderSnapshot`].
#[derive(Debug)]
pub enum DecodedProviderSnapshot {
    Current(ProviderSnapshot),
    Unsupported {
        schema_version: String,
        raw: serde_json::Value,
    },
}

impl DecodedProviderSnapshot {
    pub fn blocking(&self) -> BlockingStatus {
        match self {
            DecodedProviderSnapshot::Current(_) => BlockingStatus::NotBlocking,
            DecodedProviderSnapshot::Unsupported { .. } => {
                BlockingStatus::Indeterminate(IndeterminateReason::UnsupportedSchemaVersion)
            }
        }
    }
}

pub fn decode_provider_snapshot(
    raw: &serde_json::Value,
) -> Result<DecodedProviderSnapshot, QuotaWindowError> {
    let schema_version = raw
        .get("schema_version")
        .and_then(|v| v.as_str())
        .ok_or(QuotaWindowError::MissingSchemaVersion)?
        .to_string();
    if schema_version != QUOTA_WINDOW_SCHEMA_VERSION {
        return Ok(DecodedProviderSnapshot::Unsupported {
            schema_version,
            raw: raw.clone(),
        });
    }
    let snapshot: ProviderSnapshot = serde_json::from_value(raw.clone())
        .map_err(|e| QuotaWindowError::Unrecognized(e.to_string()))?;
    Ok(DecodedProviderSnapshot::Current(snapshot))
}

impl QuotaWindow {
    /// Evaluates this window's current state against `evidence` as of
    /// `now`. Pure and deterministic: the same inputs always produce the
    /// same [`WindowEvaluation`], regardless of evidence ordering (see
    /// `tests.rs`'s permutation-determinism fixture).
    pub fn evaluate(&self, evidence: &QuotaEvidence<'_>, now: OffsetDateTime) -> WindowEvaluation {
        evaluate::evaluate(self, evidence, now)
    }
}

/// Deduplicates by id, keeping the first after sorting by
/// `(id, occurred_at, value)` — used by [`evaluate`] so replayed/
/// duplicated receipts never count twice.
pub(crate) fn dedup_usage_by_id(usage: &[QuotaUsage]) -> Vec<QuotaUsage> {
    let mut sorted: Vec<&QuotaUsage> = usage.iter().collect();
    sorted.sort_by(|a, b| {
        a.id.0
            .cmp(&b.id.0)
            .then(a.occurred_at.cmp(&b.occurred_at))
            .then(a.amount.value.cmp(&b.amount.value))
    });
    let mut seen = HashSet::new();
    sorted
        .into_iter()
        .filter(|u| seen.insert(u.id))
        .cloned()
        .collect()
}

pub(crate) fn dedup_holds_by_id(holds: &[OutstandingHold]) -> Vec<OutstandingHold> {
    let mut seen = HashSet::new();
    holds
        .iter()
        .filter(|h| seen.insert(h.id))
        .cloned()
        .collect()
}
