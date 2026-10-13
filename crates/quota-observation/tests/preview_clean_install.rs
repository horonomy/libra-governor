//! HORO-1857 preview clean-install smoke test.
//!
//! # What this test CAN check today
//!
//! - That the sample extension in
//!   `examples/quota-observation-extension/` actually conforms to the
//!   one-shot stdio protocol it documents: a fresh copy of it, with
//!   nothing but a newly-written registration pointed at it, answers
//!   both `describe` and `snapshot` correctly.
//! - That a freshly-installed-and-approved registration verifies
//!   cleanly (digest pinning works against a real file on disk, not
//!   just in-memory fixtures).
//! - Disable/uninstall/re-register lifecycle behavior: disabling stops
//!   invocation before anything is measured, removing the registration
//!   file makes the pool disappear from `load_registrations` entirely,
//!   and re-registering the same `(account_fingerprint, pool_alias)`
//!   from scratch loads cleanly — proving this crate keeps no stale
//!   `PoolId` binding anywhere outside the registration file itself.
//! - Digest pinning: tampering with the installed executable after
//!   approval is caught as `ExecutableDigestMismatch`.
//!
//! # What this test CANNOT check
//!
//! - **A real operator approval flow.** Step 3 below uses
//!   `common::approve` — the same test-only self-approval loop the
//!   rest of this crate's test suite uses, driven directly through
//!   `identity::verify_identity`'s own mismatch-reported values. This
//!   is **not** a real operator approval workflow; none exists in this
//!   codebase (no CLI command performs registration/approval at all —
//!   see `docs/quota-observation/README.md`'s status banner).
//! - **A real config directory.** The registration directory here is a
//!   throwaway `tempfile::tempdir()`, not wherever a real deployment's
//!   config actually lives.
//! - **Any CLI command**, since none exists for this protocol yet.
//! - **A downloaded artifact against a published checksum.** This test
//!   copies a file already present in this repository checkout; it
//!   never downloads anything or compares against an externally
//!   published checksum (see `docs/quota-observation/preview-delivery.md`
//!   for what that would involve).
//! - **Windows.** This crate has no Windows support at all (HORO-1854's
//!   own PR description states this explicitly) — this test, like the
//!   crate itself, is Unix-only in practice (it does not run under
//!   `#[cfg(windows)]` exclusion directly, but neither does anything it
//!   exercises support that platform).
//! - **"No network."** Nothing in this test asserts the sample
//!   extension makes no network call — that property holds because of
//!   how the sample extension's own code is written (pure `/bin/sh`,
//!   `sed`, `cat`, no network-capable command invoked), not because
//!   this test proves it by observation.

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use libra_governor_quota_observation::identity::IdentityError;
use libra_governor_quota_observation::registration::{
    load_registrations, Approval, Consent, ExpectedConstraint, FixedEnvironment,
    ObservationConfidence, RegisteredContext, Registration, Transport, TransportKind, TrustMode,
};
use libra_governor_quota_observation::runner::{self, InvokeError};
use libra_governor_quota_observation::wire::{
    BootstrapOperation, BootstrapRequest, BootstrapResponse, Coverage, Metric, Scope, Unit,
};

use common::{approve, fixed_now, observe, SUBPROCESS_SERIAL};

/// Copies the whole sample extension directory (script + template +
/// README) into `dest_dir/install`, `chmod`s the script executable, and
/// returns the installed script's absolute path — "the install" step 1
/// calls for.
fn install_sample_extension(dest_dir: &Path) -> PathBuf {
    let source_dir =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/quota-observation-extension");
    let install_dir = dest_dir.join("install");
    fs::create_dir_all(&install_dir).unwrap();
    for entry in fs::read_dir(&source_dir).unwrap() {
        let entry = entry.unwrap();
        let file_name = entry.file_name();
        fs::copy(entry.path(), install_dir.join(&file_name)).unwrap();
    }
    let script = install_dir.join("quota-ext.sh");
    let mut perms = fs::metadata(&script).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&script, perms).unwrap();
    script
}

/// Builds the registration that matches the sample extension's own
/// hardcoded identity set exactly (extension_id, context, and the
/// `tokens_month` constraint's scope/metric/unit/account_fingerprint —
/// see `examples/quota-observation-extension/quota-ext.sh`'s canned
/// snapshot response and `registration.template.json`). Uses obviously
/// fake values everywhere an operator identity is needed
/// (`fp-smoke-0001`, a `*.invalid` endpoint owner), per this ticket's
/// own instruction. `approval.approved` is already `true` here (with
/// blank digest placeholders) because `common::approve` requires that
/// going in — it only fills in digests, it does not flip `approved`.
fn build_registration(executable: &Path) -> Registration {
    Registration {
        registration_id: "reg-smoke-install".into(),
        extension_id: "org.example.sample-quota-ext".into(),
        extension_version: "0.1.0".into(),
        enabled: true,
        transport: Transport {
            kind: TransportKind::OneshotStdio,
            executable: executable.to_str().unwrap().to_owned(),
            args: vec![],
            environment: FixedEnvironment {
                lang: "C.UTF-8".into(),
                lc_all: "C.UTF-8".into(),
            },
        },
        context: RegisteredContext {
            selector: "sample-session".into(),
            handle: "sample-handle".into(),
            principal: "sample-principal".into(),
            authority: "sample-authority".into(),
        },
        expected: vec![ExpectedConstraint {
            id: "tokens_month".into(),
            scope: Scope::Account {
                handle: "sample-account".into(),
            },
            metric: Metric::TotalTokens,
            unit: Unit::Tokens,
            window_uuid: "40000000-0000-4000-8000-000000000001".into(),
            provider: "sample-provider".into(),
            account_fingerprint: "fp-smoke-0001".into(),
            pool_alias: "fp-smoke-pool".into(),
            max_staleness_secs: 300,
            window_length_secs: 2_592_000,
            capacity_ceiling: None,
        }],
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
            runtime_files: vec![],
            endpoint_owner: "owner@example.invalid".into(),
            consent: Consent {
                consented_by: "smoke-test@example.invalid".into(),
                recorded_at_reference: "ticket:HORO-1857-smoke".into(),
            },
        },
    }
}

