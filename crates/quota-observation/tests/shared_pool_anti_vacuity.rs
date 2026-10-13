//! HORO-1855 Phase A — the founder-mandated shared-pool attacks: the
//! USD-500×N failure mode (one real allowance must never be observed
//! as N independent full-capacity pools) and its neighbors — disabled
//! duplicates, cross-account leakage, host-identity promotion, and
//! stale/partial/auth-required results never looking usable.
//!
//! See `tests/README.md` for the full P/N/compat matrix.

mod common;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::AtomicBool;
use std::thread;

use libra_governor_quota_observation::freshness::{self, FreshnessError};
use libra_governor_quota_observation::identity::IdentityError;
use libra_governor_quota_observation::registration::{
    load_registrations, Approval, CapacityCeiling, Consent, ExpectedConstraint, FixedEnvironment,
    ObservationConfidence, RegisteredContext, Registration, RegistrationError, RuntimeFileDigest,
    Transport, TransportKind, TrustMode,
};
use libra_governor_quota_observation::runner::{self, InvokeError};
use libra_governor_quota_observation::wire::{
    ErrorCode, Metric, Quantity, Scope, SnapshotOperation, SnapshotRequest, Unit,
};

use common::{
    copy_script, fixture_path, observe, sample_expected, sample_registration, unique_request_id,
    write_template, ObserveError, SUBPROCESS_SERIAL,
};

// ---------------------------------------------------------------------
// Shared builders
// ---------------------------------------------------------------------

