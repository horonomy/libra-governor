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
//! - **FOUND, then FIXED in this same commit (HORO-1781 fast-follow):**
//!   `pacing::step`'s window-headroom accounting used to be blind to a
//!   `Spend` event's actual amount for an *active* task. A task's
//!   contribution to every window's headroom was frozen at its `hold`
//!   (recorded at `Start` time) until it completed — a `Spend` event only
//!   ever adjusted that principal's *future* backoff ratio, never the
//!   window arithmetic other candidates were checked against.
//!   `window_overrun_via_blind_in_flight_overspend` below now proves the
//!   fix: `pending_for_window` injects `hold.value.max(spend_so_far)`, so
//!   at most one of the eight dependent tasks can ever be admitted once a
//!   real `Spend` report shows the first task overrunning. Both gaps were
//!   BURST-only in this form: SUSTAIN's global one-task concurrency slot
//!   (ADR-0016 decision #5) prevents a second task from ever being a
//!   candidate while the first is active, so this exact construction never
//!   reproduced under SUSTAIN (now additionally covered by dedicated
//!   SUSTAIN regression tests below, closing that previously-unverified
//!   gap).
//! - **FOUND, then FIXED in this same commit:** `OutstandingHold`'s own
//!   contract (`quota_window/mod.rs`, `from_reservation`'s docs: "every
//!   `Active` reservation counts, with no `expires_at` filter... this
//!   contract must not invent a second expiry rule") used to be violated
//!   by `split_pending_as_of`, which keyed an active task's hold
//!   settlement on its *estimated* `projected_complete_at` — never on an
//!   actual `TaskCompleted` event. `split_pending_as_of` and
//!   `PendingHold::completes_at` are now gone entirely: every pending
//!   hold counts as outstanding for as long as its task remains `active`,
//!   and only a real `TaskCompleted` event ever settles it.
//!   `window_overrun_via_duration_estimate_wrong_and_task_still_active`
//!   below now proves the fix: task B never starts while A remains
//!   active, and only starts at `A_completion + window_length` once a
//!   real completion is actually supplied.
//! - **CONFIRMED, independently reproduced (already disclosed by the
//!   implementer):** `PacingPreference::{Sustain::horizon_secs,
//!   Burst::target_end}` and `Policy::time.deadline` are never read by
//!   `forecast`/`step`/`simulate` — a scenario running arbitrarily far
//!   past a declared deadline is scheduled exactly as if no deadline
//!   existed. See `deadline_and_horizon_fields_are_never_consulted`.
//!   **Deliberately deferred to a separate ticket, not addressed by this
//!   fast-follow** — see "Known limitations" in the PR description.
//! - **CONFIRMED:** `EstimateRevised` is recorded but never applied — a
//!   revised-upward estimate that would make a task structurally
//!   infeasible is silently ignored and the task still starts at its
//!   stale (now-wrong) hold. See `estimate_revised_event_is_a_no_op`.
//!   **Deliberately deferred to a separate ticket.**
//! - **CONFIRMED:** `Proposal` has no variant that could mean cancel —
//!   verified by an exhaustive match with no wildcard arm, which would
//!   fail to compile if a third variant were ever added.
//! - **CONFIRMED:** SUSTAIN vs BURST under the same policy, same contract,
//!   with fanout/window shapes the implementer's own fixtures never used,
//!   produce genuinely different schedules (positive control).
//! - **CONFIRMED:** three independent runs of the same scenario produce
//!   byte-identical serialized output (via `serde_json`, not `Debug`) —
//!   unaffected by this fix (the digest's own scenario is capped by
//!   fanout before any forecast runs).

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
// 1. Window overrun via blind in-flight overspend — FOUND, then FIXED in
//    this same commit (HORO-1781 fast-follow)
// ---------------------------------------------------------------------

