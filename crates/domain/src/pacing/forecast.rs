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
    BlockingStatus, OutstandingHold, QuotaAmount, QuotaEvidence, QuotaUnit, QuotaWindow,
    QuotaWindowId, Relief, WindowKind, WindowState,
};
use crate::reservation::{completion_reserve_for, ReservationId};
use crate::resource_amount::{ResourceAmount, ResourceKind};
use crate::{CompletionContract, Estimate, Policy};

use super::{synthetic_uuid, UnavailableReason, MAX_PROBE_STEPS};
use crate::progressive::{RemainingResource, RemainingWorkEstimate};

/// One quota window's evidence, plus the information needed to resolve
/// [`Relief::AfterOutstandingHoldsSettle`] into a concrete next-probe
/// instant.
pub struct WindowInput<'a> {
    pub window: &'a QuotaWindow,
    /// Evidence for this window **excluding** the candidate task's own
    /// synthetic need — [`earliest_safe_admit`] injects that itself at
    /// each probe step. Already scoped to this window (unit-filtered,
    /// caller-selected) the same way [`QuotaWindow::evaluate`] expects.
    pub evidence: QuotaEvidence<'a>,
    /// Ascending, caller-supplied instants at which an existing
    /// outstanding hold already present in `evidence.holds` is projected
    /// to settle into usage. Only consulted when this window's relief is
    /// `AfterOutstandingHoldsSettle`; need not be sorted strictly (this
    /// module sorts defensively), but must include every such instant or
    /// [`UnavailableReason::NoProjectedRelief`] may fire spuriously.
    pub hold_completions: &'a [OffsetDateTime],
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

    let mut t = start_at;
    let mut limiting_from_last_round: Option<QuotaWindowId> = None;

    for _ in 0..MAX_PROBE_STEPS {
        let mut latest_relief: Option<(OffsetDateTime, QuotaWindowId)> = None;

        for input in windows {
            let need = match window_need(
                input.window,
                need_estimate,
                contract,
                estimate_for_reserve,
                policy,
                mode,
                backoff_numerator,
                backoff_denominator,
            ) {
                Ok(need) => need,
                Err(reason) => return super::NextAdmit::Unavailable(reason),
            };

            match check_window(input, need.as_ref(), t) {
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
    t: OffsetDateTime,
) -> WindowOutcome {
    let window = input.window;

    if is_gauge(window) {
        // See module docs: a Gauge window has no quantity to inject a
        // need against, so this checks only its own current blocking
        // answer. A Gauge can therefore never report `BlockedAt` here —
        // only `Admit` or `Unavailable`, which keeps the caller's loop
        // from waiting on a window that structurally cannot resolve.
        let eval = window.evaluate(&input.evidence, t);
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
        return WindowOutcome::Unavailable(UnavailableReason::NoEstimateInWindowUnit(window.id()));
    }
    if let Some(limit) = limit_or_capacity(window) {
        if need.value > limit {
            return WindowOutcome::Unavailable(UnavailableReason::NeedExceedsWindowLimit(
                window.id(),
            ));
        }
    }

    let synthetic_id = ReservationId(synthetic_uuid(window.id().0, SYNTHETIC_NEED_TAG));
    let synthetic_hold = OutstandingHold::projected(synthetic_id, need.clone());
    let mut holds: Vec<OutstandingHold> = input.evidence.holds.to_vec();
    holds.push(synthetic_hold);
    let evidence = QuotaEvidence {
        usage: input.evidence.usage,
        holds: &holds,
        snapshots: input.evidence.snapshots,
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
                match input.hold_completions.iter().filter(|c| **c > t).min() {
                    Some(next) => WindowOutcome::BlockedAt(*next),
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

/// Computes this window's need for the candidate task: the frozen
/// estimate's p90 remaining-resource quantile in the window's own unit,
/// floored at [`completion_reserve_for`]'s figure when that figure's
/// resource kind matches, inflated by the Sustain continuity reserve
/// (`continuity_reserve_bp × limit`, window-limit-based, never estimate-
/// based) and by the integer backoff ratio. Returns `Ok(None)` only for
/// a Gauge window (no quantity applies there — see module docs);
/// returns `Err` for every honest reason this figure cannot be computed
/// at all.
#[allow(clippy::too_many_arguments)]
fn window_need(
    window: &QuotaWindow,
    estimate: &RemainingWorkEstimate,
    contract: Option<&CompletionContract>,
    estimate_for_reserve: Option<&Estimate>,
    policy: &Policy,
    mode: ForecastMode,
    backoff_numerator: u64,
    backoff_denominator: u64,
) -> Result<Option<QuotaAmount>, UnavailableReason> {
    if is_gauge(window) {
        return Ok(None);
    }

    let Some(kind) = resource_kind_for_unit(window.unit()) else {
        return Err(UnavailableReason::NoEstimateInWindowUnit(window.id()));
    };

    let base = match &estimate.resource {
        RemainingResource::Quantiles {
            kind: est_kind,
            p90,
            ..
        } => {
            if *est_kind != kind {
                return Err(UnavailableReason::NoEstimateInWindowUnit(window.id()));
            }
            *p90
        }
        RemainingResource::Insufficient { .. } | RemainingResource::Unavailable { .. } => {
            return Err(UnavailableReason::EstimateInsufficient);
        }
    };

    let mut need = resource_amount_to_quota_amount(base, window.unit())
        .ok_or(UnavailableReason::NoEstimateInWindowUnit(window.id()))?;

    if let Some(contract) = contract {
        let reserve = completion_reserve_for(contract, estimate_for_reserve, policy);
        if reserve.amount.kind() == kind {
            if let Some(floor) = resource_amount_to_quota_amount(reserve.amount, window.unit()) {
                need.value = need.value.max(floor.value);
            }
        }
    }

    if let ForecastMode::Sustain {
        continuity_reserve_bp,
    } = mode
    {
        if let Some(limit) = limit_or_capacity(window) {
            let addition = (limit as u128)
                .saturating_mul(continuity_reserve_bp as u128)
                .saturating_div(10_000);
            need.value = need
                .value
                .saturating_add(u64::try_from(addition).unwrap_or(u64::MAX));
        }
    }

    let scaled = (need.value as u128)
        .saturating_mul(backoff_numerator.max(1) as u128)
        .saturating_div(backoff_denominator.max(1) as u128);
    need.value = u64::try_from(scaled).unwrap_or(u64::MAX);

    Ok(Some(need))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::completion_contract::{CompletionContract, CompletionCriterion};
    use crate::pacing::tests::test_estimate;
    use crate::policy::AutonomyBoundary;
    use crate::progressive::RemainingResource;
    use crate::quota_window::{EntitlementSource, QuotaScope, QuotaSubject, QuotaWindowId};
    use crate::{economic_attribution::PrincipalId, Confidence};

    fn quality_floor() -> CompletionContract {
        CompletionContract::first(vec![CompletionCriterion::required("tests pass")])
    }

    fn policy() -> Policy {
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

    fn policy_requiring_medium_confidence() -> Policy {
        let mut p = policy();
        p.min_confidence = Confidence::Medium;
        p
    }

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
            evidence: QuotaEvidence {
                usage: &[],
                holds: &[],
                snapshots: &[],
            },
            hold_completions: &[],
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
            evidence: QuotaEvidence {
                usage: &[],
                holds: &[],
                snapshots: &[],
            },
            hold_completions: &[],
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
            evidence: QuotaEvidence {
                usage: &[],
                holds: &[],
                snapshots: &[],
            },
            hold_completions: &[],
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
            evidence: QuotaEvidence {
                usage: &[],
                holds: &[],
                snapshots: &[],
            },
            hold_completions: &[],
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
            evidence: QuotaEvidence {
                usage: &usage,
                holds: &[],
                snapshots: &[],
            },
            hold_completions: &[],
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
}
