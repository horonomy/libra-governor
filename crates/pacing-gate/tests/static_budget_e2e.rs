//! `StaticBudget` runs end to end against every fixture without
//! panicking, and actually admits and completes work — the minimal
//! "driver/world/strategy layer is solid" check PR-A owns (full
//! verifier/metrics output is PR-B's scope).

use libra_governor_pacing_gate::drive::{run_trace, DriveLogEntry};
use libra_governor_pacing_gate::fixtures;
use libra_governor_pacing_gate::strategy::StaticBudget;

#[test]
fn static_budget_completes_every_fixture_without_panicking() {
    for (name, _) in fixtures::ALL {
        let spec = fixtures::load(name).unwrap();
        let outcome = run_trace(&spec, Box::new(StaticBudget::new(3)))
            .unwrap_or_else(|e| panic!("{name}: {e}"));

        let started = outcome
            .log
            .iter()
            .filter(|(_, e)| matches!(e, DriveLogEntry::Started { .. }))
            .count();
        let completed = outcome
            .log
            .iter()
            .filter(|(_, e)| matches!(e, DriveLogEntry::Completed { .. }))
            .count();
        assert!(started > 0, "{name}: StaticBudget admitted nothing");
        assert_eq!(
            started, completed,
            "{name}: every started task must eventually complete"
        );
        assert_eq!(
            started,
            spec.tasks.len(),
            "{name}: StaticBudget has no quota awareness, so it must eventually \
             admit every task in the trace, never leave one stuck"
        );
    }
}

#[test]
fn static_budget_never_exceeds_its_own_concurrency_cap() {
    let spec = fixtures::load("t1_multi_window").unwrap();
    let outcome = run_trace(&spec, Box::new(StaticBudget::new(2))).unwrap();

    let mut concurrent = 0i64;
    let mut max_concurrent = 0i64;
    for (_, entry) in &outcome.log {
        match entry {
            DriveLogEntry::Started { .. } => {
                concurrent += 1;
                max_concurrent = max_concurrent.max(concurrent);
            }
            DriveLogEntry::Completed { .. } => concurrent -= 1,
            _ => {}
        }
    }
    assert!(
        max_concurrent <= 2,
        "StaticBudget::new(2) must never run more than 2 tasks concurrently, saw {max_concurrent}"
    );
}