/// Task A (principal `p`) starts holding 100 tokens but a `Spend` event
/// later reports its *real* usage at 850 tokens, well before it
/// completes. Task Z (principal `z`, tiny, short-lived) exists only to
/// gate eight dependent tasks B1..B8 (principal `q`, 100 tokens each)
/// behind a `TaskCompleted` event, so the second admission pass runs with
/// A still active and already known (via `Spend`) to be overrunning.
///
/// Window: sliding, 6h, limit 1000. `pending_for_window` now injects
/// `hold.value.max(spend_so_far)` for an active task, so A's real 850 —
/// not its original 100-token hold — is what every other candidate is
/// checked against: 10 settled from Z + 850 pending from A + 100 = 960 <
/// 1000 admits exactly one B task; a second would push to 1060 > 1000 and
/// correctly refuses. Before the fix, `pending_for_window` always read
/// `ActiveTask::hold` (frozen at `Start`, 100, never the larger reported
/// `Spend`), so the simulator saw only 10 + 100 = 110 of committed usage
/// and admitted all eight — a genuine ~1.8x window overrun. This test now
/// proves that overrun is closed, not merely documents that it existed.
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
    let held_unavailable_b_count = trace
        .iter()
        .flat_map(|(_, proposals)| proposals.iter())
        .filter(|p| {
            matches!(
                p,
                Proposal::Hold {
                    next: libra_governor_domain::pacing::NextAdmit::Unavailable(
                        libra_governor_domain::pacing::UnavailableReason::NoProjectedRelief(_)
                    )
                }
            )
        })
        .count();

    assert_eq!(
        started_b_count, 1,
        "exactly one of the eight B tasks must be admitted — the fix makes `pending_for_window` \
         inject A's real reported spend (850), not its stale 100-token hold, so a second B task \
         would genuinely overrun the window and must be refused"
    );
    assert_eq!(
        held_unavailable_b_count, 7,
        "the remaining seven B tasks must end up refused as Unavailable(NoProjectedRelief) — \
         there is no pending hold whose real completion could ever relieve this window once A's \
         reported spend alone already consumes the window's headroom"
    );

    // Ground truth, computed by hand from the real `Spend` report (not
    // from re-calling `evaluate` or any of the module's own machinery):
    // Z settles at 10, A's *real* usage is 850 (not its 100-token hold).
    // With exactly one B task admitted: 10 + 850 + 100 = 960 <= 1000 —
    // genuinely safe, not an overrun.
    let real_consumption = 10u64 + 850 + 100 * started_b_count as u64;
    assert_eq!(
        real_consumption, 960,
        "real consumption with exactly one B task admitted must be 960, safely under the \
         window's 1000-token limit"
    );
    assert!(real_consumption <= 1000);

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
//    — FOUND, then FIXED in this same commit (HORO-1781 fast-follow)
// ---------------------------------------------------------------------

/// Task A holds 900 tokens (near the window's 1000-token limit) but its
/// duration estimate (60s) is badly wrong: no `TaskCompleted` event for A
/// is ever delivered in this scenario, modeling a task that is still
/// genuinely running well past its estimate. Task B (a different
/// principal) needs 900 tokens too and used to be only *temporarily*
/// blocked by A's pending hold, purely on an estimate-based schedule.
///
/// `split_pending_as_of` used to convert a pending hold to ordinary
/// "settled" usage the instant the probe time passed the hold's
/// *estimated* `projected_complete_at` — never gated on an actual
/// `TaskCompleted` event. That function and `PendingHold::completes_at`
/// are gone: a pending hold now counts as outstanding for as long as its
/// task remains `active`, with no second, estimate-based expiry rule. B
/// must therefore never start while A remains active — proven below —
/// and the companion test proves B starts at exactly the instant A's real
/// settled spend ages out of the window, once a real `TaskCompleted` is
/// actually supplied.
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
    // No `TaskCompleted` for A is ever supplied — the fix must never let
    // B start on an estimate-based schedule while A remains genuinely
    // active.
    let (final_state, trace) = simulate(&events, &policy(), &scenario);

    assert_eq!(
        final_state.active_count(),
        1,
        "only A is ever active — B must never be admitted while A remains active with no real \
         completion ever supplied"
    );
    assert!(
        final_state.completed().is_empty(),
        "no TaskCompleted event was ever supplied for either task"
    );

    let b_started = trace
        .iter()
        .flat_map(|(_, proposals)| proposals.iter())
        .any(|p| {
            matches!(
                p,
                Proposal::Start {
                    task: SimTaskId(2),
                    ..
                }
            )
        });
    assert!(
        !b_started,
        "B must never start while A remains active with no real completion — the fix removes \
         `split_pending_as_of`'s estimate-based settlement that used to let B start the instant \
         A's ESTIMATED (not real) completion aged out of the window"
    );

    let b_held_unavailable = trace
        .iter()
        .flat_map(|(_, proposals)| proposals.iter())
        .any(|p| {
            matches!(
                p,
                Proposal::Hold {
                    next: libra_governor_domain::pacing::NextAdmit::Unavailable(
                        libra_governor_domain::pacing::UnavailableReason::NoProjectedRelief(_)
                    )
                }
            )
        });
    assert!(
        b_held_unavailable,
        "B must be refused as Unavailable(NoProjectedRelief) — A's pending hold alone already \
         consumes the window's full headroom and, with no `completes_at` of its own any more, \
         carries no projected relief instant for the probe to report; only a real \
         `TaskCompleted` for A can ever unblock B now (see the companion test below)"
    );
}