fn write_registration_file(registration_dir: &Path, file_name: &str, registration: &Registration) {
    fs::write(
        registration_dir.join(file_name),
        serde_json::to_string_pretty(registration).unwrap(),
    )
    .unwrap();
}

fn describe_request_bytes(request_id: &str) -> Vec<u8> {
    let request = BootstrapRequest {
        bootstrap_version: "libra.quota-bootstrap/1".into(),
        request_id: request_id.into(),
        operation: BootstrapOperation::Describe,
        supported_versions: vec!["libra.quota-observation/0.1".into()],
    };
    let mut body = serde_json::to_vec(&request).unwrap();
    body.push(b'\n');
    body
}

#[test]
fn clean_install_describe_snapshot_and_full_lifecycle() {
    let _guard = SUBPROCESS_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().unwrap();
    let registration_dir = temp.path().join("registrations");
    fs::create_dir_all(&registration_dir).unwrap();

    // --- Step 1: install ---
    let script = install_sample_extension(temp.path());

    // --- Step 2: write a registration into a tempdir registration-dir ---
    let registration_file = "sample.json";
    write_registration_file(
        &registration_dir,
        registration_file,
        &build_registration(&script),
    );

    // --- Step 3: load_registrations, then approve (TEST-ONLY SELF-APPROVAL) ---
    //
    // `approve()` here is `tests/common/mod.rs`'s own self-approval
    // loop, which repeatedly calls the crate's public
    // `identity::verify_identity` and copies whichever digest it
    // reports a mismatch for back into `approval.*_sha256`, until it
    // converges. This is NOT a real operator approval flow — no such
    // flow exists anywhere in this codebase yet (no CLI command
    // performs registration or approval). It stands in for "an
    // operator correctly measured and recorded the real digests",
    // which is the only thing `invoke()` itself actually checks.
    let loaded = load_registrations(&registration_dir).unwrap();
    assert_eq!(loaded.len(), 1, "exactly one registration should load");
    let approved_registration = approve(loaded.into_iter().next().unwrap());

    // --- Step 4: invoke describe, parse as BootstrapResponse ---
    let cancel = AtomicBool::new(false);
    let describe_raw = runner::invoke(
        &approved_registration,
        &describe_request_bytes("smoke-describe-1"),
        &cancel,
    )
    .expect("describe should succeed against a freshly-approved, freshly-installed extension");
    let describe_response: BootstrapResponse =
        runner::parse_single_frame(&describe_raw).expect("describe response should parse");
    describe_response
        .validate()
        .expect("describe response should satisfy BootstrapResponse::validate");
    let describe_result = describe_response
        .result
        .expect("describe should report a result, not an error, for a healthy install");
    assert_eq!(describe_result.extension_id, "org.example.sample-quota-ext");
    assert_eq!(describe_result.extension_version, "0.1.0");
    assert!(describe_result
        .capabilities
        .iter()
        .any(|c| c == "quota.snapshot"));

    // --- Step 5: invoke snapshot, apply_account_binding + is_fresh, assert complete coverage ---
    let observed = observe(&approved_registration, fixed_now());
    let observe_ok = observed
        .result
        .expect("snapshot round trip should succeed against a freshly-approved, fresh reading");
    assert_eq!(observe_ok.bound.coverage, Coverage::Complete);
    assert_eq!(observe_ok.bound.constraints.len(), 1);
    assert_eq!(observe_ok.bound.missing.len(), 0);
    assert_eq!(
        observe_ok.freshness.get("tokens_month"),
        Some(&true),
        "the sample's canned reading is fresh relative to tests/common::fixed_now()"
    );

    // --- Step 6a: disable -> IdentityError::Disabled ---
    let mut disabled = approved_registration.clone();
    disabled.enabled = false;
    let error = runner::invoke(
        &disabled,
        &describe_request_bytes("smoke-describe-2"),
        &cancel,
    )
    .unwrap_err();
    assert!(matches!(
        error,
        InvokeError::Identity(IdentityError::Disabled)
    ));

    // --- Step 6b: remove the file -> load_registrations returns empty ---
    fs::remove_file(registration_dir.join(registration_file)).unwrap();
    let loaded_after_removal = load_registrations(&registration_dir).unwrap();
    assert!(
        loaded_after_removal.is_empty(),
        "removing the only registration file should leave the directory loading empty"
    );

    // --- Step 6c: re-register the same pool -> loads clean (no stale PoolId binding) ---
    write_registration_file(
        &registration_dir,
        registration_file,
        &build_registration(&script),
    );
    let reloaded = load_registrations(&registration_dir).unwrap();
    assert_eq!(
        reloaded.len(),
        1,
        "re-registering the identical (account_fingerprint, pool_alias) pair from scratch, \
         after the only prior registration file was deleted, must load cleanly — this crate \
         keeps no PoolId binding anywhere outside the registration file itself"
    );

    // --- Step 6d: tamper the copied executable -> ExecutableDigestMismatch ---
    fs::write(&script, "#!/bin/sh\nexit 1\n").unwrap();
    let error = runner::invoke(
        &approved_registration,
        &describe_request_bytes("smoke-describe-3"),
        &cancel,
    )
    .unwrap_err();
    assert!(matches!(
        error,
        InvokeError::Identity(IdentityError::ExecutableDigestMismatch { .. })
    ));
}
