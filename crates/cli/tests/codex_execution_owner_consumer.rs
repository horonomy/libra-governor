//! HORO-1714 PR-2: the Codex ExecutionOwner consumer route, gated off by
//! default (`LIBRA_GOVERNOR_CODEX_EXECUTION_OWNER`). Drives the real
//! compiled `libra-governor` binary exactly like `agent_contract.rs` does.
//! Claude Code never reaches this branch regardless of the flag -- that
//! invariant is `agent_contract.rs`'s own job to protect, not this file's.

use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::Once;

mod support;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_libra-governor")
}

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

fn run_subcommand(
    subcommand: &[&str],
    state_dir: &std::path::Path,
    owner_gate_on: bool,
    stdin_payload: &str,
) -> std::process::Output {
    warm_up_binary();
    let mut command = Command::new(bin());
    command
        .args(subcommand)
        .env("LIBRA_GOVERNOR_STATE_DIR", state_dir)
        .env_remove("LIBRA_GOVERNOR_CODEX_EXECUTION_OWNER");
    if owner_gate_on {
        command.env("LIBRA_GOVERNOR_CODEX_EXECUTION_OWNER", "1");
    }
    let mut child = command
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
    child.wait_with_output().unwrap()
}

fn additional_context(stdout: &[u8]) -> String {
    let value: serde_json::Value = serde_json::from_slice(stdout)
        .unwrap_or_else(|e| panic!("hook stdout was not valid JSON: {e}\nstdout: {stdout:?}"));
    value["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .expect("additionalContext must be a string")
        .to_string()
}

fn daemon_log(state_dir: &std::path::Path) -> String {
    std::fs::read_to_string(state_dir.join("daemon.log")).unwrap_or_default()
}

/// Gate off (the default -- no env var set): a Codex prompt must take the
/// exact same legacy session-only Preflight path Claude Code uses. This is
/// not a byte-exact golden (that's `agent_contract.rs`'s job); it only
/// proves the owner route is not silently always-on.
#[test]
fn codex_gate_off_uses_the_legacy_session_only_path() {
    let state_dir = support::DaemonState::new();
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("Cargo.toml"), "[package]\nname=\"x\"").unwrap();

    let payload = serde_json::json!({
        "session_id": "gate-off-session",
        "cwd": repo.path(),
        "prompt": "fix the bug",
    })
    .to_string();

    let output = run_subcommand(
        &["codex-hook", "user-prompt-submit"],
        state_dir.path(),
        false,
        &payload,
    );
    assert!(output.status.success());
    let outer = additional_context(&output.stdout);
    // The legacy path double-wraps (see agent_contract.rs's own note);
    // this only needs to prove it's the legacy shape, not match it exactly.
    assert!(
        outer.contains("Preflight complete"),
        "gate off must use the legacy Preflight path: {outer}"
    );
    assert!(
        !daemon_log(state_dir.path()).contains("ExecutionOwner"),
        "gate off must never touch the owner route at all"
    );
}

/// Gate on, agent-absent identity (no `agent_id` in the payload --
/// decision A's own case): a fresh Codex prompt applies via the owner
/// route and renders through the exact same `format_additional_context`
/// Claude Code's legacy path uses.
#[test]
fn codex_gate_on_agent_absent_prompt_applies_via_the_owner_route() {
    let state_dir = support::DaemonState::new();
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("Cargo.toml"), "[package]\nname=\"x\"").unwrap();

    let payload = serde_json::json!({
        "session_id": "gate-on-agent-absent-session",
        "cwd": repo.path(),
        "prompt": "fix the bug",
        "turn_id": "turn-1",
    })
    .to_string();

    let output = run_subcommand(
        &["codex-hook", "user-prompt-submit"],
        state_dir.path(),
        true,
        &payload,
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let ctx = additional_context(&output.stdout);
    assert!(
        ctx.contains("Preflight complete"),
        "an Applied Prompt effect must render the real PreflightResult: {ctx}"
    );
    assert!(
        !ctx.starts_with("[libra-governor] no effect"),
        "a fresh agent-absent prompt on a new lane must not be refused: {ctx}"
    );
}

