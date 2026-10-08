//! Earliest-safe-admit forecast (HORO-1765 PR-a).
//!
//! [`earliest_safe_admit`] answers: given a candidate task's frozen
//! remaining-work need and a set of quota windows (each with its own
//! currently-outstanding holds), what is the earliest instant every
//! window admits that need at once? It never mutates anything — the
//! caller (the state machine added in this ticket's second PR) is
//! responsible for turning the answer into a real reservation.
//!
//! # Why a synthetic hold, not a second blocking calculation
//!
//! [`crate::quota_window::QuotaWindow::evaluate`] already computes
//! `BlockingStatus`/`Relief` as a pure function of evidence. Injecting
//! the candidate's need as one more
//! [`crate::quota_window::OutstandingHold`] (via the `pub(crate)`
//! [`crate::quota_window::OutstandingHold::projected`] seam) and reading
//! the same evaluation back means this module never re-derives
//! sliding/fixed/bucket arithmetic, and can never silently drift from
//! what `evaluate` actually enforces.
//!
//! # The Gauge exception
//!
//! `evaluate`'s `OpaqueProviderSnapshot` branch reads only provider
//! snapshots — it has no notion of "add this candidate's need and
//! re-check," because an opaque gauge is a single percentage/threshold
//! reading, not a ledger this module could subtract a quantity from.
//! Injecting a hold there would silently do nothing (the classic
//! "mismatched unit evidence gets dropped, task looks free" bug class
//! this campaign has hit before) — so a Gauge window is checked only on
//! its own current `blocking` answer, with no synthetic hold involved
//! (see [`check_window`]).

use time::OffsetDateTime;

use crate::quota_window::{
    BlockingStatus, OutstandingHold, ProviderSnapshot, QuotaAmount, QuotaEvidence, QuotaUnit,
    QuotaUsage, QuotaWindow, QuotaWindowId, Relief, WindowKind, WindowState,
};
use crate::reservation::{completion_reserve_for, ReservationId};
use crate::resource_amount::{ResourceAmount, ResourceKind};
use crate::{CompletionContract, EconomicEventId, Estimate, Policy};

use super::{synthetic_uuid, UnavailableReason, MAX_PROBE_STEPS};
use crate::progressive::{RemainingResource, RemainingWorkEstimate};

/// A task's hold against one window, not yet known to have settled.
/// `id` must be unique per *task* (not just per window) — the caller
/// (`pacing::step`) is responsible for deriving it so two different
/// tasks' holds on the same window never collide (a prior self-review
/// pass in this module found exactly that bug: deriving a hold's id from
/// the window alone made every in-flight task's hold collapse to one,
/// which `quota_window::dedup_holds_by_id` then silently deduplicated —
/// the window looked far less constrained than it really was).
#[derive(Debug, Clone, PartialEq)]
pub struct PendingHold {
    pub id: ReservationId,
    pub amount: QuotaAmount,
    /// The instant this hold is projected to settle into ordinary usage.
    /// Before this instant the forecast treats it as an outstanding
    /// hold (reduces headroom, never counted twice); from this instant
    /// on it is folded into `usage` instead — the mechanism that lets a
    /// sliding/fixed window's *cumulative* spend, not just its
    /// currently-concurrent holds, actually constrain admission (AC2:
    /// "including simultaneous in-flight work").
    pub completes_at: OffsetDateTime,
}

/// One quota window's evidence: already-settled usage, in-flight holds
/// not yet known to have settled ([`PendingHold`]), and any provider
/// snapshots (for a Gauge window). Already scoped to this window (unit-
/// filtered, caller-selected) the same way [`QuotaWindow::evaluate`]
/// expects.
pub struct WindowInput<'a> {
    pub window: &'a QuotaWindow,
    pub usage: &'a [QuotaUsage],
    pub pending: &'a [PendingHold],
    pub snapshots: &'a [ProviderSnapshot],
}