/// Companion to `window_overrun_via_duration_estimate_wrong_and_task_still_active`:
/// same scenario, but A gets a *real* `TaskCompleted` at `t_complete`
/// (deliberately not `t0+60`, so this cannot be confused with the old
/// estimate-based instant the prior version of this test asserted). B
/// must start at exactly `t_complete + 21_600` — the instant A's real
/// settled spend (1,800 tokens, the larger of its reported `Spend` and
/// its 900-token hold) ages out of the 6h sliding window — and not one
/// instant before.
#[test]
fn window_overrun_fix_b_starts_exactly_when_a_real_completion_settles_and_ages_out() {
    let w = sliding_window(10, 1000, 21_600); // 6h
    let t0 = OffsetDateTime::UNIX_EPOCH;
    let t_complete = t0 + time::Duration::hours(1);

    let tasks = TaskSet::validated(vec![
        task(1, "p", vec![], 900, 60),
        task(2, "q", vec![], 900, 60),
    ])
    .unwrap();

    let scenario = Scenario {
        tasks: &tasks,
        windows: std::slice::from_ref(&w),
        contract: None,
        preference: PacingPreference::Burst {
            target_end: t0 + time::Duration::hours(12),
            max_fanout: 2,
        },
    };

    let events = vec![
        PacingEvent::ModeChanged { at: t0, seq: 0 },
        PacingEvent::Spend {
            at: t0 + time::Duration::seconds(10),
            seq: 1,
            task: SimTaskId(1),
            amount: QuotaAmount::new(QuotaUnit::Tokens, 1_800),
        },
        PacingEvent::TaskCompleted {
            at: t_complete,
            seq: 2,
            task: SimTaskId(1),
        },
    ];
    let (final_state, trace) = simulate(&events, &policy(), &scenario);

    let b_starts: Vec<OffsetDateTime> = trace
        .iter()
        .flat_map(|(_, proposals)| proposals.iter())
        .filter_map(|p| match p {
            Proposal::Start {
                task: SimTaskId(2),
                at,
                ..
            } => Some(*at),
            _ => None,
        })
        .collect();

    let expected = t_complete + time::Duration::seconds(21_600);
    assert_eq!(
        b_starts,
        vec![expected],
        "B must start exactly once, exactly when A's real settled spend (1,800 tokens, \
         recorded at A's actual completion instant, not its estimate) ages out of the 6h \
         sliding window — never before, and never on the old estimate-based instant"
    );

    assert_eq!(
        final_state.active_count(),
        1,
        "only B remains active once A has genuinely completed"
    );
    assert_eq!(
        final_state.completed().len(),
        1,
        "A is the only task that ever completes in this scenario"
    );
}

// ---------------------------------------------------------------------
// 2b. SUSTAIN regression coverage (new, HORO-1781 fast-follow): the two
//     bugs above were confirmed BURST-only because SUSTAIN's one-task
//     concurrency slot (`fanout_cap == 1`) already stops a second
//     candidate from ever reaching the forecast while the first task
//     remains active. These two tests close that "unverified for
//     SUSTAIN" gap by proving it holds under a real overrun, not just by
//     inspecting the `try_admit` loop's structure.
// ---------------------------------------------------------------------

