//! Read-only, CLI-facing rendering support for the pacing simulator
//! (HORO-1767, `quota explain --replay`).
//!
//! Everything here answers "what does the evidence show, as of this
//! instant" from a [`PacerState`] a caller already produced via
//! [`super::simulate::simulate_until`] — it adds no new consumer and
//! advances nothing past what the caller already computed. The
//! zero-live-wiring guard (`domain/tests/pacing_not_wired_live.rs`) scans
//! only `daemon/src`, `gateway/src`, and `ledger/src`; this file lives in
//! `domain` and is reached only from `crates/cli`, so it changes nothing
//! that guard checks — see that test's own docs and
//! `docs/adr/0016-sustain-burst-pacing.md`.
//!
//! # Why `evaluate_at` never settles a hold early
//!
//! An active task's hold stays an *outstanding* [`OutstandingHold`] for
//! every instant up to and including `at`, even past its own projected
//! completion — unlike `forecast::split_pending_as_of`, which the probe
//! loop uses to let a *future* relief hop fold an about-to-finish hold
//! into settled usage for the purpose of asking "is it safe to start a
//! new task". A CLI explain surface answering "what does the evidence
//! show as of `at`" must never present a *projected* completion as
//! *actual* settled spend before a real `TaskCompleted` event says so —
//! presenting a simulated projection as an observed fact is exactly the
//! false-claim class HORO-1767 exists to remove. Every window evaluation
//! this module produces therefore counts every currently active task's
//! hold as outstanding, never as settled, regardless of how its
//! `projected_complete_at` compares to `at`.

use std::collections::BTreeMap;

use time::OffsetDateTime;

use crate::quota_window::{OutstandingHold, ProviderSnapshot, QuotaAmount, QuotaEvidence};

use super::step::{pending_for_window, PacerState, Scenario};
use super::SimTaskId;

/// Evaluates every window in `scenario` against `state`'s own evidence
/// (its append-only settled `usage_log`, plus a synthetic
/// [`OutstandingHold`] for every currently active task's hold — see
/// module docs for why none of those holds are ever folded into settled
/// usage here) and the caller-supplied `snapshots`, as of `at`. Reuses
/// [`pending_for_window`] (the same evidence-selection `step` itself
/// uses) and [`crate::quota_window::QuotaWindow::evaluate`] rather than
/// re-deriving window arithmetic — see `forecast.rs`'s own module docs
/// for why this crate never re-derives that a second time.
pub fn evaluate_at(
    state: &PacerState,
    scenario: &Scenario<'_>,
    snapshots: &[ProviderSnapshot],
    at: OffsetDateTime,
) -> Vec<crate::quota_window::WindowEvaluation> {
    scenario
        .windows
        .iter()
        .map(|window| {
            let holds: Vec<OutstandingHold> = pending_for_window(window, state)
                .into_iter()
                .map(|p| OutstandingHold::projected(p.id, p.amount))
                .collect();
            let evidence = QuotaEvidence {
                usage: state.usage_log(),
                holds: &holds,
                snapshots,
            };
            window.evaluate(&evidence, at)
        })
        .collect()
}

