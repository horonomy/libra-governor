//! HORO-1855 Phase A — semantic conformance at the wire+binding level:
//! exact domain mapping, account binding, and freshness, driven
//! directly through `crates/quota-observation`'s `wire` and
//! `freshness` public functions (no subprocess). See `tests/README.md`
//! for the full P/N/compat matrix, including which halves of
//! P02-P06/P09 are out of scope here (the evaluator
//! Blocking/Indeterminate halves — this crate has no evaluator).

mod common;

use std::collections::HashMap;

use time::macros::datetime;
use time::OffsetDateTime;

use libra_governor_quota_observation::freshness::{self, FreshnessError};
use libra_governor_quota_observation::registration::{
    Approval, Consent, ExpectedConstraint, FixedEnvironment, ObservationConfidence,
    RegisteredContext, Registration, RuntimeFileDigest, Transport, TransportKind, TrustMode,
};
use libra_governor_quota_observation::wire::{
    Constraint, Coverage, ErrorCode, Metric, MissingConstraint, MissingReason, ObservedContext,
    Quantity, Reading, RetryKind, Scope, SnapshotResponse, SnapshotResult, Unit,
    UpstreamEnforcement, Window, WireError, WireValidationError,
};

fn now() -> OffsetDateTime {
    common::fixed_now()
}

/// Binds a response, panicking on failure without echoing the `Err`'s
/// `Debug` output (which embeds the registration's account fingerprint).
/// CodeQL's `rust/cleartext-logging` query flags `.unwrap()` here because
/// a test panic message is, in its sink model, indistinguishable from a
/// production log line — even though these fixtures are entirely
/// synthetic. A static panic message sidesteps that without weakening
/// what the test actually checks (the assertions that follow this call).
fn bind(
    result: &SnapshotResult,
    requested: &[String],
    registration: &Registration,
) -> libra_governor_quota_observation::freshness::BoundSnapshotResult {
    freshness::apply_account_binding(result, requested, registration)
        .unwrap_or_else(|_| panic!("apply_account_binding failed on a known-good test fixture"))
}

fn expected(
    id: &str,
    fingerprint: &str,
    max_staleness_secs: u64,
    unit: Unit,
) -> ExpectedConstraint {
    ExpectedConstraint {
        id: id.into(),
        scope: Scope::Principal {
            handle: "principal_a".into(),
        },
        metric: Metric::TotalTokens,
        unit,
        window_uuid: "20000000-0000-4000-8000-000000000001".into(),
        provider: "examplecorp".into(),
        account_fingerprint: fingerprint.into(),
        pool_alias: "eng-shared-pool".into(),
        max_staleness_secs,
        window_length_secs: 2_592_000,
        capacity_ceiling: None,
    }
}

fn registration_with(expected_entries: Vec<ExpectedConstraint>) -> Registration {
    registration_with_ctx(
        RegisteredContext {
            selector: "profile_a".into(),
            handle: "context_a".into(),
            principal: "principal_a".into(),
            authority: "authority_a".into(),
        },
        expected_entries,
    )
}

/// Same as [`registration_with`] but with a caller-supplied
/// `RegisteredContext` — needed whenever a test's `SnapshotResult`
/// declares a non-default `context`/`authority` (e.g. the scenario
/// B/C fixtures), since `apply_account_binding`'s whole-response
/// `verify_context_binding` check (HORO-1854 B3) now rejects any
/// response whose context/authority disagrees with the registration.
fn registration_with_ctx(
    context: RegisteredContext,
    expected_entries: Vec<ExpectedConstraint>,
) -> Registration {
    Registration {
        registration_id: "reg-semantic".into(),
        extension_id: "examplecorp.quota".into(),
        extension_version: "0.1.0".into(),
        enabled: true,
        transport: Transport {
            kind: TransportKind::OneshotStdio,
            executable: "/bin/true".into(),
            args: vec![],
            environment: FixedEnvironment {
                lang: "C.UTF-8".into(),
                lc_all: "C.UTF-8".into(),
            },
        },
        context,
        expected: expected_entries,
        binding_revision: "1".into(),
        binding_verified_at: "2027-01-18T11:59:00Z".into(),
        scope_configured_at: "2027-01-18T11:58:00Z".into(),
        timeout_ms: 3_000,
        trust_mode: TrustMode::ObservationOnly,
        observation_confidence: ObservationConfidence::Low,
        approval: Approval {
            approved: true,
            revision: "1".into(),
            executable_sha256: String::new(),
            manifest_sha256: String::new(),
            launch_tuple_sha256: String::new(),
            runtime_files: Vec::<RuntimeFileDigest>::new(),
            endpoint_owner: "ops-team@examplecorp.invalid".into(),
            consent: Consent {
                consented_by: "operator".into(),
                recorded_at_reference: "ticket:EXAMPLE-1".into(),
            },
        },
    }
}

fn constraint(
    id: &str,
    reading: Reading,
    observed_at: &str,
    valid_until: Option<&str>,
    fingerprint: &str,
    enforcement: UpstreamEnforcement,
) -> Constraint {
    constraint_with_unit(
        id,
        reading,
        observed_at,
        valid_until,
        fingerprint,
        enforcement,
        Unit::Tokens,
    )
}

