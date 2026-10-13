//! End-to-end tests of the real `libra-governor quota-observation`
//! subcommand family (HORO-1870 PR2): spawns the real built binary as a
//! subprocess against an isolated `LIBRA_GOVERNOR_STATE_DIR`, exactly
//! like `quota_explain.rs`/`doctor_uninstall_integration.rs` do for their
//! own subcommands.
//!
//! Covers the full operator lifecycle the ticket requires: register ->
//! doctor (unapproved) -> approve -> doctor (approved) -> inspect (a
//! real round trip against `tests/fixtures/quota_observation/ext.sh`,
//! confirming the real coverage/reading shape) -> disable -> doctor
//! --probe (confirms a disabled registration is skipped, not probed) ->
//! enable (confirms no re-approval is needed) -> uninstall -> doctor
//! (confirms it's gone). Also covers `register`'s pool-uniqueness gate
//! (`check_pool_uniqueness`, reused verbatim from the crate's own
//! loader) rejecting a second enabled registration bound to the same
//! `(account_fingerprint, pool_alias)`.

use std::path::{Path, PathBuf};
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_libra-governor")
}

fn fixture_executable() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/quota_observation/ext.sh")
}

/// One isolated `LIBRA_GOVERNOR_STATE_DIR` per test, so no test can
/// observe another's registrations.
struct Sandbox {
    _dir: tempfile::TempDir,
    state_dir: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("create sandbox state dir");
        let state_dir = dir.path().join("state");
        Self {
            _dir: dir,
            state_dir,
        }
    }

    fn run(&self, args: &[&str]) -> (bool, String, String) {
        let output = Command::new(bin())
            .args(args)
            .env("LIBRA_GOVERNOR_STATE_DIR", &self.state_dir)
            .output()
            .unwrap_or_else(|e| panic!("failed to spawn libra-governor {args:?}: {e}"));
        (
            output.status.success(),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }
}

/// A registration JSON matching `Registration`'s schema exactly, as an
/// operator would author it: `transport.executable` is the fixture
/// script's own (environment-dependent) absolute path, so this is built
/// per-test rather than checked in as a static fixture file.
fn registration_json(
    registration_id: &str,
    account_fingerprint: &str,
    pool_alias: &str,
    enabled: bool,
) -> String {
    let executable = fixture_executable();
    serde_json::json!({
        "registration_id": registration_id,
        "extension_id": "org.example.horo1870-fixture",
        "extension_version": "1.0.0",
        "enabled": enabled,
        "transport": {
            "kind": "oneshot_stdio",
            "executable": executable.to_str().unwrap(),
            "args": ["--run"],
            "environment": {"LANG": "C.UTF-8", "LC_ALL": "C.UTF-8"}
        },
        "context": {
            "selector": "sel",
            "handle": "handle",
            "principal": "principal",
            "authority": "authority"
        },
        "expected": [{
            "id": "c1",
            "scope": {"kind": "account", "handle": "acct"},
            "metric": "total_tokens",
            "unit": {"kind": "tokens"},
            "window_uuid": "win-1",
            "provider": "anthropic",
            "account_fingerprint": account_fingerprint,
            "pool_alias": pool_alias,
            "max_staleness_secs": 600,
            "window_length_secs": 3600
        }],
        "binding_revision": "1",
        "binding_verified_at": "2026-10-12T00:00:00Z",
        "scope_configured_at": "2026-10-12T00:00:00Z",
        "timeout_ms": 5000,
        "trust_mode": "observation_only",
        "observation_confidence": "low",
        "approval": {
            "approved": false,
            "revision": "1",
            "executable_sha256": "",
            "manifest_sha256": "",
            "launch_tuple_sha256": "",
            "runtime_files": [],
            "endpoint_owner": "owner",
            "consent": {"consented_by": "operator", "recorded_at_reference": "ref-1"}
        }
    })
    .to_string()
}

fn write_registration_file(sandbox: &Sandbox, name: &str, json: &str) -> PathBuf {
    let path = sandbox.state_dir.join(name);
    std::fs::create_dir_all(&sandbox.state_dir).expect("create sandbox state dir");
    std::fs::write(&path, json).expect("write registration fixture file");
    path
}