/// Gate on: a Tool-completed event with no prior bound Prompt has no
/// association to attach to. This must be an explicit, typed no-effect
/// result -- never a silent fallback to the legacy `ToolInvoked` counter
/// path, and never a crash.
#[test]
fn codex_gate_on_tool_with_no_bound_task_is_an_explicit_no_effect_never_a_legacy_fallback() {
    let state_dir = support::DaemonState::new();
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("Cargo.toml"), "[package]\nname=\"x\"").unwrap();

    // Spawn the daemon via an unrelated lane first -- post-tool-use itself
    // never spawns it (by contract), so an orphan tool call against a
    // cold state dir would otherwise fail on "daemon unreachable", not on
    // the `Missing` association this test actually wants to exercise.
    let warm_up_payload = serde_json::json!({
        "session_id": "gate-on-orphan-tool-unrelated-session",
        "cwd": repo.path(),
        "prompt": "unrelated",
        "turn_id": "unrelated-turn",
    })
    .to_string();
    assert!(run_subcommand(
        &["codex-hook", "user-prompt-submit"],
        state_dir.path(),
        true,
        &warm_up_payload,
    )
    .status
    .success());

    let payload = serde_json::json!({
        "session_id": "gate-on-orphan-tool-session",
        "tool_name": "Bash",
        "tool_use_id": "call-1",
        "turn_id": "orphan-turn",
    })
    .to_string();

    let output = run_subcommand(
        &["codex-hook", "post-tool-use"],
        state_dir.path(),
        true,
        &payload,
    );
    assert!(output.status.success());
    assert!(
        output.stdout.is_empty(),
        "post-tool-use must never print to stdout, owner route or not"
    );
    let log = daemon_log(state_dir.path());
    assert!(
        log.contains("post-tool-use: no effect:"),
        "an orphan tool call must be logged as an explicit no-effect, from the owner \
         route's own log line (not the legacy fire_and_forget path, which never logs \
         this message): {log}"
    );
}

/// Gate on, full prompt -> tool -> stop loop, agent-absent identity: Stop
/// finalizes via the owner route and prints the same Execution Receipt
/// summary shape the legacy path does.
#[test]
fn codex_gate_on_full_loop_finalizes_via_the_owner_route() {
    let state_dir = support::DaemonState::new();
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("Cargo.toml"), "[package]\nname=\"x\"").unwrap();
    let session_id = "gate-on-full-loop-session";

    let prompt_payload = serde_json::json!({
        "session_id": session_id,
        "cwd": repo.path(),
        "prompt": "fix the bug",
        "turn_id": "turn-1",
    })
    .to_string();
    let prompt_output = run_subcommand(
        &["codex-hook", "user-prompt-submit"],
        state_dir.path(),
        true,
        &prompt_payload,
    );
    assert!(prompt_output.status.success());
    assert!(additional_context(&prompt_output.stdout).contains("Preflight complete"));

    let tool_payload = serde_json::json!({
        "session_id": session_id,
        "tool_name": "Bash",
        "tool_use_id": "call-1",
        "turn_id": "turn-1",
    })
    .to_string();
    let tool_output = run_subcommand(
        &["codex-hook", "post-tool-use"],
        state_dir.path(),
        true,
        &tool_payload,
    );
    assert!(tool_output.status.success());

    let stop_payload = serde_json::json!({
        "session_id": session_id,
        "turn_id": "turn-1",
    })
    .to_string();
    let stop_output = run_subcommand(
        &["codex-hook", "stop"],
        state_dir.path(),
        true,
        &stop_payload,
    );
    assert!(
        stop_output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&stop_output.stderr)
    );
    assert!(
        stop_output.stdout.is_empty(),
        "stop must never print to stdout"
    );
    let summary = String::from_utf8_lossy(&stop_output.stderr);
    assert!(
        summary.contains("Execution Receipt"),
        "a real Applied Stop effect must render the receipt summary: {summary}"
    );
    assert!(
        summary.contains("Tool calls: 1"),
        "the one real tool call must be counted: {summary}"
    );
}