/// Same as [`constraint`] but with a caller-supplied `unit` — needed
/// whenever a test's matching `expected[]` entry (built via
/// [`expected`]) declares a non-`Tokens` unit (e.g. scenario B/C's
/// `UsdCents`/`Requests`/`OpaqueCredit` constraints), since
/// `apply_account_binding`'s unit cross-check (HORO-1854 B3) now
/// rejects any constraint whose `unit` disagrees with its
/// `expected[]` entry.
fn constraint_with_unit(
    id: &str,
    reading: Reading,
    observed_at: &str,
    valid_until: Option<&str>,
    fingerprint: &str,
    enforcement: UpstreamEnforcement,
    unit: Unit,
) -> Constraint {
    Constraint {
        id: id.into(),
        scope: Scope::Principal {
            handle: "principal_a".into(),
        },
        metric: Metric::TotalTokens,
        unit,
        window: Window::Opaque {
            declared_reset_at: None,
        },
        reading,
        observed_at: observed_at.into(),
        valid_until: valid_until.map(|s| s.to_string()),
        upstream_enforcement: enforcement,
        account_fingerprint: fingerprint.into(),
    }
}

fn observed_context() -> ObservedContext {
    ObservedContext {
        selector: "profile_a".into(),
        handle: "context_a".into(),
        principal: "principal_a".into(),
        epoch: "epoch_b".into(),
    }
}

/// Scenario B's own context (distinct from the default `profile_a`
/// context the other scenarios use) — must match `registration_with_ctx`'s
/// context exactly or `verify_context_binding` rejects the whole
/// response (HORO-1854 B3).
fn observed_context_b() -> ObservedContext {
    ObservedContext {
        selector: "profile_b".into(),
        handle: "context_b".into(),
        principal: "principal_b".into(),
        epoch: "epoch_c".into(),
    }
}

// ---------------------------------------------------------------------
// P02 — scenario A: exact used 600000/limit 1000000, age/expiry
// independent of the reading's own values.
// ---------------------------------------------------------------------

#[test]
fn p02_scenario_a_exact_used_and_limit_preserved() {
    let registration = registration_with(vec![expected(
        "tokens_month",
        "acct-fx9k2m1p",
        300,
        Unit::Tokens,
    )]);
    let result = SnapshotResult {
        context: observed_context(),
        authority: "authority_a".into(),
        context_valid_until: None,
        coverage: Coverage::Complete,
        constraints: vec![constraint(
            "tokens_month",
            Reading::Used {
                value: Quantity::parse("600000").unwrap(),
                limit: Quantity::parse("1000000").unwrap(),
            },
            "2027-01-18T12:00:00Z",
            Some("2027-01-18T12:01:00Z"),
            "acct-fx9k2m1p",
            UpstreamEnforcement::Unknown,
        )],
        missing: vec![],
    };
    let requested = vec!["tokens_month".to_string()];
    let bound = bind(&result, &requested, &registration);
    assert_eq!(bound.coverage, Coverage::Complete);
    match &bound.constraints[0].reading {
        Reading::Used { value, limit } => {
            assert_eq!(value.value(), 600_000);
            assert_eq!(limit.value(), 1_000_000);
        }
        other => panic!("expected Used, got {other:?}"),
    }
    assert!(freshness::is_fresh(&bound.constraints[0], 300, None, now(), now()).unwrap());
}

// ---------------------------------------------------------------------
// P03 — scenario B: team budget (spend/usd_cents) and hourly requests
// (requests) are two separate constraints with independent units and
// windows; `remaining`'s value/limit are preserved exactly as reported
// — this crate performs no used<->remaining conversion anywhere (it
// has no evaluator), so "exact used 880" is a derived quantity an
// evaluator would compute, not something this layer produces.
// ---------------------------------------------------------------------

#[test]
fn p03_scenario_b_separate_units_and_windows_no_conversion_performed() {
    let registration = registration_with_ctx(
        RegisteredContext {
            selector: "profile_b".into(),
            handle: "context_b".into(),
            principal: "principal_b".into(),
            authority: "authority_b".into(),
        },
        vec![
            expected("team_budget", "acct-tm3qz8x4", 300, Unit::UsdCents),
            expected("requests_hour", "acct-tm3qz8x4", 45, Unit::Requests),
        ],
    );
    let result = SnapshotResult {
        context: observed_context_b(),
        authority: "authority_b".into(),
        context_valid_until: None,
        coverage: Coverage::Complete,
        constraints: vec![
            constraint_with_unit(
                "team_budget",
                Reading::Used {
                    value: Quantity::parse("25000").unwrap(),
                    limit: Quantity::parse("100000").unwrap(),
                },
                "2027-01-18T12:00:00Z",
                Some("2027-01-18T12:01:00Z"),
                "acct-tm3qz8x4",
                UpstreamEnforcement::Soft,
                Unit::UsdCents,
            ),
            constraint_with_unit(
                "requests_hour",
                Reading::Remaining {
                    value: Quantity::parse("120").unwrap(),
                    limit: Quantity::parse("1000").unwrap(),
                },
                "2027-01-18T12:00:00Z",
                Some("2027-01-18T12:00:45Z"),
                "acct-tm3qz8x4",
                UpstreamEnforcement::Hard,
                Unit::Requests,
            ),
        ],
        missing: vec![],
    };
    let requested = vec!["team_budget".to_string(), "requests_hour".to_string()];
    let bound = bind(&result, &requested, &registration);
    assert_eq!(bound.constraints.len(), 2);
    let requests = bound
        .constraints
        .iter()
        .find(|c| c.id == "requests_hour")
        .unwrap();
    match &requests.reading {
        Reading::Remaining { value, limit } => {
            assert_eq!(value.value(), 120);
            assert_eq!(limit.value(), 1_000);
            // No "used" field exists anywhere on Constraint/Reading — an
            // evaluator computing 1000-120=880 is Phase-B/out of scope.
        }
        other => panic!("expected Remaining, got {other:?}"),
    }
    assert_eq!(requests.upstream_enforcement, UpstreamEnforcement::Hard);
}