/// Task A (principal `p`) overruns its hold via a real `Spend` report and
/// never completes. SUSTAIN's hard one-slot concurrency gate
/// (`working.active.len() >= fanout_cap` with `fanout_cap == 1`) must
/// refuse task B as a candidate at all — not merely refuse to admit it —
/// so the overrun never even reaches a window check.
#[test]
fn sustain_overspend_blocks_second_start_while_task_remains_active() {
    let w = sliding_window(11, 500, 21_600);
    let t0 = OffsetDateTime::UNIX_EPOCH;
    let tasks = TaskSet::validated(vec![
        task(1, "p", vec![], 100, 60),
        task(2, "p", vec![], 100, 60),
    ])
    .unwrap();
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
    let events = vec![
        PacingEvent::ModeChanged { at: t0, seq: 0 },
        // A real overrun report, well past the window's own 500-token
        // limit — but no `TaskCompleted` for task 1 is ever supplied.
        PacingEvent::Spend {
            at: t0 + time::Duration::seconds(10),
            seq: 1,
            task: SimTaskId(1),
            amount: QuotaAmount::new(QuotaUnit::Tokens, 600),
        },
    ];
    let (final_state, trace) = simulate(&events, &policy(), &scenario);

    let proposal_count: usize = trace.iter().map(|(_, proposals)| proposals.len()).sum();
    let start_count = trace
        .iter()
        .flat_map(|(_, proposals)| proposals.iter())
        .filter(|p| matches!(p, Proposal::Start { .. }))
        .count();
    assert_eq!(
        start_count, 1,
        "only task 1 ever starts — SUSTAIN's one-slot concurrency gate refuses a second \
         candidate while the first remains active, overrun or not"
    );
    assert_eq!(
        proposal_count, 1,
        "task 2 must never even generate a Hold proposal — the concurrency gate breaks the \
         admission loop before task 2 is ever considered a candidate"
    );
    assert_eq!(final_state.active_count(), 1);
    assert!(final_state.completed().is_empty());
}

/// Same scenario, but task 1 genuinely completes at `t_complete`. Task 2
/// must wait until task 1's real *settled* spend (600 tokens — larger
/// than its 100-token hold, per the same `pending_for_window`/settlement
/// fix as the BURST tests above) ages out of the 6h sliding window, not
/// one instant before — proving the fix's effect holds under SUSTAIN's
/// spacing/concurrency logic too, not just BURST's plain fanout.
#[test]
fn sustain_next_start_waits_for_overrun_settled_spend_to_age_out() {
    let w = sliding_window(12, 500, 21_600);
    let t0 = OffsetDateTime::UNIX_EPOCH;
    let t_complete = t0 + time::Duration::minutes(5);
    let tasks = TaskSet::validated(vec![
        task(1, "p", vec![], 100, 60),
        task(2, "p", vec![], 100, 60),
    ])
    .unwrap();
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
    let events = vec![
        PacingEvent::ModeChanged { at: t0, seq: 0 },
        PacingEvent::Spend {
            at: t0 + time::Duration::seconds(10),
            seq: 1,
            task: SimTaskId(1),
            amount: QuotaAmount::new(QuotaUnit::Tokens, 600),
        },
        PacingEvent::TaskCompleted {
            at: t_complete,
            seq: 2,
            task: SimTaskId(1),
        },
    ];
    let (final_state, trace) = simulate(&events, &policy(), &scenario);

    let task2_start = trace
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
        .expect(
            "task 2 must eventually start once task 1 completes and its settled spend ages out",
        );

    let expected = t_complete + time::Duration::seconds(21_600);
    assert_eq!(
        task2_start, expected,
        "task 2 must start exactly when task 1's real settled spend (600 tokens, recorded at \
         its actual completion instant) ages out of the 6h sliding window — never before"
    );
    assert_eq!(final_state.active_count(), 1);
    assert_eq!(final_state.completed().len(), 1);
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
