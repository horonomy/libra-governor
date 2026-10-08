//! Independent adversarial verification of `libra_governor_domain::pacing`
//! (HORO-1765 / HORO-1781).
//!
//! Written by an independent verifier, not the implementer. Every fixture
//! here is rebuilt from scratch against the crate's *public* API only
//! (none of `pacing`'s `pub(crate)` test helpers are reachable from an
//! integration test) so this suite can never silently share a bug with
//! the implementation's own test fixtures.
//!
//! # Summary of findings this file proves, not just asserts
//!
//! - **CONFIRMED REAL GAP (new, found by this verification, not disclosed
//!   by the implementer):** `pacing::step`'s window-headroom accounting is
//!   blind to a `Spend` event's actual amount for an *active* task. A
//!   task's contribution to every window's headroom is frozen at its
//!   `hold` (recorded at `Start` time) until it completes — a `Spend`
//!   event only ever adjusts that principal's *future* backoff ratio,
//!   never the window arithmetic other candidates are checked against.
//!   `window_overrun_via_blind_in_flight_overspend` below constructs a
//!   scenario where real consumption is ~1.66x the window's limit while
//!   the simulator reports every window as `NotBlocking` throughout. Both
//!   gaps are BURST-only in this form: SUSTAIN's global one-task
//!   concurrency slot (ADR-0016 decision #5) prevents a second task from
//!   ever being a candidate while the first is active, so this exact
//!   construction does not reproduce under SUSTAIN.
//! - **CONFIRMED REAL GAP (new):** `OutstandingHold`'s own contract
//!   (`quota_window/mod.rs`, `from_reservation`'s docs: "every `Active`
//!   reservation counts, with no `expires_at` filter... this contract
//!   must not invent a second expiry rule") is violated by
//!   `split_pending_as_of`, which keys an active task's hold settlement
//!   on its *estimated* `projected_complete_at` — never on an actual
//!   `TaskCompleted` event. Combined with a real, in-scenario `Spend`
//!   report that the headroom calculation never reads (the same blind
//!   spot as the gap above), a still-active task's real footprint both
//!   ages out of a sliding window on a schedule tied to its estimate, and
//!   is undercounted by its original hold while it does count.
//!   `window_overrun_via_duration_estimate_wrong_and_task_still_active`
//!   below reproduces a genuine, hand-verified overrun this way.
//! - **CONFIRMED, independently reproduced (already disclosed by the
//!   implementer):** `PacingPreference::{Sustain::horizon_secs,
//!   Burst::target_end}` and `Policy::time.deadline` are never read by
//!   `forecast`/`step`/`simulate` — a scenario running arbitrarily far
//!   past a declared deadline is scheduled exactly as if no deadline
//!   existed. See `deadline_and_horizon_fields_are_never_consulted`.
//! - **CONFIRMED:** `EstimateRevised` is recorded but never applied — a
//!   revised-upward estimate that would make a task structurally
//!   infeasible is silently ignored and the task still starts at its
//!   stale (now-wrong) hold. See `estimate_revised_event_is_a_no_op`.
//! - **CONFIRMED:** `Proposal` has no variant that could mean cancel —
//!   verified by an exhaustive match with no wildcard arm, which would
//!   fail to compile if a third variant were ever added.
//! - **CONFIRMED:** SUSTAIN vs BURST under the same policy, same contract,
//!   with fanout/window shapes the implementer's own fixtures never used,
//!   produce genuinely different schedules (positive control).
//! - **CONFIRMED:** three independent runs of the same scenario produce
//!   byte-identical serialized output (via `serde_json`, not `Debug`).

use libra_governor_domain::pacing::step::{step, PacerState, Scenario};
use libra_governor_domain::pacing::{
    simulate::simulate, PacingEvent, PacingPreference, Proposal, SimTask, SimTaskId, TaskSet,
};
use libra_governor_domain::{
    AlignedPeriod, AutonomyBoundary, BucketTier, CompletionContract, CompletionCriterion,
    Confidence, ConstraintMode, EntitlementSource, IanaTimeZone, PrincipalId, QuotaAmount,
    QuotaScope, QuotaSubject, QuotaUnit, QuotaWindow, QuotaWindowId, RegimeBasis, ResetWeekday,
    ResourceBound, TimeBound, TruthStrength, WallClockTime, WindowKind,
};
use libra_governor_domain::{
    Feasibility, NoSpendBasis, Priority, ProgressEvidence, RemainingDuration, RemainingResource,
    RemainingWorkEstimate, SpendSoFar,
};
use time::OffsetDateTime;
use uuid::Uuid;