// ---------------------------------------------------------------------
// P04 — scenario C: the 2^53+1 integer and the 500/400 (10500/10000)
// overdraw both survive account binding byte-identical, and the
// missing `rolling_requests` constraint keeps coverage Partial with no
// clearance of the two present constraints.
// ---------------------------------------------------------------------

#[test]
fn p04_scenario_c_large_integer_and_overdraw_survive_binding_partial_coverage() {
    let registration = registration_with_ctx(
        RegisteredContext {
            selector: "profile_c".into(),
            handle: "context_c".into(),
            principal: "principal_c".into(),
            authority: "authority_c".into(),
        },
        vec![
            expected(
                "org_credits",
                "acct-orgz9k1w2",
                600,
                Unit::OpaqueCredit {
                    namespace: "examplecredits".into(),
                },
            ),
            expected("project_budget", "acct-prjf8v3n6", 300, Unit::UsdCents),
            expected("rolling_requests", "acct-prjf8v3n6", 120, Unit::Requests),
        ],
    );
    let result = SnapshotResult {
        context: ObservedContext {
            selector: "profile_c".into(),
            handle: "context_c".into(),
            principal: "principal_c".into(),
            epoch: "epoch_d".into(),
        },
        authority: "authority_c".into(),
        context_valid_until: None,
        coverage: Coverage::Partial,
        constraints: vec![
            constraint_with_unit(
                "org_credits",
                Reading::Used {
                    value: Quantity::parse("9007199254740993").unwrap(),
                    limit: Quantity::parse("18014398509481986").unwrap(),
                },
                "2027-01-18T12:00:00Z",
                Some("2027-01-18T12:01:00Z"),
                "acct-orgz9k1w2",
                UpstreamEnforcement::Unknown,
                Unit::OpaqueCredit {
                    namespace: "examplecredits".into(),
                },
            ),
            constraint_with_unit(
                "project_budget",
                Reading::Used {
                    value: Quantity::parse("10500").unwrap(),
                    limit: Quantity::parse("10000").unwrap(),
                },
                "2027-01-18T12:00:00Z",
                Some("2027-01-18T12:01:00Z"),
                "acct-prjf8v3n6",
                UpstreamEnforcement::Hard,
                Unit::UsdCents,
            ),
        ],
        missing: vec![MissingConstraint {
            id: "rolling_requests".into(),
            reason: MissingReason::Unavailable,
        }],
    };
    let requested = vec![
        "org_credits".to_string(),
        "project_budget".to_string(),
        "rolling_requests".to_string(),
    ];
    let bound = bind(&result, &requested, &registration);
    assert_eq!(bound.coverage, Coverage::Partial);
    assert_eq!(bound.constraints.len(), 2);
    assert_eq!(bound.missing.len(), 1);
    assert_eq!(bound.missing[0].reason, MissingReason::Unavailable);

    let org_credits = bound
        .constraints
        .iter()
        .find(|c| c.id == "org_credits")
        .unwrap();
    match &org_credits.reading {
        Reading::Used { value, .. } => assert_eq!(value.value(), 9_007_199_254_740_993),
        other => panic!("expected Used, got {other:?}"),
    }
    assert!(serde_json::to_string(org_credits)
        .unwrap()
        .contains("9007199254740993"));

    let project_budget = bound
        .constraints
        .iter()
        .find(|c| c.id == "project_budget")
        .unwrap();
    match &project_budget.reading {
        Reading::Used { value, limit } => {
            assert!(
                value.value() > limit.value(),
                "overdraw must survive binding"
            );
            assert_eq!(value.value(), 10_500);
            assert_eq!(limit.value(), 10_000);
        }
        other => panic!("expected Used, got {other:?}"),
    }
}

// ---------------------------------------------------------------------
// P05 — an undisclosed reading on an opaque window: complete coverage
// is possible (no wire/binding error). The "evaluator reports
// Indeterminate" half is out of scope (no evaluator in this crate).
// ---------------------------------------------------------------------

#[test]
fn p05_undisclosed_reading_still_yields_complete_coverage() {
    let registration = registration_with(vec![expected(
        "tokens_month",
        "acct-fx9k2m1p",
        300,
        Unit::Tokens,
    )]);
    let result = SnapshotResult {
        context: observed_context(),
        authority: "authority_a".into(),
        context_valid_until: None,
        coverage: Coverage::Complete,
        constraints: vec![constraint(
            "tokens_month",
            Reading::Undisclosed,
            "2027-01-18T12:00:00Z",
            None,
            "acct-fx9k2m1p",
            UpstreamEnforcement::Unknown,
        )],
        missing: vec![],
    };
    let requested = vec!["tokens_month".to_string()];
    let bound = bind(&result, &requested, &registration);
    assert_eq!(bound.coverage, Coverage::Complete);
    assert_eq!(bound.constraints[0].reading, Reading::Undisclosed);
}

// ---------------------------------------------------------------------
// P06 — 10500 basis points (over 100%) preserved exactly, never
// clamped to 10000. The "evaluator reports Blocking" half is out of
// scope (no evaluator in this crate).
// ---------------------------------------------------------------------

