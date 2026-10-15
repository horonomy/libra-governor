//! HORO-1727 Decision B: the turn-scoped resource ceiling.
//!
//! These tests cover the two acceptance criteria that are specifically
//! about `crates/daemon`'s own pieces (the shipped default constants, and
//! the fresh-token/cache-read denomination this ceiling is measured in).
//! The policy-level mechanism itself (fold logic, backward compatibility,
//! boundary behavior, cold-start) is covered in
//! `crates/domain/src/policy.rs`'s own test module.

use libra_governor_daemon::server::{
    default_admission_policy, default_turn_bound, DEFAULT_TURN_CEILING_TOKENS,
    DEFAULT_TURN_COLD_START_REQUEST_TOKENS,
};
use libra_governor_daemon::usage::TurnUsage;
use libra_governor_domain::{ConstraintOutcome, DenyReason, ResourceAmount};

/// AC1: the default turn bound's values match the documented derivation
/// (`docs/adr/0021-turn-scoped-resource-ceiling.md`) — pinned so a future
/// accidental edit of either constant is caught here, not discovered live.
#[test]
fn ac1_default_turn_bound_matches_the_documented_derivation() {
    assert_eq!(DEFAULT_TURN_COLD_START_REQUEST_TOKENS, 120_000);
    assert_eq!(DEFAULT_TURN_CEILING_TOKENS, 400_000);

    let turn = default_turn_bound();
    assert_eq!(
        turn.cold_start_request,
        ResourceAmount::Tokens(DEFAULT_TURN_COLD_START_REQUEST_TOKENS)
    );
    assert_eq!(
        turn.ceiling,
        ResourceAmount::Tokens(DEFAULT_TURN_CEILING_TOKENS)
    );

    let policy = default_admission_policy();
    assert_eq!(
        policy.turn,
        Some(turn),
        "the default policy must carry this exact turn bound"
    );

    // The ADR's own cold-start-regression invariant: the turn cold-start
    // request must sit strictly below the default `balanced` preset's
    // lifetime elastic ceiling (100_000 * 1.25 = 125_000), or a brand-new
    // task's very first preflight would trip the LIFETIME elastic band
    // before the turn ceiling is ever reached — a behavior change this
    // ticket must not make.
    let lifetime_elastic_ceiling = policy
        .resource
        .elastic_ceiling
        .expect("balanced preset always carries an elastic ceiling");
    assert!(
        ResourceAmount::Tokens(DEFAULT_TURN_COLD_START_REQUEST_TOKENS).as_f64()
            < lifetime_elastic_ceiling.as_f64(),
        "a cold-start turn request at or above the lifetime elastic ceiling would regress a \
         brand-new task's very first preflight"
    );
}

/// AC5 corollary: a brand-new task's full preflight (empty estimate
/// history, `requested == cold_start_request`) must cleanly Admit under
/// `default_admission_policy()` — the regression the ADR calls out by
/// name. This exercises the real `Policy::evaluate` + `evaluate_turn` +
/// `PolicyDecision::with_turn_outcome` pipeline together, at the actual
/// shipped default, not a toy policy.
#[test]
fn default_admission_policy_cold_start_preflight_admits() {
    let policy = default_admission_policy();
    let cold_start_requested = policy.turn_fallback_request();

    let lifetime_decision = policy
        .evaluate(
            cold_start_requested,
            0,
            libra_governor_domain::Confidence::Low,
        )
        .expect("compatible kind");
    let turn_outcome = policy
        .evaluate_turn(cold_start_requested)
        .expect("compatible kind");
    let decision = lifetime_decision.with_turn_outcome(turn_outcome);

    assert_eq!(
        decision.admission,
        libra_governor_domain::Admission::Admit,
        "a brand-new task's cold-start preflight must admit cleanly under the shipped default"
    );
}

/// AC2 (denomination consistency): the turn check must only ever see
/// `TurnUsage::fresh_tokens()` — fresh tokens (input + cache_creation +
/// output) — never raw cache-read volume. A fixture with a large
/// cache-read count and a modest fresh-token count must admit, and the
/// test also proves the exclusion is load-bearing (not vacuous) by
/// checking that the naive four-field sum of the same fixture *would*
/// have exceeded the ceiling if cache reads were wrongly counted.
#[test]
fn ac2_turn_check_only_counts_fresh_tokens_not_cache_reads() {
    let turn = default_turn_bound();
    let policy = default_admission_policy();

    // A turn with enormous cache-read volume (a long, mostly-cached
    // conversation) but modest fresh-token volume (a small, targeted
    // edit this turn).
    let usage = TurnUsage {
        input_tokens: 10_000,
        cache_creation_input_tokens: 15_000,
        cache_read_input_tokens: 5_000_000,
        output_tokens: 20_000,
        records: 1,
    };

    let fresh = usage.fresh_tokens();
    assert_eq!(fresh, 45_000, "input + cache_creation + output only");
    assert!(
        fresh < turn.ceiling.as_f64() as u64,
        "this fixture's fresh-token total must sit comfortably under the ceiling"
    );

    let outcome = policy
        .evaluate_turn(ResourceAmount::Tokens(fresh))
        .expect("compatible kind");
    assert_eq!(
        outcome,
        ConstraintOutcome::Admit,
        "a turn with huge cache-read volume but modest fresh-token volume must admit"
    );

    // Prove the exclusion is load-bearing: the naive four-field sum of
    // this exact fixture would have denied, had cache reads been wrongly
    // counted.
    let naive_four_field_sum = usage.input_tokens
        + usage.cache_creation_input_tokens
        + usage.cache_read_input_tokens
        + usage.output_tokens;
    assert!(
        naive_four_field_sum > turn.ceiling.as_f64() as u64,
        "the fixture must be chosen so that counting cache reads WOULD have tripped the \
         ceiling, proving the exclusion actually matters here"
    );
    let naive_outcome = policy
        .evaluate_turn(ResourceAmount::Tokens(naive_four_field_sum))
        .expect("compatible kind");
    assert!(
        matches!(
            naive_outcome,
            ConstraintOutcome::Deny(DenyReason::ResourceExceedsTurnCeiling { .. })
        ),
        "the naive (wrongly cache-read-inclusive) total must deny, confirming the real check's \
         fresh-tokens-only exclusion is the thing keeping the real fixture admitted"
    );
}
