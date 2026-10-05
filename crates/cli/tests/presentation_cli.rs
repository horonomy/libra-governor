//! End-to-end tests of `libra-governor statusline presentation`
//! (HORO-1709) against the real binary and a real state directory.
//!
//! The unit suite in `statusline_provider.rs` passes the wording
//! preference in as an argument, deliberately: a renderer that reached
//! for the state directory could not be asserted on without the suite
//! depending on whichever preference the developer running it happens to
//! have recorded. That leaves exactly one thing unit tests cannot see —
//! whether the recorded file actually reaches the renderer at the process
//! edge — and that is what these tests are for. Mutating
//! `presentation::load()` to ignore the file, or `run_provider` to ignore
//! `load()`, survives the whole unit suite and fails here.
//!
//! Both scenarios use an isolated `LIBRA_GOVERNOR_STATE_DIR`, so they
//! never read or write the preference of whoever is running them.

use std::process::Command;

mod support;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_libra-governor")
}

fn run(args: &[&str], state_dir: &std::path::Path) -> std::process::Output {
    let output = Command::new(bin())
        .args(args)
        .env("LIBRA_GOVERNOR_STATE_DIR", state_dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "`{}` failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn stdout_of(args: &[&str], state_dir: &std::path::Path) -> String {
    String::from_utf8(run(args, state_dir).stdout).unwrap()
}

/// The `budget` segment of a real provider document.
fn budget_segment(state_dir: &std::path::Path) -> serde_json::Value {
    let raw = stdout_of(&["statusline", "provider"], state_dir);
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

/// Reading back a preference must report what was recorded, and an
/// untouched install must say so rather than present the default as a
/// choice someone made — the distinction HORO-1709's upgrade story rests
/// on.
#[test]
fn a_recorded_choice_is_what_the_next_read_reports() {
    let state_dir = tempfile::tempdir().unwrap();

    let before = stdout_of(&["statusline", "presentation"], state_dir.path());
    assert!(
        before.contains("budget display: percent"),
        "a fresh install must render the pre-ticket wording: {before}"
    );
    assert!(
        before.contains("nothing recorded"),
        "an absent file is not a choice: {before}"
    );

    for choice in [
        "remaining",
        "remaining+total",
        "used+remaining+total",
        "full",
    ] {
        let recorded = stdout_of(
            &["statusline", "presentation", "--budget-display", choice],
            state_dir.path(),
        );
        assert!(
            recorded.contains(&format!("budget display: {choice}")),
            "recording {choice} did not confirm it: {recorded}"
        );

        let read_back = stdout_of(&["statusline", "presentation"], state_dir.path());
        assert!(
            read_back.contains(&format!("budget display: {choice}")),
            "{choice} did not survive the round trip through the file: {read_back}"
        );
        assert!(
            !read_back.contains("nothing recorded"),
            "a recorded choice must not read back as absent: {read_back}"
        );
    }
}

/// An unknown spelling is refused, and refusing it must not disturb what
/// is already recorded.
#[test]
fn an_unknown_spelling_is_refused_without_changing_the_record() {
    let state_dir = tempfile::tempdir().unwrap();
    run(
        &["statusline", "presentation", "--budget-display", "full"],
        state_dir.path(),
    );

    let output = Command::new(bin())
        .args(["statusline", "presentation", "--budget-display", "amount"])
        .env("LIBRA_GOVERNOR_STATE_DIR", state_dir.path())
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(2),
        "a typo must be reported, not guessed at"
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("used+remaining+total"),
        "the refusal must name the accepted spellings: {stderr}"
    );

    let read_back = stdout_of(&["statusline", "presentation"], state_dir.path());
    assert!(
        read_back.contains("budget display: full"),
        "a refused argument overwrote the record: {read_back}"
    );
}

/// The one thing the unit suite cannot see: the recorded file reaching
/// the renderer, through a real daemon, a real admitted task and a real
/// `statusline provider` process.
///
/// Asserts on the *shape* of the change rather than on particular
/// figures — the envelope depends on the policy this build ships and on
/// what the estimator makes of a fresh history, neither of which this
/// test is about. What it is about: the preference moves the document,
/// and the percentage it moves does not change while it does.
#[test]
fn the_recorded_preference_reaches_the_real_statusline_provider() {
    let state_dir = support::DaemonState::new();
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("Cargo.toml"), "[package]\nname=\"x\"").unwrap();

    // `statusline provider` never spawns the daemon, so a task has to be
    // admitted through the hook first — otherwise there is no active-task
    // envelope to word.
    let payload = serde_json::json!({
        "session_id": "presentation-session",
        "cwd": repo.path(),
        "prompt": "fix the login bug",
    })
    .to_string();
    let mut child = Command::new(bin())
        .args(["hook", "user-prompt-submit"])
        .env("LIBRA_GOVERNOR_STATE_DIR", state_dir.path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(child.stdin.as_mut().unwrap(), payload.as_bytes()).unwrap();
    assert!(child.wait_with_output().unwrap().status.success());

    let default = budget_segment(state_dir.path());
    let default_label = default["label"].as_str().unwrap().to_string();
    let share = default_label
        .split('%')
        .next()
        .expect("a budget label always leads with its share")
        .to_string();
    assert_eq!(
        default_label,
        format!("{share}% budget left"),
        "the default wording must be the line that shipped before HORO-1709"
    );
    assert!(
        default["count"].is_null() && default["total"].is_null(),
        "the default wording must not add amount fields: {default}"
    );
    assert!(
        !default["budget_used"].is_null(),
        "the structured breakdown is not gated on the wording: {default}"
    );

    run(
        &["statusline", "presentation", "--budget-display", "full"],
        state_dir.path(),
    );

    let chosen = budget_segment(state_dir.path());
    let chosen_label = chosen["label"].as_str().unwrap();
    assert_ne!(
        chosen_label, default_label,
        "the recorded preference never reached the renderer"
    );
    assert!(
        chosen_label.starts_with(&format!("{share}% left,")),
        "the share must not move when only the wording does: {chosen_label}"
    );
    assert!(
        chosen_label.contains(" used"),
        "`full` must name settled spend: {chosen_label}"
    );
    assert_eq!(
        chosen["count_label"],
        serde_json::json!("tokens"),
        "a count must arrive with the noun that says what it counts: {chosen}"
    );
    assert!(
        chosen["count"].is_u64() && chosen["total"].is_u64(),
        "`full` must add the contract's own amount fields: {chosen}"
    );
    assert_eq!(
        chosen["budget_total"], default["budget_total"],
        "the envelope itself must not move with a wording preference"
    );

    // A wording preference must never be able to cost the document: it is
    // read with every failure collapsing to the default.
    std::fs::write(
        state_dir.path().join("presentation.json"),
        b"{ this is not json",
    )
    .unwrap();
    let broken = budget_segment(state_dir.path());
    assert_eq!(
        broken["label"].as_str().unwrap(),
        default_label,
        "a broken preference file must cost the wording, not the segment"
    );
}