#[test]
fn p06_utilization_over_100_percent_preserved_not_clamped() {
    let registration = registration_with(vec![expected(
        "project_budget",
        "acct-prjf8v3n6",
        300,
        Unit::PercentBasisPoints,
    )]);
    let result = SnapshotResult {
        context: observed_context(),
        authority: "authority_a".into(),
        context_valid_until: None,
        coverage: Coverage::Complete,
        constraints: vec![constraint_with_unit(
            "project_budget",
            Reading::Utilization {
                used_basis_points: Quantity::parse("10500").unwrap(),
            },
            "2027-01-18T12:00:00Z",
            None,
            "acct-prjf8v3n6",
            UpstreamEnforcement::Hard,
            Unit::PercentBasisPoints,
        )],
        missing: vec![],
    };
    let requested = vec!["project_budget".to_string()];
    let bound = bind(&result, &requested, &registration);
    match &bound.constraints[0].reading {
        Reading::Utilization { used_basis_points } => assert_eq!(used_basis_points.value(), 10_500),
        other => panic!("expected Utilization, got {other:?}"),
    }
}

// ---------------------------------------------------------------------
// P07 / N46 — the reading's own `observed_at` (source time) survives
// binding byte-for-byte; freshness is evaluated against that original
// source time plus the registered max-age, never against "now" as a
// stand-in acquisition time. Two different `now` values straddling the
// staleness boundary change the *freshness verdict* without ever
// changing the preserved `observed_at` string itself.
// ---------------------------------------------------------------------

#[test]
fn p07_n46_original_source_time_preserved_host_applies_max_age_not_acquisition_time() {
    let registration = registration_with(vec![expected(
        "tokens_month",
        "acct-fx9k2m1p",
        300,
        Unit::Tokens,
    )]);
    let result = SnapshotResult {
        context: observed_context(),
        authority: "authority_a".into(),
        context_valid_until: None,
        coverage: Coverage::Complete,
        constraints: vec![constraint(
            "tokens_month",
            Reading::Used {
                value: Quantity::parse("1").unwrap(),
                limit: Quantity::parse("2").unwrap(),
            },
            "2027-01-18T11:55:00Z", // 5 minutes before the fixed `now`
            None,
            "acct-fx9k2m1p",
            UpstreamEnforcement::Unknown,
        )],
        missing: vec![],
    };
    let requested = vec!["tokens_month".to_string()];
    let bound = bind(&result, &requested, &registration);
    assert_eq!(bound.constraints[0].observed_at, "2027-01-18T11:55:00Z");

    // Within the 300s max_staleness (now - observed_at == 330s... use a
    // closer `now` for the fresh case and a further one for stale).
    let fresh_now = datetime!(2027 - 01 - 18 11:59:30 UTC); // 270s after observed_at
    let stale_now = datetime!(2027 - 01 - 18 12:01:00 UTC); // 360s after observed_at
    assert!(freshness::is_fresh(&bound.constraints[0], 300, None, fresh_now, fresh_now).unwrap());
    assert!(!freshness::is_fresh(&bound.constraints[0], 300, None, stale_now, stale_now).unwrap());
    // observed_at is untouched by either evaluation.
    assert_eq!(bound.constraints[0].observed_at, "2027-01-18T11:55:00Z");
}

// ---------------------------------------------------------------------
// P09 — zero used / zero limit parses and binds without dividing by
// zero or being treated as healthy by anything in this crate (there is
// no evaluator here to divide at all — the in-scope guarantee is that
// the zero/zero reading survives the pipeline like any other value).
// ---------------------------------------------------------------------

#[test]
fn p09_zero_used_zero_limit_does_not_panic_and_is_preserved() {
    let registration = registration_with(vec![expected(
        "tokens_month",
        "acct-fx9k2m1p",
        300,
        Unit::Tokens,
    )]);
    let result = SnapshotResult {
        context: observed_context(),
        authority: "authority_a".into(),
        context_valid_until: None,
        coverage: Coverage::Complete,
        constraints: vec![constraint(
            "tokens_month",
            Reading::Used {
                value: Quantity::parse("0").unwrap(),
                limit: Quantity::parse("0").unwrap(),
            },
            "2027-01-18T12:00:00Z",
            None,
            "acct-fx9k2m1p",
            UpstreamEnforcement::Hard,
        )],
        missing: vec![],
    };
    let requested = vec!["tokens_month".to_string()];
    let bound = bind(&result, &requested, &registration);
    match &bound.constraints[0].reading {
        Reading::Used { value, limit } => {
            assert_eq!(value.value(), 0);
            assert_eq!(limit.value(), 0);
        }
        other => panic!("expected Used, got {other:?}"),
    }
    assert!(freshness::is_fresh(&bound.constraints[0], 300, None, now(), now()).unwrap());
}

// ---------------------------------------------------------------------
// P10 — every requested id reported missing: partial coverage, no
// constraints, never silently treated as healthy.
// ---------------------------------------------------------------------

#[test]
fn p10_all_requested_ids_missing_is_partial_with_no_constraints() {
    let registration = registration_with(vec![
        expected("tokens_month", "acct-fx9k2m1p", 300, Unit::Tokens),
        expected("requests_hour", "acct-fx9k2m1p", 45, Unit::Requests),
    ]);
    let result = SnapshotResult {
        context: observed_context(),
        authority: "authority_a".into(),
        context_valid_until: None,
        coverage: Coverage::Partial,
        constraints: vec![],
        missing: vec![
            MissingConstraint {
                id: "tokens_month".into(),
                reason: MissingReason::Unavailable,
            },
            MissingConstraint {
                id: "requests_hour".into(),
                reason: MissingReason::Timeout,
            },
        ],
    };
    let requested = vec!["tokens_month".to_string(), "requests_hour".to_string()];
    let bound = bind(&result, &requested, &registration);
    assert_eq!(bound.coverage, Coverage::Partial);
    assert!(bound.constraints.is_empty());
    assert_eq!(bound.missing.len(), 2);
}

