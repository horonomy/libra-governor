//! Confirms the `false_throttle`/`involuntary_wait` metrics are safe
//! against repeated `Offered` sightings of the same held task — see
//! `src/verify.rs`'s own unit tests
//! (`false_throttle_duration_is_bracket_interval_not_sighting_count`) for
//! the interval-math proof in isolation; this file confirms the same
//! property end to end, against a real fixture that genuinely produces
//! more than one `Offered` sighting of the same held task.

use time::OffsetDateTime;

use libra_governor_pacing_gate::drive::run_trace;
use libra_governor_pacing_gate::drive::DriveLogEntry;
use libra_governor_pacing_gate::fixtures;
use libra_governor_pacing_gate::strategy::{Capability, Strategy};
use libra_governor_pacing_gate::verify::{verify, StrategyMeta};

/// A strategy that never starts anything, regardless of `ready` — used
/// ONLY to force a task to be re-offered and held across multiple
/// distinct driver events, independent of either built-in strategy's own
/// quota-awareness (both `StaticBudget` and `Paced` happen to actually
/// START `t3_opaque_quota`'s only task immediately, each for its own
/// documented reason — `StaticBudget` because it is blind to quota
/// entirely, `Paced` because it never sees `ForeignSpend`/
/// `QuotaSnapshotTold` at all — so neither holds it long enough to
/// exercise this property; see `tests/mutants_are_caught.rs` and
/// `tests/verify_fixture_claims.rs` for what those two real behaviors
/// mean for the hard-limit/false-throttle checks instead).
struct NeverStarts;

impl Strategy for NeverStarts {
    fn name(&self) -> &str {
        "never_starts"
    }
    fn capability(&self) -> Capability {
        Capability::LiveCapable
    }
    fn decide(&mut self, _now: OffsetDateTime, _ready: &[u32]) -> Vec<u32> {
        Vec::new()
    }
}

/// `t3_opaque_quota` declares two exogenous events (a `ForeignSpend` at
/// the epoch, a `QuotaSnapshotTold` 5 seconds later) — `drive::run_trace`
/// recomputes `ready`/`decide` after EVERY queue item regardless of
/// kind, so a strategy that never starts task 1 sees it re-offered at
/// least twice.
#[test]
fn a_held_task_sighted_in_more_than_one_offered_entry_is_still_counted_once() {
    let spec = fixtures::load("t3_opaque_quota").unwrap();
    let strategy = NeverStarts;
    let meta = StrategyMeta::of(&strategy);
    let outcome = run_trace(&spec, Box::new(strategy)).unwrap();

    let offered_sightings_of_task_1 = outcome
        .log
        .iter()
        .filter(|(_, e)| matches!(e, DriveLogEntry::Offered { ready, .. } if ready.contains(&1)))
        .count();
    assert!(
        offered_sightings_of_task_1 >= 2,
        "expected task 1 to be offered at least twice (epoch + the snapshot event) — got {}; if \
         this now fails, drive.rs's own re-poll-on-every-event discipline changed and this \
         test's premise needs revisiting",
        offered_sightings_of_task_1
    );

    let report = verify(&spec, meta, &outcome);
    // Task 1 genuinely never has headroom (380 foreign + 150 own = 530
    // > 400) for the whole 60-second trace, so it must NOT be flagged as
    // false-throttled no matter how many times it is re-offered — a
    // verifier that conflated "seen declined N times" with
    // "false-throttled" would get this wrong in the other direction too
    // (a real, persistent quota shortfall is not a false throttle just
    // because it is sighted repeatedly).
    assert_eq!(
        report.false_throttle_task_count, 0,
        "task 1's hold is a real, persistent quota shortfall, not a false throttle: {:?}",
        report.per_task
    );
    let task1 = report.per_task.iter().find(|t| t.task == 1).unwrap();
    assert_eq!(
        task1.false_throttle_secs(),
        0,
        "a non-false-throttled task must report zero false-throttle seconds regardless of how \
         many times it was sighted"
    );
    assert!(
        !outcome.world.is_completed(1),
        "task 1 never gets enough headroom to start"
    );
}