/// A minimal, structurally valid [`Registration`] with exactly one
/// `expected[]` entry. The executable path need not exist on disk for
/// any test that only calls `registration.validate()` or
/// `load_registrations` — those never touch the filesystem for the
/// executable itself. Tests that call `invoke`/`identity::verify_identity`
/// pass a real script path instead (via `copy_script`).
fn base_registration(
    registration_id: &str,
    executable: &str,
    account_fingerprint: &str,
    pool_alias: &str,
) -> Registration {
    Registration {
        registration_id: registration_id.into(),
        extension_id: "examplecorp.quota".into(),
        extension_version: "0.1.0".into(),
        enabled: true,
        transport: Transport {
            kind: TransportKind::OneshotStdio,
            executable: executable.into(),
            args: vec!["ok".into(), "/tmp/does-not-need-to-exist.json".into()],
            environment: FixedEnvironment {
                lang: "C.UTF-8".into(),
                lc_all: "C.UTF-8".into(),
            },
        },
        context: RegisteredContext {
            selector: "profile_a".into(),
            handle: "context_a".into(),
            principal: "principal_a".into(),
            authority: "authority_a".into(),
        },
        expected: vec![sample_expected(
            "tokens_month",
            account_fingerprint,
            pool_alias,
        )],
        binding_revision: "1".into(),
        binding_verified_at: "2027-01-18T11:59:00Z".into(),
        scope_configured_at: "2027-01-18T11:58:00Z".into(),
        timeout_ms: 5_000,
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

fn write_registration(dir: &std::path::Path, filename: &str, registration: &Registration) {
    let body = serde_json::to_string_pretty(registration).unwrap();
    std::fs::write(dir.join(filename), body).unwrap();
}

// ---------------------------------------------------------------------
// 1. 500×sessions via the registration route: 8 registrations sharing
//    one (account_fingerprint, pool_alias) must collide into exactly
//    one pool, regardless of every other identity-shaped field
//    differing between them.
// ---------------------------------------------------------------------

#[test]
fn attack1_eight_registrations_sharing_one_pool_identity_collide() {
    let dir = tempfile::tempdir().unwrap();
    for i in 0..8 {
        let mut registration = base_registration(
            &format!("reg-{i}"),
            &format!("/opt/examplecorp/libra-quota-{i}"),
            "acct-fx9k2m1p",
            "eng-shared-pool", // identical across all 8
        );
        registration.extension_version = format!("0.{i}.0");
        registration.binding_revision = format!("rev-{i}");
        registration.context.selector = format!("profile_{i}");
        registration.context.handle = format!("context_{i}");
        registration.context.principal = format!("principal_{i}");
        registration.context.authority = format!("authority_{i}");
        registration.transport.args = vec![format!("--mode-{i}"), "/tmp/t.json".into()];
        registration.expected[0].id = format!("constraint_{i}");
        registration.expected[0].window_uuid = format!("20000000-0000-4000-8000-00000000000{i}");
        registration.expected[0].provider = format!("provider-label-{i}");
        write_registration(dir.path(), &format!("{i}.json"), &registration);
    }
    let error = load_registrations(dir.path()).unwrap_err();
    assert!(matches!(
        error,
        RegistrationError::DuplicatePoolBinding { .. }
    ));
}

#[test]
fn attack1_control_changing_only_alias_in_seven_of_eight_loads_all_eight() {
    let dir = tempfile::tempdir().unwrap();
    for i in 0..8 {
        let mut registration = base_registration(
            &format!("reg-{i}"),
            &format!("/opt/examplecorp/libra-quota-{i}"),
            "acct-fx9k2m1p",
            if i == 0 {
                "eng-shared-pool".to_string()
            } else {
                format!("eng-shared-pool-{i}")
            }
            .as_str(),
        );
        registration.expected[0].id = format!("constraint_{i}");
        write_registration(dir.path(), &format!("{i}.json"), &registration);
    }
    let loaded = load_registrations(dir.path()).unwrap();
    assert_eq!(loaded.len(), 8);
}

#[test]
fn attack1_control_each_of_the_eight_loads_alone() {
    for i in 0..8 {
        let dir = tempfile::tempdir().unwrap();
        let registration = base_registration(
            &format!("reg-{i}"),
            &format!("/opt/examplecorp/libra-quota-{i}"),
            "acct-fx9k2m1p",
            "eng-shared-pool",
        );
        write_registration(dir.path(), "only.json", &registration);
        let loaded = load_registrations(dir.path()).unwrap();
        assert_eq!(loaded.len(), 1);
    }
}

// ---------------------------------------------------------------------
// 2. Field-isolation property: every field of Registration/
//    ExpectedConstraint EXCEPT account_fingerprint and pool_alias must
//    still collide into one pool when it's the only field that
//    differs. Differing only in account_fingerprint, or only in
//    pool_alias, must load separately.
//
//    Excluded from the table, with reasons: `enabled` (a disabled
//    registration is exempt from uniqueness by design — see
//    `registration::tests::disabled_duplicate_is_allowed`);
//    `transport.environment` (validate() forces exactly C.UTF-8);
//    `transport.kind` and `trust_mode` (single-variant enums, nothing
//    else to vary to).
// ---------------------------------------------------------------------

/// Destructures both structs exhaustively (no `..`) purely so that
/// adding a new field to either one fails this file to compile — a
/// forcing function to add a new table row above, not a real check on
/// its own.
#[test]
fn exhaustiveness_guard_new_fields_must_be_added_to_the_isolation_table() {
    let r = base_registration("x", "/bin/x", "fp", "alias");
    let Registration {
        registration_id: _,
        extension_id: _,
        extension_version: _,
        enabled: _,
        transport:
            Transport {
                kind: _,
                executable: _,
                args: _,
                environment: _,
            },
        context:
            RegisteredContext {
                selector: _,
                handle: _,
                principal: _,
                authority: _,
            },
        expected,
        binding_revision: _,
        binding_verified_at: _,
        scope_configured_at: _,
        timeout_ms: _,
        trust_mode: _,
        observation_confidence: _,
        approval:
            Approval {
                approved: _,
                revision: _,
                executable_sha256: _,
                manifest_sha256: _,
                launch_tuple_sha256: _,
                runtime_files: _,
                endpoint_owner: _,
                consent:
                    Consent {
                        consented_by: _,
                        recorded_at_reference: _,
                    },
            },
    } = r;
    let ExpectedConstraint {
        id: _,
        scope: _,
        metric: _,
        unit: _,
        window_uuid: _,
        provider: _,
        account_fingerprint: _,
        pool_alias: _,
        max_staleness_secs: _,
        window_length_secs: _,
        capacity_ceiling: _,
    } = expected.into_iter().next().unwrap();
}

type RegistrationMutation = Box<dyn Fn(&mut Registration)>;

fn assert_collides(mutate: impl Fn(&mut Registration)) {
    let dir = tempfile::tempdir().unwrap();
    let a = base_registration("reg-a", "/bin/a", "fp-shared", "alias-shared");
    let mut b = base_registration("reg-a", "/bin/a", "fp-shared", "alias-shared");
    mutate(&mut b);
    write_registration(dir.path(), "a.json", &a);
    write_registration(dir.path(), "b.json", &b);
    let error = load_registrations(dir.path()).unwrap_err();
    assert!(
        matches!(error, RegistrationError::DuplicatePoolBinding { .. }),
        "mutation must still collide into one pool"
    );
}

#[test]
fn attack2_field_isolation_table() {
    let mutations: Vec<(&str, RegistrationMutation)> = vec![
        (
            "registration_id",
            Box::new(|r: &mut Registration| r.registration_id = "reg-b".into()),
        ),
        (
            "extension_id",
            Box::new(|r: &mut Registration| r.extension_id = "other.ext".into()),
        ),
        (
            "extension_version",
            Box::new(|r: &mut Registration| r.extension_version = "9.9.9".into()),
        ),
        (
            "transport.executable",
            Box::new(|r: &mut Registration| r.transport.executable = "/bin/different".into()),
        ),
        (
            "transport.args",
            Box::new(|r: &mut Registration| r.transport.args = vec!["different".into()]),
        ),
        (
            "context.selector",
            Box::new(|r: &mut Registration| r.context.selector = "profile_z".into()),
        ),
        (
            "context.handle",
            Box::new(|r: &mut Registration| r.context.handle = "context_z".into()),
        ),
        (
            "context.principal",
            Box::new(|r: &mut Registration| r.context.principal = "principal_z".into()),
        ),
        (
            "context.authority",
            Box::new(|r: &mut Registration| r.context.authority = "authority_z".into()),
        ),
        (
            "expected[0].id",
            Box::new(|r: &mut Registration| r.expected[0].id = "different_id".into()),
        ),
        (
            "expected[0].scope",
            Box::new(|r: &mut Registration| {
                r.expected[0].scope = Scope::Team {
                    handle: "team_z".into(),
                }
            }),
        ),
        (
            "expected[0].metric",
            Box::new(|r: &mut Registration| r.expected[0].metric = Metric::Requests),
        ),
        (
            "expected[0].unit",
            Box::new(|r: &mut Registration| {
                r.expected[0].unit = Unit::Requests;
                // Keep capacity_ceiling's own unit-match invariant
                // satisfied so this mutation alone doesn't also trip
                // CapacityCeilingUnitMismatch at validate() time.
                r.expected[0].capacity_ceiling = None;
            }),
        ),
        (
            "expected[0].window_uuid",
            Box::new(|r: &mut Registration| r.expected[0].window_uuid = "different-uuid".into()),
        ),
        (
            "expected[0].provider",
            Box::new(|r: &mut Registration| r.expected[0].provider = "different-provider".into()),
        ),
        (
            "expected[0].max_staleness_secs",
            Box::new(|r: &mut Registration| r.expected[0].max_staleness_secs = 60),
        ),
        (
            "expected[0].window_length_secs",
            Box::new(|r: &mut Registration| r.expected[0].window_length_secs = 3_600),
        ),
        (
            "expected[0].capacity_ceiling",
            Box::new(|r: &mut Registration| {
                r.expected[0].capacity_ceiling = Some(CapacityCeiling {
                    value: Quantity::parse("1").unwrap(),
                    unit: r.expected[0].unit.clone(),
                })
            }),
        ),
        (
            "binding_revision",
            Box::new(|r: &mut Registration| r.binding_revision = "rev-different".into()),
        ),
        (
            "binding_verified_at",
            Box::new(|r: &mut Registration| r.binding_verified_at = "2027-02-01T00:00:00Z".into()),
        ),
        (
            "scope_configured_at",
            Box::new(|r: &mut Registration| r.scope_configured_at = "2027-02-01T00:00:00Z".into()),
        ),
        (
            "timeout_ms",
            Box::new(|r: &mut Registration| r.timeout_ms = 1_000),
        ),
        (
            "observation_confidence",
            Box::new(|r: &mut Registration| {
                r.observation_confidence = ObservationConfidence::Medium
            }),
        ),
        (
            "approval.approved",
            Box::new(|r: &mut Registration| r.approval.approved = false),
        ),
        (
            "approval.revision",
            Box::new(|r: &mut Registration| r.approval.revision = "rev-different".into()),
        ),
        (
            "approval.executable_sha256",
            Box::new(|r: &mut Registration| {
                r.approval.executable_sha256 = "sha256:different".into()
            }),
        ),
        (
            "approval.manifest_sha256",
            Box::new(|r: &mut Registration| r.approval.manifest_sha256 = "sha256:different".into()),
        ),
        (
            "approval.launch_tuple_sha256",
            Box::new(|r: &mut Registration| {
                r.approval.launch_tuple_sha256 = "sha256:different".into()
            }),
        ),
        (
            "approval.runtime_files",
            Box::new(|r: &mut Registration| {
                r.approval.runtime_files = vec![RuntimeFileDigest {
                    path: "/opt/examplecorp/lib.so".into(),
                    sha256: "sha256:anything".into(),
                }]
            }),
        ),
        (
            "approval.endpoint_owner",
            Box::new(|r: &mut Registration| {
                r.approval.endpoint_owner = "other-owner@examplecorp.invalid".into()
            }),
        ),
        (
            "approval.consent.consented_by",
            Box::new(|r: &mut Registration| {
                r.approval.consent.consented_by = "someone-else".into()
            }),
        ),
        (
            "approval.consent.recorded_at_reference",
            Box::new(|r: &mut Registration| {
                r.approval.consent.recorded_at_reference = "ticket:DIFFERENT".into()
            }),
        ),
    ];
    for (name, mutate) in mutations {
        let before =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| assert_collides(&mutate)));
        assert!(
            before.is_ok(),
            "field {name:?} must still collide when changed alone"
        );
    }
}

