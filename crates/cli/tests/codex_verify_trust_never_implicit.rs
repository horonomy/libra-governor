//! Regression test for ADR-0015 requirement 1/2 (HORO-1906): passive
//! `--help`/`list`/inspection output must never launch Codex
//! `app-server`, under any circumstance. Starting `app-server` is only
//! ever authorized from the one explicit, operator-invoked `doctor
//! --verify-trust` subcommand.
//!
//! This is a *behavioral* regression test, not a source-text scan: it
//! points `LIBRA_GOVERNOR_CODEX_BINARY` at a canary script that leaves a
//! marker file behind the instant it is ever executed, runs every
//! passive/informational subcommand this binary exposes, and asserts
//! the marker was never created. A positive-control case at the end
//! (`doctor --verify-trust` itself) proves the canary is actually
//! capable of catching an implicit launch, rather than passing only
//! because the canary itself is unreachable for some unrelated reason.

use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_libra-governor")
}

/// A `sh` script that touches `$CANARY_MARKER` (an absolute path passed
/// via env, never argv, so it also doubles as evidence that nothing
/// reads/relies on argv shape) the instant it is executed, regardless of
/// how it is invoked, then exits non-zero so nothing downstream could
/// mistake it for a successful app-server session.
fn write_canary(dir: &std::path::Path) -> std::path::PathBuf {
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("canary-codex.sh");
    let mut file = std::fs::File::create(&path).unwrap();
    writeln!(file, "#!/bin/sh\ntouch \"$CANARY_MARKER\"\nexit 1").unwrap();
    drop(file);
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o700);
    std::fs::set_permissions(&path, perms).unwrap();
    path
}

struct Fixture {
    _dir: tempfile::TempDir,
    canary: std::path::PathBuf,
    marker: std::path::PathBuf,
    codex_home: std::path::PathBuf,
    state_dir: std::path::PathBuf,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let canary = write_canary(dir.path());
    let marker = dir.path().join("spawned.marker");
    let codex_home = dir.path().join("codex_home");
    let state_dir = dir.path().join("state_dir");
    std::fs::create_dir_all(&codex_home).unwrap();
    std::fs::create_dir_all(&state_dir).unwrap();
    Fixture {
        _dir: dir,
        canary,
        marker,
        codex_home,
        state_dir,
    }
}

fn run(fixture: &Fixture, args: &[&str]) -> std::process::Output {
    Command::new(bin())
        .args(args)
        .env("LIBRA_GOVERNOR_STATE_DIR", &fixture.state_dir)
        .env("LIBRA_GOVERNOR_CODEX_HOME", &fixture.codex_home)
        .env("LIBRA_GOVERNOR_CODEX_BINARY", &fixture.canary)
        .env("CANARY_MARKER", &fixture.marker)
        .output()
        .expect("spawn libra-governor")
}

#[test]
fn passive_surfaces_never_touch_the_canary_codex_binary() {
    let fixture = fixture();
    // Wire real hooks first so `doctor`'s own codex-hooks finding has
    // something to inspect — this test must hold even when hooks ARE
    // installed, not only in the (trivially passing) case where there
    // is nothing to check.
    let install_output = run(&fixture, &["install", "--agent", "codex"]);
    assert!(
        install_output.status.success(),
        "install --agent codex should succeed in the fixture: {:?}",
        install_output
    );
    assert!(
        !fixture.marker.exists(),
        "install --agent codex only writes hooks.json; it must never itself start app-server"
    );

    for args in [
        vec!["--help"],
        vec!["-h"],
        vec!["help"],
        vec!["--version"],
        vec!["-V"],
        vec!["version"],
        vec!["doctor"],
        vec!["doctor", "--json"],
        vec!["agents"],
        vec!["agents", "--json"],
        vec!["not-a-real-subcommand"],
    ] {
        run(&fixture, &args);
        assert!(
            !fixture.marker.exists(),
            "{args:?} must never start app-server (canary marker was created)"
        );
    }
}

/// Positive control: proves the canary and marker mechanism actually
/// works — without this, the test above could pass vacuously if the
/// canary were unreachable for an unrelated reason (wrong path, exec
/// bit lost, etc).
#[test]
fn the_one_explicit_verify_trust_subcommand_does_reach_the_canary() {
    let fixture = fixture();
    let install_output = run(&fixture, &["install", "--agent", "codex"]);
    assert!(install_output.status.success());
    run(&fixture, &["doctor", "--verify-trust"]);
    assert!(
        fixture.marker.exists(),
        "doctor --verify-trust should reach the configured codex binary — if this fails, the \
         test above may be passing for the wrong reason"
    );
}
