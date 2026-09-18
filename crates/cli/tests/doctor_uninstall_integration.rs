//! End-to-end tests of the real `libra-governor` binary's `doctor`,
//! `install`, and `uninstall` subcommands (HORO-1150). Mirrors
//! `hook_cli_integration.rs`'s pattern: spawn the real built binary as a
//! subprocess, drive it with real env vars and a real temp state dir /
//! Claude settings dir, assert on its real stdout/stderr/exit code.
//!
//! # What this file is the automated evidence for
//!
//! - A never-installed machine: `doctor` reports "not installed" and
//!   exits `0` — and, just as importantly, never creates `ledger.sqlite3`
//!   or spawns a daemon merely by being asked to diagnose.
//! - The full `install` -> `doctor` (healthy) -> `uninstall --yes` ->
//!   `doctor` (not installed again) lifecycle the ticket asks for.
//! - `doctor` correctly flags a deliberately corrupt `config.json` and a
//!   missing daemon as unhealthy/absent with a real diagnostic message,
//!   not a fabricated one.
//! - The secret-safety guarantee: a fake credential reference placed in
//!   `config.json` never appears in `doctor`'s stdout or stderr, in
//!   either human or `--json` form — a stronger guarantee than reasoning
//!   about which fields exist, because it survives future field
//!   additions to `DoctorResult`.

use std::process::{Command, Stdio};
use std::sync::Once;
use std::time::Duration;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_libra-governor")
}

/// Same one-time warm-up rationale as `hook_cli_integration.rs`.
fn warm_up_binary() {
    static WARM_UP: Once = Once::new();
    WARM_UP.call_once(|| {
        let _ = Command::new(bin())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    });
}

struct Sandbox {
    _state_parent: tempfile::TempDir,
    _claude_parent: tempfile::TempDir,
    _bin_dir: tempfile::TempDir,
    state_dir: std::path::PathBuf,
    claude_dir: std::path::PathBuf,
    /// A private copy of the shared `CARGO_BIN_EXE_libra-governor`
    /// binary. `uninstall --yes` deletes the exact binary path its
    /// install marker recorded once it matches `current_exe()` — see
    /// `uninstall_cmd`'s module docs — and every test in this file
    /// shares one on-disk `CARGO_BIN_EXE_libra-governor` binary. Without
    /// a private copy, one test's `uninstall --yes` would delete the
    /// binary every other test in this same test run also needs to
    /// exec, an entirely different hazard from the real-world behavior
    /// this file is meant to exercise.
    bin_path: std::path::PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        warm_up_binary();
        let state_parent = tempfile::tempdir().unwrap();
        let claude_parent = tempfile::tempdir().unwrap();
        let bin_dir = tempfile::tempdir().unwrap();
        let state_dir = state_parent.path().join("state");
        let claude_dir = claude_parent.path().join("claude");
        std::fs::create_dir_all(&claude_dir).unwrap();

        let bin_path = bin_dir.path().join("libra-governor");
        std::fs::copy(bin(), &bin_path).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin_path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        // Each private copy is a distinct file on disk and pays its own
        // first-execution OS validation cost (macOS Gatekeeper/AMFI) the
        // same way the original binary does — see
        // `hook_cli_integration.rs`'s `warm_up_binary` docs. Absorb it
        // here, untimed, so a test's own timing assertion only measures
        // this tool's own logic.
        let _ = Command::new(&bin_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();

        Sandbox {
            _state_parent: state_parent,
            _claude_parent: claude_parent,
            _bin_dir: bin_dir,
            state_dir,
            claude_dir,
            bin_path,
        }
    }

