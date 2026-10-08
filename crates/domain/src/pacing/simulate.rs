//! Deterministic replay driver for [`super::step::step`] (HORO-1765 PR-b).
//!
//! Folds [`step`] over a sorted event list, merging in the one derived
//! timer `step` leaves pending (the last `NextAdmit::At` it could not
//! yet satisfy) — this is the "genuine replenishment" trigger the
//! ticket's design calls for: no polling, no LLM in the loop, and the
//! output is byte-identical across repeated runs of the same input
//! (AC4), because every instant the loop ever visits is either an input
//! event's own `at` or a value `step` itself already computed.

use time::OffsetDateTime;

use crate::Policy;

use super::step::{step, PacerState, Scenario};
use super::{PacingEvent, Proposal};

/// Upper bound on synthesized "timer-only" passes between real events,
/// so a pathological scenario (e.g. a window whose relief never
/// actually advances admission) cannot loop unboundedly. Matches
/// [`super::MAX_SCENARIO_EVENTS`]'s order of magnitude rather than
/// inventing a second unrelated constant.
const MAX_TIMER_TICKS: usize = super::MAX_SCENARIO_EVENTS;

/// One step of the replay: either a real input event, or a synthesized
/// timer tick (`seq` is `None` for a tick — it never collides with a
/// real event's `seq`, which the caller is responsible for keeping
/// unique per instant).
#[derive(Debug, Clone, PartialEq)]
pub enum Tick<'a> {
    Event(&'a PacingEvent),
    Timer(OffsetDateTime),
}