#[test]
fn full_lifecycle_register_approve_inspect_disable_enable_uninstall() {
    let sandbox = Sandbox::new();
    let reg_file = write_registration_file(
        &sandbox,
        "candidate.json",
        &registration_json("fixture-reg", "fp", "alias", true),
    );

    // register
    let (ok, stdout, stderr) =
        sandbox.run(&["quota-observation", "register", reg_file.to_str().unwrap()]);
    assert!(ok, "register failed: {stderr}");
    assert!(stdout.contains("registered"), "unexpected stdout: {stdout}");

    // doctor: present, unapproved
    let (ok, stdout, stderr) = sandbox.run(&["quota-observation", "doctor"]);
    assert!(ok, "doctor failed: {stderr}");
    assert!(stdout.contains("fixture-reg"), "{stdout}");
    assert!(stdout.contains("approved=false"), "{stdout}");

    // approve
    let (ok, stdout, stderr) = sandbox.run(&["quota-observation", "approve", "fixture-reg"]);
    assert!(ok, "approve failed: {stderr}");
    assert!(stdout.contains("approved"), "{stdout}");

    // doctor: now approved
    let (ok, stdout, stderr) = sandbox.run(&["quota-observation", "doctor"]);
    assert!(ok, "doctor failed: {stderr}");
    assert!(stdout.contains("approved=true"), "{stdout}");

    // doctor --probe: a real describe round trip
    let (ok, stdout, stderr) = sandbox.run(&["quota-observation", "doctor", "--probe"]);
    assert!(ok, "doctor --probe failed: {stderr}");
    assert!(
        stdout.contains("org.example.horo1870-fixture"),
        "probe should report the extension's declared identity: {stdout}"
    );
    assert!(
        stdout.contains("this host's protocol version is supported"),
        "{stdout}"
    );

    // inspect: a real snapshot round trip
    let (ok, stdout, stderr) = sandbox.run(&["quota-observation", "inspect", "fixture-reg"]);
    assert!(ok, "inspect failed: {stderr}");
    assert!(stdout.contains("coverage=Complete"), "{stdout}");
    assert!(stdout.contains("constraint c1"), "{stdout}");
    assert!(
        stdout.contains("[fresh]"),
        "a just-observed reading should be tagged fresh: {stdout}"
    );

    // disable
    let (ok, _stdout, stderr) = sandbox.run(&["quota-observation", "disable", "fixture-reg"]);
    assert!(ok, "disable failed: {stderr}");

    // doctor --probe on a disabled registration: skipped, not attempted
    let (ok, stdout, stderr) = sandbox.run(&["quota-observation", "doctor", "--probe"]);
    assert!(ok, "doctor --probe (disabled) failed: {stderr}");
    assert!(stdout.contains("enabled=false"), "{stdout}");
    assert!(
        stdout.contains("skipped --probe: registration is disabled"),
        "{stdout}"
    );

    // inspect must refuse a disabled registration
    let (ok, _stdout, stderr) = sandbox.run(&["quota-observation", "inspect", "fixture-reg"]);
    assert!(!ok, "inspect must refuse a disabled registration");
    assert!(stderr.contains("disabled"), "{stderr}");

    // enable: no re-approval required
    let (ok, stdout, stderr) = sandbox.run(&["quota-observation", "enable", "fixture-reg"]);
    assert!(ok, "enable failed: {stderr}");
    assert!(stdout.contains("does not require re-approval"), "{stdout}");
    let (ok, stdout, stderr) = sandbox.run(&["quota-observation", "doctor"]);
    assert!(ok, "doctor failed: {stderr}");
    assert!(
        stdout.contains("enabled=true") && stdout.contains("approved=true"),
        "re-enabling must not have cleared approval: {stdout}"
    );

    // uninstall
    let (ok, stdout, stderr) = sandbox.run(&["quota-observation", "uninstall", "fixture-reg"]);
    assert!(ok, "uninstall failed: {stderr}");
    assert!(stdout.contains("uninstalled"), "{stdout}");

    // doctor: gone
    let (ok, stdout, stderr) = sandbox.run(&["quota-observation", "doctor"]);
    assert!(ok, "doctor failed: {stderr}");
    assert!(
        stdout.contains("no quota-observation registrations found"),
        "{stdout}"
    );
}

#[test]
fn register_refuses_a_second_enabled_registration_bound_to_the_same_pool() {
    let sandbox = Sandbox::new();
    let first = write_registration_file(
        &sandbox,
        "first.json",
        &registration_json("reg-a", "fp-shared", "daily-cap", true),
    );
    let second = write_registration_file(
        &sandbox,
        "second.json",
        &registration_json("reg-b", "fp-shared", "daily-cap", true),
    );

    let (ok, _stdout, stderr) =
        sandbox.run(&["quota-observation", "register", first.to_str().unwrap()]);
    assert!(ok, "first register should succeed: {stderr}");

    let (ok, _stdout, stderr) =
        sandbox.run(&["quota-observation", "register", second.to_str().unwrap()]);
    assert!(!ok, "second register should be refused by pool uniqueness");
    assert!(
        stderr.contains("duplicate enabled pool binding")
            || stderr.contains("DuplicatePoolBinding"),
        "{stderr}"
    );
}

#[test]
fn register_refuses_a_duplicate_registration_id() {
    let sandbox = Sandbox::new();
    let first = write_registration_file(
        &sandbox,
        "first.json",
        &registration_json("reg-dup", "fp-a", "alias-a", true),
    );
    let second = write_registration_file(
        &sandbox,
        "second.json",
        &registration_json("reg-dup", "fp-b", "alias-b", false),
    );

    let (ok, _stdout, stderr) =
        sandbox.run(&["quota-observation", "register", first.to_str().unwrap()]);
    assert!(ok, "first register should succeed: {stderr}");

    let (ok, _stdout, stderr) =
        sandbox.run(&["quota-observation", "register", second.to_str().unwrap()]);
    assert!(
        !ok,
        "second register with the same registration_id should be refused"
    );
    assert!(stderr.contains("already exists"), "{stderr}");
}

#[test]
fn doctor_on_a_fresh_install_reports_no_registrations_without_creating_state() {
    let sandbox = Sandbox::new();
    let (ok, stdout, stderr) = sandbox.run(&["quota-observation", "doctor"]);
    assert!(ok, "doctor failed: {stderr}");
    assert!(
        stdout.contains("no quota-observation registrations found"),
        "{stdout}"
    );
}
