//! `kill`/pidfile denial — part of T9/T10's cross-UID surface
//! (ADR-0019 §3, §6 PR-1: "`kill`/pidfile denial" is explicitly called
//! out as in scope for PR-1 since it needs no design-dependent code,
//! only the OS-permission boundary). `kill(2)` against the provider
//! must fail with `EPERM`, and the agent must not be able to read the
//! provider's pidfile (it lives inside the `0700` state directory).

use libra_governor_two_principal_harness::{
    assert_disposable, positive_control, read_pidfile_as_root, reset_state_dirs, run_as, LG_AGENT,
    LG_PROVIDER, PROVIDER_STATE_DIR, SAMEUID_STATE_DIR,
};

const PATH_ENV: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

#[test]
#[ignore = "runs only inside harness/two-principal/run.sh's disposable container"]
fn agent_kill_against_the_provider_gets_eperm_and_the_pidfile_is_unreadable() {
    assert_disposable();
    reset_state_dirs();

    // Sensitivity control: lg-agent killing its OWN same-UID daemon, and
    // reading its OWN pidfile, must both succeed.
    let (mut sameuid_daemon, _) = positive_control(&LG_AGENT, SAMEUID_STATE_DIR, "/home/lg-agent");
    let sameuid_pid = sameuid_daemon.pid;
    assert!(
        read_pidfile_as_root(SAMEUID_STATE_DIR).is_some(),
        "sensitivity fixture: a pidfile must exist"
    );
    let sameuid_pidfile_read = run_as(
        &LG_AGENT,
        "cat",
        &[&format!("{SAMEUID_STATE_DIR}/daemon.pid")],
        &[("PATH", PATH_ENV), ("HOME", "/home/lg-agent")],
    );
    assert!(
        sameuid_pidfile_read.status.success(),
        "sensitivity control: lg-agent must be able to read its own pidfile"
    );
    let sameuid_kill = run_as(
        &LG_AGENT,
        "kill",
        &["-9", &sameuid_pid.to_string()],
        &[("PATH", PATH_ENV), ("HOME", "/home/lg-agent")],
    );
    assert!(
        sameuid_kill.status.success(),
        "sensitivity control: lg-agent must be able to kill its own same-UID daemon"
    );
    sameuid_daemon.kill9_and_reap(); // idempotent cleanup if the kill above raced

    // Positive control + two-principal run.
    let (mut daemon, _task_id) = positive_control(&LG_PROVIDER, PROVIDER_STATE_DIR, "/root");
    let pid = daemon.pid;

    let pidfile_read = run_as(
        &LG_AGENT,
        "cat",
        &[&format!("{PROVIDER_STATE_DIR}/daemon.pid")],
        &[("PATH", PATH_ENV), ("HOME", "/home/lg-agent")],
    );
    assert!(
        !pidfile_read.status.success(),
        "lg-agent must NOT be able to read the provider's pidfile"
    );

    let kill_attempt = run_as(
        &LG_AGENT,
        "kill",
        &["-9", &pid.to_string()],
        &[("PATH", PATH_ENV), ("HOME", "/home/lg-agent")],
    );
    assert!(
        !kill_attempt.status.success(),
        "lg-agent's kill -9 against the provider must fail"
    );
    let stderr = String::from_utf8_lossy(&kill_attempt.stderr).to_lowercase();
    assert!(
        stderr.contains("not permitted") || stderr.contains("operation not permitted"),
        "expected EPERM, got: {stderr}"
    );

    assert!(
        daemon.is_alive(),
        "the provider must still be alive after the failed kill"
    );
    let root_pidfile = read_pidfile_as_root(PROVIDER_STATE_DIR);
    assert!(
        root_pidfile.is_some(),
        "root can still read the pidfile (sanity check)"
    );

    daemon.kill9_and_reap();
}
