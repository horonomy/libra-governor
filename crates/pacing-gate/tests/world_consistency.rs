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