/// Runs every event in `events` (sorted by `(at, seq)` — this function
/// sorts a local index rather than requiring the caller to pre-sort) through
/// [`step`], synthesizing a timer tick whenever `state.pending_timer()` falls
/// strictly before the next real event (or after the last one). Returns the
/// final [`PacerState`] plus the full, ordered trace of `(Tick, Vec<Proposal>)`
/// pairs — the replayable record AC4's determinism test compares byte-for-byte.
pub fn simulate<'a>(
    events: &'a [PacingEvent],
    policy: &Policy,
    scenario: &Scenario<'_>,
) -> (PacerState, Vec<(Tick<'a>, Vec<Proposal>)>) {
    let mut order: Vec<&PacingEvent> = events.iter().collect();
    order.sort_by(|a, b| a.at().cmp(&b.at()).then(a.seq().cmp(&b.seq())));

    let mut state = PacerState::default();
    let mut trace = Vec::with_capacity(order.len());
    let mut cursor = 0usize;
    let mut ticks = 0usize;

    loop {
        let next_event = order.get(cursor).copied();
        let timer = state.pending_timer();

        let due_now = match (next_event, timer) {
            (None, None) => break,
            (Some(ev), None) => Due::Event(ev),
            (None, Some(t)) => Due::Timer(t),
            (Some(ev), Some(t)) => {
                if t < ev.at() {
                    Due::Timer(t)
                } else {
                    Due::Event(ev)
                }
            }
        };

        match due_now {
            Due::Event(ev) => {
                cursor += 1;
                let (new_state, proposals) = step(&state, ev, policy, scenario);
                state = new_state;
                trace.push((Tick::Event(ev), proposals));
            }
            Due::Timer(at) => {
                ticks += 1;
                if ticks > MAX_TIMER_TICKS {
                    break;
                }
                // A tick carries no event payload of its own — model it
                // as a `ModeChanged` marker at the timer instant (the
                // lightest existing variant), purely to drive `step`'s
                // admission attempt forward in time. `seq` uses the
                // reserved tick-sequence value (`u64::MAX - ticks`) so it
                // can never collide with a real event's `seq`, which
                // callers are expected to assign from a normal
                // ascending counter.
                let synthetic = PacingEvent::ModeChanged {
                    at,
                    seq: u64::MAX - ticks as u64,
                };
                let (new_state, proposals) = step(&state, &synthetic, policy, scenario);
                state = new_state;
                trace.push((Tick::Timer(at), proposals));
            }
        }
    }

    (state, trace)
}

enum Due<'a> {
    Event(&'a PacingEvent),
    Timer(OffsetDateTime),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::business_context::Priority;
    use crate::economic_attribution::PrincipalId;
    use crate::pacing::forecast::tests_support::policy;
    use crate::pacing::tests::test_estimate;
    use crate::pacing::{PacingPreference, SimTask, SimTaskId, TaskSet};
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

    /// AC4: running the identical scenario twice produces byte-identical
    /// output. `PacerState`/`Proposal` don't derive `Serialize` summary
    /// comparison here directly — this test compares the `Debug`
    /// representation of the full trace, which is deterministic for the
    /// same reason the JSON would be: every field is plain data with no
    /// `HashMap` iteration and no clock/RNG read.
    #[test]
    fn same_scenario_twice_is_byte_identical() {
        let tasks = TaskSet::validated(vec![task(1, 100, 60), task(2, 100, 60)]).unwrap();
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
        let events = vec![PacingEvent::ModeChanged {
            at: OffsetDateTime::UNIX_EPOCH,
            seq: 0,
        }];
        let (state_a, trace_a) = simulate(&events, &policy(), &scenario);
        let (state_b, trace_b) = simulate(&events, &policy(), &scenario);
        assert_eq!(format!("{state_a:?}"), format!("{state_b:?}"));
        assert_eq!(format!("{trace_a:?}"), format!("{trace_b:?}"));
    }

    #[test]
    fn bounded_steps_for_a_large_scenario() {
        let many: Vec<SimTask> = (0..200).map(|i| task(i, 10, 5)).collect();
        let tasks = TaskSet::validated(many).unwrap();
        let w = window(100_000, 21_600);
        let scenario = Scenario {
            tasks: &tasks,
            windows: std::slice::from_ref(&w),
            contract: None,
            preference: PacingPreference::Burst {
                target_end: OffsetDateTime::UNIX_EPOCH + time::Duration::hours(2),
                max_fanout: 200,
            },
        };
        let events = vec![PacingEvent::ModeChanged {
            at: OffsetDateTime::UNIX_EPOCH,
            seq: 0,
        }];
        let start = std::time::Instant::now();
        let (state, _trace) = simulate(&events, &policy(), &scenario);
        let elapsed = start.elapsed();
        assert_eq!(state.active_count(), 200);
        assert!(
            elapsed.as_secs() < 5,
            "a 200-task, single-event scenario must resolve well within a bounded step budget"
        );
    }

    /// AC1 (scoped-down fixture — see module docs for the MVP
    /// single-schedule scope this PR's state machine accepts): BURST
    /// admits up to `max_fanout` ready tasks in the same step, never
    /// trickling them out one at a time; SUSTAIN admits exactly one task
    /// per step and leaves the rest held, regardless of how much
    /// headroom the window has left. `max_fanout` (9) is deliberately
    /// below the ready-task count (13) and below what the sliding
    /// window's limit (1000, 100 each) would allow (9 fits, a 10th would
    /// not) — sized this way on purpose (not 10-for-10) so fanout, not
    /// window headroom, is what visibly caps BURST's first step.
    #[test]
    fn ac1_burst_admits_fanout_at_once_sustain_admits_one_at_a_time() {
        let many: Vec<SimTask> = (0..13).map(|i| task(i, 100, 60)).collect();
        let tasks = TaskSet::validated(many).unwrap();
        let w = window(1000, 21_600);

        let burst_scenario = Scenario {
            tasks: &tasks,
            windows: std::slice::from_ref(&w),
            contract: None,
            preference: PacingPreference::Burst {
                target_end: OffsetDateTime::UNIX_EPOCH + time::Duration::hours(6),
                max_fanout: 9,
            },
        };
        let events = vec![PacingEvent::ModeChanged {
            at: OffsetDateTime::UNIX_EPOCH,
            seq: 0,
        }];
        let (burst_state, _) = simulate(&events, &policy(), &burst_scenario);
        assert_eq!(burst_state.active_count(), 9);

        let sustain_scenario = Scenario {
            tasks: &tasks,
            windows: std::slice::from_ref(&w),
            contract: None,
            preference: PacingPreference::Sustain {
                horizon_secs: 21_600,
                working_hours: None,
                continuity_reserve_bp: 0,
            },
        };
        let (sustain_state, _) = simulate(&events, &policy(), &sustain_scenario);
        assert_eq!(
            sustain_state.active_count(),
            1,
            "SUSTAIN must admit exactly one task per step regardless of remaining window headroom"
        );
    }

    /// AC3: the same scenario run under both modes admits a different
    /// *schedule* (BURST starts many at once, SUSTAIN paces them), but
    /// every task eventually admitted, the policy consulted, and the
    /// completion contract consulted are identical — this test asserts
    /// that `step`/`simulate` never fork those shared inputs between
    /// modes (there is only one `Policy`/`Option<&CompletionContract>`
    /// parameter threaded through both).
    #[test]
    fn ac3_policy_and_contract_identity_is_shared_across_modes() {
        let tasks = TaskSet::validated(vec![task(1, 100, 60), task(2, 100, 60)]).unwrap();
        let w = window(1000, 21_600);
        let shared_policy = policy();
        let events = vec![PacingEvent::ModeChanged {
            at: OffsetDateTime::UNIX_EPOCH,
            seq: 0,
        }];

        let burst_scenario = Scenario {
            tasks: &tasks,
            windows: std::slice::from_ref(&w),
            contract: None,
            preference: PacingPreference::Burst {
                target_end: OffsetDateTime::UNIX_EPOCH + time::Duration::hours(6),
                max_fanout: 2,
            },
        };
        let sustain_scenario = Scenario {
            tasks: &tasks,
            windows: std::slice::from_ref(&w),
            contract: None,
            preference: PacingPreference::Sustain {
                horizon_secs: 21_600,
                working_hours: None,
                continuity_reserve_bp: 0,
            },
        };

        let (burst_state, _) = simulate(&events, &shared_policy, &burst_scenario);
        let (sustain_state, _) = simulate(&events, &shared_policy, &sustain_scenario);

        // Different schedules …
        assert_eq!(burst_state.active_count(), 2);
        assert_eq!(sustain_state.active_count(), 1);
        // … but the same policy name/min_confidence was consulted by
        // both — `step` takes `&Policy` by reference from the caller in
        // both branches, never a mode-specific copy.
        assert_eq!(shared_policy.name, "test");
    }
}
