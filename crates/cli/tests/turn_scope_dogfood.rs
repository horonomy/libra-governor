//! Real-daemon acceptance of the turn-scoped default (HORO-1727 S8, ADR-0024).
//!
//! Every scenario drives the real binary against an isolated state
//! directory: a hook admits a task through a real daemon, then a real
//! `statusline provider` process reads the budget back. Unit tests cover the
//! gate and the renderer in isolation; what only this layer can show is that
//! the choice made when the task is created survives the process edge, a
//! config file appearing later, and a malformed config.

use std::process::{Command, Stdio};

mod support;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_libra-governor")
}

/// Admit one task through the real `user-prompt-submit` hook.
fn admit(state_dir: &std::path::Path, repo: &std::path::Path) {
    let payload = serde_json::json!({
        "session_id": "turn-scope-dogfood",
        "cwd": repo,
        "prompt": "fix the login bug",
    })
    .to_string();
    let mut child = Command::new(bin())
        .args(["hook", "user-prompt-submit"])
        .env("LIBRA_GOVERNOR_STATE_DIR", state_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(child.stdin.as_mut().unwrap(), payload.as_bytes()).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "the hook failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The `budget` segment of a real provider document.
fn budget_segment(state_dir: &std::path::Path) -> serde_json::Value {
    let output = Command::new(bin())
        .args(["statusline", "provider"])
        .env("LIBRA_GOVERNOR_STATE_DIR", state_dir)
        .output()
        .unwrap();
    assert!(output.status.success());
    let raw = String::from_utf8(output.stdout).unwrap();
    let document: serde_json::Value = serde_json::from_str(&raw)
        .unwrap_or_else(|e| panic!("provider stdout was not valid JSON: {e}\n{raw}"));
    document["segments"]
        .as_array()
        .expect("a document always has segments")
        .iter()
        .find(|segment| segment["key"] == serde_json::json!("budget"))
        .cloned()
        .unwrap_or_else(|| panic!("no budget segment in: {raw}"))
}

fn repo() -> tempfile::TempDir {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("Cargo.toml"), "[package]\nname=\"x\"").unwrap();
    repo
}

fn assert_turn_scoped(segment: &serde_json::Value) {
    assert_eq!(
        segment["budget_lifetime_scope"],
        serde_json::json!("turn_scoped"),
        "expected a turn-scoped budget: {segment}"
    );
    assert!(
        !segment["budget_turn_ceiling"].is_null(),
        "a turn-scoped budget must name its turn ceiling: {segment}"
    );
    assert!(
        segment["budget_total"].is_null() && segment["budget_remaining"].is_null(),
        "a turn-scoped budget must not invent a lifetime total: {segment}"
    );
}

fn assert_lifetime_bounded(segment: &serde_json::Value) {
    assert!(
        segment["budget_lifetime_scope"] != serde_json::json!("turn_scoped"),
        "expected a lifetime-bounded budget: {segment}"
    );
    assert!(
        !segment["budget_total"].is_null(),
        "a lifetime-bounded budget carries its total: {segment}"
    );
}

#[test]
fn a_daemon_with_no_config_admits_and_discloses_a_turn_scoped_budget() {
    let state_dir = support::DaemonState::new();
    let repo = repo();
    admit(state_dir.path(), repo.path());
    assert_turn_scoped(&budget_segment(state_dir.path()));
}

#[test]
fn an_explicit_config_keeps_the_task_lifetime_bounded() {
    let state_dir = support::DaemonState::new();
    std::fs::write(
        state_dir.path().join("config.json"),
        r#"{"policy": {"preset": "balanced"}}"#,
    )
    .unwrap();
    let repo = repo();
    admit(state_dir.path(), repo.path());
    assert_lifetime_bounded(&budget_segment(state_dir.path()));
}

#[test]
fn a_malformed_config_keeps_the_task_lifetime_bounded() {
    let state_dir = support::DaemonState::new();
    std::fs::write(state_dir.path().join("config.json"), b"{ not json").unwrap();
    let repo = repo();
    admit(state_dir.path(), repo.path());
    assert_lifetime_bounded(&budget_segment(state_dir.path()));
}

/// An empty config file is still a config file: only a daemon with no
/// `config.json` at all is turn-scoped (ADR-0024).
#[test]
fn an_empty_config_file_keeps_the_task_lifetime_bounded() {
    let state_dir = support::DaemonState::new();
    std::fs::write(state_dir.path().join("config.json"), b"{}").unwrap();
    let repo = repo();
    admit(state_dir.path(), repo.path());
    assert_lifetime_bounded(&budget_segment(state_dir.path()));
}

/// The scope is persisted with the task, so a config file written after the
/// task was created changes nothing for it.
#[test]
fn a_config_added_after_admission_does_not_rescope_the_task() {
    let state_dir = support::DaemonState::new();
    let repo = repo();
    admit(state_dir.path(), repo.path());
    assert_turn_scoped(&budget_segment(state_dir.path()));

    std::fs::write(
        state_dir.path().join("config.json"),
        r#"{"policy": {"preset": "balanced"}}"#,
    )
    .unwrap();
    assert_turn_scoped(&budget_segment(state_dir.path()));
}
