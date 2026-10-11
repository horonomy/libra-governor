//! T9 — the `/proc`, `ptrace` and `sudo` parts of "credential or
//! signing-oracle misuse" (ADR-0019 §3, §6 PR-1). No signing oracle
//! exists yet (that is PR-3's `AttestOutcome`), so PR-1 proves only the
//! kernel-enforced cross-UID denials this threat also depends on:
//! `/proc/<pid>/environ`, `/proc/<pid>/mem`, `ptrace(2)`, and `sudo`.
//!
//! Every ptrace assertion prints `/proc/sys/kernel/yama/ptrace_scope` so
//! a reviewer can see whether denial came from the UID boundary, from
//! Yama, or (in the sensitivity control) neither — on this harness's
//! own development environment (Docker Desktop's Linux VM on macOS),
//! that file does not exist at all (Yama is not compiled in), so denial
//! there is attributable to the UID boundary alone; a `ubuntu-latest`
//! GitHub Actions runner may have Yama present at its default
//! restricted scope (1), in which case the sensitivity control
//! deliberately ptraces a *child* of the attacking process (always
//! permitted under scope 1) rather than an unrelated same-UID process,
//! so it is not itself blocked by Yama.

use libra_governor_two_principal_harness::{
    assert_disposable, positive_control, reset_state_dirs, run_as, yama_ptrace_scope, LG_AGENT,
    LG_PROVIDER, LG_SUDOPROBE, PROVIDER_STATE_DIR,
};

const PATH_ENV: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

#[test]
#[ignore = "runs only inside harness/two-principal/run.sh's disposable container"]
fn t09a_agent_cannot_read_providers_proc_environ_or_mem() {
    assert_disposable();
    reset_state_dirs();

    let (mut daemon, _task_id) = positive_control(&LG_PROVIDER, PROVIDER_STATE_DIR, "/root");
    let pid = daemon.pid;

    // Sensitivity control: lg-agent reading its OWN /proc/<pid>/environ
    // must succeed — same mechanism, same UID.
    let self_read = run_as(
        &LG_AGENT,
        "sh",
        &["-c", "cat /proc/$$/environ > /dev/null"],
        &[("PATH", PATH_ENV), ("HOME", "/home/lg-agent")],
    );
    assert!(
        self_read.status.success(),
        "sensitivity control: lg-agent must be able to read its own /proc/<pid>/environ"
    );

    for name in ["environ", "mem"] {
        let attack = run_as(
            &LG_AGENT,
            "cat",
            &[&format!("/proc/{pid}/{name}")],
            &[("PATH", PATH_ENV), ("HOME", "/home/lg-agent")],
        );
        assert!(
            !attack.status.success(),
            "lg-agent must NOT be able to read the provider's /proc/{pid}/{name}"
        );
    }

    daemon.kill9_and_reap();
}

