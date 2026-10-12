//! HORO-1855 Phase A — process and identity negatives, driven directly
//! through `crates/quota-observation`'s public `runner::invoke` (not
//! through `common::observe`, which wires the whole snapshot
//! round-trip; these tests exercise transport/identity failures that
//! don't require a well-formed snapshot request at all).
//!
//! See `tests/README.md` for the full P/N/compat matrix.

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use libra_governor_quota_observation::identity::IdentityError;
use libra_governor_quota_observation::runner::{self, InvokeError, RunError};

use common::{copy_script, fixture_path, sample_registration, write_template, SUBPROCESS_SERIAL};

fn describe_request_bytes(request_id: &str) -> Vec<u8> {
    let mut body = format!(
        r#"{{"bootstrap_version":"libra.quota-bootstrap/1","request_id":"{request_id}","operation":"describe","supported_versions":["libra.quota-observation/0.1"]}}"#
    )
    .into_bytes();
    body.push(b'\n');
    body
}

fn snapshot_request_bytes(request_id: &str) -> Vec<u8> {
    let mut body = format!(
        r#"{{"protocol_version":"libra.quota-observation/0.1","request_id":"{request_id}","operation":"snapshot","context_selector":"profile_a","constraint_ids":["tokens_month"],"max_age_seconds":300,"budget_ms":3000}}"#
    )
    .into_bytes();
    body.push(b'\n');
    body
}

// ---------------------------------------------------------------------
// N06 — a 65537-byte response (one byte past the 64 KiB cap)
// ---------------------------------------------------------------------

#[test]
fn n06_oversized_response_is_rejected() {
    let _guard = SUBPROCESS_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let script = copy_script(dir.path());
    // A template whose single string value alone is large enough to
    // push the full frame past MAX_RESPONSE_BYTES (65536), via the
    // "authority" field.
    let oversized_authority = "x".repeat(70_000);
    let template_body = format!(
        r#"{{"protocol_version":"libra.quota-observation/0.1","request_id":"__REQUEST_ID__","extension_id":"examplecorp.quota","extension_version":"0.1.0","result":{{"context":{{"selector":"profile_a","handle":"context_a","principal":"principal_a","epoch":"epoch_b"}},"authority":"{oversized_authority}","coverage":"complete","constraints":[{{"id":"tokens_month","scope":{{"kind":"principal","handle":"principal_a"}},"metric":"total_tokens","unit":{{"kind":"tokens"}},"window":{{"kind":"opaque"}},"reading":{{"kind":"undisclosed"}},"observed_at":"2027-01-18T12:00:00Z","upstream_enforcement":"unknown","account_fingerprint":"acct-fx9k2m1p"}}],"missing":[]}}}}"#
    );
    let template = write_template(dir.path(), "oversized.json", &template_body);
    let registration = sample_registration(
        &script,
        "ok",
        &template,
        None,
        "tokens_month",
        "acct-fx9k2m1p",
        "eng-shared-pool",
    );
    let cancel = AtomicBool::new(false);
    let error = runner::invoke(&registration, &snapshot_request_bytes("n06"), &cancel).unwrap_err();
    assert!(matches!(
        error,
        InvokeError::Run(RunError::ResponseTooLarge)
    ));
}

#[test]
fn n06_control_normal_sized_response_is_accepted() {
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
    let cancel = AtomicBool::new(false);
    let frame =
        runner::invoke(&registration, &snapshot_request_bytes("n06-ctrl"), &cancel).unwrap();
    assert!(!frame.is_empty());
}

// ---------------------------------------------------------------------
// N10/N12/N13 — a leader that produces a valid frame but backgrounds a
// descendant holding stdout open never reaches EOF; the whole owned
// process group must still be killed within the deadline.
// ---------------------------------------------------------------------

#[test]
fn n10_n12_n13_frame_then_hold_times_out_within_its_short_deadline() {
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
    let start = Instant::now();
    let cancel = AtomicBool::new(false);
    let error = runner::invoke(&registration, &snapshot_request_bytes("n10"), &cancel).unwrap_err();
    assert!(matches!(error, InvokeError::Run(RunError::Timeout)));
    assert!(
        start.elapsed() < Duration::from_secs(10),
        "the owned process group must be killed promptly, not left to a 10s default"
    );
}

#[test]
fn n10_control_ok_mode_with_the_same_short_deadline_succeeds() {
    let _guard = SUBPROCESS_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let script = copy_script(dir.path());
    let template_body = std::fs::read_to_string(fixture_path("a-response.json")).unwrap();
    let template = write_template(dir.path(), "a.json", template_body.trim());
    let mut registration = sample_registration(
        &script,
        "ok",
        &template,
        None,
        "tokens_month",
        "acct-fx9k2m1p",
        "eng-shared-pool",
    );
    registration.timeout_ms = 300;
    let registration = common::approve(registration);
    let cancel = AtomicBool::new(false);
    let frame =
        runner::invoke(&registration, &snapshot_request_bytes("n10-ctrl"), &cancel).unwrap();
    assert!(!frame.is_empty());
}