#[test]
fn attack2_control_differing_only_in_account_fingerprint_loads_separately() {
    let dir = tempfile::tempdir().unwrap();
    let a = base_registration("reg-a", "/bin/a", "fp-a", "alias-shared");
    let b = base_registration("reg-b", "/bin/a", "fp-b", "alias-shared");
    write_registration(dir.path(), "a.json", &a);
    write_registration(dir.path(), "b.json", &b);
    let loaded = load_registrations(dir.path()).unwrap();
    assert_eq!(loaded.len(), 2);
}

#[test]
fn attack2_control_differing_only_in_pool_alias_loads_separately() {
    let dir = tempfile::tempdir().unwrap();
    let a = base_registration("reg-a", "/bin/a", "fp-shared", "alias-a");
    let b = base_registration("reg-b", "/bin/a", "fp-shared", "alias-b");
    write_registration(dir.path(), "a.json", &a);
    write_registration(dir.path(), "b.json", &b);
    let loaded = load_registrations(dir.path()).unwrap();
    assert_eq!(loaded.len(), 2);
}

// ---------------------------------------------------------------------
// 3. Concurrency safety: 8 threads observing one approved registration
//    concurrently must all succeed, all see the same limit string
//    verbatim, and each result's request_id must equal what its own
//    thread sent — plus an anti-replay control proving one session's
//    frame can never be accepted as another session's response.
//
//    This is NOT the "500 x N sessions must never multiply into N
//    pools" property (that's attack1's job, via the registration-file
//    route, which genuinely feeds `load_registrations` multiple
//    registrations). All 8 threads below observe the *same single*
//    approved `Registration` value, so there is only ever one
//    `PoolId` in play by construction — a prior version of this test
//    computed a `PoolId` per thread and asserted the resulting set's
//    length was 1, which could never have failed regardless of what
//    the pool-uniqueness code does, and has been removed.
// ---------------------------------------------------------------------