#[test]
#[ignore = "runs only inside harness/two-principal/run.sh's disposable container"]
fn t09b_agent_cannot_ptrace_the_provider() {
    assert_disposable();
    reset_state_dirs();

    let scope = yama_ptrace_scope();
    println!("t09b: /proc/sys/kernel/yama/ptrace_scope = {scope:?}");

    // Sensitivity control: lg-agent ptrace-attaches (via `gdb -p`) to a
    // CHILD of its own attacking process — always permitted regardless
    // of Yama's default restricted scope (1), and clearly same-UID —
    // proving the ptrace mechanism itself is live in this container, not
    // blocked by this environment's own seccomp/Yama posture.
    let mut spawn_cmd = libra_governor_two_principal_harness::command_as(
        &LG_AGENT,
        "sleep",
        &["30"],
        &[("PATH", PATH_ENV), ("HOME", "/home/lg-agent")],
    );
    let mut own_child = spawn_cmd.spawn().expect("spawn agent's own sleep child");
    let own_child_pid = own_child.id();
    std::thread::sleep(std::time::Duration::from_millis(100));

    let sensitivity_attach = run_as(
        &LG_AGENT,
        "gdb",
        &[
            "-nx",
            "-batch",
            "-p",
            &own_child_pid.to_string(),
            "-ex",
            "detach",
            "-ex",
            "quit",
        ],
        &[("PATH", PATH_ENV), ("HOME", "/home/lg-agent")],
    );
    assert!(
        sensitivity_attach.status.success(),
        "sensitivity control: lg-agent must be able to ptrace its own child; stderr={}",
        String::from_utf8_lossy(&sensitivity_attach.stderr)
    );

    own_child.kill().ok();
    own_child.wait().ok();

    // Real attempt: lg-agent against the provider. `gdb`'s own exit code
    // is not treated as the sole oracle (its exit path for an attach
    // failure is not always distinct from other gdb-internal failures)
    // — the oracle is the kernel's own denial text in stderr, plus the
    // daemon remaining alive and unaffected.
    let (mut daemon, _task_id) = positive_control(&LG_PROVIDER, PROVIDER_STATE_DIR, "/root");
    let pid = daemon.pid;

    let attack = run_as(
        &LG_AGENT,
        "gdb",
        &[
            "-nx",
            "-batch",
            "-p",
            &pid.to_string(),
            "-ex",
            "detach",
            "-ex",
            "quit",
        ],
        &[("PATH", PATH_ENV), ("HOME", "/home/lg-agent")],
    );
    let stderr = String::from_utf8_lossy(&attack.stderr).to_lowercase();
    assert!(
        !attack.status.success()
            || stderr.contains("not permitted")
            || stderr.contains("operation not permitted"),
        "lg-agent's gdb attach must fail; stderr={stderr}"
    );
    assert!(
        daemon.is_alive(),
        "the provider must be unaffected by the failed ptrace attempt"
    );

    daemon.kill9_and_reap();
}

#[test]
#[ignore = "runs only inside harness/two-principal/run.sh's disposable container"]
fn t09c_agent_has_no_sudo_to_the_provider_identity() {
    use libra_governor_two_principal_harness::run_as_allow_setuid;

    assert_disposable();

    // Both principals run WITHOUT `--no-new-privs` for this one test —
    // confirmed empirically that NNP blocks sudo's own setuid-root
    // escalation unconditionally (sudo prints "the 'no new privileges'
    // flag is set", not a policy decision), which would make even
    // lg-sudoprobe's legitimate grant fail and make lg-agent's denial
    // attributable to NNP rather than to having no grant. See
    // `command_as_allow_setuid`'s doc comment.

    // Sensitivity control: lg-sudoprobe DOES have a container-only
    // NOPASSWD grant (provision.sh) and must succeed — proving `sudo`
    // itself functions in this environment, so lg-agent's failure below
    // is attributable to having no grant, not to `sudo` being broken or
    // absent.
    let probe = run_as_allow_setuid(
        &LG_SUDOPROBE,
        "sudo",
        &["-n", "-u", "lg-provider", "true"],
        &[("PATH", PATH_ENV), ("HOME", "/home/lg-sudoprobe")],
    );
    assert!(
        probe.status.success(),
        "sensitivity control: lg-sudoprobe's NOPASSWD grant must work; stderr={}",
        String::from_utf8_lossy(&probe.stderr)
    );

    for args in [vec!["-n", "true"], vec!["-n", "-u", "lg-provider", "true"]] {
        let attack = run_as_allow_setuid(
            &LG_AGENT,
            "sudo",
            &args,
            &[("PATH", PATH_ENV), ("HOME", "/home/lg-agent")],
        );
        assert!(
            !attack.status.success(),
            "lg-agent must NOT have sudo of any kind (args={args:?})"
        );
        let stderr = String::from_utf8_lossy(&attack.stderr).to_lowercase();
        assert!(
            stderr.contains("not allowed")
                || stderr.contains("a password is required")
                || stderr.contains("no password was provided")
                || stderr.contains("not in the sudoers file"),
            "expected a sudoers-POLICY denial for args={args:?} \
             (not NNP/not-found/some other environment artifact), got: {stderr}"
        );
    }
}