/// Per in-scope task, the honest `(hold, actual-spend-basis)` pair a CLI
/// explain surface needs — never a raw sum of `Spend` events (see
/// `step.rs`'s own docs on why `spend_so_far` tracks a *max*, not a
/// running total) and never a reservation presented as if it were
/// observed spend.
///
/// - A still-active task reports its current hold, and its actual spend
///   *basis* iff at least one `Spend` event was reported for it — `None`,
///   never `0`, when nothing was ever reported (an unreported spend is
///   unknown, not zero).
/// - A completed task reports no current hold (`None` — it is no longer
///   reserving anything) and its settled usage record, which `step`
///   itself already computed as `max(reported spend, settlement floor)`
///   at `TaskCompleted` — this module does not recompute that figure, it
///   only looks it up.
pub fn task_actuals(state: &PacerState) -> BTreeMap<SimTaskId, (Option<QuotaAmount>, Option<u64>)> {
    let mut out = BTreeMap::new();
    for (task, hold) in state.active_holds() {
        let spend = state.spend_so_far_for(task);
        out.insert(task, (Some(hold.clone()), spend));
    }
    for task in state.completed() {
        let settled = state.settled_usage_for(*task).map(|a| a.value);
        out.insert(*task, (None, settled));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::business_context::Priority;
    use crate::economic_attribution::PrincipalId;
    use crate::pacing::forecast::tests_support::policy;
    use crate::pacing::simulate::simulate_until;
    use crate::pacing::tests::test_estimate;
    use crate::pacing::{PacingEvent, PacingPreference, SimTask, TaskSet};
    use crate::progressive::{RemainingDuration, RemainingResource};
    use crate::quota_window::{
        EntitlementSource, QuotaScope, QuotaSubject, QuotaUnit, QuotaWindow, QuotaWindowId,
        WindowKind,
    };
    use crate::resource_amount::{ResourceAmount, ResourceKind};
    use crate::Confidence;

    fn window(limit: u64, length_secs: u64) -> QuotaWindow {
        QuotaWindow::validated(
            QuotaWindowId::new(),
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

    fn task(id: u32, tokens: u64, duration_secs: u64) -> SimTask {
        let mut e = test_estimate();
        e.confidence = Confidence::High;
        e.resource = RemainingResource::Quantiles {
            kind: ResourceKind::Tokens,
            p50: ResourceAmount::Tokens(tokens),
            p80: ResourceAmount::Tokens(tokens),
            p90: ResourceAmount::Tokens(tokens),
            conditional_n: 20,
            weakest_truth: crate::economic_event::TruthStrength::Metered,
        };
        e.duration = RemainingDuration::Quantiles {
            p50_secs: duration_secs,
            p80_secs: duration_secs,
            p90_secs: duration_secs,
            conditional_n: 20,
        };
        SimTask {
            id: SimTaskId(id),
            principal: PrincipalId("p".to_string()),
            depends_on: vec![],
            priority: Priority::Normal,
            deadline: None,
            estimate: e,
        }
    }

    /// `evaluate_at`'s computed holds must match what `step` itself would
    /// compute at the same instant — checked at `t0` itself (before any
    /// task's projected completion), so the single active task's hold is
    /// unambiguously still outstanding under both this module's logic and
    /// `step`'s own `try_admit`/`pending_for_window` evidence-building.
    #[test]
    fn evaluate_at_matches_what_step_itself_would_compute() {
        let tasks = TaskSet::validated(vec![task(1, 100, 600)]).unwrap();
        let w = window(1000, 21_600);
        let scenario = Scenario {
            tasks: &tasks,
            windows: std::slice::from_ref(&w),
            contract: None,
            preference: PacingPreference::Burst {
                target_end: OffsetDateTime::UNIX_EPOCH + time::Duration::hours(1),
                max_fanout: 5,
            },
        };
        let t0 = OffsetDateTime::UNIX_EPOCH;
        let events = vec![PacingEvent::ModeChanged { at: t0, seq: 0 }];
        let (state, _, exhausted) = simulate_until(&events, &policy(), &scenario, None);
        assert!(!exhausted);
        assert_eq!(state.active_count(), 1, "the one task must have started");

        let evals = evaluate_at(&state, &scenario, &[], t0);
        assert_eq!(evals.len(), 1);
        let crate::quota_window::WindowState::Period(period) = &evals[0].state else {
            panic!("a Sliding window always evaluates to WindowState::Period");
        };
        // The active task's 100-token hold is outstanding, zero usage
        // settled yet (no `TaskCompleted` has been processed) — exactly
        // what independently re-evaluating the window by hand (as
        // `pending_for_window`'s own caller, `try_admit`, does inside
        // `step`) would show.
        assert_eq!(period.outstanding, 100);
        assert_eq!(period.settled, 0);
    }

    /// `task_actuals`: an active task with no reported `Spend` is
    /// `Unknown` (`None`), never `0` — and a task with a reported `Spend`
    /// reports that exact figure, never a sum across multiple `Spend`
    /// events for the same task (`step`'s own `spend_so_far` already
    /// tracks a max, not a running total — see its docs).
    #[test]
    fn task_actuals_reports_unknown_not_zero_for_unreported_spend() {
        let tasks = TaskSet::validated(vec![task(1, 100, 600), task(2, 100, 600)]).unwrap();
        let w = window(1000, 21_600);
        let scenario = Scenario {
            tasks: &tasks,
            windows: std::slice::from_ref(&w),
            contract: None,
            preference: PacingPreference::Burst {
                target_end: OffsetDateTime::UNIX_EPOCH + time::Duration::hours(1),
                max_fanout: 5,
            },
        };
        let t0 = OffsetDateTime::UNIX_EPOCH;
        let events = vec![
            PacingEvent::ModeChanged { at: t0, seq: 0 },
            PacingEvent::Spend {
                at: t0 + time::Duration::seconds(10),
                seq: 1,
                task: SimTaskId(2),
                amount: crate::quota_window::QuotaAmount::new(QuotaUnit::Tokens, 40),
            },
        ];
        let (state, _, exhausted) = simulate_until(&events, &policy(), &scenario, None);
        assert!(!exhausted);

        let actuals = task_actuals(&state);
        let (hold1, spend1) = actuals.get(&SimTaskId(1)).expect("task 1 is active");
        assert!(hold1.is_some());
        assert_eq!(
            *spend1, None,
            "task 1 never reported a Spend — Unknown, not 0"
        );

        let (hold2, spend2) = actuals.get(&SimTaskId(2)).expect("task 2 is active");
        assert!(hold2.is_some());
        assert_eq!(*spend2, Some(40));
    }

    /// A completed task's actual is its settled usage record
    /// (`max(reported spend, settlement floor)`, computed once by `step`
    /// at `TaskCompleted`), and it no longer reports a current hold.
    #[test]
    fn task_actuals_reports_settled_usage_for_a_completed_task() {
        let tasks = TaskSet::validated(vec![task(1, 100, 60)]).unwrap();
        let w = window(1000, 21_600);
        let scenario = Scenario {
            tasks: &tasks,
            windows: std::slice::from_ref(&w),
            contract: None,
            preference: PacingPreference::Burst {
                target_end: OffsetDateTime::UNIX_EPOCH + time::Duration::hours(1),
                max_fanout: 5,
            },
        };
        let t0 = OffsetDateTime::UNIX_EPOCH;
        let complete_at = t0 + time::Duration::seconds(60);
        let events = vec![
            PacingEvent::ModeChanged { at: t0, seq: 0 },
            PacingEvent::TaskCompleted {
                at: complete_at,
                seq: 1,
                task: SimTaskId(1),
            },
        ];
        let (state, _, exhausted) = simulate_until(&events, &policy(), &scenario, None);
        assert!(!exhausted);
        assert!(state.completed().contains(&SimTaskId(1)));

        let actuals = task_actuals(&state);
        let (hold, spend) = actuals.get(&SimTaskId(1)).expect("task 1 settled");
        assert_eq!(*hold, None, "a completed task no longer holds anything");
        assert_eq!(
            *spend,
            Some(100),
            "settles at the reservation floor when no Spend was reported"
        );
    }
}
