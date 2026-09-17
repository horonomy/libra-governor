//! End-to-end tests of the real `libra-governor` binary's `hook
//! user-prompt-submit` and `statusline` subcommands: a fresh daemon
//! spawned on demand, a malformed stdin payload, and a daemon that
//! cannot be reached at all. Every scenario must complete quickly and
//! print valid `hookSpecificOutput` JSON to stdout — this is the
//! "real local smoke test" surface a reviewer can also run by hand (see
//! `integrations/claude-code/README.md`).

use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::Once;
use std::time::{Duration, Instant};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_libra-governor")
}

/// Spawns the binary once, untimed, before any test's timing assertions
/// run. On some platforms (notably macOS Gatekeeper/AMFI) the *first*
/// execution of a freshly built binary pays a one-time OS validation
/// cost that has nothing to do with this program's own logic — without
/// this warm-up, that cost would land inside whichever test happens to
/// spawn the binary first and make its wall-clock budget assertion
/// flaky on a clean build. Every test below calls this before starting
/// its own clock.
fn warm_up_binary() {
    static WARM_UP: Once = Once::new();
    WARM_UP.call_once(|| {
        // No subcommand -> prints usage and exits 2 almost immediately
        // once past OS validation; output is irrelevant here.
        let _ = Command::new(bin())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    });
}

fn run_hook(state_dir: &std::path::Path, stdin_payload: &str) -> (std::process::Output, Duration) {
    warm_up_binary();
    let mut child = Command::new(bin())
        .args(["hook", "user-prompt-submit"])
        .env("LIBRA_GOVERNOR_STATE_DIR", state_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(stdin_payload.as_bytes())
        .unwrap();

    let start = Instant::now();
    let output = child.wait_with_output().unwrap();
    (output, start.elapsed())
}

fn additional_context(stdout: &[u8]) -> String {
    let value: serde_json::Value = serde_json::from_slice(stdout)
        .unwrap_or_else(|e| panic!("hook stdout was not valid JSON: {e}\nstdout: {stdout:?}"));
    value["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .expect("additionalContext must be a string")
        .to_string()
}

#[test]
fn hook_spawns_daemon_and_returns_a_preflight_summary() {
    let state_dir = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("Cargo.toml"), "[package]\nname=\"x\"").unwrap();
    std::fs::create_dir_all(repo.path().join("src")).unwrap();
    std::fs::write(repo.path().join("src/login.rs"), "// login").unwrap();

    let payload = serde_json::json!({
        "session_id": "cli-integration-session",
        "cwd": repo.path(),
        "prompt": "fix the login bug",
        "hook_event_name": "UserPromptSubmit",
        "transcript_path": "/tmp/does-not-matter.jsonl",
    })
    .to_string();

    let (output, elapsed) = run_hook(state_dir.path(), &payload);

    assert!(
        output.status.success(),
        "hook must always exit 0 (advisory only); stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "hook took {elapsed:?}, expected well within its own budget"
    );

    let ctx = additional_context(&output.stdout);
    assert!(ctx.contains("Preflight complete"));
    assert!(ctx.contains("required"));
}

#[test]
fn hook_handles_malformed_stdin_payload_gracefully() {
    let state_dir = tempfile::tempdir().unwrap();
    let (output, elapsed) = run_hook(state_dir.path(), "this is not json");

    assert!(
        output.status.success(),
        "malformed payload must not crash the hook"
    );
    assert!(elapsed < Duration::from_secs(5));

    let ctx = additional_context(&output.stdout);
    assert!(ctx.to_lowercase().contains("malformed"));
}

#[test]
fn hook_degrades_gracefully_when_the_daemon_cannot_start() {
    // A regular file where the state directory should be: `daemon run`
    // cannot create it, so the spawned daemon exits immediately and no
    // one ever binds the socket. The hook must still return within its
    // own spawn-wait budget rather than hanging.
    let parent = tempfile::tempdir().unwrap();
    let blocked_state_dir = parent.path().join("blocked");
    std::fs::write(&blocked_state_dir, "not a directory").unwrap();

    let payload = serde_json::json!({
        "session_id": "cli-integration-unreachable",
        "cwd": parent.path(),
        "prompt": "anything",
    })
    .to_string();

    let (output, elapsed) = run_hook(&blocked_state_dir, &payload);

    assert!(
        output.status.success(),
        "daemon-unreachable path must still exit 0"
    );
    assert!(
        elapsed < Duration::from_secs(8),
        "hook took {elapsed:?}, expected to give up within its spawn-wait budget, not hang"
    );

    let ctx = additional_context(&output.stdout);
    assert!(
        ctx.to_lowercase().contains("unavailable") || ctx.to_lowercase().contains("unreachable"),
        "expected a daemon-unavailable message, got: {ctx}"
    );
}

#[test]
fn statusline_never_spawns_and_reports_placeholder_when_daemon_is_down() {
    warm_up_binary();
    let state_dir = tempfile::tempdir().unwrap();

    let start = Instant::now();
    let output = Command::new(bin())
        .arg("statusline")
        .env("LIBRA_GOVERNOR_STATE_DIR", state_dir.path())
        .output()
        .unwrap();
    let elapsed = start.elapsed();

    assert!(output.status.success());
    assert!(
        elapsed < Duration::from_secs(2),
        "statusline must never spawn a daemon and must return near-instantly: took {elapsed:?}"
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(stdout.trim(), "libra: -");

    // No daemon process should have been left behind by a statusline run.
    assert!(
        !state_dir.path().join("daemon.sock").exists(),
        "statusline must never create the daemon socket itself"
    );
}
