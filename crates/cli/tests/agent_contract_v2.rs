//! Codex adapter contract fixtures and malformed/unknown-event cases
//! (HORO-1157).
//!
//! `crates/cli` is a bin crate, so `tests/` cannot link its private
//! `agent` module — this drives the real compiled `libra-governor`
//! binary via stdin exactly as `hook_cli_integration.rs` and
//! `agent_contract.rs` already do. Fixtures live in
//! `crates/cli/tests/fixtures/agent/{claude-code,codex}/` — the Codex
//! ones carry a `_source` field (ignored by parsing, like every other
//! extra field) citing what was actually verified vs. transcribed; see
//! that field's text in each fixture file and
//! `docs/adr/0004-agent-adapter-contract.md`.
//!
//! # Cases covered
//!
//! - Happy path per agent per entry point (6 total):
//!   `{claude_code,codex}_{user_prompt_submit,post_tool_use,stop}_happy_path`.
//! - Cross-agent equivalence: the same `session_id`/`cwd`/`prompt` fed
//!   through each agent's own fixture produces byte-identical
//!   `additionalContext` (task id normalized out) — the concrete proof
//!   that `crate::agent::run`'s core logic is not duplicated per agent.
//! - Malformed JSON and a missing required field, for all three entry
//!   points, for both agents: exit 0, no panic, valid/empty output.
//! - An unrecognized `hook_event_name` and a recognized-but-unwired one
//!   (`SessionStart`): exit 0, no panic.
//! - Codex's extra fields (`turn_id`, `permission_mode`, `tool_use_id`,
//!   `agent_id`/`agent_type`, `last_assistant_message`,
//!   `stop_hook_active`) tolerated without rejection.

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

fn fixture(relative_path: &str) -> String {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/agent")
        .join(relative_path);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("could not read fixture {}: {e}", path.display()))
}

/// Loads a fixture and substitutes the `__CWD__` placeholder with a
/// real temp directory path, so the daemon's reconnaissance step has an
/// actual filesystem to scan.
fn fixture_with_cwd(relative_path: &str, cwd: &std::path::Path) -> String {
    fixture(relative_path).replace("__CWD__", &cwd.display().to_string().replace('\\', "\\\\"))
}

fn sample_repo() -> tempfile::TempDir {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("Cargo.toml"), "[package]\nname=\"x\"").unwrap();
    repo
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

