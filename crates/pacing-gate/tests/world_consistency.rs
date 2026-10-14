//! The world model's own internal-accounting invariants — never a check
//! of strategy *correctness* (that's PR-B's verifier). Runs every
//! fixture against both strategies and asserts `World::assert_consistent`
//! holds at the end, plus a couple of cross-checked arithmetic
//! invariants on the final world state.

use libra_governor_pacing_gate::drive::run_trace;
use libra_governor_pacing_gate::fixtures;
use libra_governor_pacing_gate::strategy::{Paced, StaticBudget};

#[test]
fn world_is_internally_consistent_after_every_fixture_with_static_budget() {
    for (name, _) in fixtures::ALL {
        let spec = fixtures::load(name).unwrap();
        let outcome = run_trace(&spec, Box::new(StaticBudget::new(3)))
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        outcome
            .world
            .assert_consistent()
            .unwrap_or_else(|problems| panic!("{name}: world inconsistency: {problems:?}"));
        assert_eq!(
            outcome.world.active_count(),
            0,
            "{name}: every started task must also complete by the end of the trace"
        );
    }
}

#[test]
fn world_is_internally_consistent_after_every_fixture_with_paced() {
    for (name, _) in fixtures::ALL {
        let spec = fixtures::load(name).unwrap();
        let strategy = Paced::new(&spec.tasks, &spec.windows, &spec.initial_preference);
        let outcome =
            run_trace(&spec, Box::new(strategy)).unwrap_or_else(|e| panic!("{name}: {e}"));
        outcome
            .world
            .assert_consistent()
            .unwrap_or_else(|problems| panic!("{name}: world inconsistency: {problems:?}"));
    }
}

/// Regression for the `Strategy::next_wake` fix (drive.rs): before it,
/// `Paced` never scheduled a poll at a `Hold`'s own wake instant, so a
/// quota-only hold with no other fixture event to re-trigger `decide`
/// stranded ready work forever. t1 has no exogenous events at all, so
/// without `next_wake` task 4 (the one held until the hourly window's
/// 3600s relief) would never have gotten a second look.
#[test]
fn paced_completes_every_task_on_t1_multi_window() {
    let spec = fixtures::load("t1_multi_window").unwrap();
    let strategy = Paced::new(&spec.tasks, &spec.windows, &spec.initial_preference);
    let outcome = run_trace(&spec, Box::new(strategy)).unwrap();
    for task in &spec.tasks {
        assert!(
            outcome.world.is_completed(task.id),
            "task {} never completed under Paced on t1_multi_window",
            task.id
        );
    }
}

/// Regression for the `Strategy::drain_pending_starts` fix (strategy.rs):
/// before it, `Paced::notify`'s `TaskCompleted`/`Spend`/
/// `PreferenceChanged` handling discarded every `Proposal::Start`
/// `pacing::step` returned, leaving `PacerState` believing a task had
/// started while the real `World` was never told — permanently
/// stranding it (it could never again look "ready" to the strategy's own
/// admission pass). t4's task 2 only ever becomes ready as a direct
/// result of task 1's `TaskCompleted` notify, with no later poll to
/// paper over the gap.
#[test]
fn paced_completes_every_task_on_t4_delayed_settlement() {
    let spec = fixtures::load("t4_delayed_settlement").unwrap();
    let strategy = Paced::new(&spec.tasks, &spec.windows, &spec.initial_preference);
    let outcome = run_trace(&spec, Box::new(strategy)).unwrap();
    for task in &spec.tasks {
        assert!(
            outcome.world.is_completed(task.id),
            "task {} never completed under Paced on t4_delayed_settlement",
            task.id
        );
    }
}

/// Documents (does not fix) a THIRD, distinct desync this session found
/// and decided not to chase down, per the "never modifies pacing/*"
/// invariant `lib.rs` already states for this crate: `Paced::on_restart`
/// resets to `PacerState::default()`, which wipes `completed` along with
/// the genuinely-ephemeral fields (`active`/`backoff_x100`/
/// `pending_timer`) — but `completed` is a durable ground-truth fact a
/// real daemon would reload from its ledger, not in-memory scheduling
/// state a crash would actually lose. After t5's restart, `Paced`
/// believes task 1 (already real-world-complete) is still eligible and
/// re-proposes starting it; the driver correctly refuses (logged as
/// `DriveLogEntry::Offered { rejected: [1], .. }`), but that refusal
/// consumes the strategy's one Sustain-mode fanout slot forever — task 2
/// is never reconsidered again. Fixing this correctly needs a
/// `domain::pacing::step::PacerState` constructor/method this crate
/// cannot add (`domain/src/pacing` is out of scope here). Filed as
/// HORO-1881, per the ticket brief's own "lean toward a new ticket"
/// guidance for this finding.
#[test]
fn paced_currently_strands_a_dependent_task_after_a_restart_on_t5() {
    use libra_governor_pacing_gate::drive::DriveLogEntry;

    let spec = fixtures::load("t5_daemon_restart").unwrap();
    let strategy = Paced::new(&spec.tasks, &spec.windows, &spec.initial_preference);
    let outcome = run_trace(&spec, Box::new(strategy)).unwrap();

    assert!(
        outcome.world.is_completed(1),
        "task 1 completes before the restart regardless"
    );
    assert!(
        !outcome.world.is_completed(2),
        "this test documents CURRENT (buggy) behavior — task 2 must currently NOT complete; \
         if this now fails, the restart-recovery gap above has been fixed and this test (and \
         its doc comment) should be deleted, not adjusted"
    );
    let rejected_task1_post_restart = outcome.log.iter().any(|(_, entry)| {
        matches!(
            entry,
            DriveLogEntry::Offered { rejected, .. } if rejected.contains(&1)
        )
    });
    assert!(
        rejected_task1_post_restart,
        "expected the driver to log a rejected re-proposal of task 1 after the restart"
    );
}

#[test]
fn true_occupancy_never_exceeds_total_real_spend_plus_open_claims() {
    let spec = fixtures::load("t6_overrun_backoff").unwrap();
    let outcome = run_trace(&spec, Box::new(StaticBudget::new(4))).unwrap();
    let window = outcome
        .world
        .window("main")
        .expect("t6_overrun_backoff declares a `main` window");
    // Every settled record plus every still-open claim, summed by hand
    // from the public fields, must equal `true_occupancy()` exactly —
    // the invariant the method itself claims to compute.
    let by_hand: u64 = window.settled.iter().sum::<u64>() + window.open.values().sum::<u64>();
    assert_eq!(by_hand, window.true_occupancy());
}

#[test]
fn foreign_spend_is_visible_in_true_occupancy_even_though_no_task_reported_it() {
    let spec = fixtures::load("t2_concurrent_session").unwrap();
    let outcome = run_trace(&spec, Box::new(StaticBudget::new(2))).unwrap();
    let window = outcome
        .world
        .window("shared")
        .expect("t2_concurrent_session declares a `shared` window");
    assert!(
        window.settled.contains(&300),
        "the foreign session's 300-token spend must appear in the window's settled ledger \
         even though no task in this trace ever reported it itself"
    );
}