/// Splits `pending` as of probe time `t`: holds not yet settled stay
/// outstanding; holds whose `completes_at <= t` become ordinary usage
/// records dated at their completion instant. Returns owned `Vec`s
/// because [`QuotaWindow::evaluate`] borrows its evidence by reference
/// and this module has nowhere else to stash them for the duration of
/// one `check_window` call.
fn split_pending_as_of(
    pending: &[PendingHold],
    t: OffsetDateTime,
) -> (Vec<OutstandingHold>, Vec<QuotaUsage>) {
    let mut holds = Vec::new();
    let mut settled = Vec::new();
    for p in pending {
        if p.completes_at <= t {
            if let Ok(usage) =
                QuotaUsage::new(EconomicEventId(p.id.0), p.completes_at, p.amount.clone())
            {
                settled.push(usage);
            }
        } else {
            holds.push(OutstandingHold::projected(p.id, p.amount.clone()));
        }
    }
    (holds, settled)
}

/// The caller-chosen pacing mode this probe is being run under. Only the
/// `continuity_reserve_bp` figure (added to Sustain's need per window,
/// per the ticket's design) differs between modes at the forecast layer
/// — everything else about *when* fanout happens or how many tasks run
/// concurrently is the state machine's concern (this ticket's second
/// PR), not this pure forecast function's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForecastMode {
    Sustain { continuity_reserve_bp: u16 },
    Burst,
}