fn additional_context(stdout: &[u8]) -> String {
    let value: serde_json::Value = serde_json::from_slice(stdout)
        .unwrap_or_else(|e| panic!("hook stdout was not valid JSON: {e}\nstdout: {stdout:?}"));
    value["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .expect("additionalContext must be a string")
        .to_string()
}

/// See `crates/cli/tests/agent_contract.rs`'s `innermost_additional_context`
/// docs: the pre-existing `UserPromptSubmit` success path double-encodes
/// its output. This helper handles both the single- and double-wrapped
/// shape so these tests assert on the real text either way.
fn resilient_additional_context(stdout: &[u8]) -> String {
    let outer = additional_context(stdout);
    match serde_json::from_str::<serde_json::Value>(&outer) {
        Ok(inner) => inner["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .map(str::to_string)
            .unwrap_or(outer),
        Err(_) => outer,
    }
}

// ---------------------------------------------------------------------
// Happy path: 6 cases (2 agents x 3 entry points).
// ---------------------------------------------------------------------

#[test]
fn claude_code_user_prompt_submit_happy_path() {
    let state_dir = tempfile::tempdir().unwrap();
    let repo = sample_repo();
    let payload = fixture_with_cwd("claude-code/user-prompt-submit.json", repo.path());

    let output = run_subcommand(&["hook", "user-prompt-submit"], state_dir.path(), &payload);
    assert!(output.status.success());
    let ctx = resilient_additional_context(&output.stdout);
    assert!(ctx.contains("Preflight complete"), "{ctx}");

    kill_daemon_for(state_dir.path());
}

#[test]
fn claude_code_post_tool_use_happy_path() {
    let state_dir = tempfile::tempdir().unwrap();
    let repo = sample_repo();
    let payload = fixture_with_cwd("claude-code/post-tool-use.json", repo.path());

    let output = run_subcommand(&["hook", "post-tool-use"], state_dir.path(), &payload);
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
}

#[test]
fn claude_code_stop_happy_path() {
    let state_dir = tempfile::tempdir().unwrap();
    let repo = sample_repo();
    let preflight = fixture_with_cwd("claude-code/user-prompt-submit.json", repo.path());
    run_subcommand(
        &["hook", "user-prompt-submit"],
        state_dir.path(),
        &preflight,
    );

    let payload = fixture_with_cwd("claude-code/stop.json", repo.path());
    let output = run_subcommand(&["hook", "stop"], state_dir.path(), &payload);
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    let summary = String::from_utf8_lossy(&output.stderr);
    assert!(summary.contains("Execution Receipt"), "{summary}");

    kill_daemon_for(state_dir.path());
}

#[test]
fn codex_user_prompt_submit_happy_path() {
    let state_dir = tempfile::tempdir().unwrap();
    let repo = sample_repo();
    let payload = fixture_with_cwd("codex/user-prompt-submit.json", repo.path());

    let output = run_subcommand(
        &["codex-hook", "user-prompt-submit"],
        state_dir.path(),
        &payload,
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let ctx = resilient_additional_context(&output.stdout);
    assert!(ctx.contains("Preflight complete"), "{ctx}");

    kill_daemon_for(state_dir.path());
}

#[test]
fn codex_post_tool_use_happy_path() {
    let state_dir = tempfile::tempdir().unwrap();
    let repo = sample_repo();
    let payload = fixture_with_cwd("codex/post-tool-use.json", repo.path());

    let output = run_subcommand(&["codex-hook", "post-tool-use"], state_dir.path(), &payload);
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
}

#[test]
fn codex_stop_happy_path() {
    let state_dir = tempfile::tempdir().unwrap();
    let repo = sample_repo();
    let preflight = fixture_with_cwd("codex/user-prompt-submit.json", repo.path());
    run_subcommand(
        &["codex-hook", "user-prompt-submit"],
        state_dir.path(),
        &preflight,
    );

    let payload = fixture_with_cwd("codex/stop.json", repo.path());
    let output = run_subcommand(&["codex-hook", "stop"], state_dir.path(), &payload);
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    let summary = String::from_utf8_lossy(&output.stderr);
    assert!(summary.contains("Execution Receipt"), "{summary}");

    kill_daemon_for(state_dir.path());
}

// ---------------------------------------------------------------------
// Cross-agent equivalence — the concrete proof core logic is shared.
// ---------------------------------------------------------------------

/// Replaces every UUID-shaped token with `<ID>`, mirroring
/// `agent_contract.rs`'s `normalize_ids`.
fn normalize_ids(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < bytes.len() {
        if let Some(len) = uuid_len_at(input, i) {
            out.push_str("<ID>");
            i += len;
        } else {
            let ch = input[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

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

#[test]
fn cross_agent_equivalence_same_session_cwd_prompt_yields_identical_additional_context() {
    // Both fixtures share session_id "fixture-session" and prompt
    // "fix the login bug" by construction — see the fixture files'
    // _source comments.
    let repo = sample_repo();

    let claude_state = tempfile::tempdir().unwrap();
    let claude_payload = fixture_with_cwd("claude-code/user-prompt-submit.json", repo.path());
    let claude_output = run_subcommand(
        &["hook", "user-prompt-submit"],
        claude_state.path(),
        &claude_payload,
    );
    assert!(claude_output.status.success());
    let claude_ctx = normalize_ids(&resilient_additional_context(&claude_output.stdout));

    let codex_state = tempfile::tempdir().unwrap();
    let codex_payload = fixture_with_cwd("codex/user-prompt-submit.json", repo.path());
    let codex_output = run_subcommand(
        &["codex-hook", "user-prompt-submit"],
        codex_state.path(),
        &codex_payload,
    );
    assert!(codex_output.status.success());
    let codex_ctx = normalize_ids(&resilient_additional_context(&codex_output.stdout));

    assert_eq!(
        claude_ctx, codex_ctx,
        "identical session_id/cwd/prompt through each agent's own entry point must produce \
         byte-identical additionalContext (id-normalized) — proof the core translation logic \
         is not duplicated per agent"
    );

    kill_daemon_for(claude_state.path());
    kill_daemon_for(codex_state.path());
}

// ---------------------------------------------------------------------
// Malformed JSON — exit 0, no panic, for every entry point, both agents.
// ---------------------------------------------------------------------

#[test]
fn malformed_json_is_handled_gracefully_by_every_entry_point() {
    let malformed = fixture("malformed.json");
    let cases: [&[&str]; 6] = [
        &["hook", "user-prompt-submit"],
        &["hook", "post-tool-use"],
        &["hook", "stop"],
        &["codex-hook", "user-prompt-submit"],
        &["codex-hook", "post-tool-use"],
        &["codex-hook", "stop"],
    ];
    for subcommand in cases {
        let state_dir = tempfile::tempdir().unwrap();
        let output = run_subcommand(subcommand, state_dir.path(), &malformed);
        assert!(
            output.status.success(),
            "{subcommand:?} must exit 0 on malformed JSON; stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        if subcommand[1] == "user-prompt-submit" {
            let ctx = resilient_additional_context(&output.stdout);
            assert!(
                ctx.to_lowercase().contains("malformed"),
                "{subcommand:?}: {ctx}"
            );
        } else {
            assert!(
                output.stdout.is_empty(),
                "{subcommand:?} must print nothing to stdout on malformed input"
            );
        }
    }
}

// ---------------------------------------------------------------------
// Missing required field — exit 0, no panic, for every entry point.
// ---------------------------------------------------------------------

#[test]
fn missing_required_field_is_handled_gracefully_by_every_entry_point() {
    let repo = sample_repo();
    let cases: [(&[&str], &str); 6] = [
        (
            &["hook", "user-prompt-submit"],
            "missing-field-user-prompt-submit.json",
        ),
        (
            &["hook", "post-tool-use"],
            "missing-field-post-tool-use.json",
        ),
        (&["hook", "stop"], "missing-field-stop.json"),
        (
            &["codex-hook", "user-prompt-submit"],
            "missing-field-user-prompt-submit.json",
        ),
        (
            &["codex-hook", "post-tool-use"],
            "missing-field-post-tool-use.json",
        ),
        (&["codex-hook", "stop"], "missing-field-stop.json"),
    ];
    for (subcommand, fixture_name) in cases {
        let state_dir = tempfile::tempdir().unwrap();
        let payload = fixture_with_cwd(fixture_name, repo.path());
        let output = run_subcommand(subcommand, state_dir.path(), &payload);
        assert!(
            output.status.success(),
            "{subcommand:?} must exit 0 on a missing required field; stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

// ---------------------------------------------------------------------
// Unrecognized / recognized-but-unwired hook_event_name.
// ---------------------------------------------------------------------

#[test]
fn an_unrecognized_hook_event_name_is_handled_gracefully() {
    let state_dir = tempfile::tempdir().unwrap();
    let repo = sample_repo();
    let payload = fixture_with_cwd("unknown-event-user-prompt-submit.json", repo.path());

    let output = run_subcommand(&["hook", "user-prompt-submit"], state_dir.path(), &payload);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let ctx = resilient_additional_context(&output.stdout);
    assert!(ctx.to_lowercase().contains("unrecognized"), "{ctx}");
}

#[test]
fn a_recognized_but_unwired_lifecycle_event_is_handled_gracefully() {
    let state_dir = tempfile::tempdir().unwrap();
    let repo = sample_repo();
    let payload = fixture_with_cwd("recognized-unwired-user-prompt-submit.json", repo.path());

    let output = run_subcommand(&["hook", "user-prompt-submit"], state_dir.path(), &payload);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let ctx = resilient_additional_context(&output.stdout);
    assert!(ctx.to_lowercase().contains("unwired"), "{ctx}");
}

// ---------------------------------------------------------------------
// Codex's extra fields tolerated without rejection (all 3 entry points).
// ---------------------------------------------------------------------

#[test]
fn codex_extra_fields_are_tolerated_at_every_entry_point() {
    let repo = sample_repo();

    let state_dir = tempfile::tempdir().unwrap();
    let prompt_payload = fixture_with_cwd("codex/user-prompt-submit.json", repo.path());
    let prompt_output = run_subcommand(
        &["codex-hook", "user-prompt-submit"],
        state_dir.path(),
        &prompt_payload,
    );
    assert!(prompt_output.status.success());

    let tool_payload = fixture_with_cwd("codex/post-tool-use.json", repo.path());
    let tool_output = run_subcommand(
        &["codex-hook", "post-tool-use"],
        state_dir.path(),
        &tool_payload,
    );
    assert!(tool_output.status.success());

    let stop_payload = fixture_with_cwd("codex/stop.json", repo.path());
    let stop_output = run_subcommand(&["codex-hook", "stop"], state_dir.path(), &stop_payload);
    assert!(stop_output.status.success());

    kill_daemon_for(state_dir.path());
}
