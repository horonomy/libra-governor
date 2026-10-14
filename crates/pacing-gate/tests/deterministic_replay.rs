//! AC-equivalent of `pacing::simulate`'s own "same scenario twice is
//! byte-identical" regression, for this crate's driver instead: running
//! the same frozen trace against the same strategy kind twice must
//! produce an identical log, since every instant the driver visits is
//! derived deterministically from fixed fixture data (`TRACE_EPOCH` plus
//! offsets), never a wall clock or RNG read.

use libra_governor_pacing_gate::drive::run_trace;
use libra_governor_pacing_gate::fixtures;
use libra_governor_pacing_gate::strategy::{Paced, StaticBudget};

fn assert_replay_is_deterministic(
    fixture: &str,
    strategy: impl Fn() -> Box<dyn libra_governor_pacing_gate::strategy::Strategy>,
) {
    let spec = fixtures::load(fixture).unwrap_or_else(|e| panic!("{fixture}: {e}"));
    let a = run_trace(&spec, strategy()).unwrap_or_else(|e| panic!("{fixture} run A: {e}"));
    let b = run_trace(&spec, strategy()).unwrap_or_else(|e| panic!("{fixture} run B: {e}"));
    assert_eq!(
        format!("{:?}", a.log),
        format!("{:?}", b.log),
        "{fixture}: two runs of the same trace/strategy must produce an identical log"
    );
}

#[test]
fn static_budget_replay_is_deterministic_across_every_fixture() {
    for (name, _) in fixtures::ALL {
        assert_replay_is_deterministic(name, || Box::new(StaticBudget::new(3)));
    }
}

#[test]
fn paced_replay_is_deterministic_across_every_fixture() {
    for (name, _) in fixtures::ALL {
        assert_replay_is_deterministic(name, || {
            let spec = fixtures::load(name).expect("already validated to parse");
            Box::new(Paced::new(
                &spec.tasks,
                &spec.windows,
                &spec.initial_preference,
            ))
        });
    }
}