// ---------------------------------------------------------------------
// N31 — stale (past max_staleness) and N32 — valid_until already
// elapsed even though the reading is still within max_staleness.
// ---------------------------------------------------------------------

#[test]
fn n31_stale_past_max_staleness_is_not_fresh() {
    let c = constraint(
        "tokens_month",
        Reading::Undisclosed,
        "2027-01-18T11:00:00Z", // 1h30m before fixed now (12:00:30)
        None,
        "acct-fx9k2m1p",
        UpstreamEnforcement::Unknown,
    );
    assert!(!freshness::is_fresh(&c, 300, None, now(), now()).unwrap());
}

#[test]
fn n31_control_within_max_staleness_is_fresh() {
    let c = constraint(
        "tokens_month",
        Reading::Undisclosed,
        "2027-01-18T12:00:00Z", // 30s before fixed now
        None,
        "acct-fx9k2m1p",
        UpstreamEnforcement::Unknown,
    );
    assert!(freshness::is_fresh(&c, 300, None, now(), now()).unwrap());
}

#[test]
fn n32_valid_until_already_elapsed_is_not_fresh_even_within_max_staleness() {
    let c = constraint(
        "tokens_month",
        Reading::Undisclosed,
        "2027-01-18T12:00:00Z", // 30s before now: well within max_staleness
        Some("2027-01-18T12:00:15Z"), // but valid_until elapsed 15s before now
        "acct-fx9k2m1p",
        UpstreamEnforcement::Unknown,
    );
    assert!(!freshness::is_fresh(&c, 300, None, now(), now()).unwrap());
}

#[test]
fn n32_control_valid_until_not_yet_elapsed_is_fresh() {
    let c = constraint(
        "tokens_month",
        Reading::Undisclosed,
        "2027-01-18T12:00:00Z",
        Some("2027-01-18T12:01:00Z"),
        "acct-fx9k2m1p",
        UpstreamEnforcement::Unknown,
    );
    assert!(freshness::is_fresh(&c, 300, None, now(), now()).unwrap());
}

// ---------------------------------------------------------------------
// N33/N34 — wrong context selector / wrong principal-authority-scope
// handle (gap G6, now closed): commit `7400315` ("Bind
// unit/metric/scope/context, not just fingerprint", HORO-1854
// adversarial review, B3) added `verify_context_binding`, called from
// `apply_account_binding` before any per-constraint binding is applied
// — a response whose declared `context`/`authority` disagrees with the
// registration's own is rejected *in its entirety*, not demoted
// constraint-by-constraint. Before this fix, nothing compared the
// response's `context` fields against the registration's at all.
// ---------------------------------------------------------------------

#[test]
fn n33_wrong_context_selector_rejects_the_whole_response() {
    let registration = registration_with(vec![expected(
        "tokens_month",
        "acct-fx9k2m1p",
        300,
        Unit::Tokens,
    )]);
    let mut context = observed_context();
    context.selector = "wrong-selector".into();
    let result = SnapshotResult {
        context,
        authority: "authority_a".into(),
        context_valid_until: None,
        coverage: Coverage::Complete,
        constraints: vec![constraint(
            "tokens_month",
            Reading::Undisclosed,
            "2027-01-18T12:00:00Z",
            None,
            "acct-fx9k2m1p",
            UpstreamEnforcement::Unknown,
        )],
        missing: vec![],
    };
    let requested = vec!["tokens_month".to_string()];
    assert!(matches!(
        freshness::apply_account_binding(&result, &requested, &registration).unwrap_err(),
        FreshnessError::ContextMismatch {
            field: "selector",
            ..
        }
    ));
}

#[test]
fn n34_wrong_authority_rejects_the_whole_response() {
    let registration = registration_with(vec![expected(
        "tokens_month",
        "acct-fx9k2m1p",
        300,
        Unit::Tokens,
    )]);
    let result = SnapshotResult {
        context: observed_context(),
        authority: "wrong-authority".into(),
        context_valid_until: None,
        coverage: Coverage::Complete,
        constraints: vec![constraint(
            "tokens_month",
            Reading::Undisclosed,
            "2027-01-18T12:00:00Z",
            None,
            "acct-fx9k2m1p",
            UpstreamEnforcement::Unknown,
        )],
        missing: vec![],
    };
    let requested = vec!["tokens_month".to_string()];
    assert!(matches!(
        freshness::apply_account_binding(&result, &requested, &registration).unwrap_err(),
        FreshnessError::ContextMismatch {
            field: "authority",
            ..
        }
    ));
}

#[test]
fn n34_wrong_principal_handle_rejects_the_whole_response() {
    let registration = registration_with(vec![expected(
        "tokens_month",
        "acct-fx9k2m1p",
        300,
        Unit::Tokens,
    )]);
    let mut context = observed_context();
    context.principal = "wrong-principal".into();
    let result = SnapshotResult {
        context,
        authority: "authority_a".into(),
        context_valid_until: None,
        coverage: Coverage::Complete,
        constraints: vec![constraint(
            "tokens_month",
            Reading::Undisclosed,
            "2027-01-18T12:00:00Z",
            None,
            "acct-fx9k2m1p",
            UpstreamEnforcement::Unknown,
        )],
        missing: vec![],
    };
    let requested = vec!["tokens_month".to_string()];
    assert!(matches!(
        freshness::apply_account_binding(&result, &requested, &registration).unwrap_err(),
        FreshnessError::ContextMismatch {
            field: "principal",
            ..
        }
    ));
}

