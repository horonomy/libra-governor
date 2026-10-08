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
///
/// Thin wrapper over [`simulate_until`] with no upper bound — see that
/// function's docs for the general form (added for HORO-1767's `quota
/// explain --as-of` replay, which must never process an event/timer past
/// the caller's chosen instant).
pub fn simulate<'a>(
    events: &'a [PacingEvent],
    policy: &Policy,
    scenario: &Scenario<'_>,
) -> (PacerState, Vec<(Tick<'a>, Vec<Proposal>)>) {
    let (state, trace, _tick_budget_exhausted) = simulate_until(events, policy, scenario, None);
    (state, trace)
}

/// Runs every event in `events` through [`step`] exactly as [`simulate`]
/// does, but never processes an event or synthesized timer tick whose
/// instant is strictly later than `until` (when `Some`) — the instant
/// itself is still processed. `until: None` behaves identically to the
/// unbounded form.
///
/// The third return value is `true` iff the loop stopped because
/// [`MAX_TIMER_TICKS`] was exhausted before every due instant up to
/// `until` was processed — a caller (HORO-1767's `quota explain`) must
/// treat that as "the forecast could not be resolved", never silently
/// report whatever partial state happened to result, since a resolved
/// `pending_timer` in that case is an artifact of the budget running out,
/// not a genuine answer.
pub fn simulate_until<'a>(
    events: &'a [PacingEvent],
    policy: &Policy,
    scenario: &Scenario<'_>,
    until: Option<OffsetDateTime>,
) -> (PacerState, Vec<(Tick<'a>, Vec<Proposal>)>, bool) {
    let mut order: Vec<&PacingEvent> = events.iter().collect();
    order.sort_by(|a, b| a.at().cmp(&b.at()).then(a.seq().cmp(&b.seq())));

    let mut state = PacerState::default();
    let mut trace = Vec::with_capacity(order.len());
    let mut cursor = 0usize;
    let mut ticks = 0usize;
    let mut tick_budget_exhausted = false;

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

        let due_at = match due_now {
            Due::Event(ev) => ev.at(),
            Due::Timer(t) => t,
        };
        if let Some(until) = until {
            if due_at > until {
                break;
            }
        }

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
                    tick_budget_exhausted = true;
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

    (state, trace, tick_budget_exhausted)
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
        EntitlementSource, QuotaAmount, QuotaScope, QuotaSubject, QuotaUnit, QuotaWindow,
        QuotaWindowId, WindowKind,
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

        // Different schedules under the one shared `Policy` — the
        // non-tautological half (the same contract/policy is actually
        // *consulted* identically, not merely referenced by both
        // `Scenario`s) is `ac3_completion_reserve_floor_is_identical_
        // across_modes` below.
        assert_eq!(burst_state.active_count(), 2);
        assert_eq!(sustain_state.active_count(), 1);
        let _ = &shared_policy;
    }

    /// AC3, non-tautological half: with an actual `CompletionContract`
    /// supplied, both modes apply the *same* completion-reserve floor to
    /// the hold they admit a task at — demonstrating the contract is
    /// genuinely consulted identically by both, not merely that the two
    /// `Scenario`s happen to hold the same Rust reference.
    #[test]
    fn ac3_completion_reserve_floor_is_identical_across_modes() {
        use crate::completion_contract::{CompletionContract, CompletionCriterion};
        use crate::reservation::completion_reserve_for;

        let contract = CompletionContract::first(vec![
            CompletionCriterion::required("tests pass"),
            CompletionCriterion::required("docs updated"),
        ]);
        let shared_policy = policy();
        let reserve = completion_reserve_for(&contract, None, &shared_policy);

        let tasks = TaskSet::validated(vec![task(1, 10, 60)]).unwrap();
        let w = window(1_000_000, 21_600);
        let events = vec![PacingEvent::ModeChanged {
            at: OffsetDateTime::UNIX_EPOCH,
            seq: 0,
        }];

        let burst_scenario = Scenario {
            tasks: &tasks,
            windows: std::slice::from_ref(&w),
            contract: Some(&contract),
            preference: PacingPreference::Burst {
                target_end: OffsetDateTime::UNIX_EPOCH + time::Duration::hours(6),
                max_fanout: 1,
            },
        };
        let sustain_scenario = Scenario {
            tasks: &tasks,
            windows: std::slice::from_ref(&w),
            contract: Some(&contract),
            preference: PacingPreference::Sustain {
                horizon_secs: 21_600,
                working_hours: None,
                continuity_reserve_bp: 0,
            },
        };

        let (_, burst_trace) = simulate(&events, &shared_policy, &burst_scenario);
        let (_, sustain_trace) = simulate(&events, &shared_policy, &sustain_scenario);

        let burst_hold = first_start_hold(&burst_trace).expect("burst must admit the one task");
        let sustain_hold =
            first_start_hold(&sustain_trace).expect("sustain must admit the one task");

        assert!(
            burst_hold.value >= reserve.amount.as_f64() as u64,
            "burst's admitted hold must respect the completion reserve floor"
        );
        assert_eq!(
            burst_hold.value, sustain_hold.value,
            "the same contract/policy must floor both modes' hold identically"
        );
    }

    fn first_start_hold(trace: &[(Tick<'_>, Vec<Proposal>)]) -> Option<QuotaAmount> {
        trace.iter().find_map(|(_, proposals)| {
            proposals.iter().find_map(|p| match p {
                Proposal::Start { hold, .. } => Some(hold.clone()),
                _ => None,
            })
        })
    }

    /// AC2, independent of `step`'s own internal pending-hold machinery:
    /// rebuilds every in-flight hold at each `Start` proposal's instant
    /// from the trace alone (distinct ids, not reusing
    /// `pending_hold_id`), calls `QuotaWindow::evaluate` directly, and
    /// asserts `NotBlocking`. This is the check that would have caught
    /// the hold-id-collision bug `distinct_tasks_get_distinct_hold_ids_
    /// so_concurrent_holds_genuinely_stack` (in `step.rs`) now guards —
    /// an invariant over `simulate`'s own output, not a test that reuses
    /// the code under test to check itself.
    #[test]
    fn ac2_every_start_independently_verified_not_blocking() {
        let many: Vec<SimTask> = (0..11).map(|i| task(i, 100, 60)).collect();
        let tasks = TaskSet::validated(many).unwrap();
        let w = window(1000, 21_600);
        let scenario = Scenario {
            tasks: &tasks,
            windows: std::slice::from_ref(&w),
            contract: None,
            preference: PacingPreference::Burst {
                target_end: OffsetDateTime::UNIX_EPOCH + time::Duration::hours(1),
                max_fanout: 20,
            },
        };
        let events = vec![PacingEvent::ModeChanged {
            at: OffsetDateTime::UNIX_EPOCH,
            seq: 0,
        }];
        let (_, trace) = simulate(&events, &policy(), &scenario);

        // Independently-tracked (amount, completes_at) pairs, keyed by a
        // fresh v4 id per start (never derived the way `step`'s internal
        // machinery derives its own ids) — the point is that a bug in
        // `step`'s derivation must not be able to hide from this check.
        // Before `completes_at`, a pair counts as an outstanding hold;
        // from `completes_at` on it is folded into usage instead —
        // independently reconstructing the same split `forecast`'s
        // `split_pending_as_of` makes, not by calling it.
        let mut independent_pending: Vec<(QuotaAmount, OffsetDateTime)> = Vec::new();
        let mut started_any = false;
        for (tick, proposals) in &trace {
            let at = match tick {
                Tick::Event(e) => e.at(),
                Tick::Timer(t) => *t,
            };
            for p in proposals {
                if let Proposal::Start { hold, .. } = p {
                    started_any = true;
                    let mut evidence_holds = Vec::new();
                    let mut evidence_usage = Vec::new();
                    for (amount, completes_at) in &independent_pending {
                        let id = crate::reservation::ReservationId(uuid::Uuid::new_v4());
                        if *completes_at <= at {
                            if let Ok(usage) = crate::quota_window::QuotaUsage::new(
                                crate::EconomicEventId(id.0),
                                *completes_at,
                                amount.clone(),
                            ) {
                                evidence_usage.push(usage);
                            }
                        } else {
                            evidence_holds.push(crate::quota_window::OutstandingHold::projected(
                                id,
                                amount.clone(),
                            ));
                        }
                    }
                    // The candidate task's own hold must also be in the
                    // evidence being checked — omitting it would only
                    // ever catch the original bug one task late (e.g.
                    // with 9 already in flight, a wrongly-admitted 10th
                    // would still read back as `NotBlocking` if its own
                    // hold weren't counted).
                    let this_id = crate::reservation::ReservationId(uuid::Uuid::new_v4());
                    evidence_holds.push(crate::quota_window::OutstandingHold::projected(
                        this_id,
                        hold.clone(),
                    ));
                    let evidence = crate::quota_window::QuotaEvidence {
                        usage: &evidence_usage,
                        holds: &evidence_holds,
                        snapshots: &[],
                    };
                    let eval = w.evaluate(&evidence, at);
                    assert_eq!(
                        eval.blocking,
                        crate::quota_window::BlockingStatus::NotBlocking,
                        "a Start proposal must independently verify as NotBlocking \
                         against every other hold already active at that instant"
                    );
                    independent_pending.push((hold.clone(), at + time::Duration::seconds(60)));
                }
            }
        }
        assert!(
            started_any,
            "scenario must have produced at least one Start to check"
        );
        // Unlike the single-step regression test in `step.rs` (which
        // checks only the *first* pass, where exactly 9 of 11 fit), this
        // test lets `simulate` run the derived timer forward across
        // relief hops with no end condition, so eventually all 11 tasks
        // start once the sliding window ages enough usage out — the
        // invariant actually being checked is that every single one of
        // those 11 starts, whenever it happens, independently verifies
        // as `NotBlocking`.
        assert_eq!(independent_pending.len(), 11);
    }

    /// AC5's control-run half: "backoff reduces new commitments" — never
    /// a permanent lockout, and never affecting an unrelated principal.
    /// Principal `p` runs task A then task B (B depends on A); principal
    /// `q` runs task C then task D (D depends on C), entirely
    /// independently. A overruns its hold (`Spend` 400 against a
    /// 100-token hold, a 4x ratio) and completes *on time* — this run
    /// deliberately does not rely on a late completion to keep backoff
    /// alive, since `TaskCompleted`'s reset is keyed on cost (spend vs.
    /// hold), not duration. C never overruns.
    ///
    /// Window limit arithmetic (sliding, Tokens): once A and C settle,
    /// usage is 100+100=200 in the control run, but 400+100=500 in the
    /// overrun run (A's real overspend genuinely consumed more of the
    /// shared window — that asymmetry is correct, not a test artifact).
    /// With limit=700: D's *uninflated* need (100) admits in both runs
    /// (200+100=300 and 500+100=600, both < 700); B's need is 100 in
    /// control (200+100=300, admits) but backoff-inflated to 400 in the
    /// overrun run (500+400=900 > 700, blocks) — isolating exactly
    /// backoff's effect on B, independent of q's task D.
    #[test]
    fn ac5_backoff_delays_later_starts_for_the_overrunning_principal() {
        let w = window(700, 21_600);
        let build_tasks = || {
            TaskSet::validated(vec![
                task_for(1, "p", vec![], 100, 60),
                task_for(2, "p", vec![SimTaskId(1)], 100, 60),
                task_for(3, "q", vec![], 100, 60),
                task_for(4, "q", vec![SimTaskId(3)], 100, 60),
            ])
            .unwrap()
        };
        fn scenario_for<'a>(tasks: &'a TaskSet, w: &'a QuotaWindow) -> Scenario<'a> {
            Scenario {
                tasks,
                windows: std::slice::from_ref(w),
                contract: None,
                preference: PacingPreference::Burst {
                    target_end: OffsetDateTime::UNIX_EPOCH + time::Duration::hours(12),
                    max_fanout: 4,
                },
            }
        }
        let t0 = OffsetDateTime::UNIX_EPOCH;
        let complete_at = t0 + time::Duration::seconds(60);

        let control_tasks = build_tasks();
        let control_events = vec![
            PacingEvent::ModeChanged { at: t0, seq: 0 },
            PacingEvent::TaskCompleted {
                at: complete_at,
                seq: 1,
                task: SimTaskId(1),
            },
            PacingEvent::TaskCompleted {
                at: complete_at,
                seq: 2,
                task: SimTaskId(3),
            },
        ];
        let (_, control_trace) = simulate(
            &control_events,
            &policy(),
            &scenario_for(&control_tasks, &w),
        );

        let overrun_tasks = build_tasks();
        let overrun_events = vec![
            PacingEvent::ModeChanged { at: t0, seq: 0 },
            PacingEvent::Spend {
                at: t0 + time::Duration::seconds(10),
                seq: 1,
                task: SimTaskId(1),
                amount: QuotaAmount::new(QuotaUnit::Tokens, 400),
            },
            PacingEvent::TaskCompleted {
                at: complete_at,
                seq: 2,
                task: SimTaskId(1),
            },
            PacingEvent::TaskCompleted {
                at: complete_at,
                seq: 3,
                task: SimTaskId(3),
            },
        ];
        let (_, overrun_trace) = simulate(
            &overrun_events,
            &policy(),
            &scenario_for(&overrun_tasks, &w),
        );

        let start_of = |trace: &[(Tick<'_>, Vec<Proposal>)], id: u32| {
            trace.iter().find_map(|(_, proposals)| {
                proposals.iter().find_map(|p| match p {
                    Proposal::Start { task, at, .. } if *task == SimTaskId(id) => Some(*at),
                    _ => None,
                })
            })
        };

        let control_b = start_of(&control_trace, 2).expect("control run must admit task B");
        let overrun_b = start_of(&overrun_trace, 2)
            .expect("task B must still eventually start — backoff throttles, never bans");
        assert!(
            overrun_b > control_b,
            "the overrunning principal's task B must start strictly later \
             with the overrun than without it (control: {control_b:?}, overrun: {overrun_b:?})"
        );

        let control_d = start_of(&control_trace, 4).expect("control run must admit task D");
        let overrun_d = start_of(&overrun_trace, 4)
            .expect("task D (principal q, unaffected by p's overrun) must start");
        assert_eq!(
            control_d, overrun_d,
            "principal q's task D must be unaffected by principal p's overrun — \
             this also exercises try_admit's skip-not-break fix: D must not wait \
             behind a blocked B in the same principal-independent pass"
        );
        assert_eq!(control_d, complete_at);
    }

    fn task_for(
        id: u32,
        principal: &str,
        depends_on: Vec<SimTaskId>,
        tokens: u64,
        duration_secs: u64,
    ) -> SimTask {
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
            principal: PrincipalId(principal.to_string()),
            depends_on,
            priority: Priority::Normal,
            deadline: None,
            estimate: e,
        }
    }

    /// Domain-level regression for HORO-1767: `simulate_until(.., None)`
    /// must be byte-identical to `simulate`'s own output — the refactor
    /// that introduced `simulate_until` must be behavior-preserving, not
    /// just "equivalent in the cases this PR happens to exercise".
    #[test]
    fn simulate_until_with_no_bound_matches_simulate() {
        let tasks = TaskSet::validated(vec![task(1, 100, 60), task(2, 100, 60)]).unwrap();
        let w = window(1000, 21_600);
        let scenario = Scenario {
            tasks: &tasks,
            windows: std::slice::from_ref(&w),
            contract: None,
            preference: PacingPreference::Sustain {
                horizon_secs: 21_600,
                working_hours: None,
                continuity_reserve_bp: 0,
            },
        };
        let t0 = OffsetDateTime::UNIX_EPOCH;
        let events = vec![
            PacingEvent::ModeChanged { at: t0, seq: 0 },
            PacingEvent::TaskCompleted {
                at: t0 + time::Duration::seconds(60),
                seq: 1,
                task: SimTaskId(1),
            },
        ];
        let (state_a, trace_a) = simulate(&events, &policy(), &scenario);
        let (state_b, trace_b, exhausted) = simulate_until(&events, &policy(), &scenario, None);
        assert!(!exhausted);
        assert_eq!(format!("{state_a:?}"), format!("{state_b:?}"));
        assert_eq!(format!("{trace_a:?}"), format!("{trace_b:?}"));
    }

    /// `simulate_until` must never process an event or a synthesized
    /// timer tick whose instant is strictly later than `until` — task 2's
    /// spacing-gated start (t0+2160s, per `sustain_spacing_is_enforced_
    /// even_when_a_slot_frees_up_early` in `step.rs`) must not appear in
    /// the trace when `until` is set to a cutoff before it.
    #[test]
    fn simulate_until_never_processes_past_the_cutoff() {
        let tasks = TaskSet::validated(vec![task(1, 100, 60), task(2, 100, 60)]).unwrap();
        let w = window(1000, 21_600);
        let scenario = Scenario {
            tasks: &tasks,
            windows: std::slice::from_ref(&w),
            contract: None,
            preference: PacingPreference::Sustain {
                horizon_secs: 21_600,
                working_hours: None,
                continuity_reserve_bp: 0,
            },
        };
        let t0 = OffsetDateTime::UNIX_EPOCH;
        let events = vec![
            PacingEvent::ModeChanged { at: t0, seq: 0 },
            PacingEvent::TaskCompleted {
                at: t0 + time::Duration::seconds(60),
                seq: 1,
                task: SimTaskId(1),
            },
        ];
        let cutoff = t0 + time::Duration::seconds(300);
        let (state, trace, exhausted) = simulate_until(&events, &policy(), &scenario, Some(cutoff));
        assert!(!exhausted);
        for (tick, _) in &trace {
            let at = match tick {
                Tick::Event(e) => e.at(),
                Tick::Timer(t) => *t,
            };
            assert!(at <= cutoff, "tick at {at:?} exceeds cutoff {cutoff:?}");
        }
        assert!(
            state.pending_timer().map(|t| t > cutoff).unwrap_or(false)
                || state.pending_timer().is_none(),
            "task 2's spacing-gated start must still be pending past the cutoff, \
             never silently resolved"
        );
    }
}
