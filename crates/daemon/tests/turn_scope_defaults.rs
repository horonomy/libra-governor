//! HORO-1727 S4: what a turn-scoped (no-config) task is, and is not, allowed
//! to do under the shipped default policy.

use libra_governor_daemon::default_admission_policy;
use libra_governor_domain::{Admission, Confidence, ResourceAmount};

/// A turn-scoped decision: the lifetime resource bound is skipped and the
/// turn ceiling is folded in, exactly as the preflight does.
fn turn_scoped_admission(requested: u64, projected: u64) -> Admission {
    let policy = default_admission_policy();
    let decision = policy
        .evaluate_without_lifetime_resource(ResourceAmount::Tokens(projected), 0, Confidence::Low)
        .unwrap();
    let turn = policy
        .evaluate_turn(ResourceAmount::Tokens(requested))
        .unwrap();
    decision.with_turn_outcome(turn).admission
}

#[test]
fn a_fresh_turn_scoped_install_admits_past_the_legacy_150k_and_a_cold_start() {
    let policy = default_admission_policy();
    assert_eq!(
        policy.turn_fallback_request(),
        ResourceAmount::Tokens(120_000)
    );

    // A cold-start turn on top of 200k of settled spend is past the legacy
    // 150k hard ceiling and the 125k elastic band.
    assert_eq!(turn_scoped_admission(120_000, 320_000), Admission::Admit);

    // Negative control: the lifetime-bounded decision for the same
    // projection is not an Admit.
    let bounded = policy
        .evaluate(ResourceAmount::Tokens(320_000), 0, Confidence::Low)
        .unwrap();
    assert_ne!(bounded.admission, Admission::Admit);
}
