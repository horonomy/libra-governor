//! HORO-1618: a `--help` appended after a real subcommand must never reach
//! that subcommand's handler. `root_help_version.rs` already covers the
//! root `--help`/`--version` paths by output text; this file instruments
//! actual side effects for representative mutating/live-state subcommands,
//! so a regression that wired `--help` through to the real handler (e.g.
//! adding a `["install", "--help"] => install_cmd::run()` match arm) would
//! fail this test even though the handler would still print something
//! help-shaped on its way to touching real state.
//!
//! Isolation: `LIBRA_GOVERNOR_STATE_DIR` and `LIBRA_GOVERNOR_CLAUDE_DIR`
//! point every state-touching command (`doctor`, `install`, `uninstall`,
//! `daemon stop`) at a fresh empty scratch directory per test. Today,
//! `main.rs`'s dispatch match has no `--help`-suffixed arm for any
//! subcommand, so `<subcommand> --help` always falls through to the
//! generic "unknown or missing subcommand" arm without ever matching
//! `["doctor"]`/`["install"]`/etc. — this test is the regression guard
//! that keeps it that way: if that ever changes, the scratch directory
//! must still end up empty.

use std::path::Path;
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_libra-governor")
}

/// Runs `libra-governor <args> --help`, isolated to a fresh scratch state
/// dir, and asserts the scratch dir was never created or populated.
fn assert_help_suffix_is_side_effect_free(args: &[&str]) {
    let scratch = tempfile::tempdir().expect("create scratch dir");
    let state_dir = scratch.path().join("state");
    let claude_dir = scratch.path().join("dot-claude");

    let mut full_args: Vec<&str> = args.to_vec();
    full_args.push("--help");

    let output = Command::new(bin())
        .args(&full_args)
        .env("LIBRA_GOVERNOR_STATE_DIR", &state_dir)
        .env("LIBRA_GOVERNOR_CLAUDE_DIR", &claude_dir)
        .env("HOME", scratch.path())
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn libra-governor {full_args:?}: {e}"));

    assert!(
        dir_is_empty_or_absent(&state_dir),
        "{full_args:?} touched the state dir ({state_dir:?}) -- a real \
         handler mutated state instead of exiting through help/parser logic"
    );
    assert!(
        dir_is_empty_or_absent(&claude_dir),
        "{full_args:?} touched ~/.claude ({claude_dir:?}) -- install's \
         real handler ran instead of exiting through help/parser logic"
    );
    assert!(
        !String::from_utf8_lossy(&output.stdout).contains("healthy")
            && !String::from_utf8_lossy(&output.stdout).contains("unreachable"),
        "{full_args:?} printed doctor-shaped live diagnostic output: {:?}",
        output.stdout
    );
}

fn dir_is_empty_or_absent(dir: &Path) -> bool {
    match std::fs::read_dir(dir) {
        Ok(mut entries) => entries.next().is_none(),
        Err(_) => true,
    }
}

#[test]
fn doctor_help_does_not_run_the_diagnostic_handler() {
    assert_help_suffix_is_side_effect_free(&["doctor"]);
}

#[test]
fn install_help_does_not_wire_claude_settings() {
    assert_help_suffix_is_side_effect_free(&["install"]);
}

#[test]
fn uninstall_help_does_not_remove_state() {
    assert_help_suffix_is_side_effect_free(&["uninstall"]);
}

#[test]
fn gateway_status_help_does_not_query_the_daemon() {
    assert_help_suffix_is_side_effect_free(&["gateway", "status"]);
}

#[test]
fn daemon_stop_help_does_not_signal_any_process() {
    assert_help_suffix_is_side_effect_free(&["daemon", "stop"]);
}