#[test]
fn n33_n34_control_matching_context_and_authority_is_accepted() {
    let registration = registration_with(vec![expected(
        "tokens_month",
        "acct-fx9k2m1p",
        300,
        Unit::Tokens,
    )]);
    let result = SnapshotResult {
        context: observed_context(),
        authority: "authority_a".into(),
        context_valid_until: None,
        coverage: Coverage::Complete,
        constraints: vec![constraint(
            "tokens_month",
            Reading::Undisclosed,
            "2027-01-18T12:00:00Z",
            None,
            "acct-fx9k2m1p",
            UpstreamEnforcement::Unknown,
        )],
        missing: vec![],
    };
    let requested = vec!["tokens_month".to_string()];
    assert!(freshness::apply_account_binding(&result, &requested, &registration).is_ok());
}

// ---------------------------------------------------------------------
// Unit/metric/scope binding mismatch (also part of B3's fix,
// previously only `account_fingerprint` was checked): a constraint
// whose `unit`/`metric`/`scope` disagrees with its matching
// `expected[]` entry — even with a matching `account_fingerprint` —
// must be demoted to `missing`, not silently accepted as `Complete`.
// ---------------------------------------------------------------------

#[test]
fn unit_mismatch_is_rejected_into_missing_not_silently_accepted() {
    let registration = registration_with(vec![expected(
        "tokens_month",
        "acct-fx9k2m1p",
        300,
        Unit::Tokens,
    )]);
    let result = SnapshotResult {
        context: observed_context(),
        authority: "authority_a".into(),
        context_valid_until: None,
        coverage: Coverage::Complete,
        constraints: vec![constraint_with_unit(
            "tokens_month",
            Reading::Undisclosed,
            "2027-01-18T12:00:00Z",
            None,
            "acct-fx9k2m1p", // fingerprint matches — this alone used to be enough.
            UpstreamEnforcement::Unknown,
            Unit::UsdCents, // but the unit does not match expected[]'s Tokens.
        )],
        missing: vec![],
    };
    let requested = vec!["tokens_month".to_string()];
    let bound = bind(&result, &requested, &registration);
    assert_eq!(bound.coverage, Coverage::Partial);
    assert!(bound.constraints.is_empty());
    assert_eq!(bound.missing[0].reason, MissingReason::UnsupportedUnit);
}

#[test]
fn metric_mismatch_is_rejected_into_missing_not_silently_accepted() {
    let registration = registration_with(vec![expected(
        "tokens_month",
        "acct-fx9k2m1p",
        300,
        Unit::Tokens,
    )]);
    let mut mismatched = constraint(
        "tokens_month",
        Reading::Undisclosed,
        "2027-01-18T12:00:00Z",
        None,
        "acct-fx9k2m1p", // fingerprint matches — this alone used to be enough.
        UpstreamEnforcement::Unknown,
    );
    mismatched.metric = Metric::Requests; // but the metric does not match expected[]'s TotalTokens.
    let result = SnapshotResult {
        context: observed_context(),
        authority: "authority_a".into(),
        context_valid_until: None,
        coverage: Coverage::Complete,
        constraints: vec![mismatched],
        missing: vec![],
    };
    let requested = vec!["tokens_month".to_string()];
    let bound = bind(&result, &requested, &registration);
    assert_eq!(bound.coverage, Coverage::Partial);
    assert!(bound.constraints.is_empty());
    assert_eq!(bound.missing[0].reason, MissingReason::UnsupportedMetric);
}

#[test]
fn scope_mismatch_is_rejected_into_missing_not_silently_accepted() {
    let registration = registration_with(vec![expected(
        "tokens_month",
        "acct-fx9k2m1p",
        300,
        Unit::Tokens,
    )]);
    let mut mismatched = constraint(
        "tokens_month",
        Reading::Undisclosed,
        "2027-01-18T12:00:00Z",
        None,
        "acct-fx9k2m1p", // fingerprint matches — this alone used to be enough.
        UpstreamEnforcement::Unknown,
    );
    // Scope kind/handle does not match expected[]'s
    // `Scope::Principal { handle: "principal_a" }` — reuses
    // `AccountFingerprintMismatch` per `apply_account_binding`'s own
    // documented reasoning (no dedicated wire-schema reason variant
    // exists for "scope disagrees").
    mismatched.scope = Scope::Principal {
        handle: "someone-elses-principal".into(),
    };
    let result = SnapshotResult {
        context: observed_context(),
        authority: "authority_a".into(),
        context_valid_until: None,
        coverage: Coverage::Complete,
        constraints: vec![mismatched],
        missing: vec![],
    };
    let requested = vec!["tokens_month".to_string()];
    let bound = bind(&result, &requested, &registration);
    assert_eq!(bound.coverage, Coverage::Partial);
    assert!(bound.constraints.is_empty());
    assert_eq!(
        bound.missing[0].reason,
        MissingReason::AccountFingerprintMismatch
    );
}

// ---------------------------------------------------------------------
// N36 — a response constraint id the request never asked for.
// ---------------------------------------------------------------------