#[test]
fn attack3_eight_concurrent_sessions_are_all_correct_and_never_corrupt_each_others_frames() {
    let _guard = SUBPROCESS_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let script = copy_script(dir.path());
    // 120 used / 500 limit — the fixture reports one shared pool's
    // real state; no capture path (shared across threads, per the
    // design's own warning against a shared capture file).
    let template_body = r#"{"protocol_version":"libra.quota-observation/0.1","request_id":"__REQUEST_ID__","extension_id":"examplecorp.quota","extension_version":"0.1.0","result":{"context":{"selector":"profile_a","handle":"context_a","principal":"principal_a","epoch":"epoch_pool"},"authority":"authority_a","coverage":"complete","constraints":[{"id":"tokens_month","scope":{"kind":"principal","handle":"principal_a"},"metric":"total_tokens","unit":{"kind":"tokens"},"window":{"kind":"opaque"},"reading":{"kind":"used","value":"120","limit":"500"},"observed_at":"2027-01-18T12:00:00Z","valid_until":"2027-01-18T12:01:00Z","upstream_enforcement":"hard","account_fingerprint":"acct-fx9k2m1p"}],"missing":[]}}"#;
    let template = write_template(dir.path(), "pool.json", template_body);
    let registration = sample_registration(
        &script,
        "ok",
        &template,
        None,
        "tokens_month",
        "acct-fx9k2m1p",
        "eng-shared-pool",
    );

    let results: Vec<_> = thread::scope(|scope| {
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let registration = &registration;
                scope.spawn(move || common::observe(registration, common::fixed_now()))
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    assert_eq!(results.len(), 8);
    for observed in &results {
        let ok = observed.result.as_ref().unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(ok.bound.constraints.len(), 1);
        match &ok.bound.constraints[0].reading {
            libra_governor_quota_observation::wire::Reading::Used { limit, .. } => {
                assert_eq!(
                    limit.as_str(),
                    "500",
                    "the limit string must be verbatim \"500\""
                );
            }
            _other => panic!("expected Used, got a different Reading variant"),
        }
    }

    for observed in &results {
        // Each observation's own request_id is exactly what its own
        // thread sent — never another thread's.
        assert!(!observed.raw_response.is_empty());
        let body = String::from_utf8(observed.raw_response.clone()).unwrap();
        assert!(body.contains(&observed.request.request_id));
    }
}

#[test]
fn attack3_anti_replay_control_one_sessions_frame_is_rejected_as_another_sessions_response() {
    let _guard = SUBPROCESS_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let script = copy_script(dir.path());
    let template_body = std::fs::read_to_string(fixture_path("a-response.json")).unwrap();
    let template = write_template(dir.path(), "a.json", template_body.trim());
    let registration = sample_registration(
        &script,
        "ok",
        &template,
        None,
        "tokens_month",
        "acct-fx9k2m1p",
        "eng-shared-pool",
    );

    // Session A's real round trip, captured.
    let session_a = observe(&registration, common::fixed_now());
    let response_a: libra_governor_quota_observation::wire::SnapshotResponse =
        runner::parse_single_frame(&session_a.raw_response).unwrap();
    assert_eq!(response_a.request_id, session_a.request.request_id);

    // Session B's own request — never sent, built only to demonstrate
    // that the host-side replay guard rejects A's frame as B's answer.
    let request_b = SnapshotRequest {
        protocol_version: session_a.request.protocol_version.clone(),
        request_id: unique_request_id("session-b"),
        operation: SnapshotOperation::Snapshot,
        context_selector: session_a.request.context_selector.clone(),
        constraint_ids: session_a.request.constraint_ids.clone(),
        max_age_seconds: session_a.request.max_age_seconds,
        budget_ms: session_a.request.budget_ms,
    };
    let error =
        freshness::verify_response_identifiers(&request_b, &response_a, &registration).unwrap_err();
    assert!(matches!(error, FreshnessError::RequestIdMismatch { .. }));
}

// ---------------------------------------------------------------------
// 4. A disabled duplicate can't be smuggled in: it loads today (by
//    design — disabled registrations are exempt from the uniqueness
//    check), but invoke() still refuses it as-stored, and flipping
//    `enabled` in memory without re-approval is refused at the
//    manifest gate rather than silently succeeding.
//
//    This does NOT, by itself, prove the manifest digest covers
//    `enabled` specifically: the disabled copy below is stored with a
//    blank `manifest_sha256`, so the second assertion would pass the
//    same way for ANY manifest mismatch, covered field or not — it
//    can't distinguish "enabled is in the digest" from "the digest is
//    simply wrong". That property can't be proven through this
//    crate's public API at all (`verify_identity` returns
//    `IdentityError::Disabled` before it ever measures a manifest for
//    a disabled registration, and the black-box `approve()` helper
//    refuses to run against anything not already enabled); it's
//    proven instead by
//    `identity::tests::flipping_enabled_without_reapproval_trips_the_manifest_digest`
//    in `src/identity.rs`, which has the crate-internal access needed
//    to compute real digests while `enabled == false`.
// ---------------------------------------------------------------------

#[test]
fn attack4_disabled_duplicate_loads_alongside_the_enabled_original() {
    let dir = tempfile::tempdir().unwrap();
    let a = base_registration("reg-a", "/bin/a", "fp-shared", "pool-shared");
    let mut b = base_registration("reg-b-smuggled", "/bin/b", "fp-shared", "pool-shared");
    b.enabled = false;
    write_registration(dir.path(), "a.json", &a);
    write_registration(dir.path(), "b.json", &b);
    let loaded = load_registrations(dir.path()).unwrap();
    assert_eq!(
        loaded.len(),
        2,
        "a disabled duplicate is exempt from the uniqueness check today"
    );
}

#[test]
fn attack4_invoke_refuses_the_disabled_copy_and_flipping_enabled_in_memory_is_refused_at_the_manifest_gate(
) {
    let _guard = SUBPROCESS_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let script = copy_script(dir.path());
    let template_body = std::fs::read_to_string(fixture_path("a-response.json")).unwrap();
    let template = write_template(dir.path(), "a.json", template_body.trim());

    // Get the real, correctly-measured executable digest via a
    // throwaway enabled registration — `approve()` is the only way to
    // learn this value (digest_file is pub(crate)), and the digest
    // itself depends only on the executable's bytes, never on
    // `enabled`, so it is equally correct for the disabled copy below.
    let throwaway = common::approve(sample_registration(
        &script,
        "ok",
        &template,
        None,
        "tokens_month",
        "acct-fx9k2m1p",
        "eng-shared-pool",
    ));
    let correct_executable_sha256 = throwaway.approval.executable_sha256.clone();

    // Registration B, saved disabled, with the correct executable
    // digest but a manifest/launch-tuple digest that was never
    // computed for *any* state (blank) — approval.approved is true
    // regardless, since `enabled` and `approval.approved` are
    // independent fields.
    let mut disabled_as_stored = sample_registration(
        &script,
        "ok",
        &template,
        None,
        "tokens_month",
        "acct-fx9k2m1p",
        "eng-shared-pool",
    );
    disabled_as_stored.enabled = false;
    disabled_as_stored.approval.executable_sha256 = correct_executable_sha256;
    disabled_as_stored.approval.manifest_sha256 = String::new();
    disabled_as_stored.approval.launch_tuple_sha256 = String::new();

    let cancel = AtomicBool::new(false);
    let request = br#"{"protocol_version":"libra.quota-observation/0.1","request_id":"attack4","operation":"snapshot","context_selector":"profile_a","constraint_ids":["tokens_month"],"max_age_seconds":300,"budget_ms":3000}
"#;
    let error = runner::invoke(&disabled_as_stored, request, &cancel).unwrap_err();
    assert!(matches!(
        error,
        InvokeError::Identity(IdentityError::Disabled)
    ));

    // Flip enabled=true in memory only, without re-approving.
    let mut flipped = disabled_as_stored.clone();
    flipped.enabled = true;
    let error = runner::invoke(&flipped, request, &cancel).unwrap_err();
    assert!(
        matches!(
            error,
            InvokeError::Identity(IdentityError::ManifestDigestMismatch { .. })
        ),
        "a disabled copy whose manifest digest was never genuinely computed (blank, as stored) \
         must still be refused at the manifest gate when flipped to enabled in memory, never \
         silently accepted and never an executable mismatch (the executable digest was \
         deliberately set correctly to isolate this) — note this does not by itself prove the \
         digest covers `enabled`, see this test's section comment and \
         `identity::tests::flipping_enabled_without_reapproval_trips_the_manifest_digest` for that"
    );
}

// ---------------------------------------------------------------------
// 5. Wrong account: one of two constraints reports a different
//    account's fingerprint — coverage downgrades to Partial, the
//    mismatched constraint moves to missing with
//    AccountFingerprintMismatch, and its numeric value is absent from
//    bound constraints. Echo guard: the request this host sent never
//    carries any expected fingerprint (the wire protocol has no field
//    for one), so an extension could never have reflected it back.
// ---------------------------------------------------------------------

#[test]
fn attack5_wrong_account_fingerprint_is_rehomed_to_missing_and_never_echoed_in_the_request() {
    let _guard = SUBPROCESS_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let script = copy_script(dir.path());
    // Context/authority match `sample_registration`'s default
    // (`profile_a`/`context_a`/`principal_a`/`authority_a`) rather than
    // the arbitrary `profile_b`/`authority_b` this fixture used to
    // carry: HORO-1854 B3's whole-response `verify_context_binding`
    // check now rejects the entire response outright on a
    // context/authority mismatch, which would otherwise hide the
    // account-fingerprint-specific behavior this test exists to
    // isolate behind an unrelated, earlier rejection.
    let template_body = r#"{"protocol_version":"libra.quota-observation/0.1","request_id":"__REQUEST_ID__","extension_id":"examplecorp.quota","extension_version":"0.1.0","result":{"context":{"selector":"profile_a","handle":"context_a","principal":"principal_a","epoch":"epoch_c"},"authority":"authority_a","coverage":"complete","constraints":[{"id":"team_budget","scope":{"kind":"team","handle":"team_a"},"metric":"spend","unit":{"kind":"usd_cents"},"window":{"kind":"opaque"},"reading":{"kind":"used","value":"25000","limit":"100000"},"observed_at":"2027-01-18T12:00:00Z","upstream_enforcement":"soft","account_fingerprint":"acct-tm3qz8x4"},{"id":"requests_hour","scope":{"kind":"team","handle":"team_a"},"metric":"requests","unit":{"kind":"requests"},"window":{"kind":"opaque"},"reading":{"kind":"remaining","value":"120","limit":"1000"},"observed_at":"2027-01-18T12:00:00Z","upstream_enforcement":"hard","account_fingerprint":"acct-WRONG-ACCOUNT"}],"missing":[]}}"#;
    let template = write_template(dir.path(), "wrong-account.json", template_body);
    let capture = dir.path().join("capture.json");

    let mut registration = sample_registration(
        &script,
        "ok",
        &template,
        Some(&capture),
        "team_budget",
        "acct-tm3qz8x4",
        "eng-team-pool",
    );
    // `sample_expected`'s defaults (`Scope::Principal`/`Metric::TotalTokens`/
    // `Unit::Tokens`) model the `tokens_month` fixture, not this
    // fixture's `team_budget`/`requests_hour` shape — corrected here so
    // the new unit/metric/scope binding checks (HORO-1854 B3) don't
    // fire on a mismatch this test isn't exercising, which would mask
    // the account-fingerprint mismatch under test.
    registration.expected[0].scope = Scope::Team {
        handle: "team_a".into(),
    };
    registration.expected[0].metric = Metric::Spend;
    registration.expected[0].unit = Unit::UsdCents;
    // `sample_expected`'s default `capacity_ceiling` is `Some(..unit:
    // Tokens)`; `Registration::validate` rejects a ceiling whose unit
    // disagrees with its own entry's `unit` (`CapacityCeilingUnitMismatch`),
    // so it must be cleared here rather than left pointing at the
    // now-wrong `Tokens` unit.
    registration.expected[0].capacity_ceiling = None;
    let mut requests_hour_expected =
        sample_expected("requests_hour", "acct-tm3qz8x4", "eng-team-pool-requests");
    requests_hour_expected.scope = Scope::Team {
        handle: "team_a".into(),
    };
    requests_hour_expected.metric = Metric::Requests;
    requests_hour_expected.unit = Unit::Requests;
    requests_hour_expected.capacity_ceiling = None;
    registration.expected.push(requests_hour_expected);
    let registration = common::approve(registration);

    let observed = common::observe(&registration, common::fixed_now());
    let ok = observed.result.unwrap();
    assert_eq!(
        ok.bound.coverage,
        libra_governor_quota_observation::wire::Coverage::Partial
    );
    assert_eq!(ok.bound.constraints.len(), 1);
    assert_eq!(ok.bound.constraints[0].id, "team_budget");
    assert_eq!(ok.bound.missing.len(), 1);
    assert_eq!(ok.bound.missing[0].id, "requests_hour");
    assert_eq!(
        ok.bound.missing[0].reason,
        libra_governor_quota_observation::wire::MissingReason::AccountFingerprintMismatch
    );
    // The mismatched constraint's numeric value (120/1000) must be
    // absent from bound.constraints entirely.
    assert!(!ok.bound.constraints.iter().any(|c| c.id == "requests_hour"));

    let captured_request = std::fs::read_to_string(&capture).unwrap();
    assert!(!captured_request.contains("acct-tm3qz8x4"));
    assert!(!captured_request.contains("acct-WRONG-ACCOUNT"));
}

// ---------------------------------------------------------------------
// 6. Host identity is not promoted to a session/task identity. This
//    crate has no session concept at all (structural-only check): the
//    child process receives nothing from this host process's
//    environment beyond LANG/LC_ALL, and the host's own captured
//    request carries exactly the SnapshotRequest's 7 wire fields, none
//    of them account/pool identity.
//
//    An earlier version of this test also asserted that `pool_x` and
//    `pool_y` — each built directly from `registration_x`/
//    `registration_y`'s own literal `account_fingerprint`/`pool_alias`
//    constructor arguments, never from anything in either response —
//    compared equal. Since both registrations were constructed with
//    the identical literal pool identity to begin with, that assertion
//    could never have failed regardless of whether the response could
//    influence `PoolId` derivation, and has been removed; this test no
//    longer makes any claim about response-derived identity.
// ---------------------------------------------------------------------

#[test]
fn attack6_host_environment_never_reaches_the_child_and_the_captured_request_carries_no_identity_fields(
) {
    let _guard = SUBPROCESS_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let script = copy_script(dir.path());

    // SAFETY: mutated only while holding SUBPROCESS_SERIAL, which
    // every subprocess-spawning test in this crate's test binaries
    // also holds for its duration.
    unsafe {
        std::env::set_var("LIBRA_SESSION_ID", "canary-session-id-not-real");
        std::env::set_var("FAKE_API_TOKEN", "canary-not-a-secret");
    }

    let template_epoch_x = r#"{"protocol_version":"libra.quota-observation/0.1","request_id":"__REQUEST_ID__","extension_id":"examplecorp.quota","extension_version":"0.1.0","result":{"context":{"selector":"profile_a","handle":"context_a","principal":"principal_a","epoch":"epoch_x"},"authority":"authority_a","coverage":"complete","constraints":[{"id":"tokens_month","scope":{"kind":"principal","handle":"principal_a"},"metric":"total_tokens","unit":{"kind":"tokens"},"window":{"kind":"opaque"},"reading":{"kind":"undisclosed"},"observed_at":"2027-01-18T12:00:00Z","upstream_enforcement":"unknown","account_fingerprint":"acct-fx9k2m1p"}],"missing":[]}}"#;
    let template_epoch_y = template_epoch_x.replace("epoch_x", "epoch_y");
    let path_x = write_template(dir.path(), "epoch_x.json", template_epoch_x);
    let path_y = write_template(dir.path(), "epoch_y.json", &template_epoch_y);

    let capture_x = dir.path().join("env_x.txt");
    let capture_y = dir.path().join("env_y.txt");
    let capture_req_x = dir.path().join("req_x.txt");

    let registration_x = sample_registration(
        &script,
        "env_probe",
        &path_x,
        Some(&capture_x),
        "tokens_month",
        "acct-fx9k2m1p",
        "eng-shared-pool",
    );
    let registration_y = sample_registration(
        &script,
        "env_probe",
        &path_y,
        Some(&capture_y),
        "tokens_month",
        "acct-fx9k2m1p",
        "eng-shared-pool",
    );

    let observed_x = common::observe(&registration_x, common::fixed_now());
    let observed_y = common::observe(&registration_y, common::fixed_now());
    assert_ne!(observed_x.request.request_id, observed_y.request.request_id);

    let ok_x = observed_x.result.unwrap();
    let ok_y = observed_y.result.unwrap();
    assert_eq!(ok_x.bound.context.epoch, "epoch_x");
    assert_eq!(ok_y.bound.context.epoch, "epoch_y");

    // Environment allowlist, empirically confirmed on this platform
    // under env_clear(): only LANG, LC_ALL, PWD, SHLVL and `_` (the
    // last-command path env sets for itself) ever appear.
    let allowed: HashSet<&str> = ["LANG", "LC_ALL", "PWD", "SHLVL", "_"]
        .into_iter()
        .collect();
    for capture in [&capture_x, &capture_y] {
        let dump = std::fs::read_to_string(capture).unwrap();
        let mut seen = HashMap::new();
        for line in dump.lines() {
            if let Some((key, value)) = line.split_once('=') {
                seen.insert(key.to_string(), value.to_string());
            }
        }
        assert_eq!(seen.get("LANG").map(String::as_str), Some("C.UTF-8"));
        assert_eq!(seen.get("LC_ALL").map(String::as_str), Some("C.UTF-8"));
        for forbidden in ["HOME", "USER", "PATH", "LIBRA_SESSION_ID", "FAKE_API_TOKEN"] {
            assert!(
                !seen.contains_key(forbidden),
                "{forbidden} must never reach the child"
            );
        }
        for key in seen.keys() {
            assert!(
                allowed.contains(key.as_str()),
                "unexpected env key leaked to child: {key}"
            );
        }
    }

    // The captured request's own key set is exactly the 7
    // SnapshotRequest fields — rebuild registration_x with a capture
    // path on the request itself to check this precisely.
    let registration_x_captured = {
        let mut r = registration_x.clone();
        // "ok", not "env_probe": env_probe redirects the capture path
        // to an /usr/bin/env dump instead of the raw request bytes.
        r.transport.args = vec![
            "ok".to_string(),
            path_x.to_str().unwrap().to_string(),
            capture_req_x.to_str().unwrap().to_string(),
        ];
        common::approve(r)
    };
    common::observe(&registration_x_captured, common::fixed_now());
    let captured_request: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&capture_req_x).unwrap()).unwrap();
    let mut keys: Vec<&str> = captured_request
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec![
            "budget_ms",
            "constraint_ids",
            "context_selector",
            "max_age_seconds",
            "operation",
            "protocol_version",
            "request_id",
        ]
    );

    unsafe {
        std::env::remove_var("LIBRA_SESSION_ID");
        std::env::remove_var("FAKE_API_TOKEN");
    }
}