/// Computes the earliest instant every window in `windows` admits
/// `need_estimate`'s candidate task at once, walking relief hops up to
/// [`MAX_PROBE_STEPS`] times. `contract`/`estimate_for_reserve` feed
/// [`completion_reserve_for`] exactly as that function already requires
/// (see its own docs); pass `None` for either when no completion
/// contract applies to this candidate (the floor is simply skipped).
/// `backoff_numerator`/`backoff_denominator` scale the need by an
/// integer overrun ratio (the caller is responsible for capping it at
/// 4x per AC5; this function does not re-clamp it) — pass `(1, 1)` for
/// no backoff. All arithmetic is integer/checked/saturating; this
/// function performs no floating-point computation of its own.
#[allow(clippy::too_many_arguments)]
pub fn earliest_safe_admit(
    windows: &[WindowInput<'_>],
    need_estimate: &RemainingWorkEstimate,
    policy: &Policy,
    contract: Option<&CompletionContract>,
    estimate_for_reserve: Option<&Estimate>,
    mode: ForecastMode,
    backoff_numerator: u64,
    backoff_denominator: u64,
    start_at: OffsetDateTime,
) -> super::NextAdmit {
    if need_estimate.confidence < policy.min_confidence {
        return super::NextAdmit::Unavailable(UnavailableReason::BelowMinConfidence);
    }

    // Computed once for the whole candidate — see `compute_task_need`'s
    // own docs for why this must be a single figure, not one recomputed
    // (and potentially clamped differently) inside each window's check.
    let all_windows: Vec<&QuotaWindow> = windows.iter().map(|w| w.window).collect();
    let need = match compute_task_need(
        &all_windows,
        need_estimate,
        contract,
        estimate_for_reserve,
        policy,
        backoff_numerator,
        backoff_denominator,
    ) {
        Ok(need) => need,
        Err(reason) => return super::NextAdmit::Unavailable(reason),
    };

    let mut t = start_at;
    let mut limiting_from_last_round: Option<QuotaWindowId> = None;

    for _ in 0..MAX_PROBE_STEPS {
        let mut latest_relief: Option<(OffsetDateTime, QuotaWindowId)> = None;

        for input in windows {
            match check_window(input, need.as_ref(), mode, t) {
                WindowOutcome::Admit => {}
                WindowOutcome::BlockedAt(relief_at) => {
                    if relief_at <= t {
                        // `evaluate`'s own invariants guarantee a strictly
                        // future relief instant whenever it reports
                        // `Blocking` at `t` (see module docs / the
                        // `relief_is_strictly_future` test below) — this
                        // branch exists only so a future regression in
                        // that invariant fails a probe loop honestly
                        // instead of spinning forever.
                        return super::NextAdmit::Unavailable(UnavailableReason::BeyondHorizon);
                    }
                    match latest_relief {
                        Some((at, _)) if at >= relief_at => {}
                        _ => latest_relief = Some((relief_at, input.window.id())),
                    }
                }
                WindowOutcome::Unavailable(reason) => {
                    return super::NextAdmit::Unavailable(reason);
                }
            }
        }

        match latest_relief {
            None => {
                return match limiting_from_last_round {
                    None => super::NextAdmit::Now,
                    Some(limiting) => super::NextAdmit::At { at: t, limiting },
                };
            }
            Some((next_t, limiting)) => {
                t = next_t;
                limiting_from_last_round = Some(limiting);
            }
        }
    }

    super::NextAdmit::Unavailable(UnavailableReason::ProbeBudgetExhausted)
}

enum WindowOutcome {
    Admit,
    BlockedAt(OffsetDateTime),
    Unavailable(UnavailableReason),
}

fn is_gauge(window: &QuotaWindow) -> bool {
    matches!(window.kind(), WindowKind::OpaqueProviderSnapshot { .. })
}

fn limit_or_capacity(window: &QuotaWindow) -> Option<u64> {
    match window.kind() {
        WindowKind::FixedAligned { limit, .. } => Some(*limit),
        WindowKind::Sliding { limit, .. } => Some(*limit),
        WindowKind::RefillBucket { capacity, .. } => Some(*capacity),
        WindowKind::OpaqueProviderSnapshot { .. } => None,
    }
}

fn extract_relief(state: &WindowState) -> Relief {
    match state {
        WindowState::Period(p) => p.relief.clone(),
        WindowState::Bucket(b) => b.relief.clone(),
        WindowState::Gauge(g) => g.relief.clone(),
    }
}

fn check_window(
    input: &WindowInput<'_>,
    need: Option<&QuotaAmount>,
    mode: ForecastMode,
    t: OffsetDateTime,
) -> WindowOutcome {
    let window = input.window;
    let (pending_holds, settled_from_pending) = split_pending_as_of(input.pending, t);

    if is_gauge(window) {
        // See module docs: a Gauge window has no quantity to inject a
        // need against, so this checks only its own current blocking
        // answer. A Gauge can therefore never report `BlockedAt` here —
        // only `Admit` or `Unavailable`, which keeps the caller's loop
        // from waiting on a window that structurally cannot resolve.
        let evidence = QuotaEvidence {
            usage: input.usage,
            holds: &[],
            snapshots: input.snapshots,
        };
        let eval = window.evaluate(&evidence, t);
        return match eval.blocking {
            BlockingStatus::NotBlocking => WindowOutcome::Admit,
            BlockingStatus::Indeterminate(reason) => WindowOutcome::Unavailable(
                UnavailableReason::WindowIndeterminate(window.id(), reason),
            ),
            BlockingStatus::Blocking => match extract_relief(&eval.state) {
                Relief::ProviderDeclared(_) => {
                    WindowOutcome::Unavailable(UnavailableReason::ProviderDeclaredReliefOnly)
                }
                _ => WindowOutcome::Unavailable(UnavailableReason::ReliefUnknown),
            },
        };
    }

    let Some(need) = need else {
        return WindowOutcome::Unavailable(UnavailableReason::NoEstimateInWindowUnit(window.id()));
    };
    if need.unit != *window.unit() {
        // `compute_task_need` computed this figure against a different
        // window's unit (or this window simply isn't one the task's
        // estimate has a figure for at all) — the feasibility/clamping
        // it already did is against *other* windows, not this one, so
        // this window's own constraint is honestly unknown.
        return WindowOutcome::Unavailable(UnavailableReason::NoEstimateInWindowUnit(window.id()));
    }

    // Sustain's continuity reserve is injected here, per window, on top
    // of the one task-level need — see `compute_task_need`'s docs for
    // why it is never folded into the recorded figure itself.
    let mut injected = need.clone();
    injected.value = injected
        .value
        .saturating_add(continuity_addition(window, mode));

    let synthetic_id = ReservationId(synthetic_uuid(window.id().0, SYNTHETIC_NEED_TAG));
    let synthetic_hold = OutstandingHold::projected(synthetic_id, injected);
    let mut holds: Vec<OutstandingHold> = pending_holds;
    holds.push(synthetic_hold);
    let mut usage: Vec<QuotaUsage> = input.usage.to_vec();
    usage.extend(settled_from_pending);
    let evidence = QuotaEvidence {
        usage: &usage,
        holds: &holds,
        snapshots: input.snapshots,
    };
    let eval = window.evaluate(&evidence, t);
    match eval.blocking {
        BlockingStatus::NotBlocking => WindowOutcome::Admit,
        BlockingStatus::Indeterminate(reason) => {
            WindowOutcome::Unavailable(UnavailableReason::WindowIndeterminate(window.id(), reason))
        }
        BlockingStatus::Blocking => match extract_relief(&eval.state) {
            Relief::At(x) => WindowOutcome::BlockedAt(x),
            Relief::AfterOutstandingHoldsSettle => {
                match input
                    .pending
                    .iter()
                    .map(|p| p.completes_at)
                    .filter(|c| *c > t)
                    .min()
                {
                    Some(next) => WindowOutcome::BlockedAt(next),
                    None => WindowOutcome::Unavailable(UnavailableReason::NoProjectedRelief(
                        window.id(),
                    )),
                }
            }
            Relief::ProviderDeclared(_) => {
                WindowOutcome::Unavailable(UnavailableReason::ProviderDeclaredReliefOnly)
            }
            Relief::Unknown => WindowOutcome::Unavailable(UnavailableReason::ReliefUnknown),
            // Defensive only: `evaluate` never reports `Blocking` with a
            // `NotBlocking` relief: see `evaluate.rs`'s own invariants.
            Relief::NotBlocking => WindowOutcome::Admit,
        },
    }
}

const SYNTHETIC_NEED_TAG: u8 = 0xA5;

fn resource_kind_for_unit(unit: &QuotaUnit) -> Option<ResourceKind> {
    match unit {
        QuotaUnit::UsdCents => Some(ResourceKind::Usd),
        QuotaUnit::Tokens => Some(ResourceKind::Tokens),
        QuotaUnit::Percent => Some(ResourceKind::QuotaPercent),
        QuotaUnit::Requests | QuotaUnit::OpaqueCredit(_) => None,
    }
}

/// Computes **one** recorded need for the candidate task — not a
/// separate figure per window. A prior version of this module computed
/// need independently inside each window's own check, which let a
/// window with a smaller limit or a larger Sustain continuity addition
/// clamp or inflate the figure differently than another window sharing
/// the same unit; `recorded_hold` then recorded whichever window's
/// figure happened to be largest, which could be *more* than what some
/// other window had actually verified safe — AC2's "fits all enforceable
/// hard windows" requires one number that is simultaneously true for
/// every one of them, not a per-window figure reconciled after the fact.
///
/// The frozen estimate's p90 remaining-resource quantile (in the shared
/// unit every matching, non-Gauge window uses) is floored at
/// [`completion_reserve_for`]'s figure, then scaled by the integer
/// backoff ratio, then clamped to one tick below the *smallest* limit
/// among every matching window — the largest amount any of them could
/// ever admit. The feasibility check (`NeedExceedsWindowLimit`) runs
/// against the *unscaled* figure against that same smallest limit:
/// backoff must only throttle (via the clamp), never permanently lock a
/// principal out by making an inflated need "infeasible" forever.
///
/// Sustain's continuity reserve (`continuity_reserve_bp × limit`) is
/// deliberately **not** part of this recorded figure — it is spare
/// per-window admission headroom, not an amount the task actually
/// reserves or ever settles as usage. [`check_window`] adds it only to
/// what gets injected into *that* window's own evaluation.
pub(crate) fn compute_task_need(
    windows: &[&QuotaWindow],
    estimate: &RemainingWorkEstimate,
    contract: Option<&CompletionContract>,
    estimate_for_reserve: Option<&Estimate>,
    policy: &Policy,
    backoff_numerator: u64,
    backoff_denominator: u64,
) -> Result<Option<QuotaAmount>, UnavailableReason> {
    let (kind, base) = match &estimate.resource {
        RemainingResource::Quantiles { kind, p90, .. } => (*kind, *p90),
        RemainingResource::Insufficient { .. } | RemainingResource::Unavailable { .. } => {
            return Err(UnavailableReason::EstimateInsufficient);
        }
    };

    let matching: Vec<&&QuotaWindow> = windows
        .iter()
        .filter(|w| !is_gauge(w) && resource_kind_for_unit(w.unit()) == Some(kind))
        .collect();
    let Some(first) = matching.first() else {
        // No non-Gauge window in this unit at all — nothing to compute
        // a feasibility figure against. Each window's own `check_window`
        // call still independently reports `NoEstimateInWindowUnit` for
        // itself when its unit doesn't match.
        return Ok(None);
    };
    let unit = first.unit().clone();

    let mut need = resource_amount_to_quota_amount(base, &unit)
        .ok_or_else(|| UnavailableReason::NoEstimateInWindowUnit(first.id()))?;

    if let Some(contract) = contract {
        let reserve = completion_reserve_for(contract, estimate_for_reserve, policy);
        if reserve.amount.kind() == kind {
            if let Some(floor) = resource_amount_to_quota_amount(reserve.amount, &unit) {
                need.value = need.value.max(floor.value);
            }
        }
    }

    let smallest_limit = matching
        .iter()
        .filter_map(|w| limit_or_capacity(w).map(|limit| (limit, w.id())))
        .min_by_key(|(limit, _)| *limit);

    if let Some((limit, limiting_window)) = smallest_limit {
        // `>=`, not `>`: `remaining <= 0` blocks at a need exactly equal
        // to the limit (see `evaluate_sliding`/`evaluate_period`), so an
        // equal need can never actually be admitted either.
        if need.value >= limit {
            return Err(UnavailableReason::NeedExceedsWindowLimit(limiting_window));
        }
    }

    let scaled_raw = (need.value as u128)
        .saturating_mul(backoff_numerator.max(1) as u128)
        .saturating_div(backoff_denominator.max(1) as u128);
    need.value = u64::try_from(scaled_raw).unwrap_or(u64::MAX);

    if let Some((limit, _)) = smallest_limit {
        if limit > 0 {
            need.value = need.value.min(limit - 1);
        }
    }

    Ok(Some(need))
}

/// The Sustain continuity-reserve addition for one window — see
/// [`compute_task_need`]'s docs for why this is computed per-window and
/// kept out of the single recorded need.
fn continuity_addition(window: &QuotaWindow, mode: ForecastMode) -> u64 {
    let ForecastMode::Sustain {
        continuity_reserve_bp,
    } = mode
    else {
        return 0;
    };
    let Some(limit) = limit_or_capacity(window) else {
        return 0;
    };
    let addition = (limit as u128)
        .saturating_mul(continuity_reserve_bp as u128)
        .saturating_div(10_000);
    u64::try_from(addition).unwrap_or(u64::MAX)
}

fn resource_amount_to_quota_amount(
    amount: ResourceAmount,
    unit: &QuotaUnit,
) -> Option<QuotaAmount> {
    let converted = QuotaAmount::try_from(amount).ok()?;
    if converted.unit == *unit {
        Some(converted)
    } else {
        None
    }
}

/// Shared test fixtures reused by sibling pacing modules' own test
/// suites (`step`'s tests need a valid [`Policy`] too, and duplicating
/// this builder would risk the two fixtures silently drifting apart).
#[cfg(test)]
pub(crate) mod tests_support {
    use crate::policy::AutonomyBoundary;
    use crate::resource_amount::ResourceAmount;
    use crate::{CompletionContract, CompletionCriterion, Confidence, Policy};

    fn quality_floor() -> CompletionContract {
        CompletionContract::first(vec![CompletionCriterion::required("tests pass")])
    }

    pub(crate) fn policy() -> Policy {
        Policy::validated(
            "test",
            crate::policy::ResourceBound {
                mode: crate::policy::ConstraintMode::Elastic,
                target: ResourceAmount::Tokens(1_000_000),
                elastic_ceiling: Some(ResourceAmount::Tokens(2_000_000)),
                hard_ceiling: ResourceAmount::Tokens(3_000_000),
            },
            crate::policy::TimeBound {
                mode: crate::policy::ConstraintMode::Elastic,
                target_secs: 3600,
                elastic_ceiling_secs: Some(7200),
                hard_ceiling_secs: Some(10_800),
                deadline: None,
            },
            quality_floor(),
            Confidence::Low,
            AutonomyBoundary::AskOnApproval,
        )
        .unwrap()
    }

    pub(crate) fn policy_requiring_medium_confidence() -> Policy {
        let mut p = policy();
        p.min_confidence = Confidence::Medium;
        p
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pacing::tests::test_estimate;
    use crate::progressive::RemainingResource;
    use crate::quota_window::{EntitlementSource, QuotaScope, QuotaSubject, QuotaWindowId};
    use crate::{economic_attribution::PrincipalId, Confidence};
    use tests_support::{policy, policy_requiring_medium_confidence};

    fn sliding_window(id: QuotaWindowId, limit: u64, length_secs: u64) -> QuotaWindow {
        QuotaWindow::validated(
            id,
            QuotaScope {
                subject: QuotaSubject::Principal(PrincipalId("p".to_string())),
                source: EntitlementSource::OperatorConfigured,
                confidence: Confidence::High,
                observed_at: OffsetDateTime::UNIX_EPOCH,
                valid_until: None,
            },
            QuotaUnit::Tokens,
            WindowKind::Sliding { length_secs, limit },
        )
        .unwrap()
    }

    fn estimate_with_tokens_p90(value: u64) -> RemainingWorkEstimate {
        let mut e = test_estimate();
        e.confidence = Confidence::High;
        e.resource = RemainingResource::Quantiles {
            kind: ResourceKind::Tokens,
            p50: ResourceAmount::Tokens(value),
            p80: ResourceAmount::Tokens(value),
            p90: ResourceAmount::Tokens(value),
            conditional_n: 20,
            weakest_truth: crate::economic_event::TruthStrength::Metered,
        };
        e
    }

    #[test]
    fn admits_now_when_headroom_exists() {
        let id = QuotaWindowId::new();
        let window = sliding_window(id, 1000, 21_600);
        let estimate = estimate_with_tokens_p90(100);
        let input = WindowInput {
            window: &window,
            usage: &[],
            pending: &[],
            snapshots: &[],
        };
        let result = earliest_safe_admit(
            &[input],
            &estimate,
            &policy(),
            None,
            None,
            ForecastMode::Burst,
            1,
            1,
            OffsetDateTime::UNIX_EPOCH,
        );
        assert_eq!(result, super::super::NextAdmit::Now);
    }

    #[test]
    fn unavailable_when_need_exceeds_window_unit_mismatch() {
        let id = QuotaWindowId::new();
        // Requests-unit window, but the estimate only carries Tokens —
        // exactly AC2's "Requests-window-with-Tokens-only-estimate"
        // fixture.
        let window = QuotaWindow::validated(
            id,
            QuotaScope {
                subject: QuotaSubject::Principal(PrincipalId("p".to_string())),
                source: EntitlementSource::OperatorConfigured,
                confidence: Confidence::High,
                observed_at: OffsetDateTime::UNIX_EPOCH,
                valid_until: None,
            },
            QuotaUnit::Requests,
            WindowKind::Sliding {
                length_secs: 60,
                limit: 10,
            },
        )
        .unwrap();
        let estimate = estimate_with_tokens_p90(100);
        let input = WindowInput {
            window: &window,
            usage: &[],
            pending: &[],
            snapshots: &[],
        };
        let result = earliest_safe_admit(
            &[input],
            &estimate,
            &policy(),
            None,
            None,
            ForecastMode::Burst,
            1,
            1,
            OffsetDateTime::UNIX_EPOCH,
        );
        assert_eq!(
            result,
            super::super::NextAdmit::Unavailable(UnavailableReason::NoEstimateInWindowUnit(id))
        );
    }

    #[test]
    fn unavailable_when_need_exceeds_window_limit() {
        let id = QuotaWindowId::new();
        let window = sliding_window(id, 50, 21_600);
        let estimate = estimate_with_tokens_p90(100);
        let input = WindowInput {
            window: &window,
            usage: &[],
            pending: &[],
            snapshots: &[],
        };
        let result = earliest_safe_admit(
            &[input],
            &estimate,
            &policy(),
            None,
            None,
            ForecastMode::Burst,
            1,
            1,
            OffsetDateTime::UNIX_EPOCH,
        );
        assert_eq!(
            result,
            super::super::NextAdmit::Unavailable(UnavailableReason::NeedExceedsWindowLimit(id))
        );
    }

    #[test]
    fn below_min_confidence_is_refused_before_any_window_check() {
        let id = QuotaWindowId::new();
        let window = sliding_window(id, 1000, 21_600);
        let mut estimate = estimate_with_tokens_p90(100);
        estimate.confidence = Confidence::Low;
        let p = policy_requiring_medium_confidence();
        assert!(p.min_confidence > Confidence::Low);
        let input = WindowInput {
            window: &window,
            usage: &[],
            pending: &[],
            snapshots: &[],
        };
        let result = earliest_safe_admit(
            &[input],
            &estimate,
            &p,
            None,
            None,
            ForecastMode::Burst,
            1,
            1,
            OffsetDateTime::UNIX_EPOCH,
        );
        assert_eq!(
            result,
            super::super::NextAdmit::Unavailable(UnavailableReason::BelowMinConfidence)
        );
    }

    #[test]
    fn waits_for_sliding_window_relief_when_blocked() {
        let id = QuotaWindowId::new();
        let window = sliding_window(id, 150, 21_600);
        let estimate = estimate_with_tokens_p90(100);
        let t0 = OffsetDateTime::UNIX_EPOCH;
        let usage = vec![crate::quota_window::QuotaUsage::new(
            crate::EconomicEventId(uuid::Uuid::new_v4()),
            t0,
            QuotaAmount::new(QuotaUnit::Tokens, 100),
        )
        .unwrap()];
        let input = WindowInput {
            window: &window,
            usage: &usage,
            pending: &[],
            snapshots: &[],
        };
        let result = earliest_safe_admit(
            &[input],
            &estimate,
            &policy(),
            None,
            None,
            ForecastMode::Burst,
            1,
            1,
            t0,
        );
        match result {
            super::super::NextAdmit::At { at, limiting } => {
                assert_eq!(at, t0 + time::Duration::seconds(21_600));
                assert_eq!(limiting, id);
            }
            other => panic!("expected At, got {other:?}"),
        }
    }

    /// AC1, the ticket's own fixture: "In sliding six-hour + fixed
    /// weekly case burst correctly forecasts post-reset six-hour
    /// starvation." Nine tasks' worth of usage (900 tokens) lands at
    /// `t0` (a Monday 00:00 UTC, so the weekly window's period boundary
    /// is unambiguous); the 10th candidate's 100-token need pushes the
    /// 6h sliding window (limit 1000) to exactly its limit, while the
    /// weekly window (limit 1200) still has 200 tokens of headroom at
    /// `t0` and even 6h later — so the 10th task must wait exactly 6h,
    /// and the *sliding* window (not the weekly one) is correctly named
    /// as the limiting constraint, never the other way round.
    #[test]
    fn ac1_burst_forecasts_post_reset_six_hour_starvation() {
        let sliding_id = QuotaWindowId::new();
        let sliding = sliding_window(sliding_id, 1000, 21_600);

        let weekly_id = QuotaWindowId::new();
        let utc = crate::quota_window::IanaTimeZone::new("UTC").expect("UTC is a known IANA zone");
        let weekly = QuotaWindow::validated(
            weekly_id,
            QuotaScope {
                subject: QuotaSubject::Principal(PrincipalId("p".to_string())),
                source: EntitlementSource::OperatorConfigured,
                confidence: Confidence::High,
                observed_at: OffsetDateTime::UNIX_EPOCH,
                valid_until: None,
            },
            QuotaUnit::Tokens,
            WindowKind::FixedAligned {
                period: crate::quota_window::AlignedPeriod::Week {
                    starts_on: crate::quota_window::ResetWeekday::Monday,
                    at: crate::quota_window::WallClockTime::new(0, 0).unwrap(),
                },
                time_zone: utc,
                limit: 1200,
            },
        )
        .unwrap();

        // 2024-01-01 is a Monday — t0 sits exactly at the weekly
        // window's own reset boundary, so "post-reset" is literal here.
        let t0 = time::macros::datetime!(2024-01-01 00:00:00 UTC);
        let estimate = estimate_with_tokens_p90(100);

        // Nine tasks' usage, already settled at t0 (burst started all
        // nine at once and they are modeled as immediately-counted spend
        // for this fixture — see module docs on `PendingHold` for why a
        // settled-before-now pending hold becomes ordinary usage).
        let usage: Vec<QuotaUsage> = (0..9)
            .map(|_| {
                QuotaUsage::new(
                    crate::EconomicEventId(uuid::Uuid::new_v4()),
                    t0,
                    QuotaAmount::new(QuotaUnit::Tokens, 100),
                )
                .unwrap()
            })
            .collect();

        let windows = vec![
            WindowInput {
                window: &sliding,
                usage: &usage,
                pending: &[],
                snapshots: &[],
            },
            WindowInput {
                window: &weekly,
                usage: &usage,
                pending: &[],
                snapshots: &[],
            },
        ];

        let result = earliest_safe_admit(
            &windows,
            &estimate,
            &policy(),
            None,
            None,
            ForecastMode::Burst,
            1,
            1,
            t0,
        );

        match result {
            super::super::NextAdmit::At { at, limiting } => {
                assert_eq!(
                    at,
                    t0 + time::Duration::seconds(21_600),
                    "the 10th task must wait exactly 6h for the sliding window to clear"
                );
                assert_eq!(
                    limiting, sliding_id,
                    "the sliding window, not the weekly one, must be named as limiting — \
                     the weekly window still has 200 tokens of headroom at t0"
                );
            }
            other => panic!("expected At(+6h) limited by the sliding window, got {other:?}"),
        }
    }

    /// Regression test for a real bug an independent review found: the
    /// need used to check each window — and the figure ultimately
    /// recorded as the Start's hold — used to be computed independently
    /// *per window*, so a Sustain continuity reserve (window-limit-
    /// scaled, different for a 1000-limit sliding window vs. a
    /// 1200-limit weekly one) could make one window's check pass with a
    /// different injected amount than another's, and `recorded_hold`
    /// would then record whichever was largest — a figure that was
    /// never actually checked, consistently, against every window.
    /// `compute_task_need` is now computed once for the whole candidate,
    /// with continuity applied only as a per-window addition at
    /// injection time, never folded into the recorded figure.
    #[test]
    fn ac2_one_recorded_need_consistently_checked_against_every_window() {
        let sliding_id = QuotaWindowId::new();
        let sliding = sliding_window(sliding_id, 1000, 21_600);
        let weekly_id = QuotaWindowId::new();
        let utc = crate::quota_window::IanaTimeZone::new("UTC").unwrap();
        let weekly = QuotaWindow::validated(
            weekly_id,
            QuotaScope {
                subject: QuotaSubject::Principal(PrincipalId("p".to_string())),
                source: EntitlementSource::OperatorConfigured,
                confidence: Confidence::High,
                observed_at: OffsetDateTime::UNIX_EPOCH,
                valid_until: None,
            },
            QuotaUnit::Tokens,
            WindowKind::FixedAligned {
                period: crate::quota_window::AlignedPeriod::Week {
                    starts_on: crate::quota_window::ResetWeekday::Monday,
                    at: crate::quota_window::WallClockTime::new(0, 0).unwrap(),
                },
                time_zone: utc,
                limit: 1200,
            },
        )
        .unwrap();

        let t0 = time::macros::datetime!(2024-01-01 00:00:00 UTC);
        let estimate = estimate_with_tokens_p90(100);

        // The one figure `compute_task_need` would record — confirmed
        // identical regardless of which window's limit or continuity
        // addition one might otherwise have been tempted to key it off.
        let all_windows = vec![&sliding, &weekly];
        let recorded = compute_task_need(&all_windows, &estimate, None, None, &policy(), 1, 1)
            .unwrap()
            .expect("both windows share a unit the estimate has a figure for");
        assert_eq!(
            recorded.value, 100,
            "no continuity addition belongs in the recorded figure"
        );

        // 850 tokens of existing usage: tight enough that the sliding
        // window (1000 limit, continuity addition 100) blocks, while
        // the weekly window (1200 limit, continuity addition 120) does
        // not — demonstrating the two windows' *different* per-window
        // continuity additions never leak into what's recorded, only
        // into what's injected at check time.
        let usage: Vec<QuotaUsage> = (0..1)
            .map(|_| {
                QuotaUsage::new(
                    crate::EconomicEventId(uuid::Uuid::new_v4()),
                    t0,
                    QuotaAmount::new(QuotaUnit::Tokens, 850),
                )
                .unwrap()
            })
            .collect();

        let windows = vec![
            WindowInput {
                window: &sliding,
                usage: &usage,
                pending: &[],
                snapshots: &[],
            },
            WindowInput {
                window: &weekly,
                usage: &usage,
                pending: &[],
                snapshots: &[],
            },
        ];

        let result = earliest_safe_admit(
            &windows,
            &estimate,
            &policy(),
            None,
            None,
            ForecastMode::Sustain {
                continuity_reserve_bp: 1000,
            },
            1,
            1,
            t0,
        );
        match result {
            super::super::NextAdmit::At { limiting, .. } => {
                assert_eq!(
                    limiting, sliding_id,
                    "the sliding window's own (smaller) continuity addition is what \
                     blocks here — the weekly window's own larger addition never \
                     affects the recorded need, only its own injected check"
                );
            }
            other => panic!("expected the sliding window to be limiting, got {other:?}"),
        }
    }
}