#[test]
fn n36_unrequested_constraint_id_is_rejected() {
    let result = SnapshotResult {
        context: observed_context(),
        authority: "a".into(),
        context_valid_until: None,
        coverage: Coverage::Complete,
        constraints: vec![constraint(
            "not_requested",
            Reading::Undisclosed,
            "2027-01-18T12:00:00Z",
            None,
            "acct-fx9k2m1p",
            UpstreamEnforcement::Unknown,
        )],
        missing: vec![],
    };
    let requested = vec!["tokens_month".to_string()];
    assert_eq!(
        result.validate_wire_coverage(&requested).unwrap_err(),
        WireValidationError::UnrequestedConstraintId("not_requested".into())
    );
}

// ---------------------------------------------------------------------
// N37 — duplicate ids across success/missing.
// ---------------------------------------------------------------------

#[test]
fn n37_duplicate_id_across_constraints_and_missing_is_rejected() {
    let result = SnapshotResult {
        context: observed_context(),
        authority: "a".into(),
        context_valid_until: None,
        coverage: Coverage::Partial,
        constraints: vec![constraint(
            "tokens_month",
            Reading::Undisclosed,
            "2027-01-18T12:00:00Z",
            None,
            "acct-fx9k2m1p",
            UpstreamEnforcement::Unknown,
        )],
        missing: vec![MissingConstraint {
            id: "tokens_month".into(),
            reason: MissingReason::Timeout,
        }],
    };
    let requested = vec!["tokens_month".to_string()];
    assert_eq!(
        result.validate_wire_coverage(&requested).unwrap_err(),
        WireValidationError::DuplicateConstraintId("tokens_month".into())
    );
}

// ---------------------------------------------------------------------
// N38 — a requested id present in neither constraints nor missing.
// ---------------------------------------------------------------------

#[test]
fn n38_requested_id_in_neither_constraints_nor_missing_is_rejected() {
    let result = SnapshotResult {
        context: observed_context(),
        authority: "a".into(),
        context_valid_until: None,
        coverage: Coverage::Partial,
        constraints: vec![],
        missing: vec![MissingConstraint {
            id: "requests_hour".into(),
            reason: MissingReason::Timeout,
        }],
    };
    let requested = vec!["tokens_month".to_string(), "requests_hour".to_string()];
    assert_eq!(
        result.validate_wire_coverage(&requested).unwrap_err(),
        WireValidationError::MissingConstraintId("tokens_month".into())
    );
}

// ---------------------------------------------------------------------
// N39 — Coverage::Complete declared while `missing` is non-empty.
// ---------------------------------------------------------------------

#[test]
fn n39_false_complete_with_nonempty_missing_is_rejected() {
    let result = SnapshotResult {
        context: observed_context(),
        authority: "a".into(),
        context_valid_until: None,
        coverage: Coverage::Complete,
        constraints: vec![constraint(
            "tokens_month",
            Reading::Undisclosed,
            "2027-01-18T12:00:00Z",
            None,
            "acct-fx9k2m1p",
            UpstreamEnforcement::Unknown,
        )],
        missing: vec![MissingConstraint {
            id: "requests_hour".into(),
            reason: MissingReason::Timeout,
        }],
    };
    let requested = vec!["tokens_month".to_string(), "requests_hour".to_string()];
    assert_eq!(
        result.validate_wire_coverage(&requested).unwrap_err(),
        WireValidationError::CompleteCoverageViolated
    );
}

// ---------------------------------------------------------------------
// N40 — Coverage::Partial declared while `missing` is empty.
// ---------------------------------------------------------------------

#[test]
fn n40_partial_with_empty_missing_is_rejected() {
    let result = SnapshotResult {
        context: observed_context(),
        authority: "a".into(),
        context_valid_until: None,
        coverage: Coverage::Partial,
        constraints: vec![constraint(
            "tokens_month",
            Reading::Undisclosed,
            "2027-01-18T12:00:00Z",
            None,
            "acct-fx9k2m1p",
            UpstreamEnforcement::Unknown,
        )],
        missing: vec![],
    };
    let requested = vec!["tokens_month".to_string()];
    assert_eq!(
        result.validate_wire_coverage(&requested).unwrap_err(),
        WireValidationError::PartialCoverageViolated
    );
}

// ---------------------------------------------------------------------
// N44 — an empty configured expected[] set is a loader error.
// ---------------------------------------------------------------------

#[test]
fn n44_empty_configured_expected_set_is_rejected_at_load_time() {
    let registration = registration_with(vec![]);
    assert!(matches!(
        registration.validate().unwrap_err(),
        libra_governor_quota_observation::registration::RegistrationError::EmptyExpected()
    ));
}

#[test]
fn n44_control_nonempty_expected_set_validates() {
    let registration = registration_with(vec![expected(
        "tokens_month",
        "acct-fx9k2m1p",
        300,
        Unit::Tokens,
    )]);
    assert!(registration.validate().is_ok());
}

// ---------------------------------------------------------------------
// N48 — context_valid_until earlier than the reading's own
// valid_until: the context's own expiry still governs independently.
// ---------------------------------------------------------------------

