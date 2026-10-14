//! Backs up two specific claims `strategy.rs`'s own doc comments make
//! about `Paced`'s incompleteness on `x0_unguarded`/`t6_overrun_backoff`
//! ("x0's and t6's own incompleteness are separately legitimate (a real
//! `PastTargetEnd` cutoff and a real AC1 false-throttle respectively —
//! see `verify.rs`)") — this test file is that "see verify.rs".

use libra_governor_pacing_gate::drive::run_trace;
use libra_governor_pacing_gate::fixtures;
use libra_governor_pacing_gate::strategy::Paced;
use libra_governor_pacing_gate::verify::{verify, StrategyMeta};

/// `x0_unguarded`'s window only ever has headroom for one of its five
/// 100-token tasks within the whole trace (its 21600s window never ages
/// anything out across the ~150s this trace spans — see
/// `verify::Occupancy`'s own docs). `Paced` therefore completes only 1/5
/// — but that incompleteness must show ZERO quota waste and ZERO
/// false-throttles: there genuinely was no headroom left for any of the
/// other four, so declining them is correct behavior, not a false
/// throttle.
#[test]
fn paced_incompleteness_on_x0_is_a_real_quota_cutoff_not_a_false_throttle() {
    let spec = fixtures::load("x0_unguarded").unwrap();
    let strategy = Paced::new(&spec.tasks, &spec.windows, &spec.initial_preference);
    let meta = StrategyMeta::of(&strategy);
    let outcome = run_trace(&spec, Box::new(strategy)).unwrap();
    let report = verify(&spec, meta, &outcome);

    assert_eq!(
        report.tasks_completed, 1,
        "exactly one of x0's five tasks should ever fit"
    );
    assert!(
        report.tasks_completed < report.tasks_total,
        "x0/Paced is genuinely incomplete"
    );
    assert_eq!(
        report.false_throttle_task_count, 0,
        "none of the other four tasks had real headroom available — their exclusion is a \
         correct quota cutoff, not a false throttle: {:?}",
        report.per_task
    );
    assert_eq!(
        report.quota_waste, 0,
        "no capacity was left unused that any declined task could actually have used"
    );
    assert!(
        report.hard_limit_violations.is_empty(),
        "Paced must never itself violate the hard limit: {:?}",
        report.hard_limit_violations
    );
}

/// `t6_overrun_backoff`'s task 1 overruns its own declared hold (reports
/// real spend of 400 against a declared 100), consuming headroom the
/// window's 700-token limit still has plenty of (400 settled, 300
/// remaining) once task 1 completes — task 2 only costs 100, well within
/// that remaining 300. `Paced` nonetheless holds task 2 back (a
/// sustain-mode backoff response to the overrun), making this a genuine,
/// real AC1 false throttle: the world's own ground truth confirms
/// capacity was available, and the strategy declined anyway.
#[test]
fn paced_holding_task2_on_t6_is_a_real_ac1_false_throttle() {
    let spec = fixtures::load("t6_overrun_backoff").unwrap();
    let strategy = Paced::new(&spec.tasks, &spec.windows, &spec.initial_preference);
    let meta = StrategyMeta::of(&strategy);
    let outcome = run_trace(&spec, Box::new(strategy)).unwrap();
    let report = verify(&spec, meta, &outcome);

    assert!(
        outcome.world.is_completed(1),
        "task 1 (the overrunning task) completes regardless"
    );
    assert!(
        !outcome.world.is_completed(2),
        "task 2 is held back by Paced's backoff response"
    );
    assert_eq!(
        report.false_throttle_task_count, 1,
        "task 2's hold must be confirmed as a real false throttle (true headroom existed): {:?}",
        report.per_task
    );
    assert!(
        report.hard_limit_violations.is_empty(),
        "Paced must never itself violate the hard limit: {:?}",
        report.hard_limit_violations
    );
}
