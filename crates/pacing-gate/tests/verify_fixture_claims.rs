//! Backs up `strategy.rs`'s own doc comments about `Paced`'s
//! incompleteness on `x0_unguarded`/`t6_overrun_backoff` — this test file
//! is that module's own "see verify.rs". x0's incompleteness is a real
//! `PastTargetEnd` quota cutoff, no false throttle. t6's incompleteness
//! IS a real AC1 false throttle at the one instant it is observed — but
//! that one instant is also the last one task 2 ever gets (`Paced` never
//! schedules a recheck), making t6's own "throttle (never cancel)"
//! fixture description not actually matched by observed behavior. See
//! each test's own doc comment for detail.

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
/// that remaining 300. `Paced` holds task 2 back at the one instant it
/// is ever observed (t=60) — a genuine, real AC1 false throttle AT THAT
/// INSTANT: the world's own ground truth confirms capacity was
/// available, and the strategy declined anyway.
///
/// PR-B finding, NOT fixed here (same `domain::pacing` out-of-scope
/// reasoning as `Strategy::on_restart`'s `completed`-wipe —
/// `world_consistency.rs`'s own t5 test): t6's fixture description
/// itself says a pacing-aware strategy is "expected to throttle (never
/// cancel)" the overrunning principal's later starts. The driver's own
/// log (confirmed empirically) shows no further `Offered` entry for task
/// 2 after t=60 at all — `Paced::next_wake` never schedules a future
/// recheck once the backoff takes effect. A throttle that is never
/// revisited is, in observed behavior, indistinguishable from a cancel —
/// so while the ONE sighting this test confirms really is a false
/// throttle (not a hard-limit violation, not quota exhaustion), this
/// test does NOT claim t6's overall behavior matches its own fixture
/// description's "never cancel" promise. Recommended follow-up, same as
/// `on_restart`'s: a human should decide whether this becomes a new
/// Jira ticket or stays PR-body text.
#[test]
fn paced_holding_task2_on_t6_is_a_real_but_never_revisited_ac1_false_throttle() {
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
        "task 2's one sighting must be confirmed as a real false throttle (true headroom \
         existed): {:?}",
        report.per_task
    );
    let task2 = report.per_task.iter().find(|t| t.task == 2).unwrap();
    assert!(
        task2.stranded_with_headroom,
        "task 2 is never revisited after its one eligible sighting — this is the \
         throttle-in-name-only finding this test documents, not silently absent: {:?}",
        task2
    );
    assert!(
        report.hard_limit_violations.is_empty(),
        "Paced must never itself violate the hard limit: {:?}",
        report.hard_limit_violations
    );
}