// ---------------------------------------------------------------------
// N11 — a valid frame followed by a non-zero exit code
// ---------------------------------------------------------------------

#[test]
fn n11_valid_frame_then_nonzero_exit_is_a_failure() {
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
    let cancel = AtomicBool::new(false);
    let error = runner::invoke(&registration, &snapshot_request_bytes("n11"), &cancel).unwrap_err();
    assert!(matches!(
        error,
        InvokeError::Run(RunError::NonZeroExit(Some(7)))
    ));
}

#[test]
fn n11_control_ok_mode_exits_zero_and_succeeds() {
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
    let cancel = AtomicBool::new(false);
    let frame =
        runner::invoke(&registration, &snapshot_request_bytes("n11-ctrl"), &cancel).unwrap();
    assert!(!frame.is_empty());
}

// ---------------------------------------------------------------------
// N14 — cancellation during collection
// ---------------------------------------------------------------------

#[test]
fn n14_cancellation_flag_stops_a_hanging_invocation_promptly() {
    let _guard = SUBPROCESS_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let script = copy_script(dir.path());
    let template_body = std::fs::read_to_string(fixture_path("a-response.json")).unwrap();
    let template = write_template(dir.path(), "a.json", template_body.trim());
    let registration = sample_registration(
        &script,
        "frame_then_hold",
        &template,
        None,
        "tokens_month",
        "acct-fx9k2m1p",
        "eng-shared-pool",
    );
    let cancel = Arc::new(AtomicBool::new(false));
    let setter = Arc::clone(&cancel);
    let handle = thread::spawn(move || {
        thread::sleep(Duration::from_millis(100));
        setter.store(true, Ordering::SeqCst);
    });
    let start = Instant::now();
    let error = runner::invoke(&registration, &snapshot_request_bytes("n14"), &cancel).unwrap_err();
    handle.join().unwrap();
    assert!(matches!(error, InvokeError::Run(RunError::Cancelled)));
    assert!(start.elapsed() < Duration::from_secs(10));
}

#[test]
fn n14_control_without_cancellation_ok_mode_succeeds() {
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
    let cancel = AtomicBool::new(false);
    let frame =
        runner::invoke(&registration, &snapshot_request_bytes("n14-ctrl"), &cancel).unwrap();
    assert!(!frame.is_empty());
}

// ---------------------------------------------------------------------
// N15 — the executable changed between describe and snapshot (also
// covers compatibility-matrix row 6: "1.0 | 1.0 describe / changed
// package snapshot -> Identity change; invalidate").
// ---------------------------------------------------------------------

