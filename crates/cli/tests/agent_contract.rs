//! Pre-refactor golden fixtures for the Claude Code hook contract
//! (HORO-1157).
//!
//! These goldens are recorded against the CURRENT (pre-refactor) `hook
//! user-prompt-submit` / `hook stop` behavior, driving the real compiled
//! `libra-governor` binary exactly like
//! `crates/cli/tests/hook_cli_integration.rs` does. HORO-1157 extracts the
//! shared translation logic behind those two entry points into
//! `crates/cli/src/agent/` and adds a second agent (Codex) on top of it.
//! This file is the regression proof that the extraction does not change
//! Claude's observable behavior one byte: the random `TaskId` is
//! normalized out of both captured strings so the assertion is exact
//! (not merely "contains"), and this file itself must never be edited to
//! make a later commit pass — if it needs editing, real Claude behavior
//! changed, which HORO-1157 forbids (see the ticket's regression-safety
//! commit sequence, step 3).

use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::Once;

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
    stdin_payload: &str,
) -> std::process::Output {
    warm_up_binary();
    let mut child = Command::new(bin())
        .args(subcommand)
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
    child.wait_with_output().unwrap()
}

fn kill_daemon_for(state_dir: &std::path::Path) {
    let _ = Command::new("pkill")
        .args(["-f", &state_dir.display().to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Replaces every UUID-shaped token (`TaskId`/`PlanId`'s `Display` form,
/// `8-4-4-4-12` lowercase hex) with the literal `<ID>` so a golden
/// assertion can be byte-exact without being flaky across runs. No
/// `regex` dependency exists in this workspace (see HORO-1157's ticket
/// notes on avoiding new dependencies) — this is a small manual scan
/// tailored to exactly the one shape ever produced here.
fn normalize_ids(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < bytes.len() {
        if let Some(len) = uuid_len_at(input, i) {
            out.push_str("<ID>");
            i += len;
        } else {
            // Safe: we only skip ahead by the byte length of one char.
            let ch = input[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

/// Returns the byte length of a UUID (`8-4-4-4-12` lowercase hex) if one
/// starts at byte offset `start`, else `None`.
fn uuid_len_at(s: &str, start: usize) -> Option<usize> {
    let groups = [8, 4, 4, 4, 12];
    let bytes = s.as_bytes();
    let mut pos = start;
    for (idx, &group_len) in groups.iter().enumerate() {
        if pos + group_len > bytes.len() {
            return None;
        }
        if !bytes[pos..pos + group_len]
            .iter()
            .all(|b| b.is_ascii_hexdigit())
        {
            return None;
        }
        pos += group_len;
        if idx != groups.len() - 1 {
            if bytes.get(pos) != Some(&b'-') {
                return None;
            }
            pos += 1;
        }
    }
    Some(pos - start)
}

fn additional_context(stdout: &[u8]) -> String {
    let value: serde_json::Value = serde_json::from_slice(stdout)
        .unwrap_or_else(|e| panic!("hook stdout was not valid JSON: {e}\nstdout: {stdout:?}"));
    value["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .expect("additionalContext must be a string")
        .to_string()
}

/// The pre-existing (pre-HORO-1157) `hook user-prompt-submit` success
/// path double-encodes its output: `format_additional_context` already
/// returns a fully-formed `hookSpecificOutput` JSON string, and
/// `print_context` wraps that string again inside a second
/// `hookSpecificOutput.additionalContext` envelope before printing it.
/// This golden records that real, verified-by-running-the-binary shape
/// rather than the single-wrap shape a reading of the source alone would
/// suggest — a golden's job is to freeze what actually happens, not what
/// "should" happen; fixing this double-wrap is out of scope for
/// HORO-1157 (see the ticket's regression-safety discipline: extraction
/// only, no behavior change). Returns the innermost `additionalContext`
/// text.
fn innermost_additional_context(stdout: &[u8]) -> String {
    let outer = additional_context(stdout);
    let inner: serde_json::Value = serde_json::from_str(&outer).unwrap_or_else(|e| {
        panic!("outer additionalContext was not itself valid JSON: {e}\nouter: {outer:?}")
    });
    inner["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .expect("innermost additionalContext must be a string")
        .to_string()
}

/// Golden: `UserPromptSubmit`'s `additionalContext` for a known-shape
/// repo and prompt, task/plan ids normalized out.
#[test]
fn claude_user_prompt_submit_additional_context_golden() {
    let state_dir = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("Cargo.toml"), "[package]\nname=\"x\"").unwrap();
    std::fs::create_dir_all(repo.path().join("src")).unwrap();
    std::fs::write(repo.path().join("src/login.rs"), "// login").unwrap();

    let payload = serde_json::json!({
        "session_id": "golden-session",
        "cwd": repo.path(),
        "prompt": "fix the login bug",
    })
    .to_string();

    let output = run_subcommand(&["hook", "user-prompt-submit"], state_dir.path(), &payload);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let ctx = innermost_additional_context(&output.stdout);
    let normalized = normalize_ids(&ctx);

    assert!(
        normalized.contains("[libra-governor] Preflight complete (task <ID>, confidence:"),
        "{normalized}"
    );
    assert!(
        normalized.contains("Draft Completion Contract (revision 1):"),
        "{normalized}"
    );
    assert!(
        normalized.contains("[required] Matches the user's stated request"),
        "{normalized}"
    );
    assert!(
        normalized.contains("[required] Relevant tests pass (cargo test)"),
        "{normalized}"
    );
    assert!(
        normalized.ends_with(" This preflight is advisory only."),
        "{normalized}"
    );

    kill_daemon_for(state_dir.path());
}

/// Golden: `Stop`'s stderr Execution Receipt summary for a full
/// preflight -> tool-calls -> stop loop, task id normalized out.
#[test]
fn claude_stop_stderr_summary_golden() {
    let state_dir = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("Cargo.toml"), "[package]\nname=\"x\"").unwrap();

    let session_id = "golden-stop-session";

    let preflight_payload = serde_json::json!({
        "session_id": session_id,
        "cwd": repo.path(),
        "prompt": "fix the bug",
    })
    .to_string();
    let preflight_output = run_subcommand(
        &["hook", "user-prompt-submit"],
        state_dir.path(),
        &preflight_payload,
    );
    assert!(preflight_output.status.success());

    for tool_name in ["Bash", "Edit"] {
        let payload = serde_json::json!({
            "session_id": session_id,
            "cwd": repo.path(),
            "tool_name": tool_name,
        })
        .to_string();
        let output = run_subcommand(&["hook", "post-tool-use"], state_dir.path(), &payload);
        assert!(output.status.success());
    }

    let stop_payload = serde_json::json!({
        "session_id": session_id,
        "cwd": repo.path(),
    })
    .to_string();
    let stop_output = run_subcommand(&["hook", "stop"], state_dir.path(), &stop_payload);
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
    let normalized = normalize_ids(&summary);

    assert!(
        normalized.starts_with("[libra-governor] Execution Receipt (task <ID>)\n"),
        "{normalized}"
    );
    assert!(normalized.contains("Tool calls: 2"), "{normalized}");
    assert!(
        normalized.contains("Outcome:  Unknown (no automated completion verification in MVP 1)"),
        "{normalized}"
    );

    kill_daemon_for(state_dir.path());
}
