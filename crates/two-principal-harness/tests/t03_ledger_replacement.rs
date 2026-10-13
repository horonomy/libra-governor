//! T3 — ledger replacement (ADR-0019 §3, §6 PR-1).
//!
//! Defense: the provider's state directory is `0700`, owned by
//! `lg-provider`, with root-owned ancestors, so `mv`/`ln -s` against the
//! ledger file are impossible for `lg-agent`. PR-1 proves exactly this
//! OS-permission boundary alone, with today's unmodified binary — there
//! is no trusted-principal mode or peer-credential check yet.
//!
//! All harness tests are `#[ignore]` by default: `cargo test --workspace`
//! on a real host never runs them, and running with `--ignored` alone
//! (no disposable marker) panics immediately via `assert_disposable`.

use std::path::Path;

use libra_governor_two_principal_harness::{
    assert_disposable, positive_control, reset_state_dirs, run_as, LedgerSnapshot, LG_AGENT,
    LG_PROVIDER, PROVIDER_STATE_DIR, SAMEUID_STATE_DIR,
};

const PATH_ENV: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

fn attack_mv(state_dir: &str, agent_home: &str) -> std::process::Output {
    run_as(
        &LG_AGENT,
        "mv",
        &[
            &format!("{state_dir}/ledger.sqlite3"),
            &format!("{state_dir}/ledger.sqlite3.pwned"),
        ],
        &[("PATH", PATH_ENV), ("HOME", agent_home)],
    )
}

fn denied(out: &std::process::Output) -> bool {
    !out.status.success() && {
        let stderr = String::from_utf8_lossy(&out.stderr).to_lowercase();
        stderr.contains("permission denied") || stderr.contains("not permitted")
    }
}

#[test]
#[ignore = "runs only inside harness/two-principal/run.sh's disposable container"]
fn t03a_agent_cannot_mv_the_ledger_out_from_under_the_provider() {
    assert_disposable();
    reset_state_dirs();

    // Pass 2 first: sensitivity control — the SAME attack against the
    // agent-owned mirror layout must succeed, proving `mv` itself works
    // in this container and the test is not vacuous.
    let (mut sameuid_daemon, _) = positive_control(&LG_AGENT, SAMEUID_STATE_DIR, "/home/lg-agent");
    sameuid_daemon.kill9_and_reap();
    let sameuid_mv = attack_mv(SAMEUID_STATE_DIR, "/home/lg-agent");
    assert!(
        sameuid_mv.status.success(),
        "sensitivity control: lg-agent must be able to mv its own ledger; stderr={}",
        String::from_utf8_lossy(&sameuid_mv.stderr)
    );

    // Pass 1 + 3: positive control, then the real two-principal attack.
    let (mut daemon, _task_id) = positive_control(&LG_PROVIDER, PROVIDER_STATE_DIR, "/root");
    daemon.kill9_and_reap();
    let before = LedgerSnapshot::capture(Path::new(PROVIDER_STATE_DIR));
    assert!(before.exists, "positive control must have created a ledger");
    assert!(
        !before.triggers.is_empty(),
        "ledger must already carry its append-only triggers"
    );

    let attack = attack_mv(PROVIDER_STATE_DIR, "/home/lg-agent");
    assert!(
        denied(&attack),
        "lg-agent must NOT be able to mv the provider's ledger; got status={:?} stderr={}",
        attack.status,
        String::from_utf8_lossy(&attack.stderr)
    );

    let after = LedgerSnapshot::capture(Path::new(PROVIDER_STATE_DIR));
    assert_eq!(
        before, after,
        "ledger snapshot must be unchanged after the failed attack"
    );
}

#[test]
#[ignore = "runs only inside harness/two-principal/run.sh's disposable container"]
fn t03b_agent_cannot_symlink_swap_the_ledger_path() {
    assert_disposable();
    reset_state_dirs();

    let (mut sameuid_daemon, _) = positive_control(&LG_AGENT, SAMEUID_STATE_DIR, "/home/lg-agent");
    sameuid_daemon.kill9_and_reap();
    let sameuid_ln = run_as(
        &LG_AGENT,
        "ln",
        &[
            "-sf",
            "/dev/null",
            &format!("{SAMEUID_STATE_DIR}/ledger.sqlite3"),
        ],
        &[("PATH", PATH_ENV), ("HOME", "/home/lg-agent")],
    );
    assert!(
        sameuid_ln.status.success(),
        "sensitivity control: lg-agent must be able to symlink-swap its own ledger path"
    );

    let (mut daemon, _task_id) = positive_control(&LG_PROVIDER, PROVIDER_STATE_DIR, "/root");
    daemon.kill9_and_reap();
    let before = LedgerSnapshot::capture(Path::new(PROVIDER_STATE_DIR));

    let attack = run_as(
        &LG_AGENT,
        "ln",
        &[
            "-sf",
            "/dev/null",
            &format!("{PROVIDER_STATE_DIR}/ledger.sqlite3"),
        ],
        &[("PATH", PATH_ENV), ("HOME", "/home/lg-agent")],
    );
    assert!(
        denied(&attack),
        "lg-agent must NOT be able to symlink-swap the provider's ledger path; stderr={}",
        String::from_utf8_lossy(&attack.stderr)
    );

    let after = LedgerSnapshot::capture(Path::new(PROVIDER_STATE_DIR));
    assert_eq!(
        before, after,
        "ledger snapshot must be unchanged after the failed attack"
    );
}

#[test]
#[ignore = "runs only inside harness/two-principal/run.sh's disposable container"]
fn t03c_agent_cannot_plant_a_ledger_before_first_start() {
    assert_disposable();
    reset_state_dirs();

    // The provider-owned directory already exists (0700, lg-provider),
    // created by `reset_state_dirs` exactly as root would before first
    // start — so planting a file inside it is already the two-principal
    // case, no separate sensitivity pass needed: an agent that cannot
    // create a file in a `0700` directory it does not own cannot "plant"
    // anything regardless of ledger-specific logic.
    let attack = run_as(
        &LG_AGENT,
        "sh",
        &[
            "-c",
            &format!("echo pwned > {PROVIDER_STATE_DIR}/ledger.sqlite3"),
        ],
        &[("PATH", PATH_ENV), ("HOME", "/home/lg-agent")],
    );
    assert!(
        !attack.status.success(),
        "lg-agent must NOT be able to create a file inside the provider's state dir"
    );
    assert!(
        !Path::new(PROVIDER_STATE_DIR)
            .join("ledger.sqlite3")
            .exists(),
        "no planted ledger file should exist after the failed attack"
    );
}