    fn run(&self, args: &[&str]) -> (std::process::Output, Duration) {
        let start = std::time::Instant::now();
        let output = Command::new(&self.bin_path)
            .args(args)
            .env("LIBRA_GOVERNOR_STATE_DIR", &self.state_dir)
            .env("LIBRA_GOVERNOR_CLAUDE_DIR", &self.claude_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .unwrap();
        (output, start.elapsed())
    }
}

fn kill_daemon_for(state_dir: &std::path::Path) {
    let _ = Command::new("pkill")
        .args(["-f", &state_dir.display().to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[test]
fn doctor_on_a_never_installed_machine_exits_zero_and_creates_nothing() {
    let sandbox = Sandbox::new();
    let (output, elapsed) = sandbox.run(&["doctor"]);

    assert!(
        output.status.success(),
        "doctor on a fresh machine must exit 0 (nothing is broken, just not installed); \
         stderr/stdout: {} {}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "doctor must never spawn a daemon or hang"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.to_lowercase().contains("not installed"));

    assert!(
        !sandbox.state_dir.exists(),
        "a read-only diagnostic must not create the state dir"
    );
}

#[test]
fn install_doctor_uninstall_doctor_lifecycle() {
    let sandbox = Sandbox::new();

    let (install_output, _) = sandbox.run(&["install"]);
    assert!(
        install_output.status.success(),
        "install failed: {}",
        String::from_utf8_lossy(&install_output.stderr)
    );

    let (doctor_output, _) = sandbox.run(&["doctor"]);
    assert!(doctor_output.status.success());
    let stdout = String::from_utf8_lossy(&doctor_output.stdout);
    assert!(
        stdout.contains("hooks and statusline wired"),
        "doctor after install must report hooks wired: {stdout}"
    );

    // uninstall --yes also removes the daemon binary itself, since the
    // install marker names this exact path (see uninstall_cmd's module
    // docs) — a real, intended consequence of a full uninstall. Keep an
    // untracked copy purely so this test can still run `doctor`
    // afterward to observe the resulting "not installed" state, the
    // same workaround `scripts/smoke-test.sh` uses for the same reason.
    let doctor_checker = sandbox.bin_path.with_file_name("doctor-checker");
    std::fs::copy(&sandbox.bin_path, &doctor_checker).unwrap();

    // uninstall's confirmation prompt reads stdin; Stdio::null() is not a
    // TTY, so the state dir deletion is safely skipped without --yes.
    // Passing --yes here exercises the confirmed deletion path.
    let (uninstall_output, _) = sandbox.run(&["uninstall", "--yes"]);
    assert!(
        uninstall_output.status.success(),
        "uninstall failed: {}",
        String::from_utf8_lossy(&uninstall_output.stderr)
    );
    assert!(
        !sandbox.bin_path.exists(),
        "uninstall --yes should have removed the binary it installed"
    );

    let doctor_again = Command::new(&doctor_checker)
        .arg("doctor")
        .env("LIBRA_GOVERNOR_STATE_DIR", &sandbox.state_dir)
        .env("LIBRA_GOVERNOR_CLAUDE_DIR", &sandbox.claude_dir)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(doctor_again.status.success());
    let stdout_again = String::from_utf8_lossy(&doctor_again.stdout);
    assert!(
        stdout_again.to_lowercase().contains("not installed"),
        "doctor after uninstall must report not-installed again: {stdout_again}"
    );

    kill_daemon_for(&sandbox.state_dir);
}

#[test]
fn uninstall_without_yes_on_a_non_interactive_stdin_never_deletes_the_state_dir() {
    let sandbox = Sandbox::new();
    sandbox.run(&["install"]);
    // Force the state dir to exist even without a daemon having run.
    std::fs::create_dir_all(&sandbox.state_dir).unwrap();
    std::fs::write(sandbox.state_dir.join("ledger.sqlite3"), b"pretend-ledger").unwrap();

    let (output, _) = sandbox.run(&["uninstall"]);
    assert!(output.status.success());
    assert!(
        sandbox.state_dir.join("ledger.sqlite3").exists(),
        "a non-interactive uninstall without --yes must never delete real ledger data"
    );

    kill_daemon_for(&sandbox.state_dir);
}

#[test]
fn doctor_flags_a_corrupt_config_json_with_a_live_daemon_as_unhealthy() {
    let sandbox = Sandbox::new();

    // Get a real daemon running via the hook path (mirrors
    // hook_cli_integration.rs), *before* writing the corrupt config —
    // handle_doctor re-reads config.json fresh on every request (the
    // same re-validate-rather-than-cache pattern GatewayStatus already
    // uses), so a daemon that started clean still reports it once the
    // file is written afterward.
    let payload = serde_json::json!({
        "session_id": "doctor-corrupt-config",
        "cwd": sandbox.state_dir.parent().unwrap(),
        "prompt": "anything",
    })
    .to_string();
    warm_up_binary();
    let mut child = Command::new(&sandbox.bin_path)
        .args(["hook", "user-prompt-submit"])
        .env("LIBRA_GOVERNOR_STATE_DIR", &sandbox.state_dir)
        .env("LIBRA_GOVERNOR_CLAUDE_DIR", &sandbox.claude_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    std::io::Write::write_all(child.stdin.as_mut().unwrap(), payload.as_bytes()).unwrap();
    child.wait().unwrap();

    std::fs::write(sandbox.state_dir.join("config.json"), "{ not json").unwrap();

    let (output, _) = sandbox.run(&["doctor", "--json"]);
    assert!(
        !output.status.success(),
        "a rejected config.json must be reported as an Error-severity finding, non-zero exit"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let value: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("doctor --json did not print valid JSON: {e}\n{stdout}"));
    let findings = value["findings"].as_array().unwrap();
    assert!(
        findings
            .iter()
            .any(|f| f["id"] == "config_file_valid" && f["severity"] == "error"),
        "expected a config_file_valid error finding: {findings:#?}"
    );

    kill_daemon_for(&sandbox.state_dir);
}

#[test]
fn doctor_with_no_daemon_and_no_socket_still_exits_zero_and_stays_fast() {
    let sandbox = Sandbox::new();
    std::fs::create_dir_all(&sandbox.state_dir).unwrap();

    let (output, elapsed) = sandbox.run(&["doctor"]);
    assert!(output.status.success());
    assert!(
        elapsed < Duration::from_secs(5),
        "connect-only must fail fast when nothing is listening, doctor took {elapsed:?}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("daemon is not running"));
}

/// The secret-safety guarantee: a fake credential *reference* (never a
/// real secret — see `SECURITY.md`) placed in `config.json`'s
/// `credential_command`/`credential_args` must never appear in `doctor`'s
/// stdout or stderr, in either human or `--json` form. Stronger than
/// reasoning about which `DoctorResult` fields exist, because it survives
/// future field additions.
#[test]
fn doctor_never_prints_a_gateway_credential_reference() {
    let sandbox = Sandbox::new();
    std::fs::create_dir_all(&sandbox.state_dir).unwrap();
    let needle = "op://vault/NEEDLE-a1b2c3d4";
    let config = serde_json::json!({
        "gateway": {
            "bind_addr": "127.0.0.1:0",
            "token_path": sandbox.state_dir.join("gateway.token"),
            "credential_mode": "governor_held",
            "credential_command": "op",
            "credential_args": ["read", needle]
        }
    });
    std::fs::write(
        sandbox.state_dir.join("config.json"),
        serde_json::to_string_pretty(&config).unwrap(),
    )
    .unwrap();

    // Start a real daemon against this exact config.json (loaded fresh
    // by `handle_doctor` on every request), so this test exercises the
    // full gateway-configured reporting path — capabilities,
    // gateway_credential_configured, everything — not just the
    // no-daemon local-only findings.
    let payload = serde_json::json!({
        "session_id": "doctor-credential-needle",
        "cwd": sandbox.state_dir.parent().unwrap(),
        "prompt": "anything",
    })
    .to_string();
    warm_up_binary();
    let mut child = Command::new(&sandbox.bin_path)
        .args(["hook", "user-prompt-submit"])
        .env("LIBRA_GOVERNOR_STATE_DIR", &sandbox.state_dir)
        .env("LIBRA_GOVERNOR_CLAUDE_DIR", &sandbox.claude_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    std::io::Write::write_all(child.stdin.as_mut().unwrap(), payload.as_bytes()).unwrap();
    child.wait().unwrap();

    for args in [vec!["doctor"], vec!["doctor", "--json"]] {
        let (output, _) = sandbox.run(&args);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !stdout.contains(needle) && !stderr.contains(needle),
            "doctor {args:?} leaked the credential reference:\nstdout: {stdout}\nstderr: {stderr}"
        );
    }

    kill_daemon_for(&sandbox.state_dir);
}