// ---------------------------------------------------------------------
// 7. Stale/partial/auth-required never looks usable.
// ---------------------------------------------------------------------

fn error_template(code: &str) -> String {
    format!(
        r#"{{"protocol_version":"libra.quota-observation/0.1","request_id":"__REQUEST_ID__","extension_id":"examplecorp.quota","extension_version":"0.1.0","error":{{"code":"{code}","retry":"never"}}}}"#
    )
}

#[test]
fn attack7_every_error_code_yields_extension_error_and_no_bound_result() {
    let _guard = SUBPROCESS_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let script = copy_script(dir.path());
    let codes = [
        ("auth_required", ErrorCode::AuthRequired),
        ("permission_denied", ErrorCode::PermissionDenied),
        ("unavailable", ErrorCode::Unavailable),
        ("timeout", ErrorCode::Timeout),
        ("incompatible_protocol", ErrorCode::IncompatibleProtocol),
        ("unsupported_capability", ErrorCode::UnsupportedCapability),
        ("invalid_configuration", ErrorCode::InvalidConfiguration),
        ("rate_limited", ErrorCode::RateLimited),
        ("context_changed", ErrorCode::ContextChanged),
    ];
    assert_eq!(codes.len(), 9);
    for (wire_name, expected_code) in codes {
        let template = write_template(
            dir.path(),
            &format!("{wire_name}.json"),
            &error_template(wire_name),
        );
        let registration = sample_registration(
            &script,
            "ok",
            &template,
            None,
            "tokens_month",
            "acct-fx9k2m1p",
            "eng-shared-pool",
        );
        let observed = observe(&registration, common::fixed_now());
        match observed.result {
            Err(ObserveError::ExtensionError(code)) => assert_eq!(code, expected_code),
            other => {
                let _ = other;
                panic!(
                    "expected ExtensionError({expected_code:?}), got a different observe() outcome"
                )
            }
        }
    }
}

