//! HORO-2160 (HORO-1718 G3): with `LIBRA_GOVERNOR_NO_AUTOSPAWN` set, an
//! admitting hook must be a plain short-lived client. It fails open and
//! leaves no daemon, socket or pid record behind; without the switch the
//! same hook does start one (the control that proves the check can fail).

use std::io::Write;
use std::process::{Command, Stdio};

mod support;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_libra-governor")
}

fn run_prompt_hook(
    subcommand: &[&str],
    state: &std::path::Path,
    no_autospawn: bool,
) -> std::process::Output {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("Cargo.toml"), "[package]\nname=\"x\"").unwrap();
    let payload = serde_json::json!({
        "session_id": "no-autospawn-session",
        "cwd": repo.path(),
        "prompt": "fix the login bug",
    })
    .to_string();
    let mut command = Command::new(bin());
    command
        .args(subcommand)
        .env("LIBRA_GOVERNOR_STATE_DIR", state)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if no_autospawn {
        command.env("LIBRA_GOVERNOR_NO_AUTOSPAWN", "1");
    } else {
        command.env_remove("LIBRA_GOVERNOR_NO_AUTOSPAWN");
    }
    let mut child = command.spawn().unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(payload.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn a_hook_with_autospawn_disabled_starts_no_daemon() {
    for subcommand in [
        ["hook", "user-prompt-submit"],
        ["codex-hook", "user-prompt-submit"],
    ] {
        let state = support::DaemonState::new();
        let output = run_prompt_hook(&subcommand, state.path(), true);
        assert!(output.status.success(), "{subcommand:?}: {output:?}");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("daemon unreachable"),
            "{subcommand:?} must fail open with the unreachable notice: {stdout}"
        );

        // The daemon is detached, so give a would-be spawn time to appear.
        std::thread::sleep(std::time::Duration::from_millis(1500));
        assert!(
            !state.path().join("daemon.sock").exists(),
            "{subcommand:?} left a daemon socket"
        );
        assert!(
            !state.path().join("daemon.pid").exists(),
            "{subcommand:?} left a daemon pid record"
        );
    }
}

#[test]
fn the_same_hook_without_the_switch_does_start_a_daemon() {
    let state = support::DaemonState::new();
    let output = run_prompt_hook(&["hook", "user-prompt-submit"], state.path(), false);
    assert!(output.status.success(), "{output:?}");
    assert!(
        state.path().join("daemon.sock").exists(),
        "control: the default hook is expected to auto-start the daemon"
    );
}
