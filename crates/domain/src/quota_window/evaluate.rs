//! Pure evaluation of a [`super::QuotaWindow`] against supplied evidence.
//! No mutable state anywhere: recording usage means appending to the
//! evidence the caller passes next time, never mutating a counter —
//! which is why a reset on one window can never affect another.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use super::{
    calendar, dedup_holds_by_id, dedup_usage_by_id, AlignedPeriod, GaugeReading, OutstandingHold,
    ProviderSnapshot, QuotaEvidence, QuotaUsage, QuotaWindow, QuotaWindowId, WindowKind,
};

/// Whether a window currently blocks admission. A stale, missing, or
/// undisclosed snapshot can never collapse to [`Self::NotBlocking`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum BlockingStatus {
    Blocking,
    NotBlocking,
    Indeterminate(IndeterminateReason),
}

impl BlockingStatus {
    /// Combines two statuses, keeping the more restrictive:
    /// `Blocking > Indeterminate > NotBlocking`. On two `Indeterminate`s,
    /// keeps `self`'s reason (first-computed wins).
    pub fn most_restrictive(self, other: Self) -> Self {
        use BlockingStatus::*;
        match (self, other) {
            (Blocking, _) | (_, Blocking) => Blocking,
            (Indeterminate(reason), _) | (_, Indeterminate(reason)) => Indeterminate(reason),
            (NotBlocking, NotBlocking) => NotBlocking,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndeterminateReason {
    NoSnapshot,
    SnapshotStale,
    SnapshotUndisclosed,
    NoDeclaredLimit,
    SnapshotUnitMismatch,
    EntitlementExpired,
    UnsupportedSchemaVersion,
}

/// The honest answer to "when does this window stop blocking".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "relief", rename_all = "snake_case")]
pub enum Relief {
    NotBlocking,
    /// Computed by Libra from the window's own reset/refill mechanics.
    At(#[serde(with = "crate::economic_event::occurred_at_wire")] OffsetDateTime),
    /// The provider's own claim, carried only from a fresh snapshot —
    /// kept distinct from [`Self::At`] so a provider's claim is never
    /// silently treated as Libra's own computed guarantee.
    ProviderDeclared(#[serde(with = "crate::economic_event::occurred_at_wire")] OffsetDateTime),
    /// Outstanding holds alone reach the limit/capacity; no amount of
    /// waiting frees it without those holds settling or releasing.
    AfterOutstandingHoldsSettle,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PeriodState {
    #[serde(with = "crate::economic_event::occurred_at_wire")]
    pub start: OffsetDateTime,
    #[serde(with = "crate::economic_event::occurred_at_wire")]
    pub end: OffsetDateTime,
    pub limit: u64,
    pub settled: u64,
    pub outstanding: u64,
    /// `limit - settled - outstanding`, computed with i128 intermediates
    /// and saturated to i64 — never clamped at zero, so an overrun stays
    /// visible.
    pub remaining: i64,
    /// Count of distinct usage records counted in this period. `0` means
    /// "nothing observed", not "verified unused" — Libra-observed usage
    /// is a lower bound when traffic can bypass the gateway.
    pub usage_records: usize,
    pub relief: Relief,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BucketState {
    pub capacity: u64,
    /// May be negative: an overdraft (spend recorded faster than the
    /// bucket could refill) stays visible rather than clamping at zero.
    pub level: i64,
    pub outstanding: u64,
    pub remaining: i64,
    pub usage_records: usize,
    pub relief: Relief,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StaleReason {
    PastValidUntil,
    ExceedsMaxStaleness,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "freshness", rename_all = "snake_case")]
pub enum GaugeFreshness {
    NoSnapshot,
    Fresh,
    Stale(StaleReason),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GaugeState {
    pub freshness: GaugeFreshness,
    pub latest: Option<ProviderSnapshot>,
    pub age_secs: Option<u64>,
    pub relief: Relief,
}

impl GaugeState {
    /// The reading to actually trust — `Some` only when `freshness` is
    /// `Fresh`. A stale snapshot stays visible in `latest` but is never
    /// trusted for an admission decision.
    pub fn trusted_reading(&self) -> Option<&GaugeReading> {
        if self.freshness == GaugeFreshness::Fresh {
            self.latest.as_ref().map(ProviderSnapshot::reading)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum WindowState {
    Period(PeriodState),
    Bucket(BucketState),
    Gauge(GaugeState),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowEvaluation {
    pub window_id: QuotaWindowId,
    #[serde(with = "crate::economic_event::occurred_at_wire")]
    pub evaluated_at: OffsetDateTime,
    pub state: WindowState,
    pub blocking: BlockingStatus,
}

/// Evidence already filtered to this window's unit, with future-dated
/// and duplicate records removed, sorted by `(occurred_at, id)`.
struct PreparedEvidence {
    usage: Vec<QuotaUsage>,
    outstanding: u64,
}

fn prepare(
    evidence: &QuotaEvidence<'_>,
    window: &QuotaWindow,
    now: OffsetDateTime,
) -> PreparedEvidence {
    let usage: Vec<QuotaUsage> = dedup_usage_by_id(evidence.usage)
        .into_iter()
        .filter(|u| u.amount().unit == *window.unit() && u.occurred_at() <= now)
        .collect();
    let mut usage = usage;
    usage.sort_by(|a, b| {
        a.occurred_at()
            .cmp(&b.occurred_at())
            .then(a.id().0.cmp(&b.id().0))
    });

    let holds: Vec<OutstandingHold> = dedup_holds_by_id(evidence.holds)
        .into_iter()
        .filter(|h| h.amount().unit == *window.unit())
        .collect();
    let outstanding: u64 = holds.iter().map(|h| h.amount().value).sum();

    PreparedEvidence { usage, outstanding }
}

/// Evaluates `window` against `evidence` as of `now`. See the module's
/// own public API (`QuotaWindow::evaluate`) for the determinism
/// guarantee this function provides.
pub(super) fn evaluate(
    window: &QuotaWindow,
    evidence: &QuotaEvidence<'_>,
    now: OffsetDateTime,
) -> WindowEvaluation {
    let (state, mut blocking) = match window.kind() {
        WindowKind::FixedAligned {
            period,
            time_zone,
            limit,
        } => evaluate_period(window, evidence, now, period, time_zone, *limit),
        WindowKind::Sliding { length_secs, limit } => {
            evaluate_sliding(window, evidence, now, *length_secs, *limit)
        }
        WindowKind::RefillBucket {
            capacity,
            refill_amount,
            refill_period_secs,
            anchored_at,
            level_at_anchor,
        } => evaluate_bucket(
            window,
            evidence,
            now,
            *capacity,
            *refill_amount,
            *refill_period_secs,
            *anchored_at,
            *level_at_anchor,
        ),
        WindowKind::OpaqueProviderSnapshot { max_staleness_secs } => {
            evaluate_gauge(window, evidence, now, *max_staleness_secs)
        }
    };

    if let Some(valid_until) = window.scope().valid_until {
        if valid_until <= now {
            blocking = blocking.most_restrictive(BlockingStatus::Indeterminate(
                IndeterminateReason::EntitlementExpired,
            ));
        }
    }

    WindowEvaluation {
        window_id: window.id(),
        evaluated_at: now,
        state,
        blocking,
    }
}

fn remaining_i64(limit: u64, settled: u64, outstanding: u64) -> i64 {
    let value = limit as i128 - settled as i128 - outstanding as i128;
    value.clamp(i64::MIN as i128, i64::MAX as i128) as i64
}

fn evaluate_period(
    window: &QuotaWindow,
    evidence: &QuotaEvidence<'_>,
    now: OffsetDateTime,
    period: &AlignedPeriod,
    time_zone: &super::IanaTimeZone,
    limit: u64,
) -> (WindowState, BlockingStatus) {
    let (start, end) = calendar::fixed_aligned_bounds(now, period, time_zone);
    let prepared = prepare(evidence, window, now);
    let in_period: Vec<&QuotaUsage> = prepared
        .usage
        .iter()
        .filter(|u| u.occurred_at() >= start && u.occurred_at() < end)
        .collect();
    let settled: u64 = in_period.iter().map(|u| u.amount().value).sum();
    let remaining = remaining_i64(limit, settled, prepared.outstanding);
    let blocking_now = remaining <= 0;

    let relief = if prepared.outstanding >= limit {
        Relief::AfterOutstandingHoldsSettle
    } else if blocking_now {
        Relief::At(end)
    } else {
        Relief::NotBlocking
    };

    let state = WindowState::Period(PeriodState {
        start,
        end,
        limit,
        settled,
        outstanding: prepared.outstanding,
        remaining,
        usage_records: in_period.len(),
        relief,
    });

    let blocking = if blocking_now {
        BlockingStatus::Blocking
    } else {
        BlockingStatus::NotBlocking
    };
    (state, blocking)
}

fn evaluate_sliding(
    window: &QuotaWindow,
    evidence: &QuotaEvidence<'_>,
    now: OffsetDateTime,
    length_secs: u64,
    limit: u64,
) -> (WindowState, BlockingStatus) {
    let window_start = now - time::Duration::seconds(length_secs as i64);
    let prepared = prepare(evidence, window, now);
    let in_window: Vec<&QuotaUsage> = prepared
        .usage
        .iter()
        .filter(|u| u.occurred_at() > window_start && u.occurred_at() <= now)
        .collect();
    let settled: u64 = in_window.iter().map(|u| u.amount().value).sum();
    let remaining = remaining_i64(limit, settled, prepared.outstanding);
    let blocking_now = remaining <= 0;

    let relief = if prepared.outstanding >= limit {
        Relief::AfterOutstandingHoldsSettle
    } else if !blocking_now {
        Relief::NotBlocking
    } else {
        // Walk the sorted in-window records; relief is the instant the
        // oldest-aging record(s) have aged out enough to clear headroom.
        let mut cumulative_cleared = 0i128;
        let mut relief_at = None;
        for usage in &in_window {
            cumulative_cleared += usage.amount().value as i128;
            let projected_settled = settled as i128 - cumulative_cleared;
            let projected_remaining =
                limit as i128 - projected_settled - prepared.outstanding as i128;
            if projected_remaining > 0 {
                relief_at = Some(usage.occurred_at() + time::Duration::seconds(length_secs as i64));
                break;
            }
        }
        relief_at.map(Relief::At).unwrap_or(Relief::Unknown)
    };

    let state = WindowState::Period(PeriodState {
        start: window_start,
        end: now,
        limit,
        settled,
        outstanding: prepared.outstanding,
        remaining,
        usage_records: in_window.len(),
        relief,
    });

    let blocking = if blocking_now {
        BlockingStatus::Blocking
    } else {
        BlockingStatus::NotBlocking
    };
    (state, blocking)
}

#[allow(clippy::too_many_arguments)]
fn evaluate_bucket(
    window: &QuotaWindow,
    evidence: &QuotaEvidence<'_>,
    now: OffsetDateTime,
    capacity: u64,
    refill_amount: u64,
    refill_period_secs: u64,
    anchored_at: OffsetDateTime,
    level_at_anchor: u64,
) -> (WindowState, BlockingStatus) {
    let prepared = prepare(evidence, window, now);
    let in_range: Vec<&QuotaUsage> = prepared
        .usage
        .iter()
        .filter(|u| u.occurred_at() >= anchored_at && u.occurred_at() <= now)
        .collect();

    // All arithmetic at millisecond resolution in i128, matching the
    // design: level_scaled = level * period_ms, so refill-per-ms stays
    // an exact integer ratio.
    let period_ms: i128 = refill_period_secs as i128 * 1000;
    let capacity_scaled: i128 = capacity as i128 * period_ms;
    let mut level_scaled: i128 = level_at_anchor as i128 * period_ms;
    let mut cursor = anchored_at;

    for usage in &in_range {
        let delta_ms = (usage.occurred_at() - cursor).whole_milliseconds();
        level_scaled = (level_scaled + refill_amount as i128 * delta_ms).min(capacity_scaled);
        level_scaled -= usage.amount().value as i128 * period_ms;
        cursor = usage.occurred_at();
    }
    let tail_ms = (now - cursor).whole_milliseconds();
    level_scaled = (level_scaled + refill_amount as i128 * tail_ms).min(capacity_scaled);

    let level = level_scaled.div_euclid(period_ms);
    let level_i64 = level.clamp(i64::MIN as i128, i64::MAX as i128) as i64;
    let remaining = level_i64 - prepared.outstanding as i64;
    let blocking_now = remaining <= 0;

    let relief = if prepared.outstanding as i128 + 1 > capacity as i128 {
        Relief::AfterOutstandingHoldsSettle
    } else if !blocking_now {
        Relief::NotBlocking
    } else {
        let deficit_scaled = (prepared.outstanding as i128 + 1) * period_ms - level_scaled;
        if deficit_scaled <= 0 {
            Relief::NotBlocking
        } else {
            let ms_needed = (deficit_scaled + refill_amount as i128 - 1) / refill_amount as i128;
            Relief::At(now + time::Duration::milliseconds(ms_needed as i64))
        }
    };

    let state = WindowState::Bucket(BucketState {
        capacity,
        level: level_i64,
        outstanding: prepared.outstanding,
        remaining,
        usage_records: in_range.len(),
        relief,
    });

    let blocking = if blocking_now {
        BlockingStatus::Blocking
    } else {
        BlockingStatus::NotBlocking
    };
    (state, blocking)
}

fn evaluate_gauge(
    window: &QuotaWindow,
    evidence: &QuotaEvidence<'_>,
    now: OffsetDateTime,
    max_staleness_secs: u64,
) -> (WindowState, BlockingStatus) {
    let candidates: Vec<&ProviderSnapshot> = evidence
        .snapshots
        .iter()
        .filter(|s| s.window_id() == window.id() && s.observed_at() <= now)
        .collect();

    let latest = candidates.into_iter().max_by(|a, b| {
        a.observed_at().cmp(&b.observed_at()).then_with(|| {
            // Tie-break: a Used reading beats Undisclosed; between two
            // Used readings the higher `used` wins (conservative).
            reading_rank(a.reading()).cmp(&reading_rank(b.reading()))
        })
    });

    let Some(latest) = latest else {
        let state = WindowState::Gauge(GaugeState {
            freshness: GaugeFreshness::NoSnapshot,
            latest: None,
            age_secs: None,
            relief: Relief::Unknown,
        });
        return (
            state,
            BlockingStatus::Indeterminate(IndeterminateReason::NoSnapshot),
        );
    };

    let age = now - latest.observed_at();
    let age_secs = age.whole_seconds().max(0) as u64;
    let past_valid_until = latest.valid_until().map(|v| v <= now).unwrap_or(false);
    let exceeds_staleness = age_secs > max_staleness_secs;

    let freshness = if past_valid_until {
        GaugeFreshness::Stale(StaleReason::PastValidUntil)
    } else if exceeds_staleness {
        GaugeFreshness::Stale(StaleReason::ExceedsMaxStaleness)
    } else {
        GaugeFreshness::Fresh
    };

    let relief = if freshness == GaugeFreshness::Fresh {
        latest
            .declared_reset_at()
            .map(Relief::ProviderDeclared)
            .unwrap_or(Relief::Unknown)
    } else {
        Relief::Unknown
    };

    let blocking = match freshness {
        GaugeFreshness::Stale(StaleReason::PastValidUntil) => {
            BlockingStatus::Indeterminate(IndeterminateReason::SnapshotStale)
        }
        GaugeFreshness::Stale(StaleReason::ExceedsMaxStaleness) => {
            BlockingStatus::Indeterminate(IndeterminateReason::SnapshotStale)
        }
        GaugeFreshness::NoSnapshot => {
            BlockingStatus::Indeterminate(IndeterminateReason::NoSnapshot)
        }
        GaugeFreshness::Fresh => match latest.reading() {
            GaugeReading::Undisclosed => {
                BlockingStatus::Indeterminate(IndeterminateReason::SnapshotUndisclosed)
            }
            GaugeReading::Used { used, limit } => {
                if used.unit != *window.unit() {
                    BlockingStatus::Indeterminate(IndeterminateReason::SnapshotUnitMismatch)
                } else {
                    match limit {
                        None if used.unit != super::QuotaUnit::Percent => {
                            BlockingStatus::Indeterminate(IndeterminateReason::NoDeclaredLimit)
                        }
                        _ => {
                            let effective_limit = limit.unwrap_or(10_000);
                            if used.value >= effective_limit {
                                BlockingStatus::Blocking
                            } else {
                                BlockingStatus::NotBlocking
                            }
                        }
                    }
                }
            }
        },
    };

    let state = WindowState::Gauge(GaugeState {
        freshness,
        latest: Some(latest.clone()),
        age_secs: Some(age_secs),
        relief,
    });
    (state, blocking)
}

fn reading_rank(reading: &GaugeReading) -> u64 {
    match reading {
        GaugeReading::Undisclosed => 0,
        GaugeReading::Used { used, .. } => used.value + 1,
    }
}