#[test]
fn n48_context_valid_until_earlier_than_reading_valid_until_still_expires_it() {
    let c = constraint(
        "tokens_month",
        Reading::Undisclosed,
        "2027-01-18T12:00:00Z",
        Some("2027-01-18T12:05:00Z"), // the reading's own validity: far in the future
        "acct-fx9k2m1p",
        UpstreamEnforcement::Unknown,
    );
    // context_valid_until already elapsed relative to `now` (12:00:30),
    // even though the reading's own valid_until has not.
    let context_valid_until = "2027-01-18T12:00:15Z";
    assert!(!freshness::is_fresh(&c, 300, Some(context_valid_until), now(), now()).unwrap());
}

#[test]
fn n48_control_context_valid_until_not_yet_elapsed_is_fresh() {
    let c = constraint(
        "tokens_month",
        Reading::Undisclosed,
        "2027-01-18T12:00:00Z",
        Some("2027-01-18T12:05:00Z"),
        "acct-fx9k2m1p",
        UpstreamEnforcement::Unknown,
    );
    let context_valid_until = "2027-01-18T12:05:00Z";
    assert!(freshness::is_fresh(&c, 300, Some(context_valid_until), now(), now()).unwrap());
}

// ---------------------------------------------------------------------
// N50 — upstream_enforcement is preserved verbatim; a soft limit is
// never retyped into a hard one anywhere in this crate.
// ---------------------------------------------------------------------

#[test]
fn n50_soft_upstream_enforcement_is_never_retyped_as_hard() {
    let registration = registration_with(vec![expected(
        "team_budget",
        "acct-tm3qz8x4",
        300,
        Unit::UsdCents,
    )]);
    let result = SnapshotResult {
        context: observed_context(),
        authority: "authority_a".into(),
        context_valid_until: None,
        coverage: Coverage::Complete,
        constraints: vec![constraint_with_unit(
            "team_budget",
            Reading::Used {
                value: Quantity::parse("25000").unwrap(),
                limit: Quantity::parse("100000").unwrap(),
            },
            "2027-01-18T12:00:00Z",
            None,
            "acct-tm3qz8x4",
            UpstreamEnforcement::Soft,
            Unit::UsdCents,
        )],
        missing: vec![],
    };
    let requested = vec!["team_budget".to_string()];
    let bound = bind(&result, &requested, &registration);
    assert_eq!(
        bound.constraints[0].upstream_enforcement,
        UpstreamEnforcement::Soft
    );
}

// ---------------------------------------------------------------------
// N35 — a context_changed wire error surfaces as a typed error with no
// bound result; there is no separate "whole context error" path beyond
// the ordinary error envelope.
// ---------------------------------------------------------------------

#[test]
fn n35_context_changed_error_returns_no_result() {
    let response = SnapshotResponse {
        protocol_version: "libra.quota-observation/0.1".into(),
        request_id: "r1".into(),
        extension_id: "examplecorp.quota".into(),
        extension_version: "0.1.0".into(),
        result: None,
        error: Some(WireError {
            code: ErrorCode::ContextChanged,
            retry: RetryKind::AfterUserAction,
            retry_after_seconds: None,
        }),
    };
    let requested = vec!["tokens_month".to_string()];
    assert!(response.validate(&requested).is_ok());
    assert!(response.result.is_none());
    assert_eq!(response.error.unwrap().code, ErrorCode::ContextChanged);
}

/// Not strictly one of the numbered IDs, but directly supports P10's
/// "no empty healthy" guarantee and N35's error path: feed every one
/// of the 9 `ErrorCode` values through the response envelope and
/// confirm each validates cleanly as an error-only response (never a
/// result), proving the error path itself is exhaustive over the enum.
#[test]
fn every_error_code_validates_as_an_error_only_response_never_a_result() {
    let codes = [
        ErrorCode::AuthRequired,
        ErrorCode::PermissionDenied,
        ErrorCode::Unavailable,
        ErrorCode::Timeout,
        ErrorCode::IncompatibleProtocol,
        ErrorCode::UnsupportedCapability,
        ErrorCode::InvalidConfiguration,
        ErrorCode::RateLimited,
        ErrorCode::ContextChanged,
    ];
    assert_eq!(codes.len(), 9);
    let mut seen: HashMap<String, ()> = HashMap::new();
    for code in codes {
        let response = SnapshotResponse {
            protocol_version: "libra.quota-observation/0.1".into(),
            request_id: "r1".into(),
            extension_id: "examplecorp.quota".into(),
            extension_version: "0.1.0".into(),
            result: None,
            error: Some(WireError {
                code,
                retry: RetryKind::Never,
                retry_after_seconds: None,
            }),
        };
        let requested = vec!["tokens_month".to_string()];
        assert!(response.validate(&requested).is_ok());
        assert!(response.result.is_none());
        seen.insert(format!("{code:?}"), ());
    }
    assert_eq!(seen.len(), 9, "all 9 ErrorCode variants must be distinct");
}

/// Used only to keep `compute_max_age_seconds` and `FreshnessError`
/// imports load-bearing for a reader following the min-over-requested
/// rule referenced by several tests above via `registration_with`.
#[test]
fn compute_max_age_seconds_is_the_min_over_requested_ids() {
    let registration = registration_with(vec![
        expected("tokens_month", "acct-fx9k2m1p", 300, Unit::Tokens),
        expected("requests_hour", "acct-fx9k2m1p", 45, Unit::Requests),
    ]);
    let requested = vec!["tokens_month".to_string(), "requests_hour".to_string()];
    assert_eq!(
        freshness::compute_max_age_seconds(&registration, &requested).unwrap(),
        45
    );
    let err = freshness::compute_max_age_seconds(&registration, &[]).unwrap_err();
    assert!(matches!(err, FreshnessError::EmptyRequested));
}