#[test]
fn attack7_all_missing_is_partial_with_zero_constraints() {
    let _guard = SUBPROCESS_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let script = copy_script(dir.path());
    let template_body = r#"{"protocol_version":"libra.quota-observation/0.1","request_id":"__REQUEST_ID__","extension_id":"examplecorp.quota","extension_version":"0.1.0","result":{"context":{"selector":"profile_a","handle":"context_a","principal":"principal_a","epoch":"epoch_b"},"authority":"authority_a","coverage":"partial","constraints":[],"missing":[{"id":"tokens_month","reason":"unavailable"}]}}"#;
    let template = write_template(dir.path(), "all-missing.json", template_body);
    let registration = sample_registration(
        &script,
        "ok",
        &template,
        None,
        "tokens_month",
        "acct-fx9k2m1p",
        "eng-shared-pool",
    );
    let observed = observe(&registration, common::fixed_now());
    let ok = observed.result.unwrap();
    assert_eq!(
        ok.bound.coverage,
        libra_governor_quota_observation::wire::Coverage::Partial
    );
    assert!(ok.bound.constraints.is_empty());
    assert_eq!(ok.bound.missing.len(), 1);
}

#[test]
fn attack7_stale_elapsed_and_future_readings_stay_present_but_not_fresh() {
    let _guard = SUBPROCESS_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let script = copy_script(dir.path());

    // (observed_at, valid_until, context_valid_until) scenarios, each
    // one single field past its limit relative to fixed_now()
    // (2027-01-18T12:00:30Z) while the others stay comfortably valid.
    let scenarios: Vec<(&str, String, Option<&str>, Option<&str>)> = vec![
        (
            "older_than_max_staleness",
            "2027-01-18T11:00:00Z".to_string(), // 1h30m old, max_staleness is 300s
            None,
            None,
        ),
        (
            "valid_until_elapsed",
            "2027-01-18T12:00:00Z".to_string(),
            Some("2027-01-18T12:00:15Z"),
            None,
        ),
        (
            "context_valid_until_elapsed",
            "2027-01-18T12:00:00Z".to_string(),
            None,
            Some("2027-01-18T12:00:15Z"),
        ),
        (
            "observed_at_more_than_5s_future",
            "2027-01-18T12:00:36Z".to_string(), // 6s past fixed_now
            None,
            None,
        ),
    ];

    for (name, observed_at, valid_until, context_valid_until) in scenarios {
        let valid_until_json = match valid_until {
            Some(v) => format!(r#","valid_until":"{v}""#),
            None => String::new(),
        };
        let context_valid_until_json = match context_valid_until {
            Some(v) => format!(r#","context_valid_until":"{v}""#),
            None => String::new(),
        };
        let template_body = format!(
            r#"{{"protocol_version":"libra.quota-observation/0.1","request_id":"__REQUEST_ID__","extension_id":"examplecorp.quota","extension_version":"0.1.0","result":{{"context":{{"selector":"profile_a","handle":"context_a","principal":"principal_a","epoch":"epoch_b"}},"authority":"authority_a"{context_valid_until_json},"coverage":"complete","constraints":[{{"id":"tokens_month","scope":{{"kind":"principal","handle":"principal_a"}},"metric":"total_tokens","unit":{{"kind":"tokens"}},"window":{{"kind":"opaque"}},"reading":{{"kind":"undisclosed"}},"observed_at":"{observed_at}"{valid_until_json},"upstream_enforcement":"unknown","account_fingerprint":"acct-fx9k2m1p"}}],"missing":[]}}}}"#
        );
        let template = write_template(dir.path(), &format!("{name}.json"), &template_body);
        let registration = sample_registration(
            &script,
            "ok",
            &template,
            None,
            "tokens_month",
            "acct-fx9k2m1p",
            "eng-shared-pool",
        );
        let observed = observe(&registration, common::fixed_now());
        let ok = observed.result.unwrap_or_else(|e| panic!("{name}: {e:?}"));
        assert_eq!(
            ok.bound.constraints.len(),
            1,
            "{name}: reading must stay present"
        );
        assert!(
            !*ok.freshness.get("tokens_month").unwrap(),
            "{name}: must not be reported fresh"
        );
    }
}

#[test]
fn attack7_valid_frame_plus_nonzero_exit_yields_no_bound_result() {
    let _guard = SUBPROCESS_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let script = copy_script(dir.path());
    let template_body = std::fs::read_to_string(fixture_path("a-response.json")).unwrap();
    let template = write_template(dir.path(), "a.json", template_body.trim());
    let registration = sample_registration(
        &script,
        "nonzero_after_frame",
        &template,
        None,
        "tokens_month",
        "acct-fx9k2m1p",
        "eng-shared-pool",
    );
    let observed = observe(&registration, common::fixed_now());
    assert!(matches!(
        observed.result.unwrap_err(),
        ObserveError::Invoke(InvokeError::Run(
            libra_governor_quota_observation::runner::RunError::NonZeroExit(Some(7))
        ))
    ));
    assert!(observed.raw_response.is_empty());
}

#[test]
fn attack7_valid_frame_while_grandchild_holds_stdout_times_out_with_no_bound_result() {
    let _guard = SUBPROCESS_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let script = copy_script(dir.path());
    let template_body = std::fs::read_to_string(fixture_path("a-response.json")).unwrap();
    let template = write_template(dir.path(), "a.json", template_body.trim());
    let mut registration = sample_registration(
        &script,
        "frame_then_hold",
        &template,
        None,
        "tokens_month",
        "acct-fx9k2m1p",
        "eng-shared-pool",
    );
    registration.timeout_ms = 300;
    let registration = common::approve(registration);
    let observed = observe(&registration, common::fixed_now());
    assert!(matches!(
        observed.result.unwrap_err(),
        ObserveError::Invoke(InvokeError::Run(
            libra_governor_quota_observation::runner::RunError::Timeout
        ))
    ));
    assert!(observed.raw_response.is_empty());
}