#[test]
fn n15_executable_changed_between_describe_and_snapshot_invalidates_trust() {
    let _guard = SUBPROCESS_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let script = copy_script(dir.path());
    let describe_template_body =
        std::fs::read_to_string(fixture_path("describe-response.json")).unwrap();
    let describe_template =
        write_template(dir.path(), "describe.json", describe_template_body.trim());
    let registration = sample_registration(
        &script,
        "ok",
        &describe_template,
        None,
        "tokens_month",
        "acct-fx9k2m1p",
        "eng-shared-pool",
    );

    // describe succeeds against the originally-approved executable.
    let cancel = AtomicBool::new(false);
    let frame = runner::invoke(
        &registration,
        &describe_request_bytes("n15-describe"),
        &cancel,
    )
    .unwrap();
    assert!(!frame.is_empty());

    // The package changes on disk before the snapshot call — same
    // registration, same approval, no re-approval performed.
    std::fs::write(&script, "#!/bin/sh\nexit 0\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(&script).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script, perms).unwrap();

    let error = runner::invoke(
        &registration,
        &snapshot_request_bytes("n15-snapshot"),
        &cancel,
    )
    .unwrap_err();
    assert!(matches!(
        error,
        InvokeError::Identity(IdentityError::ExecutableDigestMismatch { .. })
    ));
}

#[test]
fn n15_control_unchanged_executable_succeeds_for_both_describe_and_snapshot() {
    let _guard = SUBPROCESS_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let script = copy_script(dir.path());
    let describe_template_body =
        std::fs::read_to_string(fixture_path("describe-response.json")).unwrap();
    let describe_template =
        write_template(dir.path(), "describe.json", describe_template_body.trim());
    let registration = sample_registration(
        &script,
        "ok",
        &describe_template,
        None,
        "tokens_month",
        "acct-fx9k2m1p",
        "eng-shared-pool",
    );
    let cancel = AtomicBool::new(false);
    runner::invoke(
        &registration,
        &describe_request_bytes("n15-ctrl-describe"),
        &cancel,
    )
    .unwrap();
    runner::invoke(
        &registration,
        &snapshot_request_bytes("n15-ctrl-snapshot"),
        &cancel,
    )
    .unwrap();
}

// ---------------------------------------------------------------------
// N09 — stderr flood. Per the crate's own runner docs, stderr is
// discarded (`Stdio::null()`); this is a documented divergence from a
// spec reading of "excess [stderr] aborts" — acceptable because
// nothing downstream of this transport ever reads stderr at all.
// ---------------------------------------------------------------------

#[test]
fn n09_stderr_flood_does_not_abort_a_successful_invocation() {
    let _guard = SUBPROCESS_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let script = copy_script(dir.path());
    let template_body = std::fs::read_to_string(fixture_path("a-response.json")).unwrap();
    let template = write_template(dir.path(), "a.json", template_body.trim());
    let registration = sample_registration(
        &script,
        "stderr_flood",
        &template,
        None,
        "tokens_month",
        "acct-fx9k2m1p",
        "eng-shared-pool",
    );
    let cancel = AtomicBool::new(false);
    let frame = runner::invoke(&registration, &snapshot_request_bytes("n09"), &cancel).unwrap();
    assert!(!frame.is_empty());
}

// ---------------------------------------------------------------------
// N17 — config-loader env injection is rejected: `run_one_shot` calls
// `env_clear()` before spawning, so an env var set in the *test*
// process (simulating an attempt to inject config through the
// environment) can never reach the child, regardless of what the
// registration's `args` reference.
// ---------------------------------------------------------------------

#[test]
fn n17_env_injected_in_the_host_process_never_reaches_the_child() {
    let _guard = SUBPROCESS_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let script = copy_script(dir.path());
    let template_body = std::fs::read_to_string(fixture_path("a-response.json")).unwrap();
    let template = write_template(dir.path(), "a.json", template_body.trim());
    let capture = dir.path().join("env.txt");
    let registration = sample_registration(
        &script,
        "env_probe",
        &template,
        Some(&capture),
        "tokens_month",
        "acct-fx9k2m1p",
        "eng-shared-pool",
    );

    // SAFETY: this process-wide env mutation is made only while
    // holding SUBPROCESS_SERIAL, which every subprocess-spawning test
    // in this crate's test binaries also holds for its duration, so no
    // other thread observes or races this value.
    unsafe {
        std::env::set_var("LIBRA_CONFIG_INJECT", "malicious-config-override");
    }
    let cancel = AtomicBool::new(false);
    let frame = runner::invoke(&registration, &snapshot_request_bytes("n17"), &cancel).unwrap();
    assert!(!frame.is_empty());
    let env_dump = std::fs::read_to_string(&capture).unwrap();
    assert!(!env_dump.contains("LIBRA_CONFIG_INJECT"));
    assert!(!env_dump.contains("malicious-config-override"));
    unsafe {
        std::env::remove_var("LIBRA_CONFIG_INJECT");
    }
}

// ---------------------------------------------------------------------
// Changed argv invalidates trust (manifest digest covers transport.args,
// which is checked before the launch-tuple digest — assert the exact
// variant, not either-or).
// ---------------------------------------------------------------------

#[test]
fn changed_argv_gives_manifest_digest_mismatch_exactly() {
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
    let mut mutated = registration.clone();
    mutated.transport.args.push("--extra-flag".to_string());
    let cancel = AtomicBool::new(false);
    let error = runner::invoke(&mutated, &snapshot_request_bytes("argv"), &cancel).unwrap_err();
    assert!(matches!(
        error,
        InvokeError::Identity(IdentityError::ManifestDigestMismatch { .. })
    ));
}

#[test]
fn changed_argv_control_unmutated_registration_succeeds() {
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
    let cancel = AtomicBool::new(false);
    let frame =
        runner::invoke(&registration, &snapshot_request_bytes("argv-ctrl"), &cancel).unwrap();
    assert!(!frame.is_empty());
}

// ---------------------------------------------------------------------
// Describe round trip through invoke(), parsed as a BootstrapResponse
// ---------------------------------------------------------------------

#[test]
fn describe_through_invoke_parses_as_a_bootstrap_response() {
    let _guard = SUBPROCESS_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let script = copy_script(dir.path());
    let template_body = std::fs::read_to_string(fixture_path("describe-response.json")).unwrap();
    let template = write_template(dir.path(), "describe.json", template_body.trim());
    let registration = sample_registration(
        &script,
        "ok",
        &template,
        None,
        "tokens_month",
        "acct-fx9k2m1p",
        "eng-shared-pool",
    );
    let cancel = AtomicBool::new(false);
    let frame = runner::invoke(
        &registration,
        &describe_request_bytes("describe-1"),
        &cancel,
    )
    .unwrap();
    let response: libra_governor_quota_observation::wire::BootstrapResponse =
        runner::parse_single_frame(&frame).unwrap();
    assert!(response.validate().is_ok());
    assert_eq!(response.request_id, "describe-1");
    let result = response.result.unwrap();
    assert_eq!(result.extension_id, "examplecorp.quota");
    assert_eq!(result.extension_version, "0.1.0");
    assert!(result.capabilities.contains(&"quota.snapshot".to_string()));
}