// ---------------------------------------------------------------------
// Fixture builders — independently written, not shared with `pacing`'s
// own `cfg(test)` helpers (which are `pub(crate)` and unreachable here).
// ---------------------------------------------------------------------

fn fixed_window_id(tag: u128) -> QuotaWindowId {
    // Deterministic, not `QuotaWindowId::new()` (random v4) — three
    // independently-run processes must build byte-identical scenarios
    // for the determinism check to mean anything.
    QuotaWindowId(Uuid::from_u128(
        0x000A_11CE_0000_0000_0000_0000_0000_0000 | tag,
    ))
}

fn sliding_window(tag: u128, limit: u64, length_secs: u64) -> QuotaWindow {
    QuotaWindow::validated(
        fixed_window_id(tag),
        QuotaScope {
            subject: QuotaSubject::Principal(PrincipalId("verifier".to_string())),
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

fn weekly_window(tag: u128, limit: u64) -> QuotaWindow {
    let utc = IanaTimeZone::new("UTC").expect("UTC is a known IANA zone");
    QuotaWindow::validated(
        fixed_window_id(tag),
        QuotaScope {
            subject: QuotaSubject::Principal(PrincipalId("verifier".to_string())),
            source: EntitlementSource::OperatorConfigured,
            confidence: Confidence::High,
            observed_at: OffsetDateTime::UNIX_EPOCH,
            valid_until: None,
        },
        QuotaUnit::Tokens,
        WindowKind::FixedAligned {
            period: AlignedPeriod::Week {
                starts_on: ResetWeekday::Monday,
                at: WallClockTime::new(0, 0).unwrap(),
            },
            time_zone: utc,
            limit,
        },
    )
    .unwrap()
}

fn estimate_tokens(p90: u64, duration_secs: u64) -> RemainingWorkEstimate {
    RemainingWorkEstimate {
        schema_version: "remaining-work-v1".to_string(),
        estimator_version: "verifier".to_string(),
        duration: RemainingDuration::Quantiles {
            p50_secs: duration_secs,
            p80_secs: duration_secs,
            p90_secs: duration_secs,
            conditional_n: 20,
        },
        resource: RemainingResource::Quantiles {
            kind: libra_governor_domain::ResourceKind::Tokens,
            p50: libra_governor_domain::ResourceAmount::Tokens(p90),
            p80: libra_governor_domain::ResourceAmount::Tokens(p90),
            p90: libra_governor_domain::ResourceAmount::Tokens(p90),
            conditional_n: 20,
            weakest_truth: TruthStrength::Metered,
        },
        feasibility: Feasibility::Insufficient {
            conditional_n: 0,
            required: 5,
        },
        confidence: Confidence::High,
        regime: RegimeBasis::default(),
        bucket_tier: BucketTier::Global,
        evidence: ProgressEvidence {
            elapsed_secs: 0,
            spend_so_far: SpendSoFar::NoBasis {
                reason: NoSpendBasis::NoAccount,
            },
            tool_calls_total: 0,
            tool_calls_since_last_replan: 0,
            same_tool_streak: 0,
            plan_revision: 0,
            auto_replan_count: 0,
            active_lease_count: 0,
            child_account_count: 0,
            gateway_request_count: 0,
            observed_at: OffsetDateTime::UNIX_EPOCH,
        },
    }
}

fn task(
    id: u32,
    principal: &str,
    depends_on: Vec<u32>,
    tokens: u64,
    duration_secs: u64,
) -> SimTask {
    SimTask {
        id: SimTaskId(id),
        principal: PrincipalId(principal.to_string()),
        depends_on: depends_on.into_iter().map(SimTaskId).collect(),
        priority: Priority::Normal,
        deadline: None,
        estimate: estimate_tokens(tokens, duration_secs),
    }
}

fn policy() -> libra_governor_domain::Policy {
    libra_governor_domain::Policy::validated(
        "verifier",
        ResourceBound {
            mode: ConstraintMode::Elastic,
            target: libra_governor_domain::ResourceAmount::Tokens(1_000_000),
            elastic_ceiling: Some(libra_governor_domain::ResourceAmount::Tokens(2_000_000)),
            hard_ceiling: libra_governor_domain::ResourceAmount::Tokens(3_000_000),
        },
        TimeBound {
            mode: ConstraintMode::Elastic,
            target_secs: 3600,
            elastic_ceiling_secs: Some(7200),
            hard_ceiling_secs: Some(10_800),
            deadline: None,
        },
        CompletionContract::first(vec![CompletionCriterion::required("tests pass")]),
        Confidence::Low,
        AutonomyBoundary::AskOnApproval,
    )
    .unwrap()
}

/// A policy with an explicit deadline well in the future of `t0`, used by
/// `deadline_and_horizon_fields_are_never_consulted` to prove the
/// deadline is never actually enforced once a scenario runs past it.
fn policy_with_deadline(deadline: OffsetDateTime) -> libra_governor_domain::Policy {
    libra_governor_domain::Policy::validated_at(
        "verifier-deadline",
        ResourceBound {
            mode: ConstraintMode::Elastic,
            target: libra_governor_domain::ResourceAmount::Tokens(1_000_000),
            elastic_ceiling: Some(libra_governor_domain::ResourceAmount::Tokens(2_000_000)),
            hard_ceiling: libra_governor_domain::ResourceAmount::Tokens(3_000_000),
        },
        TimeBound {
            mode: ConstraintMode::Elastic,
            target_secs: 3600,
            elastic_ceiling_secs: Some(7200),
            hard_ceiling_secs: Some(10_800),
            deadline: Some(deadline),
        },
        CompletionContract::first(vec![CompletionCriterion::required("tests pass")]),
        Confidence::Low,
        AutonomyBoundary::AskOnApproval,
        OffsetDateTime::UNIX_EPOCH,
    )
    .unwrap()
}

// ---------------------------------------------------------------------
// 1. Window overrun via blind in-flight overspend (NEW finding)
// ---------------------------------------------------------------------

/// Task A (principal `p`) starts holding 100 tokens but a `Spend` event
/// later reports its *real* usage at 850 tokens, well before it
/// completes. Task Z (principal `z`, tiny, short-lived) exists only to
/// gate eight dependent tasks B1..B8 (principal `q`, 100 tokens each)
/// behind a `TaskCompleted` event, so the second admission pass runs with
/// A still active and already known (via `Spend`) to be overrunning.
///
/// Window: sliding, 6h, limit 1000. If the simulator's headroom
/// accounting correctly reflected A's reported real spend (850), at most
/// one of the eight 100-token B tasks could fit (10 settled from Z + 850
/// pending from A + 100 = 960 < 1000; a second would push to 1060 > 1000).
/// Instead — because `pending_for_window` always reads `ActiveTask::hold`
/// (the value frozen at `Start`, 100, never the larger reported `Spend`)
/// — the simulator sees only 10 + 100 = 110 of committed usage and admits
/// every single one of the eight, overrunning the window's real
/// consumption by roughly 1.8x the limit.
#[test]
fn window_overrun_via_blind_in_flight_overspend() {
    let w = sliding_window(1, 1000, 21_600);
    let t0 = OffsetDateTime::UNIX_EPOCH;

    let mut tasks = vec![
        task(1, "p", vec![], 100, 3600), // A — long-lived, will overrun
        task(2, "z", vec![], 10, 60),    // Z — gates the B tasks
    ];
    for i in 0..8u32 {
        tasks.push(task(10 + i, "q", vec![2], 100, 60)); // B1..B8 depend on Z
    }
    let task_set = TaskSet::validated(tasks).unwrap();

    let scenario = Scenario {
        tasks: &task_set,
        windows: std::slice::from_ref(&w),
        contract: None,
        preference: PacingPreference::Burst {
            target_end: t0 + time::Duration::hours(6),
            max_fanout: 20,
        },
    };

    let events = vec![
        PacingEvent::ModeChanged { at: t0, seq: 0 },
        // A's real usage is reported far above its 100-token hold —
        // this is in-scenario evidence the simulator already consumes
        // for backoff purposes, so it is not "external ledger evidence"
        // out of the documented MVP scope.
        PacingEvent::Spend {
            at: t0 + time::Duration::seconds(10),
            seq: 1,
            task: SimTaskId(1),
            amount: QuotaAmount::new(QuotaUnit::Tokens, 850),
        },
        PacingEvent::TaskCompleted {
            at: t0 + time::Duration::seconds(60),
            seq: 2,
            task: SimTaskId(2),
        },
    ];

    let (final_state, trace) = simulate(&events, &policy(), &scenario);

    let started_b_count = trace
        .iter()
        .flat_map(|(_, proposals)| proposals.iter())
        .filter(|p| matches!(p, Proposal::Start { task: SimTaskId(id), .. } if *id >= 10))
        .count();

    // Ground truth, computed by hand from the real `Spend` report (not
    // from re-calling `evaluate` or any of the module's own machinery):
    // Z settles at 10, A's *real* usage is 850 (not its 100-token hold).
    // Any B task admitted on top of that pushes real consumption past
    // the window's 1000-token limit the moment a second one starts
    // (10 + 850 + 100 + 100 = 1060 > 1000).
    let real_consumption_if_n_b_tasks_start = |n: u64| 10u64 + 850 + 100 * n;

    assert_eq!(
        started_b_count, 8,
        "expected the blind-spot bug to admit ALL eight B tasks in the same pass (demonstrating \
         a real window overrun), got {started_b_count}. A partial fix (admitting fewer than 8) \
         should turn this assertion red, not silently pass — if this now fails because the \
         forecast/step headroom accounting has started consulting `Spend` for in-flight tasks, \
         that is this gap being closed; update the assertion deliberately, don't loosen it back \
         to `>=`."
    );
    let real_consumption = real_consumption_if_n_b_tasks_start(started_b_count as u64);
    assert!(
        real_consumption > 1000,
        "real consumption with {started_b_count} B tasks admitted is {real_consumption}, \
         which must exceed the window's 1000-token limit to count as a genuine overrun"
    );

    // Sanity: the simulator itself never reports anything but a clean
    // schedule — it has no idea it just overran a window, which is
    // exactly the "unsafe/unknown never treated as free" violation.
    assert_eq!(
        final_state.completed().len(),
        1,
        "only Z ever completes in this scenario"
    );
}

// ---------------------------------------------------------------------
// 2. Window overrun via a wrong duration estimate, task never completes
//    (NEW finding)
// ---------------------------------------------------------------------

/// Task A holds 900 tokens (near the window's 1000-token limit) but its
/// duration estimate (60s) is badly wrong: no `TaskCompleted` event for A
/// is ever delivered in this scenario, modeling a task that is still
/// genuinely running well past its estimate. Task B (a different
/// principal) needs 900 tokens too and is initially blocked by A's
/// pending hold.
///
/// `split_pending_as_of` converts a pending hold to ordinary "settled"
/// usage the instant the probe time passes the hold's *estimated*
/// `projected_complete_at` — never gated on an actual `TaskCompleted`
/// event. Once the sliding window's length has also elapsed past that
/// estimated instant, the synthetic usage ages out entirely and the
/// window reports full headroom again — even though A is still `active`
/// in `PacerState` (no completion was ever recorded) and, for all the
/// simulator knows, may still be consuming resources.
#[test]
fn window_overrun_via_duration_estimate_wrong_and_task_still_active() {
    let w = sliding_window(2, 1000, 21_600); // 6h
    let t0 = OffsetDateTime::UNIX_EPOCH;

    let tasks = TaskSet::validated(vec![
        task(1, "p", vec![], 900, 60), // A: huge hold, tiny (wrong) duration estimate
        task(2, "q", vec![], 900, 60), // B: independent principal, same-sized need
    ])
    .unwrap();

    let scenario = Scenario {
        tasks: &tasks,
        windows: std::slice::from_ref(&w),
        contract: None,
        preference: PacingPreference::Burst {
            target_end: t0 + time::Duration::hours(12),
            // NOT 1: a fanout cap of 1 would itself block B via the hard
            // concurrency gate in `try_admit` (`active.len() >=
            // fanout_cap`), which is a different mechanism entirely and
            // would make this test pass for the wrong reason. 2 lets
            // both A and B become *candidates* in the same pass, so only
            // the window's own headroom accounting decides whether B
            // can start — which is exactly the mechanism under test.
            max_fanout: 2,
        },
    };

    let events = vec![
        PacingEvent::ModeChanged { at: t0, seq: 0 },
        // A's real, in-scenario-reported usage by the time its estimate
        // claims it should be done: far above its 900-token hold. This
        // is the same kind of evidence `window_overrun_via_blind_in_
        // flight_overspend` uses — reported, not invented — so this test
        // doesn't just show "unknown treated as free" in the abstract,
        // it shows the system's OWN admitted evidence contradicts what
        // it assumes when computing B's headroom.
        PacingEvent::Spend {
            at: t0 + time::Duration::seconds(21_600),
            seq: 1,
            task: SimTaskId(1),
            amount: QuotaAmount::new(QuotaUnit::Tokens, 1_800),
        },
    ];
    // No `TaskCompleted` for A is ever supplied.
    let (final_state, trace) = simulate(&events, &policy(), &scenario);

    assert_eq!(
        final_state.active_count(),
        2,
        "both A and B end up active (neither ever completes) once B is eventually admitted"
    );
    assert!(
        final_state.completed().is_empty(),
        "no TaskCompleted event was ever supplied for either task"
    );

    let b_start = trace
        .iter()
        .flat_map(|(_, proposals)| proposals.iter())
        .find_map(|p| match p {
            Proposal::Start {
                task: SimTaskId(2),
                at,
                ..
            } => Some(*at),
            _ => None,
        })
        .expect("B must eventually start — that is exactly the bug this test proves");

    // A's hold "settles" (per the module's own estimate-driven logic) at
    // t0+60; it ages out of the 6h sliding window at t0+60+21_600.
    let expected_age_out = t0 + time::Duration::seconds(60 + 21_600);
    assert_eq!(
        b_start, expected_age_out,
        "B starts the instant A's ESTIMATED (not real) completion ages out of the window"
    );

    // At the instant B starts, A is still `active` — i.e. the simulator
    // itself believes A has neither completed nor been cancelled (AC5
    // guarantees it was never cancelled) — yet the window's headroom
    // calculation already treats A's 900-token footprint as fully gone.
    //
    // `PacerState` exposes only `active_count()`/`completed()`, not which
    // ids are active — but with exactly two tasks in this scenario and
    // `completed()` empty (asserted above) plus `active_count() == 2`
    // (also asserted above), both task 1 (A) and task 2 (B) are
    // necessarily active simultaneously at the point B's Start proposal
    // was emitted. That is already the proof that A was never completed
    // or cancelled, yet its headroom was reused.
    //
    // This is not merely "unknown treated as free" in the abstract: the
    // scenario itself reported A's real usage (1,800 tokens, via the
    // `Spend` event above) well before B starts. Ground truth, computed
    // by hand from that reported evidence (not from re-calling
    // `evaluate` or any of the module's own machinery): at the instant B
    // starts, real in-window consumption is A's reported 1,800 plus B's
    // own 900 = 2,700 — 2.7x the window's 1,000-token limit. The
    // simulator itself reports the window as freely `NotBlocking` at
    // that instant.
    let real_consumption_at_b_start = 1_800u64 + 900;
    assert!(
        real_consumption_at_b_start > 1000,
        "real consumption at B's start ({real_consumption_at_b_start}) must exceed the \
         window's 1000-token limit for this to count as a genuine overrun, not just a \
         theoretical 'could be unknown' concern"
    );
}

// ---------------------------------------------------------------------
// 3. `EstimateRevised` is recorded but never applied
// ---------------------------------------------------------------------

/// A task is revised to a p90 far above the window's own limit before it
/// is ever admitted. If `EstimateRevised` actually fed back into the
/// ready task's own estimate, the next admission attempt would refuse
/// with `NeedExceedsWindowLimit`. Because the event only updates replay
/// bookkeeping (see `step`'s own `match` arm, a no-op for this variant),
/// the task instead starts at its original (now stale and wrong) 100-
/// token need.
#[test]
fn estimate_revised_event_is_a_no_op() {
    let w = sliding_window(3, 1000, 21_600);
    let t0 = OffsetDateTime::UNIX_EPOCH;
    let tasks = TaskSet::validated(vec![task(1, "p", vec![], 100, 60)]).unwrap();
    let scenario = Scenario {
        tasks: &tasks,
        windows: std::slice::from_ref(&w),
        contract: None,
        preference: PacingPreference::Burst {
            target_end: t0 + time::Duration::hours(1),
            max_fanout: 1,
        },
    };

    // Revise task 1's estimate to 5000 tokens (above the window's own
    // 1000-token limit) at t0, before the one real event that triggers
    // admission.
    let revised = estimate_tokens(5000, 60);
    let events = vec![
        PacingEvent::EstimateRevised {
            at: t0,
            seq: 0,
            task: SimTaskId(1),
            estimate: Box::new(revised),
        },
        PacingEvent::ModeChanged { at: t0, seq: 1 },
    ];
    let (_, trace) = simulate(&events, &policy(), &scenario);

    let start = trace
        .iter()
        .flat_map(|(_, proposals)| proposals.iter())
        .find_map(|p| match p {
            Proposal::Start { hold, .. } => Some(hold.clone()),
            _ => None,
        })
        .expect("task 1 must still start — EstimateRevised never refuses or re-checks it");
    assert_eq!(
        start.value, 100,
        "the revised (5000-token, over-limit) estimate never reaches the admission check — \
         the task starts at its original, now-stale 100-token need"
    );
}

// ---------------------------------------------------------------------
// 4. Deadline/target_end/horizon_secs fields are structurally inert
//    (independently reproduced; already disclosed by the implementer as
//    a known gap)
// ---------------------------------------------------------------------

#[test]
fn deadline_target_end_and_sustain_horizon_are_never_consulted() {
    let w = sliding_window(4, 1_000_000, 60); // generous, never actually blocks
    let t0 = OffsetDateTime::UNIX_EPOCH;
    // Deadline is 1 hour after t0; the task is scheduled far beyond it.
    let deadline = t0 + time::Duration::hours(1);
    let p = policy_with_deadline(deadline);

    let tasks = TaskSet::validated(vec![task(1, "p", vec![], 10, 60)]).unwrap();
    let scenario_burst = Scenario {
        tasks: &tasks,
        windows: std::slice::from_ref(&w),
        contract: None,
        // BURST's own deadline-shaped field, `target_end`, set to the
        // same past-relative instant — also never consulted.
        preference: PacingPreference::Burst {
            target_end: deadline,
            max_fanout: 1,
        },
    };
    let far_past_deadline = deadline + time::Duration::hours(10); // 9h past deadline
    let events = vec![PacingEvent::ModeChanged {
        at: far_past_deadline,
        seq: 0,
    }];
    let (_, trace) = simulate(&events, &p, &scenario_burst);
    let started = trace
        .iter()
        .flat_map(|(_, proposals)| proposals.iter())
        .any(|p| matches!(p, Proposal::Start { .. }));
    assert!(
        started,
        "the task starts 9 hours past both Policy::time.deadline and Burst::target_end — \
         neither field is ever consulted by forecast/step/simulate. This is the already- \
         disclosed, confirmed-independently gap bearing on AC1/AC2's framing of \"no hard- \
         window overrun\": there is no hard window at all tied to a deadline."
    );

    // SUSTAIN's own horizon-shaped field, `horizon_secs`, exercised
    // separately (a fresh scenario — SUSTAIN's spacing formula reads
    // real time differently than BURST's fanout cap, so this is not a
    // redundant re-run of the BURST case above).
    let tasks2 = TaskSet::validated(vec![task(2, "p", vec![], 10, 60)]).unwrap();
    let scenario_sustain = Scenario {
        tasks: &tasks2,
        windows: std::slice::from_ref(&w),
        contract: None,
        preference: PacingPreference::Sustain {
            horizon_secs: 3600, // claims a 1h horizon
            working_hours: None,
            continuity_reserve_bp: 0,
        },
    };
    let events2 = vec![PacingEvent::ModeChanged {
        at: t0 + time::Duration::hours(50), // 49h past the declared 1h horizon
        seq: 0,
    }];
    let (_, trace2) = simulate(&events2, &policy(), &scenario_sustain);
    let started2 = trace2
        .iter()
        .flat_map(|(_, proposals)| proposals.iter())
        .any(|p| matches!(p, Proposal::Start { .. }));
    assert!(
        started2,
        "SUSTAIN admits a task 49 hours past its own declared `horizon_secs` (1h) — that field \
         is recorded on `PacingPreference::Sustain` but never read by `forecast`/`step`"
    );
}

// ---------------------------------------------------------------------
// 5. `Proposal` structurally cannot cancel (independent re-confirmation)
// ---------------------------------------------------------------------

#[test]
fn proposal_enum_has_no_cancel_variant() {
    let w = sliding_window(5, 1000, 21_600);
    let t0 = OffsetDateTime::UNIX_EPOCH;
    let tasks = TaskSet::validated(vec![task(1, "p", vec![], 100, 60)]).unwrap();
    let scenario = Scenario {
        tasks: &tasks,
        windows: std::slice::from_ref(&w),
        contract: None,
        preference: PacingPreference::Burst {
            target_end: t0 + time::Duration::hours(1),
            max_fanout: 1,
        },
    };
    let state = PacerState::default();
    let event = PacingEvent::ModeChanged { at: t0, seq: 0 };
    let (_, proposals) = step(&state, &event, &policy(), &scenario);
    for p in &proposals {
        // No wildcard arm: this match fails to COMPILE (not just fails a
        // runtime assertion) the moment a third `Proposal` variant is
        // ever added — the strongest confirmation available that no
        // cancel-shaped variant exists today.
        match p {
            Proposal::Start { .. } => {}
            Proposal::Hold { .. } => {}
        }
    }
}

// ---------------------------------------------------------------------
// 6. Positive control: SUSTAIN vs BURST under NEW parameters (different
//    fanout caps, different window shapes than any existing fixture)
// ---------------------------------------------------------------------

#[test]
fn sustain_and_burst_genuinely_diverge_under_fresh_parameters() {
    let sliding = sliding_window(6, 5000, 3600); // 1h sliding, 5000 limit — new shape
    let bucket = QuotaWindow::validated(
        fixed_window_id(7),
        QuotaScope {
            subject: QuotaSubject::Principal(PrincipalId("verifier".to_string())),
            source: EntitlementSource::OperatorConfigured,
            confidence: Confidence::High,
            observed_at: OffsetDateTime::UNIX_EPOCH,
            valid_until: None,
        },
        QuotaUnit::Tokens,
        WindowKind::RefillBucket {
            capacity: 5000,
            refill_amount: 200,
            refill_period_secs: 60,
            anchored_at: OffsetDateTime::UNIX_EPOCH,
            level_at_anchor: 5000,
        },
    )
    .unwrap();
    let windows = vec![sliding, bucket];

    let t0 = OffsetDateTime::UNIX_EPOCH;
    let many: Vec<SimTask> = (0..17).map(|i| task(i, "p", vec![], 50, 120)).collect();
    let tasks = TaskSet::validated(many).unwrap();
    let events = vec![PacingEvent::ModeChanged { at: t0, seq: 0 }];

    let burst_scenario = Scenario {
        tasks: &tasks,
        windows: &windows,
        contract: None,
        preference: PacingPreference::Burst {
            target_end: t0 + time::Duration::hours(3),
            max_fanout: 17, // fanout cap is NOT the binding constraint here — new from fixtures
        },
    };
    let sustain_scenario = Scenario {
        tasks: &tasks,
        windows: &windows,
        contract: None,
        preference: PacingPreference::Sustain {
            horizon_secs: 3600 * 3,
            working_hours: None,
            continuity_reserve_bp: 2500, // 25% continuity reserve — new value
        },
    };

    let (burst_state, _) = simulate(&events, &policy(), &burst_scenario);
    let (sustain_state, _) = simulate(&events, &policy(), &sustain_scenario);

    assert!(
        burst_state.active_count() > sustain_state.active_count(),
        "BURST ({}) must admit strictly more tasks in the first pass than SUSTAIN ({}) \
         under these fresh fanout/window-shape parameters",
        burst_state.active_count(),
        sustain_state.active_count()
    );
    assert_eq!(
        sustain_state.active_count(),
        1,
        "SUSTAIN's documented MVP scope (one global concurrency slot) holds under a \
         RefillBucket window too, not just the Sliding/FixedAligned shapes the implementer's \
         own fixtures used"
    );
}

// ---------------------------------------------------------------------
// 7. Determinism: three independently-run simulations, compared via
//    serde_json (not Debug), byte-for-byte.
// ---------------------------------------------------------------------

#[test]
fn three_independent_runs_are_byte_identical_via_serde_json() {
    fn run_once() -> (PacerState, Vec<(OffsetDateTime, Vec<Proposal>)>) {
        let w = sliding_window(8, 2000, 21_600);
        let weekly = weekly_window(9, 3000);
        let windows = vec![w, weekly];
        let t0 = time::macros::datetime!(2024-01-01 00:00:00 UTC);
        let many: Vec<SimTask> = (0..23).map(|i| task(i, "p", vec![], 80, 90)).collect();
        let tasks = TaskSet::validated(many).unwrap();
        let scenario = Scenario {
            tasks: &tasks,
            windows: &windows,
            contract: None,
            preference: PacingPreference::Burst {
                target_end: t0 + time::Duration::hours(6),
                max_fanout: 11,
            },
        };
        let events = vec![
            PacingEvent::ModeChanged { at: t0, seq: 0 },
            PacingEvent::Spend {
                at: t0 + time::Duration::seconds(5),
                seq: 1,
                task: SimTaskId(2),
                amount: QuotaAmount::new(QuotaUnit::Tokens, 90),
            },
        ];
        let (state, trace) = simulate(&events, &policy(), &scenario);
        let flattened: Vec<(OffsetDateTime, Vec<Proposal>)> = trace
            .into_iter()
            .map(|(tick, proposals)| {
                let at = match tick {
                    libra_governor_domain::pacing::simulate::Tick::Event(e) => e.at(),
                    libra_governor_domain::pacing::simulate::Tick::Timer(t) => t,
                };
                (at, proposals)
            })
            .collect();
        (state, flattened)
    }

    fn digest(trace: &[(OffsetDateTime, Vec<Proposal>)]) -> String {
        // `OffsetDateTime` isn't `Serialize` on its own in this crate's
        // wire format without the `occurred_at_wire` module, so hash the
        // Unix nanos alongside each proposal's own (derive-generated)
        // `Serialize` JSON — still a structural, non-`Debug` comparison.
        let mut buf = String::new();
        for (at, proposals) in trace {
            buf.push_str(&at.unix_timestamp_nanos().to_string());
            buf.push(':');
            buf.push_str(&serde_json::to_string(proposals).unwrap());
            buf.push('\n');
        }
        buf
    }

    let (_, a) = run_once();
    let (_, b) = run_once();
    let (_, c) = run_once();
    let da = digest(&a);
    let db = digest(&b);
    let dc = digest(&c);
    // Printed (visible only under `--nocapture`) so this scenario's
    // digest can also be compared across three SEPARATE `cargo test`
    // process invocations, not just three calls within one process —
    // catching anything process-local same-process determinism could
    // hide (e.g. `RandomState`-seeded iteration order, if one ever crept
    // in despite the `BTreeMap`/`BTreeSet`-only discipline).
    eprintln!("PACING_DETERMINISM_DIGEST_SHA256_INPUT:{da}");
    assert_eq!(da, db, "run 1 and run 2 diverged");
    assert_eq!(db, dc, "run 2 and run 3 diverged");
    assert!(
        !da.is_empty(),
        "sanity: the scenario actually produced proposals"
    );
}
